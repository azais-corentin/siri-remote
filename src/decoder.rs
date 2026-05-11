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

/// Single-button bit mask -> display name, in the order Python emits them.
pub const BUTTON_NAMES: &[(u8, &str)] = &[
    (0x01, "AirPlay"),
    (0x02, "Volume Up"),
    (0x04, "Volume Down"),
    (0x08, "Play/Pause"),
    (0x10, "Siri"),
    (0x20, "Menu"),
    (0x40, "Touchpad 2-Finger"),
    (0x80, "Touchpad"),
];

pub const BUTTON_TOUCHPAD: u8 = 0x80;
pub const BUTTON_TOUCHPAD_2: u8 = 0x40;

/// Byte that marks the third octet of a touch event payload.
pub const TOUCH_EVENT_MARKER: u8 = 0x32;

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
/// returning the literal `"none"` for an empty mask (Python parity).
pub fn button_list(mask: u8) -> String {
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

// -- Touch / input decoding ---------------------------------------------------

/// Decode a 7-byte finger payload into `(x, y, pressure)`.
///
/// The arithmetic is a literal port of the formula from `events.py`. Python
/// integer division of a float truncates toward zero; Rust's `as i32` cast
/// of an `f64` does the same when the value is in range.
pub fn decode_finger(data: &[u8]) -> anyhow::Result<(i32, i32, u8)> {
    if data.len() != 7 {
        anyhow::bail!("finger payload must be 7 bytes, got {}", data.len());
    }
    let raw = (data[0] as i32) + 255 * ((data[1] as i32) & 0x07) - 230;
    let x = (raw as f64 / 15.0) as i32;
    let y = (if data[2] & 0x80 != 0 {
        data[2] as i32
    } else {
        (data[2] as i32) + 255
    }) - 188;
    let pressure = data[5];
    Ok((x, y, pressure))
}

/// Stateful decoder for HID input reports. Tracks the previous button mask so
/// every emitted line can spell out which buttons just transitioned.
pub struct InputDecoder {
    last_button: u8,
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

    /// Apply the touchpad re-mapping from `events.py`: when the report type
    /// byte is `0x02`, the touchpad bit (`0x80`) actually means the
    /// two-finger touchpad gesture (`0x40`).
    fn normalized_button(data: &[u8]) -> Option<u8> {
        if data.len() < 2 {
            return None;
        }
        let mut button = data[1];
        if data[0] == 2 && button & BUTTON_TOUCHPAD != 0 {
            button = (button & !BUTTON_TOUCHPAD) | BUTTON_TOUCHPAD_2;
        }
        Some(button)
    }

    /// Render a HID input payload. Returns either a `buttons=…; touch=…`
    /// combined description or the `unknown HID packet len=N` fallback that
    /// `events.py` emits when there is nothing recognizable.
    pub fn format(&mut self, payload: &[u8]) -> String {
        let mut parts: Vec<String> = Vec::new();

        if let Some(button) = Self::normalized_button(payload)
            && button != self.last_button
        {
            let pressed = button & !self.last_button;
            let released = self.last_button & !button;
            parts.push(format!(
                "buttons={} pressed={} released={}",
                button_list(button),
                button_list(pressed),
                button_list(released),
            ));
            self.last_button = button;
        }

        if payload.len() >= 3
            && payload[2] == TOUCH_EVENT_MARKER
            && let Some(touch) = Self::format_touch(payload)
        {
            parts.push(touch);
        }

        if parts.is_empty() {
            format!("unknown HID packet len={}", payload.len())
        } else {
            parts.join("; ")
        }
    }

    fn format_touch(data: &[u8]) -> Option<String> {
        if data.len() != 13 && data.len() != 20 {
            return Some(format!("touch marker with unsupported len={}", data.len()));
        }

        let mut fingers: Vec<(i32, i32, u8)> = Vec::with_capacity(2);
        fingers.push(decode_finger(&data[6..13]).ok()?);
        if data.len() == 20 {
            fingers.push(decode_finger(&data[13..20]).ok()?);
        }

        // Python f-string formats `bool` as `True` / `False`.
        let pressed = if data[1] & BUTTON_TOUCHPAD != 0 {
            "True"
        } else {
            "False"
        };
        let expected_count = data[0];

        let mut rendered = String::new();
        for (i, (x, y, p)) in fingers.iter().enumerate() {
            if i > 0 {
                rendered.push_str(", ");
            }
            let _ = write!(rendered, "finger{}:x={x},y={y},pressure={p}", i + 1);
        }
        Some(format!(
            "touch pressed={pressed} count={expected_count} {rendered}"
        ))
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
        assert_eq!(button_list(0x01), "AirPlay");
        assert_eq!(button_list(0x80), "Touchpad");
    }

    #[test]
    fn button_list_combination_in_table_order() {
        // 0x82 = Volume Up | Touchpad — must appear in declaration order.
        assert_eq!(button_list(0x82), "Volume Up+Touchpad");
        // 0x03 = AirPlay | Volume Up.
        assert_eq!(button_list(0x03), "AirPlay+Volume Up");
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
    fn decode_finger_requires_seven_bytes() {
        assert!(decode_finger(&[]).is_err());
        assert!(decode_finger(&[0; 6]).is_err());
        assert!(decode_finger(&[0; 8]).is_err());
        assert!(decode_finger(&[0; 7]).is_ok());
    }

    #[test]
    fn decode_finger_matches_python_arithmetic() {
        // view[0]=100, view[1]&0x07=0: raw=-130, x=int(-130/15)=-8 (trunc to 0)
        // view[2]=0x80 (high bit set), y = 0x80 - 188 = -60
        // pressure = view[5] = 42
        let f = decode_finger(&[100, 0, 0x80, 0, 0, 42, 0]).unwrap();
        assert_eq!(f, (-8, -60, 42));

        // view[0]=200, view[1]&0x07=2 (=0xA2), raw=200+510-230=480, x=32.
        // view[2]=0 (high bit clear), y=(0+255)-188=67.
        let f = decode_finger(&[200, 0xA2, 0, 0, 0, 7, 0]).unwrap();
        assert_eq!(f, (32, 67, 7));
    }

    #[test]
    fn input_decoder_emits_press_then_release_diffs() {
        let mut d = InputDecoder::new();

        // Initial state 0 -> Menu pressed (mask 0x20).
        let line = d.format(&[0x01, 0x20]);
        assert!(
            line.contains("buttons=Menu pressed=Menu released=none"),
            "unexpected first transition line: {line}",
        );

        // Menu held -> nothing changed; format returns the "unknown" fallback.
        let line = d.format(&[0x01, 0x20]);
        assert!(
            line.starts_with("unknown HID packet len="),
            "no-change frame should yield the unknown fallback, got: {line}",
        );

        // Menu released -> mask 0; pressed=none, released=Menu.
        let line = d.format(&[0x01, 0x00]);
        assert!(
            line.contains("buttons=none pressed=none released=Menu"),
            "unexpected release line: {line}",
        );
    }

    #[test]
    fn input_decoder_remaps_touchpad_on_type_2() {
        let mut d = InputDecoder::new();
        // Report type 2 + Touchpad bit (0x80) maps to two-finger (0x40).
        let line = d.format(&[0x02, 0x80]);
        assert!(
            line.contains("buttons=Touchpad 2-Finger pressed=Touchpad 2-Finger"),
            "report type 2 remap failed: {line}",
        );
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
