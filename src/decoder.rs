//! Pure parsing logic for Siri Remote events.
//!
//! Ports of the corresponding helpers in `events.py`: button mask decoding,
//! 7-byte finger payload geometry, battery / power formatting, and the input
//! decoder that tracks press/release transitions across HID input reports.

use std::fmt::Write;

use chrono::Utc;

// -- Shared constants ---------------------------------------------------------

/// Apple Bluetooth SIG company identifier. Used to recognise Apple manufacturer
/// data in BLE advertisements and pairing payloads.
pub const APPLE_COMPANY_ID: u16 = 0x004C;

/// Two-byte prefix that flags an Apple advertisement as carrying HID-over-GATT
/// pairing data. The next bytes follow `02 15 03 02 <6-byte identity address>`.
pub const APPLE_HID_MFR_PREFIX: [u8; 2] = [0x07, 0x0D];

/// 16-bit button mask → display name for the gen-3 Siri Remote (model
/// DNDJ22MG2330). Bits 0..7 come from byte 0 of the 2-byte HID Input report
/// 0xFB (system buttons), bits 8..15 from byte 1 (clickpad directional
/// clicks + Play/Pause). Empirically mapped — Apple does not publish the
/// report layout. Bits not listed have not been observed during testing
/// and decode to nothing.
///
/// The gen-1 / gen-2 remotes use a different (single-byte) layout; that
/// firmware variant is out of scope for this decoder.
pub const BUTTON_NAMES: &[(u16, &str)] = &[
    (0x0001, "Select"),
    (0x0002, "Volume Up"),
    (0x0004, "Volume Down"),
    (0x0008, "Mute"),
    (0x0010, "Power"),
    (0x0020, "Siri"),
    (0x0040, "Back"),
    (0x0080, "TV"),
    (0x0100, "Play/Pause"),
    (0x0200, "Up"),
    (0x0400, "Down"),
    (0x0800, "Left"),
    (0x1000, "Right"),
];

/// Map raw power-state byte to a human label, matching the Python `POWER_STATES` dict.
pub fn power_state(value: u8) -> Option<&'static str> {
    match value {
        0xAB => Some("charging"),
        0xAF => Some("discharging"),
        0xBB => Some("plugged-in"),
        _ => None,
    }
}

// -- Formatting helpers -------------------------------------------------------

/// Millisecond-precision UTC stamp matching Python's
/// `datetime.now(UTC).isoformat(timespec='milliseconds').replace('+00:00','Z')`.
pub fn now_stamp() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

/// Render bytes as space-separated lowercase hex, matching `bytes.hex(" ")`.
pub fn raw_hex(data: &[u8]) -> String {
    let mut s = String::with_capacity(data.len() * 3);
    for (i, b) in data.iter().enumerate() {
        if i > 0 {
            s.push(' ');
        }
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Join the set bits in `mask` using the names from `BUTTON_NAMES`,
/// returning the literal `"none"` for an empty mask.
pub fn button_list(mask: u16) -> String {
    let mut out = String::new();
    for (bit, name) in BUTTON_NAMES {
        if mask & bit != 0 {
            if !out.is_empty() {
                out.push('+');
            }
            out.push_str(name);
        }
    }
    if out.is_empty() {
        "none".to_string()
    } else {
        out
    }
}

pub fn format_battery(data: &[u8]) -> String {
    if data.is_empty() {
        return "empty battery packet".to_string();
    }
    format!("battery={}%", data[0])
}

pub fn format_power(data: &[u8]) -> String {
    if data.is_empty() {
        return "empty power packet".to_string();
    }
    let value = data[0];
    match power_state(value) {
        Some(state) => format!("power={state}"),
        None => format!("power=unknown(0x{value:02x})"),
    }
}

// -- Button input decoding ----------------------------------------------------

/// Stateful decoder for the gen-3 Siri Remote's 2-byte HID Input report
/// (report id 0xFB). Tracks the previous button mask so every emitted line
/// can spell out which buttons just transitioned.
///
/// Multi-byte HID Input reports on this firmware (touch stream 0xFC,
/// audio 0xFA, etc.) have not been reverse-engineered; callers should
/// dump them raw rather than feeding them through this decoder.
pub struct InputDecoder {
    last_button: u16,
}

impl Default for InputDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl InputDecoder {
    pub fn new() -> Self {
        Self { last_button: 0 }
    }

    /// Read both bytes of a button report as a little-endian 16-bit mask.
    /// Returns `None` for any payload that is not exactly 2 bytes.
    fn mask(data: &[u8]) -> Option<u16> {
        if data.len() != 2 {
            return None;
        }
        Some(u16::from_le_bytes([data[0], data[1]]))
    }

    /// Render a 2-byte HID button payload as a `buttons=…; pressed=…;
    /// released=…` line. Returns the `unknown HID packet len=N` fallback
    /// for any other shape, and the same fallback for repeated identical
    /// states (i.e. a state-refresh packet that doesn't actually
    /// transition any button).
    pub fn format(&mut self, payload: &[u8]) -> String {
        let Some(button) = Self::mask(payload) else {
            return format!("unknown HID packet len={}", payload.len());
        };
        if button == self.last_button {
            return format!("unknown HID packet len={}", payload.len());
        }
        let pressed = button & !self.last_button;
        let released = self.last_button & !button;
        self.last_button = button;
        format!(
            "buttons={} pressed={} released={}",
            button_list(button),
            button_list(pressed),
            button_list(released),
        )
    }
}

/// Compose a full event line — timestamp, source label, identifier slot, raw
/// hex dump, and decoded description.
///
/// `identifier` is rendered as-is into the line; callers supply something
/// like `"uuid=0000xxxx-…"`. (Python used `handle=0xHHHH`; btleplug does not
/// expose attribute handles, so the plan substitutes the UUID for the same
/// diagnostic role.)
pub fn format_event(
    source: &str,
    identifier: &str,
    payload: &[u8],
    decoder: Option<&mut InputDecoder>,
) -> String {
    let decoded = match source {
        "battery" => format_battery(payload),
        "power" => format_power(payload),
        "input" => match decoder {
            Some(d) => d.format(payload),
            None => InputDecoder::new().format(payload),
        },
        _ => format!("len={}", payload.len()),
    };
    format!(
        "{} {} {} raw={} | {}",
        now_stamp(),
        source,
        identifier,
        raw_hex(payload),
        decoded,
    )
}

// -- Tests --------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn button_list_empty_renders_none() {
        assert_eq!(button_list(0), "none");
    }

    #[test]
    fn button_list_single_bit() {
        // Byte-0 bits (system buttons).
        assert_eq!(button_list(0x0001), "Select");
        assert_eq!(button_list(0x0080), "TV");
        // Byte-1 bits (clickpad directional + Play/Pause).
        assert_eq!(button_list(0x0100), "Play/Pause");
        assert_eq!(button_list(0x1000), "Right");
    }

    #[test]
    fn button_list_combination_in_table_order() {
        // Volume Up (0x0002) + Mute (0x0008) — must appear in declaration order.
        assert_eq!(button_list(0x000A), "Volume Up+Mute");
        // Select (0x0001) + Play/Pause (0x0100) — byte-0 entry first.
        assert_eq!(button_list(0x0101), "Select+Play/Pause");
        // Up (0x0200) + Down (0x0400) — both byte-1, in table order.
        assert_eq!(button_list(0x0600), "Up+Down");
    }

    #[test]
    fn button_list_ignores_unmapped_bits() {
        // Byte 1 bits 0x2000..0x8000 are unobserved and must not produce
        // phantom names.
        assert_eq!(button_list(0xE000), "none");
        // Mapped + unmapped: only the mapped bit renders.
        assert_eq!(button_list(0x2001), "Select");
    }

    #[test]
    fn format_battery_empty_and_known() {
        assert_eq!(format_battery(&[]), "empty battery packet");
        assert_eq!(format_battery(&[42]), "battery=42%");
        assert_eq!(format_battery(&[100, 0xff]), "battery=100%");
    }

    #[test]
    fn format_power_known_and_unknown_and_empty() {
        assert_eq!(format_power(&[]), "empty power packet");
        assert_eq!(format_power(&[0xAB]), "power=charging");
        assert_eq!(format_power(&[0xAF]), "power=discharging");
        assert_eq!(format_power(&[0xBB]), "power=plugged-in");
        assert_eq!(format_power(&[0x12]), "power=unknown(0x12)");
    }

    #[test]
    fn input_decoder_emits_press_then_release_diffs() {
        let mut d = InputDecoder::new();

        // Initial state 0 -> Back pressed (byte0 bit 0x40, mask 0x0040).
        let line = d.format(&[0x40, 0x00]);
        assert!(
            line.contains("buttons=Back pressed=Back released=none"),
            "unexpected first transition line: {line}",
        );

        // Same state repeated -> "unknown HID packet" fallback (no change).
        let line = d.format(&[0x40, 0x00]);
        assert!(
            line.starts_with("unknown HID packet len="),
            "no-change frame should yield the unknown fallback, got: {line}",
        );

        // Back released -> mask 0; pressed=none, released=Back.
        let line = d.format(&[0x00, 0x00]);
        assert!(
            line.contains("buttons=none pressed=none released=Back"),
            "unexpected release line: {line}",
        );
    }

    #[test]
    fn input_decoder_combines_byte0_and_byte1_into_u16_mask() {
        let mut d = InputDecoder::new();
        // Up (byte1 bit 0x02 -> mask 0x0200) + Select (byte0 bit 0x01 -> 0x0001).
        let line = d.format(&[0x01, 0x02]);
        assert!(
            line.contains("buttons=Select+Up pressed=Select+Up released=none"),
            "expected combined byte-0/byte-1 decode, got: {line}",
        );
    }

    #[test]
    fn input_decoder_ignores_non_two_byte_payloads() {
        let mut d = InputDecoder::new();
        // The touch (0xFC) / audio (0xFA) reports must NOT be decoded as buttons.
        let line = d.format(&[0x32, 0xFA, 0x99]);
        assert!(
            line.starts_with("unknown HID packet len="),
            "non-button payload must fall through, got: {line}",
        );
        let line = d.format(&[]);
        assert!(line.starts_with("unknown HID packet len="));
    }

    #[test]
    fn format_event_battery_layout() {
        let line = format_event("battery", "uuid=abc", &[55], None);
        assert!(line.contains(" battery uuid=abc raw=37 | battery=55%"));
    }

    #[test]
    fn format_event_unknown_source_falls_back_to_len() {
        let line = format_event("other", "uuid=zzz", &[1, 2, 3], None);
        assert!(line.contains(" other uuid=zzz raw=01 02 03 | len=3"));
    }

    #[test]
    fn raw_hex_matches_python_spacing() {
        assert_eq!(raw_hex(&[]), "");
        assert_eq!(raw_hex(&[0x01]), "01");
        assert_eq!(raw_hex(&[0x01, 0xab, 0x00]), "01 ab 00");
    }
}
