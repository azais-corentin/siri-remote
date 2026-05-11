//! `siri-remote events` — connect a bonded (or pairing-mode) Siri Remote and
//! stream battery / power / HID notifications to stdout.

use std::io::Write as _;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use btleplug::api::{
    Central, CharPropFlags, Characteristic, Manager as _, Peripheral as _, ScanFilter,
};
use btleplug::platform::{Adapter, Manager, Peripheral, PeripheralId};
use futures::StreamExt;
use tokio::time::{Instant, timeout};
use uuid::Uuid;

use crate::cli::EventsArgs;
use crate::decoder::{InputDecoder, format_battery, format_event, format_power, raw_hex};
use crate::hid::{
    BATTERY_LEVEL_UUID, BATTERY_POWER_UUID, HID_REPORT_UUID, all_characteristics, find_by_uuid,
};
#[cfg(target_os = "linux")]
use crate::bluez;
use crate::scan;

const AUTH_FAILURE_MARKERS: &[&str] = &[
    "auth",
    "encrypt",
    "not authorized",
    "not permitted",
    "insufficient authentication",
    "authenticationfailed",
];

const DEVICE_INFO_CHARS: &[(&str, Uuid)] = &[
    (
        "serial",
        Uuid::from_u128(0x0000_2a25_0000_1000_8000_0080_5f9b_34fb),
    ),
    (
        "hardware",
        Uuid::from_u128(0x0000_2a27_0000_1000_8000_0080_5f9b_34fb),
    ),
    (
        "firmware",
        Uuid::from_u128(0x0000_2a26_0000_1000_8000_0080_5f9b_34fb),
    ),
    (
        "manufacturer",
        Uuid::from_u128(0x0000_2a29_0000_1000_8000_0080_5f9b_34fb),
    ),
    (
        "pnp_id",
        Uuid::from_u128(0x0000_2a50_0000_1000_8000_0080_5f9b_34fb),
    ),
];

const ADDRESS_RESOLVE_TIMEOUT: Duration = Duration::from_secs(10);
const DIRECT_CONNECT_TIMEOUT: Duration = Duration::from_secs(60);
const PAIRED_CONNECT_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone, Debug)]
struct Selection {
    address: String,
    name: String,
    peripheral_id: Option<PeripheralId>,
    identity_address: Option<String>,
    requires_pairing: bool,
    rssi: Option<i16>,
}

pub async fn run(args: EventsArgs) -> Result<u8> {
    if args.scan_seconds < 0.0 {
        anyhow::bail!("--scan-seconds must be non-negative");
    }
    if args.reconnect_delay < 0.0 {
        anyhow::bail!("--reconnect-delay must be non-negative");
    }

    let manager = Manager::new().await.context("init BLE manager")?;
    let adapters = manager.adapters().await.context("list BLE adapters")?;
    let adapter = adapters
        .into_iter()
        .next()
        .context("no BLE adapter found on this host")?;

    let selection = match choose_initial_selection(
        &adapter,
        args.address.as_deref(),
        Duration::from_secs_f64(args.scan_seconds),
    )
    .await
    {
        Ok(s) => s,
        Err(InitError::Invalid(msg)) => {
            eprintln!("{msg}");
            return Ok(2);
        }
        Err(InitError::Timeout) => {
            eprintln!(
                "Timed out waiting for a Siri Remote. If it is unpaired, hold MENU + \
                 Volume Up for pairing mode and keep it close to this host."
            );
            return Ok(1);
        }
    };

    run_forever(
        &adapter,
        selection,
        Duration::from_secs_f64(args.reconnect_delay),
    )
    .await
}

enum InitError {
    Invalid(String),
    Timeout,
}

async fn choose_initial_selection(
    adapter: &Adapter,
    address: Option<&str>,
    scan_dur: Duration,
) -> Result<Selection, InitError> {
    if let Some(addr) = address {
        let normalized = normalize_address(addr).map_err(InitError::Invalid)?;
        let selection = Selection {
            address: normalized.clone(),
            name: "requested address".to_string(),
            peripheral_id: None,
            identity_address: Some(normalized),
            requires_pairing: false,
            rssi: None,
        };
        print_selected(&selection, "requested address; connecting directly");
        return Ok(selection);
    }

    match scan::scan_for_nearest_remote(adapter, scan_dur).await {
        Some(cand) => {
            let selection = Selection {
                address: cand.last_address.clone(),
                name: cand
                    .last_name
                    .clone()
                    .unwrap_or_else(|| cand.identity_address.clone()),
                peripheral_id: Some(cand.peripheral_id),
                identity_address: Some(cand.identity_address),
                requires_pairing: false,
                rssi: Some(cand.last_rssi),
            };
            print_selected(&selection, "strongest currently advertising Siri Remote");
            Ok(selection)
        }
        None => Err(InitError::Timeout),
    }
}

fn normalize_address(addr: &str) -> Result<String, String> {
    let t = addr.trim().to_uppercase();
    let ok = t.len() == 17
        && t.bytes().enumerate().all(|(i, b)| {
            if i % 3 == 2 {
                b == b':'
            } else {
                b.is_ascii_hexdigit()
            }
        });
    if !ok {
        return Err(format!("invalid Bluetooth address: {addr:?}"));
    }
    Ok(t)
}

fn print_selected(selection: &Selection, reason: &str) {
    let rssi = match selection.rssi {
        Some(r) => format!(" rssi={r}"),
        None => String::new(),
    };
    let identity = match &selection.identity_address {
        Some(i) => format!(" identity={i}"),
        None => String::new(),
    };
    eprintln!(
        "Selected {:?} address={}{identity}{rssi} ({reason}).",
        selection.name, selection.address
    );
}

// -- Connect / stream ---------------------------------------------------------

async fn resolve_peripheral(adapter: &Adapter, selection: &Selection) -> Result<Peripheral> {
    if let Some(id) = &selection.peripheral_id
        && let Ok(p) = adapter.peripheral(id).await
    {
        return Ok(p);
    }

    let needle = selection.address.to_uppercase();
    let _ = adapter.start_scan(ScanFilter::default()).await;
    let deadline = Instant::now() + ADDRESS_RESOLVE_TIMEOUT;
    let mut last_err: Option<anyhow::Error> = None;

    while Instant::now() < deadline {
        match adapter.peripherals().await {
            Ok(list) => {
                for p in list {
                    let Ok(Some(props)) = p.properties().await else {
                        continue;
                    };
                    if props.address.to_string().to_uppercase() == needle {
                        let _ = adapter.stop_scan().await;
                        return Ok(p);
                    }
                }
            }
            Err(e) => last_err = Some(anyhow!(e.to_string())),
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let _ = adapter.stop_scan().await;
    match last_err {
        Some(e) => Err(e),
        None => Err(anyhow!(
            "could not find peripheral with address {} within {:.0}s",
            selection.address,
            ADDRESS_RESOLVE_TIMEOUT.as_secs_f64(),
        )),
    }
}

async fn connect_once(adapter: &Adapter, selection: &Selection) -> Result<Selection> {
    let peripheral = resolve_peripheral(adapter, selection).await?;

    if selection.requires_pairing {
        eprintln!("Connecting to {} with pairing...", selection.address);
        connect_with_pairing(&peripheral, selection).await?;
    } else {
        eprintln!("Connecting to {}...", selection.address);
        match timeout(DIRECT_CONNECT_TIMEOUT, peripheral.connect()).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(anyhow!(e.to_string())),
            Err(_) => {
                eprintln!(
                    "Direct connect timed out; retrying with pair=True (uses existing bond if already paired)."
                );
                connect_with_pairing(&peripheral, selection).await?;
            }
        }
    }

    peripheral
        .discover_services()
        .await
        .context("discover services")?;
    stream_connected_client(&peripheral, selection).await?;

    Ok(Selection {
        address: selection
            .identity_address
            .clone()
            .unwrap_or_else(|| selection.address.clone()),
        name: selection.name.clone(),
        peripheral_id: Some(peripheral.id()),
        identity_address: selection.identity_address.clone(),
        requires_pairing: false,
        rssi: None,
    })
}

#[cfg(target_os = "linux")]
async fn connect_with_pairing(peripheral: &Peripheral, selection: &Selection) -> Result<()> {
    let agent = crate::bluez::agent::AgentSession::register().await?;
    let conn = agent.connection().clone();
    let dev_path =
        crate::bluez::device::device_path_from_address(&conn, &selection.address).await?;
    // Best-effort Pair() — if the link is already bonded BlueZ returns
    // AlreadyExists, which we tolerate before connecting.
    let _ = crate::bluez::device::pair_explicit(&conn, &dev_path).await;
    timeout(PAIRED_CONNECT_TIMEOUT, peripheral.connect())
        .await
        .map_err(|_| anyhow!("paired connect timed out"))?
        .map_err(|e| anyhow!(e.to_string()))?;
    agent.close().await;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
async fn connect_with_pairing(peripheral: &Peripheral, _selection: &Selection) -> Result<()> {
    timeout(PAIRED_CONNECT_TIMEOUT, peripheral.connect())
        .await
        .map_err(|_| anyhow!("paired connect timed out"))?
        .map_err(|e| anyhow!(e.to_string()))?;
    Ok(())
}

async fn stream_connected_client(peripheral: &Peripheral, selection: &Selection) -> Result<()> {
    print_device_info(peripheral, selection).await;
    let (mut decoder, mut hid_stream) = configure_notifications(peripheral, selection).await?;
    drain_notifications(peripheral, &mut decoder, &mut hid_stream).await
}

/// Subscribe to battery / power notifications via btleplug (one
/// characteristic per UUID — btleplug handles them fine), then open a
/// BlueZ-D-Bus side-channel for the HID service: enable input streaming
/// by writing the magic byte to every Output report instance, then
/// `StartNotify` on every Input report instance. The latter is the only
/// way to reach the per-instance reports the Siri Remote uses, because
/// btleplug deduplicates same-UUID characteristics within a service.
#[cfg(target_os = "linux")]
async fn configure_notifications(
    peripheral: &Peripheral,
    selection: &Selection,
) -> Result<(InputDecoder, bluez::hid::InputStream)> {
    let battery_char = find_by_uuid(peripheral, BATTERY_LEVEL_UUID);
    let power_char = find_by_uuid(peripheral, BATTERY_POWER_UUID);
    let battery_ok = start_optional_notify(peripheral, battery_char.as_ref(), "battery").await;
    let power_ok = start_optional_notify(peripheral, power_char.as_ref(), "power").await;
    if !battery_ok {
        eprintln!("warning: battery notifications were not enabled");
    }
    if !power_ok {
        eprintln!("warning: power notifications were not enabled");
    }

    let conn = bluez::device::connect().await?;
    let resolve_addr = selection
        .identity_address
        .as_deref()
        .unwrap_or(&selection.address);
    let device_path = bluez::device::device_path_from_address(&conn, resolve_addr).await?;
    let hid = bluez::hid::HidSession::open(conn, &device_path).await?;

    eprintln!(
        "HID service exposes {} report characteristic(s) under {}:",
        hid.reports().len(),
        hid.device_path(),
    );
    for r in hid.reports() {
        let kind = match r.report_type {
            bluez::hid::REPORT_TYPE_INPUT => "input",
            bluez::hid::REPORT_TYPE_OUTPUT => "output",
            bluez::hid::REPORT_TYPE_FEATURE => "feature",
            _ => "unknown",
        };
        eprintln!(
            "  report id=0x{:02X} type={kind:7} flags={}",
            r.report_id,
            r.flags.join(","),
        );
    }

    // Subscribe before writing the magic byte so we don't lose any reports
    // emitted between the write and our StartNotify.
    let input_stream = hid.input_stream().await?;
    hid.write_input_enable(INPUT_ENABLE_BYTE).await?;
    eprintln!("Notifications enabled; waiting for events...");

    Ok((InputDecoder::new(), input_stream))
}

/// Magic byte the Siri Remote needs on any HID Output report before it
/// starts emitting input notifications. Same value the SiriRemote-Linux
/// reference and `events.py` use (`b"\xAF"`).
pub const INPUT_ENABLE_BYTE: u8 = 0xAF;

async fn start_optional_notify(
    peripheral: &Peripheral,
    char: Option<&Characteristic>,
    source: &str,
) -> bool {
    let Some(c) = char else {
        return false;
    };
    if !c.properties.contains(CharPropFlags::NOTIFY)
        && !c.properties.contains(CharPropFlags::INDICATE)
    {
        eprintln!(
            "warning: {source} characteristic {} is not notifiable",
            c.uuid
        );
        return false;
    }
    match peripheral.subscribe(c).await {
        Ok(_) => {
            eprintln!("Enabled {source} notifications on {}.", c.uuid);
            true
        }
        Err(e) => {
            eprintln!(
                "warning: failed to enable {source} notifications on {}: {e:?}",
                c.uuid
            );
            false
        }
    }
}

/// Read both event sources concurrently — btleplug for the battery / power
/// status notifications (those characteristics are unique by UUID, so
/// btleplug handles them) and the BlueZ D-Bus stream for every per-instance
/// HID Input report. Returns when either source closes or a periodic
/// liveness probe shows the peripheral has disconnected.
async fn drain_notifications(
    peripheral: &Peripheral,
    decoder: &mut InputDecoder,
    hid_stream: &mut bluez::hid::InputStream,
) -> Result<()> {
    let mut bt_stream = peripheral.notifications().await?;
    let stdout = std::io::stdout();
    let mut idle = tokio::time::interval(Duration::from_secs(1));
    idle.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            n = bt_stream.next() => {
                let Some(n) = n else { return Ok(()) };
                // HID Report notifications are handled via the BlueZ D-Bus
                // stream so we can address every per-instance Report. The
                // btleplug stream still emits them for the one collapsed
                // 0x2A4D characteristic it knows about — drop those here
                // to avoid printing the same payload twice.
                if n.uuid == HID_REPORT_UUID {
                    continue;
                }
                let source = classify_source(&n.uuid);
                let identifier = format!("uuid={}", n.uuid);
                let line = format_event(source, &identifier, &n.value, Some(decoder));
                let mut h = stdout.lock();
                writeln!(h, "{line}")?;
                h.flush()?;
            }
            r = hid_stream.next_report() => {
                let Some((report_id, value)) = r else { return Ok(()) };
                let identifier = format!("report_id=0x{report_id:02X}");
                // The shipped decoder reads `data[1]` as a button mask and
                // `data[2] == 0x32` as a touch-event marker. That layout
                // matches the gen-1 2-byte button report (and only that) on
                // this firmware; the gen-3 touch/audio reports have very
                // different layouts and the decoder happily produces
                // garbage like "buttons=Volume Up+Play/Pause+Siri+…" out of
                // their phase counters. Run the decoder only on the 2-byte
                // button reports; for everything else emit raw hex so the
                // user still sees the wire data without false labels.
                let line = if value.len() == 2 {
                    format_event("input", &identifier, &value, Some(decoder))
                } else {
                    format!(
                        "{} input {} raw={}",
                        crate::decoder::now_stamp(),
                        identifier,
                        raw_hex(&value),
                    )
                };
                let mut h = stdout.lock();
                writeln!(h, "{line}")?;
                h.flush()?;
            }
            _ = idle.tick() => {
                if !peripheral.is_connected().await.unwrap_or(false) {
                    return Ok(());
                }
            }
        }
    }
}

fn classify_source(uuid: &Uuid) -> &'static str {
    if *uuid == BATTERY_LEVEL_UUID {
        "battery"
    } else if *uuid == BATTERY_POWER_UUID {
        "power"
    } else if *uuid == HID_REPORT_UUID {
        "input"
    } else {
        "other"
    }
}

async fn print_device_info(peripheral: &Peripheral, selection: &Selection) {
    eprintln!("\nConnected Siri Remote");
    eprintln!("  selected_address: {}", selection.address);
    if let Some(id) = &selection.identity_address {
        eprintln!("  identity_address: {id}");
    }
    eprintln!("  selected_name: {}", selection.name);
    eprintln!("  backend: btleplug");

    let services: Vec<_> = peripheral.services().into_iter().collect();
    let chars = all_characteristics(peripheral);
    let desc_count: usize = chars.iter().map(|c| c.descriptors.len()).sum();
    eprintln!(
        "  services: {} chars: {} descriptors: {desc_count}",
        services.len(),
        chars.len()
    );
    for service in &services {
        eprintln!("  Service {}", service.uuid);
        for char in &service.characteristics {
            let props = describe_properties(char.properties);
            eprintln!("    Char {} [{props}]", char.uuid);
        }
    }

    for (label, uuid) in DEVICE_INFO_CHARS {
        let Some(c) = find_by_uuid(peripheral, *uuid) else {
            continue;
        };
        if let Ok(data) = peripheral.read(&c).await {
            eprintln!("  {label}: {}", decode_text_or_hex(&data));
        }
    }

    if let Some(c) = find_by_uuid(peripheral, BATTERY_LEVEL_UUID)
        && let Ok(data) = peripheral.read(&c).await
    {
        eprintln!("  current_{}", format_battery(&data));
    }
    if let Some(c) = find_by_uuid(peripheral, BATTERY_POWER_UUID)
        && let Ok(data) = peripheral.read(&c).await
    {
        eprintln!("  current_{}", format_power(&data));
    }
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

fn decode_text_or_hex(data: &[u8]) -> String {
    let trimmed_end = data
        .iter()
        .rposition(|b| *b != 0)
        .map(|p| p + 1)
        .unwrap_or(0);
    let trimmed = &data[..trimmed_end];
    match std::str::from_utf8(trimmed) {
        Ok(s) if !s.is_empty() => s.to_string(),
        _ => raw_hex(data),
    }
}

// -- Loop ---------------------------------------------------------------------

fn is_auth_failure(err: &anyhow::Error) -> bool {
    let detail = format!("{err:?}").to_lowercase().replace(' ', "");
    AUTH_FAILURE_MARKERS
        .iter()
        .any(|m| detail.contains(&m.replace(' ', "")))
}

async fn switch_to_pairing_scan(adapter: &Adapter) -> Result<Selection, InitError> {
    eprintln!(
        "Connection failed because the link is not bonded/authenticated. \
         Put the remote in pairing mode (hold MENU + Volume Up) and keep it nearby."
    );
    let cand = scan::scan_for_nearest_remote(adapter, Duration::from_secs(5))
        .await
        .ok_or(InitError::Timeout)?;
    let selection = Selection {
        address: cand.last_address.clone(),
        name: cand
            .last_name
            .clone()
            .unwrap_or_else(|| cand.identity_address.clone()),
        peripheral_id: Some(cand.peripheral_id),
        identity_address: Some(cand.identity_address),
        requires_pairing: true,
        rssi: Some(cand.last_rssi),
    };
    print_selected(&selection, "pairing-mode remote");
    Ok(selection)
}

async fn run_forever(
    adapter: &Adapter,
    mut selection: Selection,
    reconnect_delay: Duration,
) -> Result<u8> {
    loop {
        match connect_once(adapter, &selection).await {
            Ok(new_sel) => {
                eprintln!(
                    "Disconnected from {}; reconnecting automatically.",
                    new_sel.address
                );
                selection = new_sel;
            }
            Err(err) => {
                if !selection.requires_pairing && is_auth_failure(&err) {
                    match switch_to_pairing_scan(adapter).await {
                        Ok(s) => {
                            selection = s;
                            continue;
                        }
                        Err(InitError::Timeout) => {
                            eprintln!("Timed out scanning for a pairing-mode remote.");
                            return Ok(1);
                        }
                        Err(InitError::Invalid(msg)) => {
                            eprintln!("{msg}");
                            return Ok(2);
                        }
                    }
                }
                eprintln!("Connection failed: {err:?}; retrying.");
            }
        }
        tokio::time::sleep(reconnect_delay).await;
    }
}
