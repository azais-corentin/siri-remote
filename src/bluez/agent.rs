//! BlueZ `org.bluez.Agent1` implementation with `NoInputNoOutput` capability.
//!
//! Without a registered agent, BlueZ requests passkey confirmation (numeric
//! comparison) for the Siri Remote's pairing and the request fails because no
//! agent is wired up. Registering a `NoInputNoOutput` agent forces the
//! Bluetooth association model down to Just Works, which is what the remote
//! needs since it has no display or keyboard.

#![cfg(target_os = "linux")]

use anyhow::Result;
use zbus::{Connection, connection::Builder, interface};
use zvariant::ObjectPath;
use log::{info, warn};

const AGENT_PATH: &str = "/com/example/siri_remote_agent";
const BLUEZ_BUS: &str = "org.bluez";
const BLUEZ_PATH: &str = "/org/bluez";
const AGENT_MANAGER_IFACE: &str = "org.bluez.AgentManager1";

#[derive(Default)]
struct AutoConfirmAgent;

#[interface(name = "org.bluez.Agent1")]
impl AutoConfirmAgent {
    fn release(&self) {}

    fn request_pin_code(&self, _device: ObjectPath<'_>) -> String {
        "0000".to_string()
    }

    fn display_pin_code(&self, _device: ObjectPath<'_>, _pincode: String) {}

    fn request_passkey(&self, _device: ObjectPath<'_>) -> u32 {
        0
    }

    fn display_passkey(&self, _device: ObjectPath<'_>, _passkey: u32, _entered: u16) {}

    fn request_confirmation(&self, device: ObjectPath<'_>, passkey: u32) {
        // Returning Ok confirms the pairing; raising would reject it.
        info!("  agent: auto-confirming pair request for {device} (passkey={passkey})");
    }

    fn request_authorization(&self, _device: ObjectPath<'_>) {}

    fn authorize_service(&self, _device: ObjectPath<'_>, _uuid: String) {}

    fn cancel(&self) {}
}

/// RAII handle for a registered BlueZ pairing agent. Keep this alive for the
/// duration of any operation that needs Just Works confirmation; call
/// [`close`](Self::close) before dropping (or just drop, which best-effort
/// spawns the unregister call).
pub struct AgentSession {
    conn: Option<Connection>,
}

impl AgentSession {
    /// Open a connection to the system bus, serve the agent at `AGENT_PATH`,
    /// and register it with BlueZ as the default `NoInputNoOutput` agent.
    pub async fn register() -> Result<Self> {
        let conn = Builder::system()?
            .serve_at(AGENT_PATH, AutoConfirmAgent)?
            .build()
            .await?;

        let agent_path = ObjectPath::try_from(AGENT_PATH)?;

        conn.call_method(
            Some(BLUEZ_BUS),
            BLUEZ_PATH,
            Some(AGENT_MANAGER_IFACE),
            "RegisterAgent",
            &(agent_path.clone(), "NoInputNoOutput"),
        )
        .await?;

        conn.call_method(
            Some(BLUEZ_BUS),
            BLUEZ_PATH,
            Some(AGENT_MANAGER_IFACE),
            "RequestDefaultAgent",
            &(agent_path,),
        )
        .await?;

        info!("Registered NoInputNoOutput agent (forces Just Works pairing).");
        Ok(Self { conn: Some(conn) })
    }

    /// Borrow the underlying D-Bus connection. Lets callers reuse the same
    /// system-bus connection for follow-on Device1/Adapter1 calls instead of
    /// opening a second one.
    pub fn connection(&self) -> &Connection {
        self.conn.as_ref().expect("AgentSession used after close()")
    }

    /// Explicit, awaited UnregisterAgent. Prefer this over relying on `Drop`
    /// because the program may exit before the spawned cleanup task runs.
    pub async fn close(mut self) {
        if let Some(conn) = self.conn.take() {
            unregister(&conn).await;
        }
    }
}

impl Drop for AgentSession {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            // Best-effort unregister. If the runtime is mid-shutdown the spawn
            // may not complete; that's acceptable — BlueZ tears down agents
            // automatically when their owner disconnects.
            tokio::spawn(async move {
                unregister(&conn).await;
            });
        }
    }
}

async fn unregister(conn: &Connection) {
    let Ok(agent_path) = ObjectPath::try_from(AGENT_PATH) else {
        return;
    };
    if let Err(e) = conn
        .call_method(
            Some(BLUEZ_BUS),
            BLUEZ_PATH,
            Some(AGENT_MANAGER_IFACE),
            "UnregisterAgent",
            &(agent_path,),
        )
        .await
    {
        warn!("warning: agent unregister failed: {e:?}");
    }
}
