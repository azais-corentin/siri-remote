//! `siri-remote gamepad` — connect a bonded (or pairing-mode) Siri Remote
//! and republish it as a Linux virtual gamepad through `/dev/uinput`, so
//! games and SDL applications see a real controller.
//!
//! Wiring lives in three halves:
//!
//! - The BLE side (this module) runs on tokio via
//!   [`crate::session::Session`]; it consumes [`DeviceEvent::Buttons`]
//!   (report `0xFB`) and [`DeviceEvent::Touch`] (report [`TOUCH_REPORT_ID`]).
//! - The translator ([`state::PadState`]) is pure: it folds those events
//!   into `InputEvent` batches with no IO.
//! - The kernel side ([`pad::VirtualPad`]) owns the `/dev/uinput` fd.
//!
//! Reconnect-forever behaviour mirrors [`crate::mic`]; the virtual pad
//! persists across BLE drops so consumers keep their open
//! `/dev/input/eventN` handle instead of losing the controller every time
//! the remote disconnects. On each drop the pad is driven to neutral so a
//! link loss mid-press cannot leave a game holding a stuck button.

pub mod config;
pub mod pad;
pub mod state;

use std::time::Duration;

use anyhow::Result;
use btleplug::platform::Adapter;
use evdev::InputEvent;

#[cfg(target_os = "linux")]
use crate::bluez;
use crate::calibration::Calibration;
use crate::cli::GamepadArgs;
use crate::session::{self, DeviceEvent, InitError, Selection, Session};
use config::{FileConfig, GamepadConfig};
use pad::VirtualPad;
use state::PadState;

/// HID input report id carrying touchpad samples. Matching is by
/// [`DeviceEvent`] variant, not by this constant; it documents the wire
/// source.
const TOUCH_REPORT_ID: u8 = 0xFC;

pub async fn run(args: GamepadArgs) -> Result<u8> {
    if args.scan_seconds < 0.0 {
        anyhow::bail!("--scan-seconds must be non-negative");
    }
    if args.reconnect_delay < 0.0 {
        anyhow::bail!("--reconnect-delay must be non-negative");
    }

    let file = match args.config.as_deref() {
        // Explicit path: a missing or malformed file is a user error.
        Some(path) => match config::load_from(path) {
            Ok(f) => f,
            Err(err) => {
                eprintln!("error: {err:#}");
                return Ok(2);
            }
        },
        None => match config::default_path() {
            Some(path) if path.exists() => match config::load_from(&path) {
                Ok(f) => f,
                Err(err) => {
                    eprintln!("error: {err:#}");
                    return Ok(2);
                }
            },
            Some(path) => {
                log::debug!("no gamepad mapping at {}; using defaults", path.display());
                FileConfig::default()
            }
            None => {
                log::debug!(
                    "XDG_CONFIG_HOME and HOME both unset; using the built-in gamepad mapping"
                );
                FileConfig::default()
            }
        },
    };

    let cfg = match config::resolve(&args, &file) {
        Ok(c) => c,
        Err(err) => {
            eprintln!("error: {err:#}");
            return Ok(2);
        }
    };

    // `load` already logs a warning and yields `None` on a malformed file.
    let cal = crate::calibration::load().unwrap_or_default();

    let mut pad = match VirtualPad::open() {
        Ok(p) => p,
        Err(err) => {
            if let Some(pad::UinputUnavailable(source)) =
                err.downcast_ref::<pad::UinputUnavailable>()
            {
                eprintln!(
                    "error: cannot open /dev/uinput for writing: {source}\n\
                     \n\
                     The virtual gamepad is created through the kernel's uinput device, which is\n\
                     root-only by default. Grant this user access, then rerun:\n\
                     \n\
                     \x20   NixOS:  hardware.uinput.enable = true;   (creates the uinput group + udev rule)\n\
                     \x20           users.users.<you>.extraGroups = [ \"uinput\" \"input\" ];\n\
                     \x20   else:   load the uinput module and install a udev rule granting the\n\
                     \x20           input group write access to /dev/uinput."
                );
                return Ok(1);
            }
            return Err(err);
        }
    };

    eprintln!(
        "Registered virtual gamepad \"{}\" ({:04x}:{:04x} version {:04x}).",
        pad::DEVICE_NAME,
        pad::VENDOR,
        pad::PRODUCT,
        pad::VERSION,
    );
    for node in pad.dev_nodes() {
        eprintln!("  {}", node.display());
    }
    eprintln!(
        "Stick mode {:?}, radius {}, dead zone {}. Touchpad report 0x{TOUCH_REPORT_ID:02X} \
         drives the left stick.",
        cfg.stick_mode, cfg.stick_radius, cfg.deadzone,
    );

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

    // On Ctrl-C the tokio select! in main.rs aborts this future; the process
    // exits, the /dev/uinput fd closes, and the kernel destroys the device,
    // so no stuck input survives.
    run_forever(
        &adapter,
        selection,
        Duration::from_secs_f64(args.reconnect_delay),
        cfg,
        cal,
        pad,
    )
    .await
}

async fn run_forever(
    adapter: &Adapter,
    mut selection: Selection,
    reconnect_delay: Duration,
    cfg: GamepadConfig,
    cal: Calibration,
    mut pad: VirtualPad,
) -> Result<u8> {
    // The translator lives across reconnects: after `neutral` every cached
    // value is back at rest, so it is equivalent to a fresh one, and
    // holding it is what lets a link drop release whatever was actually
    // pressed at the time.
    let mut st = PadState::new(cfg, cal);
    let mut batch: Vec<InputEvent> = Vec::with_capacity(16);
    loop {
        let outcome = connect_and_pump(adapter, &selection, &mut st, &mut pad).await;

        // The link is down either way, so release everything a game might
        // still be holding.
        batch.clear();
        st.neutral(&mut batch);
        if !batch.is_empty()
            && let Err(err) = pad.emit(&batch)
        {
            eprintln!("error: {}", pad::PadGone(err));
            return Ok(2);
        }

        match outcome {
            Ok(new_sel) => {
                eprintln!(
                    "Disconnected from {}; reconnecting automatically.",
                    new_sel.address
                );
                selection = new_sel;
            }
            Err(err) => {
                if let Some(gone) = err.downcast_ref::<pad::PadGone>() {
                    eprintln!("error: {gone}");
                    return Ok(2);
                }
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
    st: &mut PadState,
    pad: &mut VirtualPad,
) -> Result<Selection> {
    let (peripheral, refreshed) = session::connect_once(adapter, selection).await?;
    let mut session = Session::open(peripheral, &refreshed).await?;
    let mut batch: Vec<InputEvent> = Vec::with_capacity(16);
    while let Some(ev) = session.next_event().await {
        batch.clear();
        match ev {
            DeviceEvent::Buttons { mask, .. } => st.on_buttons(mask, &mut batch),
            DeviceEvent::Touch { event, .. } => st.on_touch(&event, &mut batch),
            _ => continue,
        }
        if !batch.is_empty() {
            // A write failure means the kernel device is gone; retrying
            // the BLE link cannot fix it.
            pad.emit(&batch).map_err(pad::PadGone)?;
        }
    }
    Ok(refreshed)
}
