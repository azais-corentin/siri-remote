//! Command-line surface for the `siri-remote` binary.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use crate::gamepad::config::StickMode;

#[derive(Parser)]
#[command(
    name = "siri-remote",
    about = "Pair, stream events from, or unpair an Apple TV Siri Remote (3rd gen) over BLE.",
    version
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Find a Siri Remote in pairing mode, bond + connect, dump GATT, hold the link briefly.
    Pair(PairArgs),
    /// Connect a bonded (or pairing-mode) remote and stream battery/power/HID notifications to stdout.
    Events(EventsArgs),
    /// Connect a bonded (or pairing-mode) remote and render its live state as a ratatui dashboard.
    View(ViewArgs),
    /// Remove all paired/bonded Siri Remotes via BlueZ Adapter1.RemoveDevice (Linux only).
    Unpair(UnpairArgs),
    /// Connect a bonded (or pairing-mode) remote and dump raw HID input report bytes to stdout.
    /// Use `--touch` for touchpad frames (report id 0xFC) and/or `--mic` for microphone-audio
    /// frames emitted while the Siri button is held (report id 0xFA). At least one is required.
    Dump(DumpArgs),
    /// Connect a bonded (or pairing-mode) remote, decode the Opus microphone
    /// audio stream emitted on HID input report 0xFA while the Siri button is
    /// held, and expose it as a PipeWire virtual microphone (Audio/Source).
    Mic(MicArgs),
    /// Connect a bonded (or pairing-mode) remote and expose it as a virtual
    /// Xbox 360 gamepad through uinput: touchpad -> left stick, clickpad
    /// clicks -> D-pad, remote buttons -> face / shoulder / start buttons.
    /// Requires write access to /dev/uinput.
    Gamepad(GamepadArgs),
}

#[derive(Args, Debug, Clone)]
pub struct PairArgs {
    /// Seconds to scan for a Siri Remote in pairing mode before deciding on a candidate.
    #[arg(long, default_value_t = 5.0)]
    pub scan_seconds: f64,

    /// Seconds to hold the link open after pairing succeeds.
    #[arg(long, default_value_t = 5.0)]
    pub hold_seconds: f64,
}

#[derive(Args, Debug, Clone)]
pub struct EventsArgs {
    /// Specific bonded Siri Remote identity address (skips initial scan).
    #[arg(long)]
    pub address: Option<String>,

    /// Seconds to scan before falling back to address-based connect.
    #[arg(long, default_value_t = 5.0)]
    pub scan_seconds: f64,

    /// Delay before reconnect attempts after disconnect/failure.
    #[arg(long, default_value_t = 0.5)]
    pub reconnect_delay: f64,
}

#[derive(Args, Debug, Clone)]
pub struct UnpairArgs {
    /// Only remove this Bluetooth address. May be provided multiple times.
    #[arg(long)]
    pub address: Vec<String>,

    /// List matching Siri Remotes but do not remove them.
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,
}

#[derive(Args, Debug, Clone)]
pub struct ViewArgs {
    /// Specific bonded Siri Remote identity address (skips initial scan).
    #[arg(long)]
    pub address: Option<String>,

    /// Seconds to scan before falling back to address-based connect.
    #[arg(long, default_value_t = 5.0)]
    pub scan_seconds: f64,

    /// Delay before reconnect attempts after disconnect/failure.
    #[arg(long, default_value_t = 0.5)]
    pub reconnect_delay: f64,
}

#[derive(Args, Debug, Clone)]
pub struct DumpArgs {
    /// Specific bonded Siri Remote identity address (skips initial scan).
    #[arg(long)]
    pub address: Option<String>,

    /// Seconds to scan before falling back to address-based connect.
    #[arg(long, default_value_t = 5.0)]
    pub scan_seconds: f64,

    /// Delay before reconnect attempts after disconnect/failure.
    #[arg(long, default_value_t = 0.5)]
    pub reconnect_delay: f64,

    /// Capture touchpad-report frames (HID input report id 0xFC).
    #[arg(long, default_value_t = false)]
    pub touch: bool,

    /// Capture microphone-audio frames emitted while the Siri button is held
    /// (HID input report id 0xFA).
    #[arg(long, default_value_t = false)]
    pub mic: bool,
}

#[derive(Args, Debug, Clone)]
pub struct MicArgs {
    /// Specific bonded Siri Remote identity address (skips initial scan).
    #[arg(long)]
    pub address: Option<String>,

    /// Seconds to scan before falling back to address-based connect.
    #[arg(long, default_value_t = 5.0)]
    pub scan_seconds: f64,

    /// Delay before reconnect attempts after disconnect/failure.
    #[arg(long, default_value_t = 0.5)]
    pub reconnect_delay: f64,

    /// PipeWire node name (the stable identifier consumers use to target the
    /// virtual microphone, e.g. `pw-record --target=siri-remote`).
    #[arg(long, default_value = "siri-remote")]
    pub node_name: String,

    /// Human-readable PipeWire node description (shown in mixers / pavucontrol).
    #[arg(long, default_value = "Siri Remote microphone")]
    pub node_description: String,
}

#[derive(Args, Debug, Clone)]
pub struct GamepadArgs {
    /// Specific bonded Siri Remote identity address (skips initial scan).
    #[arg(long)]
    pub address: Option<String>,

    /// Seconds to scan before falling back to address-based connect.
    #[arg(long, default_value_t = 5.0)]
    pub scan_seconds: f64,

    /// Delay before reconnect attempts after disconnect/failure.
    #[arg(long, default_value_t = 0.5)]
    pub reconnect_delay: f64,

    /// Touchpad-to-left-stick mapping. `relative`: the touch-down point
    /// becomes the stick centre. `absolute`: the calibrated pad position maps
    /// straight onto the stick. Overrides `stick_mode` in gamepad.toml.
    /// [default: relative]
    #[arg(long, value_enum)]
    pub stick_mode: Option<StickMode>,

    /// Fraction of the touchpad span that equals full stick deflection in
    /// `relative` mode; must be in (0.0, 2.0]. Overrides `stick_radius` in
    /// gamepad.toml. [default: 0.35]
    #[arg(long)]
    pub stick_radius: Option<f64>,

    /// Radial dead zone as a fraction of full deflection; must be in
    /// [0.0, 0.9). Overrides `deadzone` in gamepad.toml. [default: 0.05]
    #[arg(long)]
    pub deadzone: Option<f64>,

    /// Explicit mapping file. Default:
    /// $XDG_CONFIG_HOME/siri-remote/gamepad.toml (optional; built-in mapping
    /// is used when absent).
    #[arg(long)]
    pub config: Option<PathBuf>,
}
