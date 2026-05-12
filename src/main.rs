//! `siri-remote` binary entry point: parses the CLI, dispatches to the
//! matching subcommand module, and treats Ctrl-C as exit code 130.

mod logger;
mod cli;
mod decoder;
mod events;
mod dump;
mod hid;
mod pair;
mod scan;
mod session;
mod unpair;
mod view;

#[cfg(target_os = "linux")]
mod bluez;

use clap::Parser;

use cli::{Cli, Command};

#[tokio::main(flavor = "multi_thread")]
async fn main() -> std::process::ExitCode {
    logger::init();
    let cli = Cli::parse();

    let work = async move {
        match cli.command {
            Command::Pair(args) => pair::run(args).await,
            Command::Events(args) => events::run(args).await,
            Command::Unpair(args) => unpair::run(args).await,
            Command::View(args) => view::run(args).await,
            Command::Dump(args) => dump::run(args).await,
        }
    };

    tokio::select! {
        result = work => match result {
            Ok(code) => std::process::ExitCode::from(code),
            Err(err) => {
                eprintln!("{err:?}");
                std::process::ExitCode::from(2)
            }
        },
        _ = tokio::signal::ctrl_c() => {
            eprintln!("Interrupted.");
            std::process::ExitCode::from(130)
        }
    }
}
