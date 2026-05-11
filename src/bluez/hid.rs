//! HID-over-GATT side-channel over BlueZ D-Bus.
//!
//! The Siri Remote's HID service has eight `Report` characteristics that all
//! share UUID `0x2A4D` and differ only by their Report Reference descriptor
//! (report id + report type) and ATT handle. `btleplug` collapses
//! same-UUID characteristics within a service to a single instance, so it
//! cannot reach the individual reports — in particular it can never address
//! the Output reports that the remote needs the `0xAF` magic byte on, nor
//! subscribe to all six Input reports.
//!
//! This module talks straight to `org.bluez.GattCharacteristic1` /
//! `org.bluez.GattDescriptor1` so every Report instance is addressable, and
//! exposes a stream that merges `PropertiesChanged` signals from every
//! subscribed Input report into a single sequence.

#![cfg(target_os = "linux")]

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use futures::StreamExt;
use zbus::message::Type as MessageType;
use zbus::{Connection, MatchRule, MessageStream};
use zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

const BLUEZ_BUS: &str = "org.bluez";
const OM_IFACE: &str = "org.freedesktop.DBus.ObjectManager";
const PROPS_IFACE: &str = "org.freedesktop.DBus.Properties";
const SERVICE_IFACE: &str = "org.bluez.GattService1";
const CHAR_IFACE: &str = "org.bluez.GattCharacteristic1";
const DESC_IFACE: &str = "org.bluez.GattDescriptor1";

const HID_SERVICE_UUID: &str = "00001812-0000-1000-8000-00805f9b34fb";
const HID_REPORT_UUID: &str = "00002a4d-0000-1000-8000-00805f9b34fb";
const REPORT_REFERENCE_UUID: &str = "00002908-0000-1000-8000-00805f9b34fb";

/// Standard HID-over-GATT report types from the Report Reference descriptor.
pub const REPORT_TYPE_INPUT: u8 = 1;
pub const REPORT_TYPE_OUTPUT: u8 = 2;
pub const REPORT_TYPE_FEATURE: u8 = 3;

/// One `Report` characteristic instance under the HID service.
#[derive(Clone, Debug)]
pub struct ReportChar {
    pub path: OwnedObjectPath,
    pub report_id: u8,
    pub report_type: u8,
    pub flags: Vec<String>,
}

impl ReportChar {
    pub fn supports_notify(&self) -> bool {
        self.flags.iter().any(|f| f == "notify" || f == "indicate")
    }

    pub fn supports_write(&self) -> bool {
        self.flags
            .iter()
            .any(|f| f == "write" || f == "write-without-response")
    }

    fn prefers_write_without_response(&self) -> bool {
        self.flags.iter().any(|f| f == "write-without-response")
    }
}

/// Snapshot of the HID service for one device.
pub struct HidSession {
    conn: Connection,
    device_path: String,
    reports: Vec<ReportChar>,
}

impl HidSession {
    /// Walk the BlueZ ObjectManager tree under `device_path`, enumerate every
    /// `0x2A4D` report characteristic, and classify each by its Report
    /// Reference descriptor. The device must already be connected and have
    /// `ServicesResolved=true` — otherwise BlueZ will not yet have populated
    /// the GATT subtree.
    pub async fn open(conn: Connection, device_path: &str) -> Result<Self> {
        let objects = get_managed_objects(&conn).await?;
        let device_prefix = format!("{device_path}/");

        // Locate every HID-service path that belongs to this device.
        let mut hid_service_paths: Vec<String> = Vec::new();
        for (path, ifaces) in &objects {
            let path_str = path.as_str();
            if !path_str.starts_with(&device_prefix) {
                continue;
            }
            let Some(props) = ifaces.get(SERVICE_IFACE) else {
                continue;
            };
            if uuid_lower(props.get("UUID")).as_deref() == Some(HID_SERVICE_UUID) {
                hid_service_paths.push(path_str.to_string());
            }
        }
        if hid_service_paths.is_empty() {
            bail!(
                "no HID service ({HID_SERVICE_UUID}) under {device_path}; \
                 wait for ServicesResolved=true before opening HidSession",
            );
        }

        // Collect every 0x2A4D characteristic under one of those services.
        let mut report_paths: Vec<(String, Vec<String>)> = Vec::new();
        for (path, ifaces) in &objects {
            let path_str = path.as_str();
            let Some(props) = ifaces.get(CHAR_IFACE) else {
                continue;
            };
            if uuid_lower(props.get("UUID")).as_deref() != Some(HID_REPORT_UUID) {
                continue;
            }
            if !hid_service_paths
                .iter()
                .any(|svc| path_str.starts_with(&format!("{svc}/")))
            {
                continue;
            }
            let flags = props
                .get("Flags")
                .map(string_vec)
                .unwrap_or_default();
            report_paths.push((path_str.to_string(), flags));
        }
        if report_paths.is_empty() {
            bail!("no HID Report characteristics ({HID_REPORT_UUID}) under {device_path}");
        }

        // For each report char, find its Report Reference descriptor path so
        // we can read (report_id, report_type). A report without that
        // descriptor cannot be classified; skip it (we only act on classified
        // ones).
        let mut reports: Vec<ReportChar> = Vec::with_capacity(report_paths.len());
        for (char_path, flags) in report_paths {
            let char_prefix = format!("{char_path}/");
            let mut ref_desc: Option<String> = None;
            for (path, ifaces) in &objects {
                let path_str = path.as_str();
                if !path_str.starts_with(&char_prefix) {
                    continue;
                }
                let Some(props) = ifaces.get(DESC_IFACE) else {
                    continue;
                };
                if uuid_lower(props.get("UUID")).as_deref() == Some(REPORT_REFERENCE_UUID) {
                    ref_desc = Some(path_str.to_string());
                    break;
                }
            }
            let Some(desc_path) = ref_desc else {
                eprintln!(
                    "warning: HID report char {char_path} has no Report Reference \
                     descriptor; cannot classify, skipping"
                );
                continue;
            };
            let bytes = read_descriptor(&conn, &desc_path)
                .await
                .with_context(|| format!("read Report Reference of {char_path}"))?;
            if bytes.len() < 2 {
                eprintln!(
                    "warning: Report Reference of {char_path} is {} byte(s); expected 2",
                    bytes.len()
                );
                continue;
            }
            reports.push(ReportChar {
                path: OwnedObjectPath::try_from(char_path.clone())
                    .context("invalid GattCharacteristic1 path")?,
                report_id: bytes[0],
                report_type: bytes[1],
                flags,
            });
        }

        // Stable ordering so log output and event order are reproducible:
        // by (report_type, report_id, path).
        reports.sort_by(|a, b| {
            a.report_type
                .cmp(&b.report_type)
                .then_with(|| a.report_id.cmp(&b.report_id))
                .then_with(|| a.path.as_str().cmp(b.path.as_str()))
        });

        Ok(Self {
            conn,
            device_path: device_path.to_string(),
            reports,
        })
    }

    pub fn reports(&self) -> &[ReportChar] {
        &self.reports
    }

    pub fn device_path(&self) -> &str {
        &self.device_path
    }

    /// Write `byte` to every writable non-Input report. Apple's "enable
    /// input" handshake on HID-over-GATT remotes asks for the magic byte on
    /// a writable Report that is not itself an Input. The exact report type
    /// is firmware-dependent: 1st/2nd-gen remotes expose it as an Output
    /// report (type 2), the 3rd-gen ships only Feature reports (type 3) and
    /// has no Output reports at all. We do not know which non-Input report
    /// instance the remote actually checks, so we write the byte to every
    /// plausible candidate and the remote ignores the rest. Bails when no
    /// candidate exists or every write fails; otherwise returns the number
    /// of successful writes.
    pub async fn write_input_enable(&self, byte: u8) -> Result<usize> {
        let candidates: Vec<&ReportChar> = self
            .reports
            .iter()
            .filter(|r| r.report_type != REPORT_TYPE_INPUT && r.supports_write())
            .collect();
        if candidates.is_empty() {
            bail!(
                "no writable Output/Feature HID Report under {}",
                self.device_path
            );
        }
        let mut ok = 0usize;
        let mut last_err: Option<anyhow::Error> = None;
        for r in candidates {
            let prefer_cmd = r.prefers_write_without_response();
            let kind = match r.report_type {
                REPORT_TYPE_OUTPUT => "output",
                REPORT_TYPE_FEATURE => "feature",
                _ => "other",
            };
            eprintln!(
                "Sending input-enable byte (0x{byte:02X}) to {kind} report \
                 id=0x{:02X} path={}.",
                r.report_id,
                r.path.as_str(),
            );
            match write_characteristic(&self.conn, r.path.as_ref(), &[byte], prefer_cmd).await {
                Ok(()) => ok += 1,
                Err(e) => {
                    eprintln!(
                        "warning: input-enable write to {} failed: {e:#}",
                        r.path.as_str()
                    );
                    last_err = Some(e);
                }
            }
        }
        if ok == 0 {
            return Err(last_err.unwrap_or_else(|| {
                anyhow::anyhow!("no Report accepted the input-enable byte")
            }));
        }
        Ok(ok)
    }

    /// Subscribe to every Input report that supports notifications and start
    /// streaming `(report_id, value)` for each `PropertiesChanged` carrying a
    /// new `Value` on one of those paths.
    ///
    /// Notifications are kept active until the returned stream is dropped;
    /// BlueZ tears them down on disconnect too.
    pub async fn input_stream(&self) -> Result<InputStream> {
        let inputs: Vec<&ReportChar> = self
            .reports
            .iter()
            .filter(|r| r.report_type == REPORT_TYPE_INPUT && r.supports_notify())
            .collect();
        if inputs.is_empty() {
            bail!(
                "no notifiable HID Input report under {}; the remote will not stream events",
                self.device_path
            );
        }

        // Build the PropertiesChanged subscription FIRST, so notifications
        // emitted between StartNotify and stream construction are not lost.
        let rule = MatchRule::builder()
            .msg_type(MessageType::Signal)
            .sender(BLUEZ_BUS)
            .map_err(|e| anyhow::anyhow!(e.to_string()))?
            .interface(PROPS_IFACE)
            .map_err(|e| anyhow::anyhow!(e.to_string()))?
            .member("PropertiesChanged")
            .map_err(|e| anyhow::anyhow!(e.to_string()))?
            .path_namespace(self.device_path.as_str())
            .map_err(|e| anyhow::anyhow!(e.to_string()))?
            .arg(0, CHAR_IFACE)
            .map_err(|e| anyhow::anyhow!(e.to_string()))?
            .build();
        let stream = MessageStream::for_match_rule(rule, &self.conn, Some(256))
            .await
            .context("subscribe to PropertiesChanged on HID chars")?;

        // Path -> report_id lookup. Pre-built so notification dispatch is
        // a single hash hit instead of a linear walk.
        let mut by_path: HashMap<String, u8> = HashMap::with_capacity(inputs.len());
        for r in &inputs {
            by_path.insert(r.path.as_str().to_string(), r.report_id);
        }

        for r in &inputs {
            start_notify(&self.conn, r.path.as_ref()).await.with_context(|| {
                format!(
                    "StartNotify on input report id=0x{:02X} path={}",
                    r.report_id,
                    r.path.as_str()
                )
            })?;
            eprintln!(
                "Enabled HID input notifications on report id=0x{:02X} path={}.",
                r.report_id,
                r.path.as_str(),
            );
        }

        Ok(InputStream {
            inner: stream,
            by_path,
        })
    }
}

/// Stream of `(report_id, value)` decoded from `PropertiesChanged` signals
/// on the subscribed Input report paths.
pub struct InputStream {
    inner: MessageStream,
    by_path: HashMap<String, u8>,
}

impl InputStream {
    /// Advance until the next decoded report, ignoring property changes that
    /// don't carry a fresh `Value` and signals on paths we didn't subscribe
    /// to. Returns `None` when the underlying D-Bus stream ends.
    pub async fn next_report(&mut self) -> Option<(u8, Vec<u8>)> {
        while let Some(msg) = self.inner.next().await {
            let Ok(msg) = msg else { continue };
            let header = msg.header();
            let Some(path) = header.path() else { continue };
            let Some(&report_id) = self.by_path.get(path.as_str()) else {
                continue;
            };
            let body = msg.body();
            let Ok((iface, changed, _invalidated)): Result<
                (String, HashMap<String, OwnedValue>, Vec<String>),
                _,
            > = body.deserialize() else {
                continue;
            };
            if iface != CHAR_IFACE {
                continue;
            }
            let Some(value) = changed.get("Value") else {
                continue;
            };
            // Value is signature 'ay'.
            let Ok(bytes) = Vec::<u8>::try_from(value.clone()) else {
                continue;
            };
            return Some((report_id, bytes));
        }
        None
    }
}
// -- low-level wrappers -------------------------------------------------------

type ManagedObjects = HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>>;

async fn get_managed_objects(conn: &Connection) -> Result<ManagedObjects> {
    let reply = conn
        .call_method(Some(BLUEZ_BUS), "/", Some(OM_IFACE), "GetManagedObjects", &())
        .await
        .context("BlueZ ObjectManager.GetManagedObjects")?;
    let body = reply.body();
    Ok(body.deserialize()?)
}

async fn read_descriptor(conn: &Connection, desc_path: &str) -> Result<Vec<u8>> {
    let path = ObjectPath::try_from(desc_path)
        .context("invalid GattDescriptor1 path")?;
    let opts: HashMap<&str, Value> = HashMap::new();
    let reply = conn
        .call_method(
            Some(BLUEZ_BUS),
            path,
            Some(DESC_IFACE),
            "ReadValue",
            &(opts,),
        )
        .await
        .context("GattDescriptor1.ReadValue")?;
    let body = reply.body();
    Ok(body.deserialize()?)
}

async fn write_characteristic(
    conn: &Connection,
    char_path: ObjectPath<'_>,
    value: &[u8],
    prefer_command: bool,
) -> Result<()> {
    let mut opts: HashMap<&str, Value> = HashMap::new();
    let kind: &str = if prefer_command { "command" } else { "request" };
    opts.insert("type", Value::from(kind));
    conn.call_method(
        Some(BLUEZ_BUS),
        char_path,
        Some(CHAR_IFACE),
        "WriteValue",
        &(value.to_vec(), opts),
    )
    .await
    .context("GattCharacteristic1.WriteValue")?;
    Ok(())
}

async fn start_notify(conn: &Connection, char_path: ObjectPath<'_>) -> Result<()> {
    conn.call_method(
        Some(BLUEZ_BUS),
        char_path,
        Some(CHAR_IFACE),
        "StartNotify",
        &(),
    )
    .await
    .context("GattCharacteristic1.StartNotify")?;
    Ok(())
}

fn uuid_lower(v: Option<&OwnedValue>) -> Option<String> {
    v.and_then(|val| val.downcast_ref::<&str>().ok())
        .map(|s| s.to_ascii_lowercase())
}

fn string_vec(v: &OwnedValue) -> Vec<String> {
    Vec::<String>::try_from(v.clone()).unwrap_or_default()
}
