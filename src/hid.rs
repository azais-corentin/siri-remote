//! HID Report characteristic enumeration, Report Reference descriptor reads,
//! and the input-enable byte writer that the Siri Remote needs before it
//! starts emitting HID notifications.

use anyhow::{Result, bail};
use btleplug::api::{CharPropFlags, Characteristic, Peripheral as _, WriteType};
use btleplug::platform::Peripheral;
use uuid::Uuid;

use crate::decoder::raw_hex;

pub const HID_REPORT_UUID: Uuid = Uuid::from_u128(0x0000_2a4d_0000_1000_8000_0080_5f9b_34fb);
pub const BATTERY_LEVEL_UUID: Uuid = Uuid::from_u128(0x0000_2a19_0000_1000_8000_0080_5f9b_34fb);
pub const BATTERY_POWER_UUID: Uuid = Uuid::from_u128(0x0000_2a1a_0000_1000_8000_0080_5f9b_34fb);
pub const REPORT_REFERENCE_UUID: Uuid = Uuid::from_u128(0x0000_2908_0000_1000_8000_0080_5f9b_34fb);

/// Byte the Siri Remote expects on any HID report characteristic before it
/// will start streaming input notifications. The same constant in Python is
/// `b"\xAF"` in `events.enable_input`.
pub const INPUT_ENABLE_BYTE: u8 = 0xAF;

pub fn all_characteristics(p: &Peripheral) -> Vec<Characteristic> {
    let mut out = Vec::new();
    for s in p.services() {
        for c in s.characteristics {
            out.push(c);
        }
    }
    out
}

pub fn find_by_uuid(p: &Peripheral, uuid: Uuid) -> Option<Characteristic> {
    all_characteristics(p).into_iter().find(|c| c.uuid == uuid)
}

pub fn hid_reports(p: &Peripheral) -> Vec<Characteristic> {
    all_characteristics(p)
        .into_iter()
        .filter(|c| c.uuid == HID_REPORT_UUID)
        .collect()
}

/// Read a characteristic's Report Reference descriptor (UUID `0x2908`), if present.
pub async fn read_report_reference(p: &Peripheral, char: &Characteristic) -> Option<Vec<u8>> {
    let desc = char
        .descriptors
        .iter()
        .find(|d| d.uuid == REPORT_REFERENCE_UUID)?;
    p.read_descriptor(desc).await.ok()
}

/// Send the input-enable byte to one of the HID Report characteristics.
///
/// Ranking matches `events.enable_input`:
///   0 — Report Reference says this is an Output report (`report_type == 2`)
///   2 — characteristic is not notifiable (some Output reports lack NOTIFY)
///   3 — everything else
///
/// (Rank 1 in Python was "handle == 0x001D". btleplug does not expose
/// attribute handles, so we drop that tiebreaker; on the verified 3rd-gen
/// remote, the handle-0x001D report carries Report Reference `02 02` and
/// therefore still wins via rank 0.)
pub async fn enable_input(p: &Peripheral) -> Result<()> {
    let reports = hid_reports(p);
    let mut candidates: Vec<(u8, Characteristic, Option<Vec<u8>>)> = Vec::new();
    for char in reports {
        let writeable = char.properties.contains(CharPropFlags::WRITE)
            || char
                .properties
                .contains(CharPropFlags::WRITE_WITHOUT_RESPONSE);
        if !writeable {
            continue;
        }
        let ref_bytes = read_report_reference(p, &char).await;
        let report_type = ref_bytes.as_ref().and_then(|r| r.get(1).copied());
        let rank = if report_type == Some(2) {
            0
        } else if !char.properties.contains(CharPropFlags::NOTIFY) {
            2
        } else {
            3
        };
        candidates.push((rank, char, ref_bytes));
    }

    if candidates.is_empty() {
        bail!("no writable HID report characteristic found");
    }

    candidates.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.uuid.cmp(&b.1.uuid)));

    for (_, char, ref_bytes) in candidates {
        let ref_text = match ref_bytes {
            Some(ref b) => raw_hex(b),
            None => "unknown".to_string(),
        };
        eprintln!(
            "Sending input-enable byte to report uuid={} report_ref={}.",
            char.uuid, ref_text
        );
        if write_enable_candidate(p, &char).await {
            return Ok(());
        }
    }
    bail!("could not send Siri Remote input-enable byte to any HID report");
}

async fn write_enable_candidate(p: &Peripheral, char: &Characteristic) -> bool {
    if char
        .properties
        .contains(CharPropFlags::WRITE_WITHOUT_RESPONSE)
    {
        match p
            .write(char, &[INPUT_ENABLE_BYTE], WriteType::WithoutResponse)
            .await
        {
            Ok(_) => return true,
            Err(e) => eprintln!(
                "input enable write-without-response failed on {}: {e:?}",
                char.uuid
            ),
        }
    }
    if char.properties.contains(CharPropFlags::WRITE) {
        match p
            .write(char, &[INPUT_ENABLE_BYTE], WriteType::WithResponse)
            .await
        {
            Ok(_) => return true,
            Err(e) => eprintln!(
                "input enable write-with-response failed on {}: {e:?}",
                char.uuid
            ),
        }
    }
    false
}
