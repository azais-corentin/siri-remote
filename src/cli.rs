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
    /// Remove all paired/bonded Siri Remotes via BlueZ Adapter1.RemoveDevice (Linux only).
    Unpair(UnpairArgs),
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
