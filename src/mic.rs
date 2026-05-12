//! `siri-remote mic` — connect a bonded (or pairing-mode) Siri Remote,
//! decode the Opus microphone audio stream the remote emits on HID
//! input report `0xFA` while the Siri button is held, and expose the
//! PCM samples as a PipeWire virtual microphone (an `Audio/Source`
//! node) so any consumer (`pw-record`, Firefox WebRTC, `arecord` via
//! the PipeWire ALSA shim, etc.) can record from it as if it were a
//! real hardware microphone.
//!
//! Wiring lives in two halves:
//!
//! - The BLE side (this module) runs on tokio via
//!   [`crate::session::Session`]; it observes
//!   [`DeviceEvent::UnknownInput`] frames with `report_id == 0xFA`
//!   and pipes their payloads into a [`MicDecoder`].
//! - The PipeWire side ([`crate::audio::PipeWireWorker`]) owns the
//!   PipeWire main-loop thread and drains the shared sample ring.
//!
//! Reconnect-forever behaviour mirrors [`crate::dump`]; the PipeWire
//! worker persists across BLE drops so the virtual microphone stays
//! visible to consumers (silenced on underflow) instead of bouncing
//! every time the remote disconnects.

use std::time::Duration;

use anyhow::{Context as _, Result};
use btleplug::platform::Adapter;

use crate::audio::{MicDecoder, PipeWireWorker, Ring};
use crate::cli::MicArgs;
use crate::session::{self, DeviceEvent, InitError, Selection, Session};
#[cfg(target_os = "linux")]
use crate::bluez;

/// HID input report id carrying microphone audio frames (Siri button held).
const MIC_REPORT_ID: u8 = 0xFA;

pub async fn run(args: MicArgs) -> Result<u8> {
    if args.scan_seconds < 0.0 {
        anyhow::bail!("--scan-seconds must be non-negative");
    }
    if args.reconnect_delay < 0.0 {
        anyhow::bail!("--reconnect-delay must be non-negative");
    }
    if args.node_name.is_empty() {
        anyhow::bail!("--node-name must be non-empty");
    }

    let worker = PipeWireWorker::spawn(args.node_name.clone(), args.node_description.clone())
        .context("starting PipeWire worker thread")?;
    let ring = worker.ring();

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

    let exit = run_forever(
        &adapter,
        selection,
        Duration::from_secs_f64(args.reconnect_delay),
        ring,
    )
    .await;

    // Worker is dropped here, which signals the PW thread to quit and
    // joins it. On Ctrl-C the tokio select! in main.rs aborts this
    // future before we get here, but the process is about to exit
    // anyway so the OS reaps the PW thread.
    drop(worker);
    exit
}

async fn run_forever(
    adapter: &Adapter,
    mut selection: Selection,
    reconnect_delay: Duration,
    ring: Ring,
) -> Result<u8> {
    loop {
        match connect_and_pump(adapter, &selection, &ring).await {
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
                         Start bluetoothd with `--noplugin=input,hog` so the kernel HoG\n\
                         plugin releases the HID service; the existing bond is preserved.",
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

async fn connect_and_pump(
    adapter: &Adapter,
    selection: &Selection,
    ring: &Ring,
) -> Result<Selection> {
    let (peripheral, refreshed) = session::connect_once(adapter, selection).await?;
    let mut session = Session::open(peripheral, &refreshed).await?;
    let mut decoder = MicDecoder::new()?;
    while let Some(ev) = session.next_event().await {
        if let DeviceEvent::UnknownInput {
            report_id: MIC_REPORT_ID,
            payload,
        } = ev
        {
            decoder.feed(&payload, ring);
        }
    }
    Ok(refreshed)
}
