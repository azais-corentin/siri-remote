//! `siri-remote events` — connect a bonded (or pairing-mode) Siri Remote and
//! stream battery / power / HID notifications to stdout.
//!
//! Connection plumbing lives in [`crate::session`]; this module is just a
//! formatter for the [`crate::session::DeviceEvent`] stream plus a
//! reconnect-forever loop.

use std::io::Write as _;
use std::time::Duration;

use anyhow::Result;
use btleplug::api::{CharPropFlags, Peripheral as _};
use btleplug::platform::{Adapter, Peripheral};
use uuid::Uuid;

use crate::cli::EventsArgs;
use crate::decoder::{button_list, format_battery, format_power, now_stamp, raw_hex};
use crate::hid::{
    BATTERY_LEVEL_UUID, BATTERY_POWER_UUID, all_characteristics, find_by_uuid,
};
use crate::session::{self, DeviceEvent, InitError, Selection, Session};
#[cfg(target_os = "linux")]
use crate::bluez;

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

pub async fn run(args: EventsArgs) -> Result<u8> {
    if args.scan_seconds < 0.0 {
        anyhow::bail!("--scan-seconds must be non-negative");
    }
    if args.reconnect_delay < 0.0 {
        anyhow::bail!("--reconnect-delay must be non-negative");
    }

    let adapter = session::make_adapter().await?;

    let selection = match session::choose_initial_selection(
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

async fn run_forever(
    adapter: &Adapter,
    mut selection: Selection,
    reconnect_delay: Duration,
) -> Result<u8> {
    loop {
        match connect_and_print(adapter, &selection).await {
            Ok(new_sel) => {
                eprintln!(
                    "Disconnected from {}; reconnecting automatically.",
                    new_sel.address
                );
                selection = new_sel;
            }
            Err(err) => {
                #[cfg(target_os = "linux")]
                if let Some(denied) = err.downcast_ref::<bluez::hid::HidInputEnableDenied>() {
                    eprintln!(
                        "error: BlueZ refused to enable HID input streaming on {}.\n\
                         Every writable HID Report on the remote returned org.bluez.Error.NotAuthorized.\n\
                         \n\
                         This happens when BlueZ's `hog` plugin (HID-over-GATT) has claimed the\n\
                         HID service so the kernel can expose it as a uinput device. While the\n\
                         plugin owns the service no other process is allowed to write to its\n\
                         Report characteristics, and the Siri Remote needs the 0xAF input-enable\n\
                         byte written to a Feature report before it will stream events.\n\
                         \n\
                         Fix: start bluetoothd without the `hog` (and `input`) plugin. Edit your\n\
                         bluetooth.service unit so ExecStart reads:\n\
                         \n\
                         \x20   ExecStart=/usr/libexec/bluetooth/bluetoothd --noplugin=input,hog\n\
                         \n\
                         (the path is /usr/lib/bluetooth/bluetoothd on some distros), then\n\
                         restart bluetoothd. The existing bond is preserved.",
                        denied.device_path,
                    );
                    return Ok(1);
                }
                if !selection.requires_pairing && session::is_auth_failure(&err) {
                    match session::switch_to_pairing_scan(adapter).await {
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

/// Connect once, dump device info, and pump events to stdout until the
/// link drops. Returns the refreshed [`Selection`] so the caller can
/// reconnect under the same identity.
async fn connect_and_print(adapter: &Adapter, selection: &Selection) -> Result<Selection> {
    let (peripheral, refreshed) = session::connect_once(adapter, selection).await?;
    print_device_info(&peripheral, &refreshed).await;
    let mut session = Session::open(peripheral, &refreshed).await?;
    print_loop(&mut session).await?;
    Ok(refreshed)
}

async fn print_loop(session: &mut Session) -> Result<()> {
    let stdout = std::io::stdout();
    while let Some(ev) = session.next_event().await {
        let line = format_event(&ev);
        let mut h = stdout.lock();
        writeln!(h, "{line}")?;
        h.flush()?;
    }
    Ok(())
}

/// Render one [`DeviceEvent`] using the exact same wire format the
/// pre-refactor `events` subcommand produced. Preserving this is what
/// keeps existing log consumers / piping workflows working.
fn format_event(ev: &DeviceEvent) -> String {
    let stamp = now_stamp();
    match ev {
        DeviceEvent::Battery { raw, .. } => format!(
            "{stamp} battery uuid={} raw={} | {}",
            BATTERY_LEVEL_UUID,
            raw_hex(raw),
            format_battery(raw),
        ),
        DeviceEvent::Power { raw, .. } => format!(
            "{stamp} power uuid={} raw={} | {}",
            BATTERY_POWER_UUID,
            raw_hex(raw),
            format_power(raw),
        ),
        DeviceEvent::UnknownOther { uuid, payload } => format!(
            "{stamp} other uuid={uuid} raw={} | len={}",
            raw_hex(payload),
            payload.len(),
        ),
        DeviceEvent::Buttons {
            report_id,
            mask,
            pressed,
            released,
            raw,
        } => {
            let decoded = if *pressed == 0 && *released == 0 {
                format!("unknown HID packet len={}", raw.len())
            } else {
                format!(
                    "buttons={} pressed={} released={}",
                    button_list(*mask),
                    button_list(*pressed),
                    button_list(*released),
                )
            };
            format!(
                "{stamp} input report_id=0x{report_id:02X} raw={} | {decoded}",
                raw_hex(raw),
            )
        }
        DeviceEvent::Touch {
            report_id,
            event,
            raw,
        } => format!(
            "{stamp} input report_id=0x{report_id:02X} raw={} | {}",
            raw_hex(raw),
            event.format(),
        ),
        DeviceEvent::UnknownInput { report_id, payload } => format!(
            "{stamp} input report_id=0x{report_id:02X} raw={}",
            raw_hex(payload),
        ),
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

