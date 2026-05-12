//! `siri-remote touch-dump` — connect a bonded (or pairing-mode) Siri Remote
//! and write one line of raw bytes per touchpad-report frame (HID input
//! report id `0xFC`) to stdout. Every other event from
//! [`crate::session::DeviceEvent`] is silently dropped — this command exists
//! solely to capture wire traces for investigating the touchpad-X decoding
//! bug.
//!
//! The connection scaffold mirrors [`crate::events`]; only the per-frame
//! sink differs.

use std::io::Write as _;
use std::time::Duration;

use anyhow::Result;
use btleplug::platform::Adapter;

use crate::cli::TouchDumpArgs;
use crate::decoder::{now_stamp, raw_hex};
use crate::session::{self, DeviceEvent, InitError, Selection, Session};
#[cfg(target_os = "linux")]
use crate::bluez;

/// HID input report id carrying touchpad samples. Anything else is ignored.
const TOUCH_REPORT_ID: u8 = 0xFC;

pub async fn run(args: TouchDumpArgs) -> Result<u8> {
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
        match connect_and_dump(adapter, &selection).await {
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

/// Connect once and pump touchpad-report bytes to stdout until the link
/// drops. Returns the refreshed [`Selection`] so the caller can reconnect
/// under the same identity.
async fn connect_and_dump(adapter: &Adapter, selection: &Selection) -> Result<Selection> {
    let (peripheral, refreshed) = session::connect_once(adapter, selection).await?;
    let mut session = Session::open(peripheral, &refreshed).await?;
    dump_loop(&mut session).await?;
    Ok(refreshed)
}

async fn dump_loop(session: &mut Session) -> Result<()> {
    let stdout = std::io::stdout();
    while let Some(ev) = session.next_event().await {
        let bytes: &[u8] = match &ev {
            DeviceEvent::Touch {
                report_id: TOUCH_REPORT_ID,
                raw,
                ..
            } => raw,
            DeviceEvent::UnknownInput {
                report_id: TOUCH_REPORT_ID,
                payload,
            } => payload,
            _ => continue,
        };
        let stamp = now_stamp();
        let hex = raw_hex(bytes);
        let mut h = stdout.lock();
        writeln!(h, "{stamp} raw={hex}")?;
        h.flush()?;
    }
    Ok(())
}
