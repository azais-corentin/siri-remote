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
    BATTERY_LEVEL_UUID, BATTERY_POWER_UUID, HID_REPORT_UUID, all_characteristics, enable_input,
    find_by_uuid, hid_reports, read_report_reference,
};
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
    let mut decoder = configure_notifications(peripheral).await?;
    drain_notifications(peripheral, &mut decoder).await
}

async fn configure_notifications(peripheral: &Peripheral) -> Result<InputDecoder> {
    let decoder = InputDecoder::new();

    let battery_char = find_by_uuid(peripheral, BATTERY_LEVEL_UUID);
    let power_char = find_by_uuid(peripheral, BATTERY_POWER_UUID);

    let battery_ok = start_optional_notify(peripheral, battery_char.as_ref(), "battery").await;
    let power_ok = start_optional_notify(peripheral, power_char.as_ref(), "power").await;

    let input_chars: Vec<Characteristic> = hid_reports(peripheral)
        .into_iter()
        .filter(|c| {
            c.properties.contains(CharPropFlags::NOTIFY)
                || c.properties.contains(CharPropFlags::INDICATE)
        })
        .collect();

    if input_chars.is_empty() {
        anyhow::bail!("no HID input notification reports were discovered");
    }

    let mut input_count = 0usize;
    for char in &input_chars {
        if start_optional_notify(peripheral, Some(char), "input").await {
            let ref_text = match read_report_reference(peripheral, char).await {
                Some(b) => raw_hex(&b),
                None => "unknown".to_string(),
            };
            eprintln!("  input report uuid={} report_ref={ref_text}", char.uuid);
            input_count += 1;
        }
    }

    if !battery_ok {
        eprintln!("warning: battery notifications were not enabled");
    }
    if !power_ok {
        eprintln!("warning: power notifications were not enabled");
    }
    if input_count == 0 {
        anyhow::bail!("no HID input notification reports were discovered");
    }

    enable_input(peripheral).await?;
    eprintln!("Notifications enabled; waiting for events...");
    Ok(decoder)
}

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

async fn drain_notifications(peripheral: &Peripheral, decoder: &mut InputDecoder) -> Result<()> {
    let mut stream = peripheral.notifications().await?;
    let stdout = std::io::stdout();
    loop {
        match timeout(Duration::from_secs(1), stream.next()).await {
            Ok(Some(n)) => {
                let source = classify_source(&n.uuid);
                let identifier = format!("uuid={}", n.uuid);
                let line = format_event(source, &identifier, &n.value, Some(decoder));
                let mut h = stdout.lock();
                writeln!(h, "{line}")?;
                h.flush()?;
            }
            Ok(None) => return Ok(()),
            Err(_) => {
                // Periodic connection check while idle.
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
