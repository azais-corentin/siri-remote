//! `siri-remote dump` — connect a bonded (or pairing-mode) Siri Remote and
//! write one line of raw bytes per matching HID input report to stdout. The
//! `--touch` flag selects touchpad-report frames (HID report id `0xFC`) and
//! `--mic` selects microphone-audio frames emitted while the Siri button is
//! held (HID report id `0xFA`). At least one of the flags must be supplied;
//! when both are passed the two streams are interleaved on stdout in arrival
//! order. Every other event from [`crate::session::DeviceEvent`] is silently
//! dropped — this command exists solely to capture wire traces for offline
//! analysis.
//!
//! The connection scaffold mirrors [`crate::events`]; only the per-frame
//! sink differs.

use std::io::Write as _;
use std::time::Duration;

use anyhow::Result;
use btleplug::platform::Adapter;

use crate::cli::DumpArgs;
use crate::decoder::{now_stamp, raw_hex};
use crate::session::{self, DeviceEvent, InitError, Selection, Session};
#[cfg(target_os = "linux")]
use crate::bluez;

/// HID input report id carrying touchpad samples.
const TOUCH_REPORT_ID: u8 = 0xFC;
/// HID input report id carrying microphone audio frames (Siri button held).
const MIC_REPORT_ID: u8 = 0xFA;

/// Which report streams the user opted into.
#[derive(Clone, Copy)]
struct Filter {
    touch: bool,
    mic: bool,
}

impl Filter {
    fn accepts(&self, report_id: u8) -> bool {
        (self.touch && report_id == TOUCH_REPORT_ID)
            || (self.mic && report_id == MIC_REPORT_ID)
    }
}

pub async fn run(args: DumpArgs) -> Result<u8> {
    if args.scan_seconds < 0.0 {
        anyhow::bail!("--scan-seconds must be non-negative");
    }
    if args.reconnect_delay < 0.0 {
        anyhow::bail!("--reconnect-delay must be non-negative");
    }
    if !args.touch && !args.mic {
        eprintln!("error: at least one of --touch or --mic must be specified");
        return Ok(2);
    }

    let filter = Filter {
        touch: args.touch,
        mic: args.mic,
    };

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
        filter,
    )
    .await
}

async fn run_forever(
    adapter: &Adapter,
    mut selection: Selection,
    reconnect_delay: Duration,
    filter: Filter,
) -> Result<u8> {
    loop {
        match connect_and_dump(adapter, &selection, filter).await {
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

/// Connect once and pump matching report bytes to stdout until the link
/// drops. Returns the refreshed [`Selection`] so the caller can reconnect
/// under the same identity.
async fn connect_and_dump(
    adapter: &Adapter,
    selection: &Selection,
    filter: Filter,
) -> Result<Selection> {
    let (peripheral, refreshed) = session::connect_once(adapter, selection).await?;
    let mut session = Session::open(peripheral, &refreshed).await?;
    dump_loop(&mut session, filter).await?;
    Ok(refreshed)
}

async fn dump_loop(session: &mut Session, filter: Filter) -> Result<()> {
    let stdout = std::io::stdout();
    while let Some(ev) = session.next_event().await {
        let (report_id, bytes): (u8, &[u8]) = match &ev {
            DeviceEvent::Touch {
                report_id, raw, ..
            } if filter.accepts(*report_id) => (*report_id, raw),
            DeviceEvent::UnknownInput { report_id, payload }
                if filter.accepts(*report_id) =>
            {
                (*report_id, payload)
            }
            _ => continue,
        };
        let stamp = now_stamp();
        let hex = raw_hex(bytes);
        let mut h = stdout.lock();
        writeln!(h, "{stamp} report_id=0x{report_id:02X} raw={hex}")?;
        h.flush()?;
    }
    Ok(())
}
