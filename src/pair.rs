//! `siri-remote pair` — scan for a Siri Remote in pairing mode, bond it, dump
//! the GATT tree, hold the link open briefly.

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use btleplug::api::{Central, CharPropFlags, Manager as _, Peripheral as _};
use btleplug::platform::Manager;

use crate::cli::PairArgs;
use crate::scan;

const OVERALL_SCAN_TIMEOUT: Duration = Duration::from_secs(60);

pub async fn run(args: PairArgs) -> Result<u8> {
    if args.scan_seconds < 0.0 {
        anyhow::bail!("--scan-seconds must be non-negative");
    }
    if args.hold_seconds < 0.0 {
        anyhow::bail!("--hold-seconds must be non-negative");
    }

    let manager = Manager::new().await.context("init BLE manager")?;
    let adapters = manager.adapters().await.context("list BLE adapters")?;
    let adapter = adapters
        .into_iter()
        .next()
        .context("no BLE adapter found on this host")?;

    let candidate = match scan::scan_for_remote(
        &adapter,
        Duration::from_secs_f64(args.scan_seconds),
        OVERALL_SCAN_TIMEOUT,
    )
    .await
    {
        Ok(c) => c,
        Err(scan::ScanError::Timeout) => {
            eprintln!(
                "Timed out waiting for a Siri Remote. Make sure it's in pairing mode \
                 (MENU + Volume Up held for ~5s) and within reach (RSSI >= -55)."
            );
            return Ok(1);
        }
        Err(scan::ScanError::Other(e)) => return Err(e),
    };

    let peripheral = adapter
        .peripheral(&candidate.peripheral_id)
        .await
        .context("get peripheral handle for selected candidate")?;
    let identity = candidate.identity_address.clone();

    eprintln!(
        "\nConnecting to {} (with pairing) ...",
        candidate.last_address
    );

    if let Err(detail) = pair_link(&peripheral, &candidate).await {
        eprintln!("{}", retry_message(&format!("{detail:?}")));
        return Ok(2);
    }
    eprintln!("Connected and paired.");

    #[cfg(target_os = "linux")]
    {
        let conn = crate::bluez::device::connect().await?;
        let dev_path =
            match crate::bluez::device::device_path_from_address(&conn, &candidate.last_address)
                .await
            {
                Ok(p) => p,
                // Fall back to identity address: BlueZ re-keys the device under
                // its resolved identity after pairing, so the random/current
                // advertising address may no longer be present.
                Err(_) => crate::bluez::device::device_path_from_address(&conn, &identity).await?,
            };
        if !crate::bluez::device::is_bonded(&conn, &dev_path)
            .await
            .unwrap_or(false)
        {
            eprintln!(
                "{}",
                retry_message(&format!(
                    "connect+pair reported success but BlueZ does not list {identity} as bonded"
                ))
            );
            return Ok(2);
        }
    }

    peripheral
        .discover_services()
        .await
        .context("discover services")?;

    eprintln!("Identity address (from advertisement manufacturer data): {identity}");
    eprintln!(
        "BlueZ now bonds the remote under this stable address (IRK exchange). \
         Use it for future reconnects; the random advertising address no longer matters."
    );

    println!("\nGATT services:");
    for service in peripheral.services() {
        println!("Service {}", service.uuid);
        for char in service.characteristics {
            let props = describe_properties(char.properties);
            println!("  Char  {}  [{}]", char.uuid, props);
            for desc in char.descriptors {
                println!("    Desc  {}", desc.uuid);
            }
        }
    }

    eprintln!(
        "\nPaired and connected. Keeping the connection open for {:.0}s...",
        args.hold_seconds
    );
    tokio::time::sleep(Duration::from_secs_f64(args.hold_seconds)).await;
    Ok(0)
}

#[cfg(target_os = "linux")]
async fn pair_link(
    peripheral: &btleplug::platform::Peripheral,
    candidate: &scan::Candidate,
) -> Result<()> {
    // The agent must outlive the Pair() call. It auto-confirms the numeric-
    // comparison request that the remote sends; without it, BlueZ rejects the
    // pairing with "Operation not permitted".
    let agent = crate::bluez::agent::AgentSession::register().await?;
    let conn = agent.connection().clone();

    let dev_path = crate::bluez::device::device_path_from_address(&conn, &candidate.last_address)
        .await
        .map_err(|e| anyhow!("locate BlueZ device path: {e}"))?;

    crate::bluez::device::pair_explicit(&conn, &dev_path)
        .await
        .map_err(|e| anyhow!("Device1.Pair: {e}"))?;

    peripheral.connect().await.context("Device1.Connect")?;

    // We deliberately keep the agent registered for the lifetime of the
    // connect; closing it explicitly here lets the unregister await rather
    // than relying on Drop's best-effort spawn.
    agent.close().await;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
async fn pair_link(
    peripheral: &btleplug::platform::Peripheral,
    _candidate: &scan::Candidate,
) -> Result<()> {
    // Off Linux the host OS surfaces its own pairing UI when the link comes up.
    peripheral.connect().await.context("connect")?;
    Ok(())
}

fn describe_properties(p: CharPropFlags) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if p.contains(CharPropFlags::BROADCAST) {
        parts.push("broadcast");
    }
    if p.contains(CharPropFlags::READ) {
        parts.push("read");
    }
    if p.contains(CharPropFlags::WRITE_WITHOUT_RESPONSE) {
        parts.push("write-without-response");
    }
    if p.contains(CharPropFlags::WRITE) {
        parts.push("write");
    }
    if p.contains(CharPropFlags::NOTIFY) {
        parts.push("notify");
    }
    if p.contains(CharPropFlags::INDICATE) {
        parts.push("indicate");
    }
    if p.contains(CharPropFlags::AUTHENTICATED_SIGNED_WRITES) {
        parts.push("authenticated-signed-writes");
    }
    if p.contains(CharPropFlags::EXTENDED_PROPERTIES) {
        parts.push("extended-properties");
    }
    parts.join(",")
}

fn retry_message(detail: &str) -> String {
    format!(
        "\nPair failed: {detail}\n\n\
         This is a known BlueZ flake with Apple HID peripherals. Once a pair\n\
         attempt fails on Linux, BlueZ removes the device entry and the BLE\n\
         address has likely also rotated, so an in-process retry can't recover.\n\
         \nTo recover:\n\
         \x20 1. Put the remote back in pairing mode (hold MENU + Volume Up ~5s).\n\
         \x20 2. Re-run: siri-remote pair\n\
         \nIf this keeps happening, restart bluetoothd to clear any stuck\n\
         session state:  systemctl restart bluetooth"
    )
}
