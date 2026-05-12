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
/// clicks + Play/Pause). Empirically mapped against the physical remote —
/// Apple does not publish the report layout. Bits not listed have not been
/// observed during testing and decode to nothing.
///
/// The gen-1 / gen-2 remotes use a different (single-byte) layout; that
/// firmware variant is out of scope for this decoder.
pub const BUTTON_NAMES: &[(u16, &str)] = &[
    (0x0001, "TV"),
    (0x0002, "Volume Up"),
    (0x0004, "Volume Down"),
    (0x0008, "Select"),
    (0x0010, "Power"),
    (0x0020, "Siri"),
    (0x0040, "Back"),
    (0x0080, "Mute"),
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

// -- Touchpad input decoding --------------------------------------------------

/// Marker byte that prefixes every touchpad HID Input report on report id
/// 0xFC. Empirically constant; matches the gen-1 / gen-2 `TOUCH_EVENT`
/// sentinel from the SiriRemote-Linux Python reference (`0x32 == 50`).
pub const TOUCH_MARKER: u8 = 0x32;

/// Wire length of a single-finger touchpad report on the gen-3 Siri Remote
/// (model DNDJ22MG2330). The report uses report id 0xFC; payload is fixed
/// at 11 bytes whether a finger is in contact or the surface is idle.
pub const TOUCH_REPORT_LEN_1F: usize = 11;

/// Wire length of a two-finger touchpad report. The first 11 bytes are
/// identical to the single-finger layout (slot 1); the trailing 7 bytes
/// repeat the slot-1 trailer (`motion` + `x` + `y` + `pressure` + `status`)
/// for slot 2.
pub const TOUCH_REPORT_LEN_2F: usize = 18;

/// Bit in byte 3 set when slot 1 (primary finger) is in contact.
pub const FINGER_SLOT_1_MASK: u8 = 0x01;
/// Bit in byte 3 set when slot 2 (secondary finger) is in contact.
/// Empirically only ever observed together with the slot-1 bit.
pub const FINGER_SLOT_2_MASK: u8 = 0x10;

/// Y-axis offset baked into the firmware's wire encoding.
///
/// Per the SiriRemote-Linux reverse-engineering notes, byte [2] of a
/// finger payload sweeps `188..=255` then `0..=38` going bottom-to-top
/// across the touchpad. We decode that as a signed value relative to
/// `188` so 0 sits at the bottom edge and ~106 at the top edge — the
/// Python reference uses the equivalent
/// `(b if b & 0x80 else b + 255) - 188` formulation.
pub const TOUCH_Y_OFFSET: i16 = 188;

/// Number of horizontal "zones" the firmware splits the touchpad into.
/// The lower 3 bits of byte [1] of a finger payload select the zone
/// (0..=7); byte [0] is the position within the zone (0..=255). X is
/// reconstructed as `byte[0] + 255 * (byte[1] & 7)`, giving an 11-bit
/// value in `0..=2040`.
pub const TOUCH_X_ZONES: u8 = 8;
/// Mask that picks the zone bits out of finger byte [1].
pub const TOUCH_X_ZONE_MASK: u8 = 0x07;

/// Maximum value the encoded X can take (`255 * (TOUCH_X_ZONES - 1) + 255`).
/// Higher physical motion wraps back to small X values.
pub const TOUCH_X_MAX: u16 = 255 * (TOUCH_X_ZONES as u16 - 1) + 255;

/// One decoded touchpad sample. Up to two simultaneous fingers are
/// reported — slot indices match the firmware's wire layout (slot 1 at
/// bytes 4..10, slot 2 at bytes 11..17). Released frames carry the
/// 11-byte payload with `finger_mask == 0` and no point data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TouchEvent {
    /// Raw byte 3. Bit 0 set ⇒ slot 1 active; bit 4 set ⇒ slot 2 active.
    /// Surfaced so callers can distinguish "released and decaying" frames
    /// from "idle" frames without re-deriving from `points`.
    pub finger_mask: u8,
    /// Little-endian 16-bit packet counter from bytes 1..2. Increments
    /// by `0x1E` per frame at the ~15 ms native rate, wraps at 0xFFFF.
    pub seq: u16,
    /// Per-slot finger data. `points[0]` is slot 1, `points[1]` is slot 2.
    /// `None` means that slot is not in contact in this frame.
    pub points: [Option<FingerData>; 2],
}

/// One finger's slice of a touchpad report.
///
/// The firmware ships seven bytes per active slot. The first three
/// pack the high-precision finger position (X across two bytes plus
/// zone bits, then Y as a signed wrap byte); the next two are an
/// undocumented payload that co-varies with pressure / contact area;
/// then pressure and a status byte. Layout matches the gen-1/gen-2
/// SiriRemote-Linux reference, confirmed empirically on gen-3.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FingerData {
    /// 11-bit horizontal position in `0..=TOUCH_X_MAX`. Increases
    /// left-to-right and wraps from `TOUCH_X_MAX` back to `0` at the
    /// zone boundary, so callers tracking continuous motion need to
    /// unwrap across consecutive samples.
    pub x: u16,
    /// Vertical position relative to the firmware's `188` offset.
    /// Increases bottom-to-top; sits roughly in `0..=106` across the
    /// touchpad's active area, with `<0` and `>106` reachable at the
    /// extreme corners.
    pub y: i16,
    /// Pressure byte. `0` immediately before release, peaks around
    /// `0x14..0x1c` for a light touch and `0x25+` for a hard click.
    pub pressure: u8,
    /// Status / quadrant byte. Bits empirically mix touch state with
    /// what looks like quadrant hints; not fully decoded, surfaced raw.
    pub status: u8,
    /// Two undocumented bytes (finger payload indices 3 and 4) that we
    /// previously misread as `(x, y)`. They scale with pressure and
    /// contact-ellipse size and do **not** carry position information.
    /// Surfaced raw for diagnostics until their role is reverse-engineered.
    pub aux: [u8; 2],
    /// Upper five bits of finger byte [1] (the lower three feed the X
    /// zone). Empirically a slow counter; surfaced for diagnostics.
    pub byte1_high: u8,
}

impl TouchEvent {
    /// Try to parse one touchpad HID Input report. Returns `None` when
    /// the payload is not a well-formed 0xFC report (wrong length or
    /// missing the 0x32 marker); the caller should fall back to a
    /// raw-hex dump in that case.
    pub fn parse(payload: &[u8]) -> Option<Self> {
        let len = payload.len();
        if (len != TOUCH_REPORT_LEN_1F && len != TOUCH_REPORT_LEN_2F)
            || payload[0] != TOUCH_MARKER
        {
            return None;
        }
        let seq = u16::from_le_bytes([payload[1], payload[2]]);
        let finger_mask = payload[3];
        let slot1 = if finger_mask & FINGER_SLOT_1_MASK != 0 {
            Some(FingerData::decode([
                payload[4],
                payload[5],
                payload[6],
                payload[7],
                payload[8],
                payload[9],
                payload[10],
            ]))
        } else {
            None
        };
        // Slot 2 data is only present in the 18-byte layout. Treat a
        // slot-2 mask bit on a short payload as a wire-format error
        // (no observed case, but reject rather than read OOB).
        let slot2 = if finger_mask & FINGER_SLOT_2_MASK != 0 {
            if len < TOUCH_REPORT_LEN_2F {
                return None;
            }
            Some(FingerData::decode([
                payload[11],
                payload[12],
                payload[13],
                payload[14],
                payload[15],
                payload[16],
                payload[17],
            ]))
        } else {
            None
        };
        Some(Self {
            finger_mask,
            seq,
            points: [slot1, slot2],
        })
    }

    /// Number of fingers currently in contact (0, 1, or 2).
    pub fn finger_count(&self) -> u8 {
        self.points.iter().filter(|p| p.is_some()).count() as u8
    }

    /// Render a single line summary. Released frames lead with
    /// `released`; touched frames lead with `fingers=N` and emit one
    /// `[1: …]` / `[2: …]` block per active slot.
    pub fn format(&self) -> String {
        let fingers = self.finger_count();
        if fingers == 0 {
            return format!(
                "touch released mask=0x{:02x} seq=0x{:04x}",
                self.finger_mask, self.seq,
            );
        }
        let mut out = format!("touch fingers={fingers}");
        for (idx, slot) in self.points.iter().enumerate() {
            if let Some(f) = slot {
                let slot_no = idx + 1;
                let _ = write!(
                    out,
                    " [{slot_no}: x={} y={} pressure={} status=0x{:02x} \
                     aux={:02x}{:02x} byte1_high=0x{:02x}]",
                    f.x, f.y, f.pressure, f.status, f.aux[0], f.aux[1], f.byte1_high,
                );
            }
        }
        let _ = write!(out, " mask=0x{:02x} seq=0x{:04x}", self.finger_mask, self.seq);
        out
    }
}

impl FingerData {
    /// Decode the 7-byte per-finger payload using the layout documented
    /// in `SiriRemote-Linux/README.md` (gen-2 wire format, confirmed
    /// empirically on the gen-3 DNDJ22MG2330):
    ///
    /// | byte | role |
    /// |------|------|
    /// | 0    | X low byte (within zone) |
    /// | 1    | bits 0..2: X zone (`& 7`); bits 3..7: unknown counter |
    /// | 2    | Y signed wrap byte, offset by `188` |
    /// | 3    | unknown (co-varies with pressure / contact size) |
    /// | 4    | unknown (co-varies with pressure / contact size) |
    /// | 5    | pressure |
    /// | 6    | status / quadrant flags |
    fn decode(b: [u8; 7]) -> Self {
        let zone = (b[1] & TOUCH_X_ZONE_MASK) as u16;
        debug_assert!(zone < TOUCH_X_ZONES as u16);
        let x = b[0] as u16 + 255 * zone;
        debug_assert!(x <= TOUCH_X_MAX);
        // Y mirrors the SiriRemote-Linux Python decoder:
        //   (b if b & 0x80 else b + 255) - 188
        // which is "treat byte as signed offset around 188" with the
        // 0..127 range shifted up by 255 so the wrap matches the
        // touchpad's bottom→top sweep without a discontinuity inside
        // the active area.
        let y = if b[2] & 0x80 != 0 {
            b[2] as i16 - TOUCH_Y_OFFSET
        } else {
            b[2] as i16 + 255 - TOUCH_Y_OFFSET
        };
        Self {
            x,
            y,
            pressure: b[5],
            status: b[6],
            aux: [b[3], b[4]],
            byte1_high: b[1] >> 3,
        }
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
        assert_eq!(button_list(0x0001), "TV");
        assert_eq!(button_list(0x0008), "Select");
        assert_eq!(button_list(0x0080), "Mute");
        // Byte-1 bits (clickpad directional + Play/Pause).
        assert_eq!(button_list(0x0100), "Play/Pause");
        assert_eq!(button_list(0x1000), "Right");
    }

    #[test]
    fn button_list_combination_in_table_order() {
        // Volume Up (0x0002) + Select (0x0008) — must appear in declaration order.
        assert_eq!(button_list(0x000A), "Volume Up+Select");
        // TV (0x0001) + Play/Pause (0x0100) — byte-0 entry first.
        assert_eq!(button_list(0x0101), "TV+Play/Pause");
        // Up (0x0200) + Down (0x0400) — both byte-1, in table order.
        assert_eq!(button_list(0x0600), "Up+Down");
    }

    #[test]
    fn button_list_ignores_unmapped_bits() {
        // Byte 1 bits 0x2000..0x8000 are unobserved and must not produce
        // phantom names.
        assert_eq!(button_list(0xE000), "none");
        // Mapped + unmapped: only the mapped bit renders.
        assert_eq!(button_list(0x2001), "TV");
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
        // Up (byte1 bit 0x02 -> mask 0x0200) + TV (byte0 bit 0x01 -> 0x0001).
        let line = d.format(&[0x01, 0x02]);
        assert!(
            line.contains("buttons=TV+Up pressed=TV+Up released=none"),
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

    #[test]
    fn touch_parse_rejects_wrong_length_and_marker() {
        assert!(TouchEvent::parse(&[]).is_none());
        // Right length, wrong marker.
        assert!(TouchEvent::parse(&[0x00; 11]).is_none());
        // Right marker, wrong length.
        assert!(TouchEvent::parse(&[0x32, 0x00, 0x00]).is_none());
    }

    #[test]
    fn touch_parse_decodes_active_touch() {
        // Captured from a real DNDJ22MG2330 mid-swipe:
        // 32 14 a6 01 c0 1e e6 9e 8e 1a a4
        let bytes = [
            0x32, 0x14, 0xa6, 0x01, 0xc0, 0x1e, 0xe6, 0x9e, 0x8e, 0x1a, 0xa4,
        ];
        let ev = TouchEvent::parse(&bytes).expect("valid touch packet");
        assert_eq!(ev.finger_mask, 0x01);
        assert_eq!(ev.finger_count(), 1);
        assert_eq!(ev.seq, 0xa614);
        let f = ev.points[0].expect("slot 1 present while touching");
        assert_eq!(f.x, 192 + 255 * 6, "x = byte0 + 255 * (byte1 & 7)");
        assert_eq!(f.y, 0xe6 - 188, "y = byte2 - 188 because bit 7 is set");
        assert_eq!(f.pressure, 0x1a);
        assert_eq!(f.status, 0xa4);
        assert_eq!(f.aux, [0x9e, 0x8e]);
        assert_eq!(f.byte1_high, 0x1e >> 3);
        assert!(ev.points[1].is_none(), "slot 2 must be empty in 11-byte payload");
    }

    #[test]
    fn touch_parse_decodes_release_with_zeroed_point() {
        // Trailing release frame from the same swipe:
        // 32 e6 a6 00 c3 ee e6 00 00 00 87
        let bytes = [
            0x32, 0xe6, 0xa6, 0x00, 0xc3, 0xee, 0xe6, 0x00, 0x00, 0x00, 0x87,
        ];
        let ev = TouchEvent::parse(&bytes).expect("valid release packet");
        assert_eq!(ev.finger_mask, 0x00);
        assert_eq!(ev.finger_count(), 0);
        assert!(ev.points.iter().all(|p| p.is_none()));
        assert_eq!(ev.seq, 0xa6e6);
    }

    #[test]
    fn touch_parse_decodes_two_finger_report() {
        // Captured from a real DNDJ22MG2330 with two fingers on the
        // touchpad: 18 bytes, mask byte 0x11 (slot 1 + slot 2).
        // 32 a0 89 11 77 60 f8 47 5a 0f 8b 41 0d e2 41 55 0e 83
        let bytes = [
            0x32, 0xa0, 0x89, 0x11, 0x77, 0x60, 0xf8, 0x47, 0x5a, 0x0f, 0x8b, 0x41, 0x0d, 0xe2,
            0x41, 0x55, 0x0e, 0x83,
        ];
        let ev = TouchEvent::parse(&bytes).expect("valid two-finger packet");
        assert_eq!(ev.finger_mask, 0x11);
        assert_eq!(ev.finger_count(), 2);
        assert_eq!(ev.seq, 0x89a0);
        let f1 = ev.points[0].expect("slot 1 active");
        assert_eq!(f1.x, 0x77 + 255 * 0, "slot1 zone is 0");
        assert_eq!(f1.y, 0xf8 - 188);
        assert_eq!(f1.pressure, 0x0f);
        assert_eq!(f1.status, 0x8b);
        assert_eq!(f1.aux, [0x47, 0x5a]);
        let f2 = ev.points[1].expect("slot 2 active");
        assert_eq!(f2.x, 0x41 + 255 * 5, "slot2 zone is 5");
        assert_eq!(f2.y, 0xe2 - 188);
        assert_eq!(f2.pressure, 0x0e);
        assert_eq!(f2.status, 0x83);
        assert_eq!(f2.aux, [0x41, 0x55]);
    }

    #[test]
    fn touch_parse_rejects_slot2_bit_on_short_payload() {
        // Mask claims slot 2 is active but payload only has 11 bytes —
        // refuse to read past the buffer.
        let bytes = [
            0x32, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        assert!(TouchEvent::parse(&bytes).is_none());
    }

    #[test]
    fn touch_format_active_includes_position_and_pressure() {
        let ev = TouchEvent::parse(&[
            0x32, 0x14, 0xa6, 0x01, 0xc0, 0x1e, 0xe6, 0x9e, 0x8e, 0x1a, 0xa4,
        ])
        .unwrap();
        let line = ev.format();
        assert!(
            line.contains("touch fingers=1"),
            "missing fingers tag: {line}",
        );
        // x = 0xc0 + 255 * (0x1e & 7) = 192 + 1530 = 1722
        // y = 0xe6 - 188 = 42 (bit 7 set)
        assert!(
            line.contains("[1: x=1722 y=42 pressure=26"),
            "missing decoded slot-1 position: {line}",
        );
        assert!(line.contains("status=0xa4"), "missing status: {line}");
        assert!(line.contains("seq=0xa614"), "missing seq: {line}");
        assert!(line.contains("aux=9e8e"), "missing aux bytes: {line}");
        assert!(line.contains("byte1_high=0x03"), "missing byte1_high: {line}");
    }

    #[test]
    fn touch_format_two_fingers_emits_both_slots() {
        let ev = TouchEvent::parse(&[
            0x32, 0xa0, 0x89, 0x11, 0x77, 0x60, 0xf8, 0x47, 0x5a, 0x0f, 0x8b, 0x41, 0x0d, 0xe2,
            0x41, 0x55, 0x0e, 0x83,
        ])
        .unwrap();
        let line = ev.format();
        assert!(line.contains("touch fingers=2"), "{line}");
        // slot 1: x = 0x77 + 255*0 = 119, y = 0xf8 - 188 = 60
        assert!(line.contains("[1: x=119 y=60 pressure=15"), "{line}");
        // slot 2: x = 0x41 + 255*5 = 1340, y = 0xe2 - 188 = 38
        assert!(line.contains("[2: x=1340 y=38 pressure=14"), "{line}");
        assert!(line.contains("mask=0x11"), "{line}");
    }

    #[test]
    fn touch_format_release_uses_released_label() {
        let ev = TouchEvent::parse(&[
            0x32, 0xe6, 0xa6, 0x00, 0xc3, 0xee, 0xe6, 0x00, 0x00, 0x00, 0x87,
        ])
        .unwrap();
        let line = ev.format();
        assert!(line.starts_with("touch released"), "expected released prefix: {line}");
        assert!(!line.contains(" x="), "released frame must not advertise x/y: {line}");
        assert!(line.contains("mask=0x00"));
    }
}
