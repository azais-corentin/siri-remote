//! `siri-remote unpair` — list and remove paired/bonded Siri Remotes via BlueZ.

use anyhow::Result;

use crate::cli::UnpairArgs;

#[cfg(target_os = "linux")]
pub async fn run(args: UnpairArgs) -> Result<u8> {
    use crate::bluez::device::{connect, list_siri_remotes, remove};

    let conn = connect().await?;

    let addresses: Option<Vec<String>> = if args.address.is_empty() {
        None
    } else {
        Some(
            args.address
                .iter()
                .map(|a| a.trim().to_uppercase())
                .collect(),
        )
    };

    let remotes = list_siri_remotes(&conn, addresses.as_deref()).await?;
    if remotes.is_empty() {
        let target = if addresses.is_some() {
            " matching requested address"
        } else {
            ""
        };
        println!("No paired/bonded Siri Remote{target} found.");
        return Ok(0);
    }

    println!("Matched Siri Remote device(s):");
    for remote in &remotes {
        println!("  {}", format_remote(remote));
    }

    if args.dry_run {
        println!("Dry run: no devices removed.");
        return Ok(0);
    }

    let mut failures = 0usize;
    for remote in &remotes {
        match remove(&conn, remote).await {
            Ok(()) => {
                println!("Unpaired {} {:?}.", remote.address, remote.display_name());
            }
            Err(e) => {
                failures += 1;
                eprintln!(
                    "Failed to unpair {} {:?}: {e:?}",
                    remote.address,
                    remote.display_name()
                );
            }
        }
    }

    if failures > 0 { Ok(2) } else { Ok(0) }
}

#[cfg(target_os = "linux")]
fn format_remote(device: &crate::bluez::device::RemoteDevice) -> String {
    let reasons = device.reasons.join(", ");
    let mut states: Vec<&str> = Vec::new();
    if device.paired {
        states.push("paired");
    }
    if device.bonded {
        states.push("bonded");
    }
    if device.trusted {
        states.push("trusted");
    }
    let state_text = if states.is_empty() {
        "known".to_string()
    } else {
        states.join("/")
    };
    format!(
        "{} {:?} [{state_text}] path={} match={reasons}",
        device.address,
        device.display_name(),
        device.path
    )
}

#[cfg(not(target_os = "linux"))]
pub async fn run(_args: UnpairArgs) -> Result<u8> {
    anyhow::bail!("`siri-remote unpair` requires Linux/BlueZ");
}
