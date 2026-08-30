//! The `/dev/uinput` device the gamepad subcommand publishes.
//!
//! It impersonates a wired Xbox 360 controller (`045e:028e`, version
//! `0x0110`, name `Microsoft X-Box 360 pad`). That identity is not
//! cosmetic: SDL2/SDL3 ship a built-in mapping keyed on
//! `030000005e0400008e02000010010000`, which is bus / vendor / product /
//! version, so Steam, Proton, SDL games, and the browser Gamepad API
//! auto-map the pad with no `SDL_GAMECONTROLLERCONFIG` and no per-user
//! configuration.
//!
//! SDL derives its button *indices* from the evdev capability bitmap in
//! ascending code order, so declaring exactly `xpad`'s set reproduces
//! `a:b0 b:b1 x:b2 y:b3 leftshoulder:b4 rightshoulder:b5 back:b6
//! start:b7 guide:b8 leftstick:b9 rightstick:b10`, axes `a0..a5`, and hat
//! `h0`. Declaring fewer keys would shift every later index and break the
//! built-in mapping, which is why [`PAD_KEYS`] includes two buttons the
//! remote cannot produce.

use std::path::PathBuf;

use anyhow::Result;
use evdev::uinput::VirtualDevice;
use evdev::{
    AbsInfo, AbsoluteAxisCode, AttributeSet, BusType, InputEvent, InputId, KeyCode, UinputAbsSetup,
};

pub const DEVICE_NAME: &str = "Microsoft X-Box 360 pad";
pub const VENDOR: u16 = 0x045E;
pub const PRODUCT: u16 = 0x028E;
pub const VERSION: u16 = 0x0110;

pub const STICK_MIN: i32 = -32768;
pub const STICK_MAX: i32 = 32767;

/// xpad's button set, ascending by code so SDL's index order matches its
/// built-in Xbox 360 mapping. `BTN_THUMBL` / `BTN_THUMBR` have no remote
/// source but must be declared for that index order to hold.
pub const PAD_KEYS: [KeyCode; 11] = [
    KeyCode::BTN_SOUTH,  // 0x130 A
    KeyCode::BTN_EAST,   // 0x131 B
    KeyCode::BTN_NORTH,  // 0x133 X
    KeyCode::BTN_WEST,   // 0x134 Y
    KeyCode::BTN_TL,     // 0x136
    KeyCode::BTN_TR,     // 0x137
    KeyCode::BTN_SELECT, // 0x13a back
    KeyCode::BTN_START,  // 0x13b
    KeyCode::BTN_MODE,   // 0x13c guide
    KeyCode::BTN_THUMBL, // 0x13d
    KeyCode::BTN_THUMBR, // 0x13e
];

/// `/dev/uinput` could not be opened for writing.
#[derive(Debug)]
pub struct UinputUnavailable(pub std::io::Error);

impl std::fmt::Display for UinputUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "cannot open /dev/uinput: {}", self.0)
    }
}

impl std::error::Error for UinputUnavailable {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

/// A write to the registered device failed, which means the kernel device
/// is gone (`uinput` unloaded, fd revoked). Retrying cannot fix it.
#[derive(Debug)]
pub struct PadGone(pub std::io::Error);

impl std::fmt::Display for PadGone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "virtual gamepad write failed: {}", self.0)
    }
}

impl std::error::Error for PadGone {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

pub struct VirtualPad {
    dev: VirtualDevice,
}

impl VirtualPad {
    /// Register the virtual pad. Fails with [`UinputUnavailable`] when
    /// `/dev/uinput` is missing or not writable by this user.
    pub fn open() -> Result<Self> {
        // This is where `/dev/uinput` is actually opened, so its error is
        // the one worth branching on.
        let builder = VirtualDevice::builder().map_err(UinputUnavailable)?;

        // Sticks: xpad's signed 16-bit range, with its fuzz / flat filters.
        let stick = || AbsInfo::new(0, STICK_MIN, STICK_MAX, 16, 128, 0);
        // Triggers: unsigned byte. Declared for capability fidelity only.
        let trigger = || AbsInfo::new(0, 0, 255, 0, 0, 0);
        // Hat: three-state per axis.
        let hat = || AbsInfo::new(0, -1, 1, 0, 0, 0);

        let dev = builder
            .name(DEVICE_NAME)
            .input_id(InputId::new(BusType::BUS_USB, VENDOR, PRODUCT, VERSION))
            .with_keys(&PAD_KEYS.iter().collect::<AttributeSet<KeyCode>>())?
            // Ascending code order, matching PAD_KEYS' rationale.
            .with_absolute_axis(&UinputAbsSetup::new(AbsoluteAxisCode::ABS_X, stick()))?
            .with_absolute_axis(&UinputAbsSetup::new(AbsoluteAxisCode::ABS_Y, stick()))?
            .with_absolute_axis(&UinputAbsSetup::new(AbsoluteAxisCode::ABS_Z, trigger()))?
            .with_absolute_axis(&UinputAbsSetup::new(AbsoluteAxisCode::ABS_RX, stick()))?
            .with_absolute_axis(&UinputAbsSetup::new(AbsoluteAxisCode::ABS_RY, stick()))?
            .with_absolute_axis(&UinputAbsSetup::new(AbsoluteAxisCode::ABS_RZ, trigger()))?
            .with_absolute_axis(&UinputAbsSetup::new(AbsoluteAxisCode::ABS_HAT0X, hat()))?
            .with_absolute_axis(&UinputAbsSetup::new(AbsoluteAxisCode::ABS_HAT0Y, hat()))?
            .build()?;

        Ok(Self { dev })
    }

    /// Post one frame. `evdev` appends `SYN_REPORT`, so callers batch a
    /// whole frame into `batch` and call this once.
    pub fn emit(&mut self, batch: &[InputEvent]) -> std::io::Result<()> {
        self.dev.emit(batch)
    }

    /// `/dev/input/eventN` paths for the startup log line; best-effort. A
    /// failed enumeration is cosmetic — the device is already live.
    pub fn dev_nodes(&mut self) -> Vec<PathBuf> {
        match self.dev.enumerate_dev_nodes_blocking() {
            Ok(nodes) => nodes.flatten().collect(),
            Err(err) => {
                log::debug!("enumerating virtual gamepad dev nodes: {err}");
                Vec::new()
            }
        }
    }
}

/// Round-trip test against the real kernel: register the pad, reopen the
/// node it creates, and confirm the identity, capabilities, and one emitted
/// frame.
///
/// Ignored because it needs write access to `/dev/uinput` and read access
/// to the resulting `/dev/input/eventN`.
///
/// Run with: `cargo test -- --ignored gamepad::pad`
///
/// On NixOS set `hardware.uinput.enable = true;` and add your user to the
/// `uinput` and `input` groups.
#[cfg(test)]
mod tests {
    use super::*;
    use evdev::{Device, EventSummary};
    use std::time::{Duration, Instant};

    /// udev applies its ACL to the new node asynchronously, so a read open
    /// straight after `build()` can lose the race.
    fn open_with_retry(path: &std::path::Path) -> Device {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match Device::open(path) {
                Ok(d) => return d,
                Err(err) if Instant::now() < deadline => {
                    log::debug!("retrying open of {}: {err}", path.display());
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
                    panic!(
                        "cannot read {}: {err}. Join the `input` group (or grant \
                         read access another way) and rerun.",
                        path.display()
                    );
                }
                Err(err) => panic!("cannot open {}: {err}", path.display()),
            }
        }
    }

    #[test]
    #[ignore = "needs write access to /dev/uinput and read access to /dev/input/eventN"]
    fn registers_an_xbox_360_pad_and_echoes_a_frame() {
        let mut pad = VirtualPad::open().expect("open /dev/uinput");
        let nodes = pad.dev_nodes();
        assert!(!nodes.is_empty(), "no /dev/input node created");

        let mut dev = open_with_retry(&nodes[0]);

        let id = dev.input_id();
        assert_eq!(id.vendor(), VENDOR);
        assert_eq!(id.product(), PRODUCT);
        assert_eq!(id.version(), VERSION);

        let keys = dev.supported_keys().expect("device declares keys");
        for k in PAD_KEYS {
            assert!(keys.contains(k), "missing {k:?}");
        }
        let axes = dev
            .supported_absolute_axes()
            .expect("device declares absolute axes");
        for a in [
            AbsoluteAxisCode::ABS_X,
            AbsoluteAxisCode::ABS_Y,
            AbsoluteAxisCode::ABS_Z,
            AbsoluteAxisCode::ABS_RX,
            AbsoluteAxisCode::ABS_RY,
            AbsoluteAxisCode::ABS_RZ,
            AbsoluteAxisCode::ABS_HAT0X,
            AbsoluteAxisCode::ABS_HAT0Y,
        ] {
            assert!(axes.contains(a), "missing {a:?}");
        }

        pad.emit(&[
            *evdev::KeyEvent::new(KeyCode::BTN_SOUTH, 1),
            *evdev::AbsoluteAxisEvent::new(AbsoluteAxisCode::ABS_X, STICK_MAX),
        ])
        .expect("emit frame");

        let mut saw_key = false;
        let mut saw_axis = false;
        let mut saw_syn_after_both = false;
        for ev in dev.fetch_events().expect("read events back") {
            match ev.destructure() {
                EventSummary::Key(_, KeyCode::BTN_SOUTH, 1) => saw_key = true,
                EventSummary::AbsoluteAxis(_, AbsoluteAxisCode::ABS_X, STICK_MAX) => {
                    saw_axis = true
                }
                EventSummary::Synchronization(..) if saw_key && saw_axis => {
                    saw_syn_after_both = true;
                    break;
                }
                _ => {}
            }
        }
        assert!(saw_key, "BTN_SOUTH press not echoed");
        assert!(saw_axis, "ABS_X = {STICK_MAX} not echoed");
        assert!(saw_syn_after_both, "no SYN_REPORT after the frame");
    }
}
