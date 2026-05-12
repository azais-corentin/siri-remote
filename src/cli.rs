//! Command-line surface for the `siri-remote` binary.

use clap::{Args, Parser, Subcommand};

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
