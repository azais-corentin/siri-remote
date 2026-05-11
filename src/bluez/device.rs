//! BlueZ Device1 / Adapter1 / ObjectManager helpers used by `pair` and `unpair`.

#![cfg(target_os = "linux")]

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use zbus::Connection;
use zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

use crate::decoder::{APPLE_COMPANY_ID, APPLE_HID_MFR_PREFIX};

const BLUEZ_BUS: &str = "org.bluez";
const DEVICE_IFACE: &str = "org.bluez.Device1";
const ADAPTER_IFACE: &str = "org.bluez.Adapter1";
const OM_IFACE: &str = "org.freedesktop.DBus.ObjectManager";
const PROPS_IFACE: &str = "org.freedesktop.DBus.Properties";

const HID_SERVICE_UUID_LOWER: &str = "00001812-0000-1000-8000-00805f9b34fb";

const REMOTE_NAME_KEYWORDS: &[&str] = &["siri remote", "apple tv remote", "apple remote"];

type ManagedObjects = HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>>;

/// Snapshot of a paired/bonded BlueZ device that matched our Siri Remote
/// fingerprint. Mirrors `RemoteDevice` from `unpair.py`.
#[derive(Clone, Debug)]
pub struct RemoteDevice {
    pub path: String,
    pub adapter_path: String,
    pub address: String,
    pub name: String,
    pub alias: String,
    pub paired: bool,
    pub bonded: bool,
    pub trusted: bool,
    pub uuids: Vec<String>,
    pub modalias: String,
    pub manufacturer_data: HashMap<u16, Vec<u8>>,
    pub reasons: Vec<String>,
}

impl RemoteDevice {
    pub fn display_name(&self) -> String {
        if !self.name.is_empty() {
            self.name.clone()
        } else if !self.alias.is_empty() {
            self.alias.clone()
        } else {
            self.address.clone()
        }
    }
}

// -- D-Bus helpers ------------------------------------------------------------

/// Open a connection to the BlueZ system bus.
pub async fn connect() -> Result<Connection> {
    Connection::system()
        .await
        .context("connect to D-Bus system bus")
}

async fn get_managed_objects(conn: &Connection) -> Result<ManagedObjects> {
    let reply = conn
        .call_method(
            Some(BLUEZ_BUS),
            "/",
            Some(OM_IFACE),
            "GetManagedObjects",
            &(),
        )
        .await
        .context("BlueZ ObjectManager.GetManagedObjects")?;
    let body = reply.body();
    Ok(body.deserialize()?)
}

fn try_str(v: &OwnedValue) -> Option<String> {
    v.downcast_ref::<&str>().ok().map(|s| s.to_string())
}

fn try_bool(v: &OwnedValue) -> Option<bool> {
    v.downcast_ref::<bool>().ok()
}

fn try_string_vec(v: &OwnedValue) -> Vec<String> {
    let Ok(cloned) = v.try_clone() else {
        return Vec::new();
    };
    Vec::<String>::try_from(cloned).unwrap_or_default()
}

fn try_mfr_map(v: &OwnedValue) -> HashMap<u16, Vec<u8>> {
    let Ok(cloned) = v.try_clone() else {
        return HashMap::new();
    };
    HashMap::<u16, Vec<u8>>::try_from(cloned).unwrap_or_default()
}

fn adapter_path_for_device(path: &str) -> Result<String> {
    let idx = path
        .rfind("/dev_")
        .with_context(|| format!("malformed BlueZ device path: {path}"))?;
    Ok(path[..idx].to_string())
}

fn build_remote(path: &str, props: &HashMap<String, OwnedValue>) -> Option<RemoteDevice> {
    let address = props
        .get("Address")
        .and_then(try_str)?
        .trim()
        .to_uppercase();
    if address.is_empty() {
        return None;
    }
    let name = props.get("Name").and_then(try_str).unwrap_or_default();
    let alias = props.get("Alias").and_then(try_str).unwrap_or_default();
    let paired = props.get("Paired").and_then(try_bool).unwrap_or(false);
    let bonded = props.get("Bonded").and_then(try_bool).unwrap_or(false);
    let trusted = props.get("Trusted").and_then(try_bool).unwrap_or(false);
    let modalias = props.get("Modalias").and_then(try_str).unwrap_or_default();
    let uuids = props
        .get("UUIDs")
        .map(try_string_vec)
        .unwrap_or_default()
        .into_iter()
        .map(|u| u.to_lowercase())
        .collect();
    let manufacturer_data = props
        .get("ManufacturerData")
        .map(try_mfr_map)
        .unwrap_or_default();
    let adapter_path = adapter_path_for_device(path).ok()?;
    Some(RemoteDevice {
        path: path.to_string(),
        adapter_path,
        address,
        name,
        alias,
        paired,
        bonded,
        trusted,
        uuids,
        modalias,
        manufacturer_data,
        reasons: Vec::new(),
    })
}

fn match_reasons(device: &RemoteDevice) -> Vec<String> {
    let mut reasons: Vec<String> = Vec::new();
    let combined = format!("{} {}", device.name, device.alias).to_lowercase();
    if REMOTE_NAME_KEYWORDS.iter().any(|kw| combined.contains(kw)) {
        reasons.push("remote-like name".to_string());
    }
    if device.uuids.iter().any(|u| u == HID_SERVICE_UUID_LOWER) {
        reasons.push("HID service".to_string());
    }
    if device
        .modalias
        .to_lowercase()
        .starts_with("bluetooth:v004c")
    {
        reasons.push("Apple modalias".to_string());
    }
    if let Some(apple_data) = device.manufacturer_data.get(&APPLE_COMPANY_ID) {
        reasons.push("Apple manufacturer data".to_string());
        if apple_data.starts_with(&APPLE_HID_MFR_PREFIX) {
            reasons.push("Apple HID manufacturer prefix".to_string());
        }
    }

    let has_name = reasons.iter().any(|r| r == "remote-like name");
    let has_hid = reasons.iter().any(|r| r == "HID service");
    let has_apple = reasons.iter().any(|r| {
        r == "Apple modalias"
            || r == "Apple manufacturer data"
            || r == "Apple HID manufacturer prefix"
    });

    if has_hid && (has_apple || has_name) {
        reasons
    } else {
        Vec::new()
    }
}

/// Enumerate every paired/bonded BlueZ device whose fingerprint matches a Siri
/// Remote. Optionally filtered to a fixed address allow-list (uppercase).
pub async fn list_siri_remotes(
    conn: &Connection,
    addresses: Option<&[String]>,
) -> Result<Vec<RemoteDevice>> {
    let want: Option<Vec<String>> = addresses.map(|addrs| {
        addrs
            .iter()
            .map(|a| a.trim().to_uppercase())
            .collect::<Vec<_>>()
    });

    let objects = get_managed_objects(conn).await?;
    let mut remotes = Vec::new();
    for (path, ifaces) in objects {
        let Some(props) = ifaces.get(DEVICE_IFACE) else {
            continue;
        };
        let path_str = path.as_str().to_string();
        let Some(mut device) = build_remote(&path_str, props) else {
            continue;
        };
        if let Some(wanted) = &want
            && !wanted.contains(&device.address)
        {
            continue;
        }
        if !device.paired && !device.bonded {
            continue;
        }
        let reasons = match_reasons(&device);
        if reasons.is_empty() {
            continue;
        }
        device.reasons = reasons;
        remotes.push(device);
    }
    remotes.sort_by(|a, b| a.address.cmp(&b.address).then_with(|| a.path.cmp(&b.path)));
    Ok(remotes)
}

/// Call `Adapter1.RemoveDevice(path)` to forget a previously paired device.
pub async fn remove(conn: &Connection, device: &RemoteDevice) -> Result<()> {
    let adapter_path = ObjectPath::try_from(device.adapter_path.as_str())
        .context("adapter path is not a valid D-Bus object path")?;
    let device_path = ObjectPath::try_from(device.path.as_str())
        .context("device path is not a valid D-Bus object path")?;
    conn.call_method(
        Some(BLUEZ_BUS),
        adapter_path,
        Some(ADAPTER_IFACE),
        "RemoveDevice",
        &(device_path,),
    )
    .await
    .context("Adapter1.RemoveDevice")?;
    Ok(())
}

/// Resolve a BlueZ device object path from a Bluetooth address by walking the
/// ObjectManager. Returns `None` if the device isn't known to BlueZ yet (try
/// scanning first).
pub async fn device_path_from_address(conn: &Connection, address: &str) -> Result<String> {
    let needle = address.trim().to_uppercase();
    let objects = get_managed_objects(conn).await?;
    for (path, ifaces) in objects {
        let Some(props) = ifaces.get(DEVICE_IFACE) else {
            continue;
        };
        let addr = props
            .get("Address")
            .and_then(try_str)
            .map(|s| s.trim().to_uppercase());
        if addr.as_deref() == Some(needle.as_str()) {
            return Ok(path.as_str().to_string());
        }
    }
    bail!("could not find BlueZ device path for address {address}")
}

/// Call `Device1.Pair()` and then mark the device as `Trusted=true`. This is
/// the explicit-pair path used by `pair.py` (Bleak's `pair=True`).
pub async fn pair_explicit(conn: &Connection, device_path: &str) -> Result<()> {
    let path = ObjectPath::try_from(device_path)
        .context("device path is not a valid D-Bus object path")?;
    conn.call_method(
        Some(BLUEZ_BUS),
        path.clone(),
        Some(DEVICE_IFACE),
        "Pair",
        &(),
    )
    .await
    .context("Device1.Pair")?;
    set_property_bool(conn, path, DEVICE_IFACE, "Trusted", true).await?;
    Ok(())
}

async fn set_property_bool(
    conn: &Connection,
    path: ObjectPath<'_>,
    iface: &str,
    name: &str,
    value: bool,
) -> Result<()> {
    let val = Value::from(value);
    conn.call_method(
        Some(BLUEZ_BUS),
        path,
        Some(PROPS_IFACE),
        "Set",
        &(iface, name, val),
    )
    .await
    .context("Properties.Set")?;
    Ok(())
}

/// Read `Device1.Bonded` for the device at `device_path`. Source of truth for
/// "did pairing actually stick". Replaces the Python `bluetoothctl devices Bonded`
/// shell-out.
pub async fn is_bonded(conn: &Connection, device_path: &str) -> Result<bool> {
    let path = ObjectPath::try_from(device_path)
        .context("device path is not a valid D-Bus object path")?;
    let reply = conn
        .call_method(
            Some(BLUEZ_BUS),
            path,
            Some(PROPS_IFACE),
            "Get",
            &(DEVICE_IFACE, "Bonded"),
        )
        .await
        .context("Properties.Get(Bonded)")?;
    let body = reply.body();
    // Properties.Get returns a variant `v`; deserialize that and read the
    // contained bool.
    let owned: OwnedValue = body.deserialize()?;
    Ok(bool::try_from(&owned).unwrap_or(false))
}
