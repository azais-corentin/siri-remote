//! Shared connection plumbing and typed event stream for the Siri Remote.
//!
//! Both `events` (stdout streaming) and `view` (ratatui dashboard) sit on top
//! of [`Session`]. Selection / scan / pairing / configure-notifications logic
//! used to live in `events.rs`; this module is the single owner so the two
//! frontends stay in lock-step.

use std::pin::Pin;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use btleplug::api::{
    Central, CharPropFlags, Characteristic, Manager as _, Peripheral as _, ScanFilter,
    ValueNotification,
};
use btleplug::platform::{Adapter, Manager, Peripheral, PeripheralId};
use futures::Stream;
use futures::StreamExt;
use tokio::time::{Instant, MissedTickBehavior, timeout};
use uuid::Uuid;

use crate::decoder::{InputDecoder, TouchDecoder, TouchEvent, power_state};
use crate::hid::{BATTERY_LEVEL_UUID, BATTERY_POWER_UUID, HID_REPORT_UUID, find_by_uuid};
use log::{info, warn};
use crate::scan;
#[cfg(target_os = "linux")]
use crate::bluez;

pub const AUTH_FAILURE_MARKERS: &[&str] = &[
    "auth",
    "encrypt",
    "not authorized",
    "not permitted",
    "insufficient authentication",
    "authenticationfailed",
];

pub const ADDRESS_RESOLVE_TIMEOUT: Duration = Duration::from_secs(10);
pub const DIRECT_CONNECT_TIMEOUT: Duration = Duration::from_secs(60);
pub const PAIRED_CONNECT_TIMEOUT: Duration = Duration::from_secs(60);

/// Magic byte the Siri Remote needs on any HID Output report before it
/// starts emitting input notifications. Same value the SiriRemote-Linux
/// reference and `events.py` use (`b"\xAF"`).
pub const INPUT_ENABLE_BYTE: u8 = 0xAF;

#[derive(Clone, Debug)]
pub struct Selection {
    pub address: String,
    pub name: String,
    pub peripheral_id: Option<PeripheralId>,
    pub identity_address: Option<String>,
    pub requires_pairing: bool,
    pub rssi: Option<i16>,
}

pub enum InitError {
    Invalid(String),
    Timeout,
}

pub async fn make_adapter() -> Result<Adapter> {
    let manager = Manager::new().await.context("init BLE manager")?;
    let adapters = manager.adapters().await.context("list BLE adapters")?;
    adapters
        .into_iter()
        .next()
        .context("no BLE adapter found on this host")
}

pub async fn choose_initial_selection(
    adapter: &Adapter,
    address: Option<&str>,
    scan_dur: Duration,
) -> Result<Selection, InitError> {
    if let Some(addr) = address {
        let normalized = normalize_address(addr).map_err(InitError::Invalid)?;
        let selection = Selection {
            address: normalized.clone(),
            name: "requested address".to_string(),
            peripheral_id: None,
            identity_address: Some(normalized),
            requires_pairing: false,
            rssi: None,
        };
        print_selected(&selection, "requested address; connecting directly");
        return Ok(selection);
    }

    // 1) BlueZ already knows about a bonded Siri Remote — by far the common
    //    case once `pair` has run once. Pick the one currently connected if
    //    any, otherwise the first bonded entry. This makes a bare
    //    `siri-remote events` invocation Just Work.
    #[cfg(target_os = "linux")]
    if let Some(selection) = pick_bonded_selection().await {
        return Ok(selection);
    }

    // 2) No bonded Siri Remote — fall back to a pair-mode scan and pair the
    //    closest remote that's broadcasting the Apple HID prefix.
    match scan::scan_for_remote(adapter, scan_dur, scan_dur + Duration::from_secs(30)).await {
        Ok(cand) => {
            let selection = Selection {
                address: cand.last_address.clone(),
                name: cand
                    .last_name
                    .clone()
                    .unwrap_or_else(|| cand.identity_address.clone()),
                peripheral_id: Some(cand.peripheral_id),
                identity_address: Some(cand.identity_address),
                requires_pairing: true,
                rssi: Some(cand.last_rssi),
            };
            print_selected(
                &selection,
                "Siri Remote in pairing mode; bonding before connecting",
            );
            Ok(selection)
        }
        Err(scan::ScanError::Timeout) => Err(InitError::Timeout),
        Err(scan::ScanError::Other(e)) => Err(InitError::Invalid(e.to_string())),
    }
}

#[cfg(target_os = "linux")]
pub async fn pick_bonded_selection() -> Option<Selection> {
    let conn = bluez::device::connect().await.ok()?;
    let remotes = bluez::device::list_siri_remotes(&conn, None).await.ok()?;
    if remotes.is_empty() {
        return None;
    }
    // Prefer the one BlueZ already has a live connection on, then the rest.
    let chosen = remotes
        .iter()
        .find(|r| r.connected)
        .unwrap_or(&remotes[0]);
    if remotes.len() > 1 {
        info!(
            "{} bonded Siri Remote(s) found; picking {} ({}). Pass --address to override.",
            remotes.len(),
            chosen.display_name(),
            chosen.address,
        );
        for r in &remotes {
            let marker = if r.address == chosen.address { "*" } else { " " };
            info!(
                "  {marker} address={} name={:?} connected={} bonded={}",
                r.address, r.name, r.connected, r.bonded,
            );
        }
    }
    let selection = Selection {
        address: chosen.address.clone(),
        name: chosen.display_name(),
        peripheral_id: None,
        identity_address: Some(chosen.address.clone()),
        requires_pairing: false,
        rssi: None,
    };
    let reason = if chosen.connected {
        "bonded and currently connected"
    } else {
        "bonded (will reconnect)"
    };
    print_selected(&selection, reason);
    Some(selection)
}

pub fn normalize_address(addr: &str) -> Result<String, String> {
    let t = addr.trim().to_uppercase();
    let ok = t.len() == 17
        && t.bytes().enumerate().all(|(i, b)| {
            if i % 3 == 2 {
                b == b':'
            } else {
                b.is_ascii_hexdigit()
            }
        });
    if !ok {
        return Err(format!("invalid Bluetooth address: {addr:?}"));
    }
    Ok(t)
}

pub fn print_selected(selection: &Selection, reason: &str) {
    let rssi = match selection.rssi {
        Some(r) => format!(" rssi={r}"),
        None => String::new(),
    };
    let identity = match &selection.identity_address {
        Some(i) => format!(" identity={i}"),
        None => String::new(),
    };
    info!(
        "Selected {:?} address={}{identity}{rssi} ({reason}).",
        selection.name, selection.address
    );
}

// -- Connect ------------------------------------------------------------------

pub async fn resolve_peripheral(adapter: &Adapter, selection: &Selection) -> Result<Peripheral> {
    if let Some(id) = &selection.peripheral_id
        && let Ok(p) = adapter.peripheral(id).await
    {
        return Ok(p);
    }

    let needle = selection.address.to_uppercase();
    let _ = adapter.start_scan(ScanFilter::default()).await;
    let deadline = Instant::now() + ADDRESS_RESOLVE_TIMEOUT;
    let mut last_err: Option<anyhow::Error> = None;

    while Instant::now() < deadline {
        match adapter.peripherals().await {
            Ok(list) => {
                for p in list {
                    let Ok(Some(props)) = p.properties().await else {
                        continue;
                    };
                    if props.address.to_string().to_uppercase() == needle {
                        let _ = adapter.stop_scan().await;
                        return Ok(p);
                    }
                }
            }
            Err(e) => last_err = Some(anyhow!(e.to_string())),
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let _ = adapter.stop_scan().await;
    match last_err {
        Some(e) => Err(e),
        None => Err(anyhow!(
            "could not find peripheral with address {} within {:.0}s",
            selection.address,
            ADDRESS_RESOLVE_TIMEOUT.as_secs_f64(),
        )),
    }
}

/// Connect (or reconnect with pairing) and discover services. Returns the
/// live [`Peripheral`] and a refreshed [`Selection`] whose `peripheral_id`
/// and `requires_pairing` are pinned to the just-established link.
pub async fn connect_once(
    adapter: &Adapter,
    selection: &Selection,
) -> Result<(Peripheral, Selection)> {
    let peripheral = resolve_peripheral(adapter, selection).await?;

    if selection.requires_pairing {
        info!("Connecting to {} with pairing...", selection.address);
        connect_with_pairing(&peripheral, selection).await?;
    } else {
        info!("Connecting to {}...", selection.address);
        match timeout(DIRECT_CONNECT_TIMEOUT, peripheral.connect()).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(anyhow!(e.to_string())),
            Err(_) => {
                info!(
                    "Direct connect timed out; retrying with pair=True (uses existing bond if already paired)."
                );
                connect_with_pairing(&peripheral, selection).await?;
            }
        }
    }

    peripheral
        .discover_services()
        .await
        .context("discover services")?;

    let refreshed = Selection {
        address: selection
            .identity_address
            .clone()
            .unwrap_or_else(|| selection.address.clone()),
        name: selection.name.clone(),
        peripheral_id: Some(peripheral.id()),
        identity_address: selection.identity_address.clone(),
        requires_pairing: false,
        rssi: None,
    };
    Ok((peripheral, refreshed))
}

#[cfg(target_os = "linux")]
pub async fn connect_with_pairing(peripheral: &Peripheral, selection: &Selection) -> Result<()> {
    let agent = crate::bluez::agent::AgentSession::register().await?;
    let conn = agent.connection().clone();
    let dev_path =
        crate::bluez::device::device_path_from_address(&conn, &selection.address).await?;
    // Best-effort Pair() — if the link is already bonded BlueZ returns
    // AlreadyExists, which we tolerate before connecting.
    let _ = crate::bluez::device::pair_explicit(&conn, &dev_path).await;
    timeout(PAIRED_CONNECT_TIMEOUT, peripheral.connect())
        .await
        .map_err(|_| anyhow!("paired connect timed out"))?
        .map_err(|e| anyhow!(e.to_string()))?;
    agent.close().await;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub async fn connect_with_pairing(peripheral: &Peripheral, _selection: &Selection) -> Result<()> {
    timeout(PAIRED_CONNECT_TIMEOUT, peripheral.connect())
        .await
        .map_err(|_| anyhow!("paired connect timed out"))?
        .map_err(|e| anyhow!(e.to_string()))?;
    Ok(())
}

// -- Configure notifications --------------------------------------------------

/// Subscribe to battery / power notifications via btleplug (one
/// characteristic per UUID — btleplug handles them fine), then open a
/// BlueZ-D-Bus side-channel for the HID service: enable input streaming
/// by writing the magic byte to every Output report instance, then
/// `StartNotify` on every Input report instance. The latter is the only
/// way to reach the per-instance reports the Siri Remote uses, because
/// btleplug deduplicates same-UUID characteristics within a service.
#[cfg(target_os = "linux")]
pub async fn configure_notifications(
    peripheral: &Peripheral,
    selection: &Selection,
) -> Result<(InputDecoder, TouchDecoder, bluez::hid::InputStream)> {
    let battery_char = find_by_uuid(peripheral, BATTERY_LEVEL_UUID);
    let power_char = find_by_uuid(peripheral, BATTERY_POWER_UUID);
    let battery_ok = start_optional_notify(peripheral, battery_char.as_ref(), "battery").await;
    let power_ok = start_optional_notify(peripheral, power_char.as_ref(), "power").await;
    if !battery_ok {
        warn!("warning: battery notifications were not enabled");
    }
    if !power_ok {
        warn!("warning: power notifications were not enabled");
    }

    let conn = bluez::device::connect().await?;
    let resolve_addr = selection
        .identity_address
        .as_deref()
        .unwrap_or(&selection.address);
    let device_path = bluez::device::device_path_from_address(&conn, resolve_addr).await?;
    let hid = bluez::hid::HidSession::open(conn, &device_path).await?;

    info!(
        "HID service exposes {} report characteristic(s) under {}:",
        hid.reports().len(),
        hid.device_path(),
    );
    for r in hid.reports() {
        let kind = match r.report_type {
            bluez::hid::REPORT_TYPE_INPUT => "input",
            bluez::hid::REPORT_TYPE_OUTPUT => "output",
            bluez::hid::REPORT_TYPE_FEATURE => "feature",
            _ => "unknown",
        };
        info!(
            "  report id=0x{:02X} type={kind:7} flags={}",
            r.report_id,
            r.flags.join(","),
        );
    }

    // Subscribe before writing the magic byte so we don't lose any reports
    // emitted between the write and our StartNotify.
    let input_stream = hid.input_stream().await?;
    hid.write_input_enable(INPUT_ENABLE_BYTE).await?;
    info!("Notifications enabled; waiting for events...");

    Ok((InputDecoder::new(), TouchDecoder::new(), input_stream))
}

async fn start_optional_notify(
    peripheral: &Peripheral,
    char: Option<&Characteristic>,
    source: &str,
) -> bool {
    let Some(c) = char else {
        return false;
    };
    if !c.properties.contains(CharPropFlags::NOTIFY)
        && !c.properties.contains(CharPropFlags::INDICATE)
    {
        warn!(
            "warning: {source} characteristic {} is not notifiable",
            c.uuid
        );
        return false;
    }
    match peripheral.subscribe(c).await {
        Ok(_) => {
            info!("Enabled {source} notifications on {}.", c.uuid);
            true
        }
        Err(e) => {
            warn!(
                "warning: failed to enable {source} notifications on {}: {e:?}",
                c.uuid
            );
            false
        }
    }
}

// -- Pairing fallback / auth detection ---------------------------------------

pub fn is_auth_failure(err: &anyhow::Error) -> bool {
    let detail = format!("{err:?}").to_lowercase().replace(' ', "");
    AUTH_FAILURE_MARKERS
        .iter()
        .any(|m| detail.contains(&m.replace(' ', "")))
}

pub async fn switch_to_pairing_scan(adapter: &Adapter) -> Result<Selection, InitError> {
    info!(
        "Connection failed because the link is not bonded/authenticated. \
         Put the remote in pairing mode (hold MENU + Volume Up) and keep it nearby."
    );
    let cand = scan::scan_for_nearest_remote(adapter, Duration::from_secs(5))
        .await
        .ok_or(InitError::Timeout)?;
    let selection = Selection {
        address: cand.last_address.clone(),
        name: cand
            .last_name
            .clone()
            .unwrap_or_else(|| cand.identity_address.clone()),
        peripheral_id: Some(cand.peripheral_id),
        identity_address: Some(cand.identity_address),
        requires_pairing: true,
        rssi: Some(cand.last_rssi),
    };
    print_selected(&selection, "pairing-mode remote");
    Ok(selection)
}

// -- Typed event stream -------------------------------------------------------

/// Power-state byte translated to a labelled variant. Matches `power_state`
/// in `decoder.rs`; `Unknown(b)` carries the unrecognised byte for diagnostics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowerState {
    Charging,
    Discharging,
    PluggedIn,
    Unknown(u8),
}

impl PowerState {
    pub fn from_byte(b: u8) -> Self {
        match power_state(b) {
            Some("charging") => Self::Charging,
            Some("discharging") => Self::Discharging,
            Some("plugged-in") => Self::PluggedIn,
            _ => Self::Unknown(b),
        }
    }
}

/// Single decoded event from the remote. Both `events` (formats to stdout)
/// and `view` (mutates ratatui state) consume this stream.
#[derive(Clone, Debug)]
pub enum DeviceEvent {
    /// 2-byte HID system-buttons report. `pressed` / `released` are the
    /// bits that transitioned vs. the previous frame; both are `0` on a
    /// state-refresh packet (mask unchanged), which matches the legacy
    /// `events` line `unknown HID packet len=2`.
    Buttons {
        report_id: u8,
        mask: u16,
        pressed: u16,
        released: u16,
        raw: [u8; 2],
    },
    /// Parsed touchpad sample. `raw` is the full wire payload (11 or 18
    /// bytes) so callers can dump it verbatim.
    Touch {
        report_id: u8,
        event: TouchEvent,
        raw: Vec<u8>,
    },
    /// HID Input report whose layout we have no parser for (audio,
    /// vendor-specific, etc.).
    UnknownInput { report_id: u8, payload: Vec<u8> },
    /// Battery level characteristic notification (0..=100).
    Battery { value: u8, raw: Vec<u8> },
    /// Battery power-state characteristic notification.
    Power { state: PowerState, raw: Vec<u8> },
    /// btleplug notification on a UUID that is neither HID Report,
    /// Battery Level, nor Battery Power.
    UnknownOther { uuid: Uuid, payload: Vec<u8> },
}

/// Ready-to-pump notification stream. Construct via [`Session::open`] after
/// `connect_once`; drive with [`Session::next_event`] until it returns `None`
/// (disconnect detected by the liveness probe).
#[cfg(target_os = "linux")]
pub struct Session {
    peripheral: Peripheral,
    bt_stream: Pin<Box<dyn Stream<Item = ValueNotification> + Send>>,
    hid_stream: bluez::hid::InputStream,
    input_decoder: InputDecoder,
    touch_decoder: TouchDecoder,
    idle: tokio::time::Interval,
}

#[cfg(target_os = "linux")]
impl Session {
    pub async fn open(peripheral: Peripheral, selection: &Selection) -> Result<Self> {
        let (input_decoder, touch_decoder, hid_stream) =
            configure_notifications(&peripheral, selection).await?;
        let bt_stream = peripheral.notifications().await?;
        let mut idle = tokio::time::interval(Duration::from_secs(1));
        idle.set_missed_tick_behavior(MissedTickBehavior::Delay);
        Ok(Self {
            peripheral,
            bt_stream,
            hid_stream,
            input_decoder,
            touch_decoder,
            idle,
        })
    }


    /// Returns the next decoded event, or `None` when the underlying link
    /// has dropped (either source closed or the liveness probe sees the
    /// peripheral disconnected).
    pub async fn next_event(&mut self) -> Option<DeviceEvent> {
        loop {
            tokio::select! {
                n = self.bt_stream.next() => {
                    let n = n?;
                    if let Some(ev) = decode_btleplug(n) {
                        return Some(ev);
                    }
                }
                r = self.hid_stream.next_report() => {
                    let (report_id, value) = r?;
                    return Some(decode_hid_report(
                        report_id,
                        value,
                        &mut self.input_decoder,
                        &mut self.touch_decoder,
                    ));
                }
                _ = self.idle.tick() => {
                    if !self.peripheral.is_connected().await.unwrap_or(false) {
                        return None;
                    }
                }
            }
        }
    }
}

fn decode_btleplug(n: ValueNotification) -> Option<DeviceEvent> {
    // HID Report notifications are handled via the BlueZ D-Bus stream so
    // we can address every per-instance Report. The btleplug stream still
    // emits them for the one collapsed 0x2A4D characteristic it knows
    // about — drop those to avoid double-decoding.
    if n.uuid == HID_REPORT_UUID {
        return None;
    }
    if n.uuid == BATTERY_LEVEL_UUID {
        if n.value.is_empty() {
            return Some(DeviceEvent::UnknownOther {
                uuid: n.uuid,
                payload: n.value,
            });
        }
        let value = n.value[0];
        return Some(DeviceEvent::Battery {
            value,
            raw: n.value,
        });
    }
    if n.uuid == BATTERY_POWER_UUID {
        if n.value.is_empty() {
            return Some(DeviceEvent::UnknownOther {
                uuid: n.uuid,
                payload: n.value,
            });
        }
        let state = PowerState::from_byte(n.value[0]);
        return Some(DeviceEvent::Power {
            state,
            raw: n.value,
        });
    }
    Some(DeviceEvent::UnknownOther {
        uuid: n.uuid,
        payload: n.value,
    })
}

fn decode_hid_report(
    report_id: u8,
    value: Vec<u8>,
    input: &mut InputDecoder,
    touch: &mut TouchDecoder,
) -> DeviceEvent {
    if value.len() == 2 {
        let mask = u16::from_le_bytes([value[0], value[1]]);
        let prev = input.advance(mask);
        let (pressed, released) = if mask == prev {
            (0, 0)
        } else {
            (mask & !prev, prev & !mask)
        };
        return DeviceEvent::Buttons {
            report_id,
            mask,
            pressed,
            released,
            raw: [value[0], value[1]],
        };
    }
    if let Some(event) = touch.parse(&value) {
        return DeviceEvent::Touch {
            report_id,
            event,
            raw: value,
        };
    }
    DeviceEvent::UnknownInput {
        report_id,
        payload: value,
    }
}

// -- Tests --------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buttons_decode_emits_transitions() {
        let mut input = InputDecoder::new();
        let mut touch = TouchDecoder::new();
        // First press of TV (0x0001).
        let ev = decode_hid_report(0xFB, vec![0x01, 0x00], &mut input, &mut touch);
        match ev {
            DeviceEvent::Buttons {
                report_id,
                mask,
                pressed,
                released,
                raw,
            } => {
                assert_eq!(report_id, 0xFB);
                assert_eq!(mask, 0x0001);
                assert_eq!(pressed, 0x0001);
                assert_eq!(released, 0x0000);
                assert_eq!(raw, [0x01, 0x00]);
            }
            _ => panic!("expected Buttons"),
        }
        // Release: pressed=0, released=TV.
        let ev = decode_hid_report(0xFB, vec![0x00, 0x00], &mut input, &mut touch);
        match ev {
            DeviceEvent::Buttons {
                mask,
                pressed,
                released,
                ..
            } => {
                assert_eq!(mask, 0x0000);
                assert_eq!(pressed, 0x0000);
                assert_eq!(released, 0x0001);
            }
            _ => panic!("expected Buttons"),
        }
        // State-refresh: same mask twice → pressed/released both zero
        // (legacy "unknown HID packet len=2" path).
        let _ = decode_hid_report(0xFB, vec![0x02, 0x00], &mut input, &mut touch);
        let ev = decode_hid_report(0xFB, vec![0x02, 0x00], &mut input, &mut touch);
        match ev {
            DeviceEvent::Buttons {
                pressed, released, ..
            } => {
                assert_eq!(pressed, 0);
                assert_eq!(released, 0);
            }
            _ => panic!("expected Buttons"),
        }
    }

    #[test]
    fn touch_decode_emits_touch_event() {
        let mut input = InputDecoder::new();
        let mut touch = TouchDecoder::new();
        // 11-byte touchpad payload: marker + seq + finger_mask(slot1) + 7 finger bytes.
        // X=128 (zone 0), Y byte 0xC0 (signed wrap path: 0xC0-188 = 4).
        let mut payload = vec![0x32, 0x00, 0x00, 0x01, 0x80, 0x00, 0xC0, 0x00, 0x00, 0x10, 0x05];
        // Sanity: 11 bytes.
        assert_eq!(payload.len(), 11);
        let ev = decode_hid_report(0xFC, payload.clone(), &mut input, &mut touch);
        match ev {
            DeviceEvent::Touch {
                report_id,
                event,
                raw,
            } => {
                assert_eq!(report_id, 0xFC);
                assert_eq!(raw, payload);
                assert_eq!(event.finger_count(), 1);
                let f = event.points[0].expect("slot 1 finger present");
                assert_eq!(f.x, 128);
                assert_eq!(f.pressure, 0x10);
                assert_eq!(f.status, 0x05);
            }
            _ => panic!("expected Touch"),
        }
        // Drain the borrow on payload so the assertion above can move it.
        payload.clear();
    }

    #[test]
    fn power_byte_classified() {
        let ev = decode_btleplug(ValueNotification {
            uuid: BATTERY_POWER_UUID,
            value: vec![0xAB],
        })
        .expect("event");
        match ev {
            DeviceEvent::Power { state, raw } => {
                assert_eq!(state, PowerState::Charging);
                assert_eq!(raw, vec![0xAB]);
            }
            _ => panic!("expected Power"),
        }
        let ev = decode_btleplug(ValueNotification {
            uuid: BATTERY_POWER_UUID,
            value: vec![0xFE],
        })
        .expect("event");
        match ev {
            DeviceEvent::Power { state, .. } => {
                assert_eq!(state, PowerState::Unknown(0xFE));
            }
            _ => panic!("expected Power"),
        }
    }

    #[test]
    fn battery_value_extracted() {
        let ev = decode_btleplug(ValueNotification {
            uuid: BATTERY_LEVEL_UUID,
            value: vec![57],
        })
        .expect("event");
        match ev {
            DeviceEvent::Battery { value, raw } => {
                assert_eq!(value, 57);
                assert_eq!(raw, vec![57]);
            }
            _ => panic!("expected Battery"),
        }
    }

    #[test]
    fn hid_report_uuid_passthrough_dropped() {
        let dropped = decode_btleplug(ValueNotification {
            uuid: HID_REPORT_UUID,
            value: vec![0x01, 0x00],
        });
        assert!(dropped.is_none());
    }
}
