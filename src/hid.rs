//! Btleplug-side GATT helpers used by the `events` command.
//!
//! HID Report (`0x2A4D`) writes and per-instance notifications are NOT done
//! through btleplug — see `bluez::hid` for that. btleplug collapses
//! same-UUID characteristics, so on the Siri Remote (8 instances of `0x2A4D`)
//! it cannot address individual Input/Output reports. This module is left
//! with only what btleplug *can* do correctly: discovery, the Battery /
//! Power Status characteristics (one per UUID), and a generic
//! find-by-UUID helper used by the device-info dump.

use btleplug::api::{Characteristic, Peripheral as _};
use btleplug::platform::Peripheral;
use uuid::Uuid;

pub const HID_REPORT_UUID: Uuid = Uuid::from_u128(0x0000_2a4d_0000_1000_8000_0080_5f9b_34fb);
pub const BATTERY_LEVEL_UUID: Uuid = Uuid::from_u128(0x0000_2a19_0000_1000_8000_0080_5f9b_34fb);
pub const BATTERY_POWER_UUID: Uuid = Uuid::from_u128(0x0000_2a1a_0000_1000_8000_0080_5f9b_34fb);

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
