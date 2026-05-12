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

    #[allow(dead_code)]
    /// Read both bytes of a button report as a little-endian 16-bit mask.
    /// Returns `None` for any payload that is not exactly 2 bytes.
    fn mask(data: &[u8]) -> Option<u16> {
        if data.len() != 2 {
            return None;
        }
        Some(u16::from_le_bytes([data[0], data[1]]))
    }

    /// Update the tracked button mask, returning the previous value. Used
    /// by [`crate::session::Session`] to compute press/release deltas
    /// without going through the human-readable [`InputDecoder::format`]
    /// path. Note: matches `format`'s legacy quirk that a state-refresh
    /// packet (identical mask) does NOT advance `last_button` — there is
    /// nothing to advance.
    pub fn advance(&mut self, mask: u16) -> u16 {
        let prev = self.last_button;
        if mask != prev {
            self.last_button = mask;
        }
        prev
    }
    #[allow(dead_code)]

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
/// repeat the slot-1 trailer (`major` + `minor` + `pressure` + `flags`)
/// for slot 2.
pub const TOUCH_REPORT_LEN_2F: usize = 18;

// Byte 3 of the touchpad report (`header`) is surfaced raw on
// `TouchEvent::header` for diagnostics; slot presence comes from the
// per-slot (major, minor, pressure) triplet instead.

/// One decoded touchpad sample. Up to two simultaneous fingers are
/// reported — slot indices match the firmware's wire layout (slot 1 at
/// bytes 4..10, slot 2 at bytes 11..17). Released frames carry the
/// 11-byte payload with every per-slot (major, minor, pressure) byte
/// at zero and no active points.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TouchEvent {
    /// Raw byte 3 of the touchpad report. Surfaced for diagnostics;
    /// not used to gate slot presence (`points` is driven by per-slot
    /// validity from major/minor/pressure).
    pub header: u8,
    /// Little-endian 16-bit packet counter from bytes 1..2. Increments
    /// by `0x1E` per frame at the ~15 ms native rate, wraps at 0xFFFF.
    pub seq: u16,
    /// Per-slot finger data. `points[0]` is slot 1, `points[1]` is slot 2.
    /// `None` means that slot is not in contact in this frame.
    pub points: [Option<FingerData>; 2],
}

/// One finger's slice of a touchpad report.
///
/// Per the gen-3 trackpad raw-byte report (confirmed empirically on
/// model DNDJ22MG2330):
///
/// | byte | role |
/// |------|------|
/// | 0    | X low byte (bits 0..7) |
/// | 1    | low nibble: X high nibble (bits 8..11); high nibble: Y low nibble (bits 0..3) |
/// | 2    | Y high byte (bits 4..11) |
/// | 3    | major (contact-ellipse long axis) |
/// | 4    | minor (contact-ellipse short axis) |
/// | 5    | pressure |
/// | 6    | flags (touch state / quadrant hints, not fully decoded) |
///
/// X and Y are 12-bit two's-complement values reconstructed from
/// bytes 0..2 (see [`decode_coord`]); both sit in `-2048..=2047`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FingerData {
    /// Sign-extended 12-bit horizontal position, in `-2048..=2047`.
    /// In practice the active touchpad area spans roughly
    /// `-2030..=+1985`; the representable range is treated as linear,
    /// so a swipe that crosses the ±2047 boundary appears
    /// discontinuous on the canvas (known limitation).
    pub x: i16,
    /// Sign-extended 12-bit vertical position, in `-2048..=2047`. In
    /// practice observed in `~-1018..=+270` across the active area.
    pub y: i16,
    /// Major contact-axis byte (byte 3 of the slot). Tracks contact
    /// size on the long ellipse axis.
    pub major: u8,
    /// Minor contact-axis byte (byte 4 of the slot). Tracks contact
    /// size on the short ellipse axis.
    pub minor: u8,
    /// Pressure byte (byte 5 of the slot). Peaks around `0x14..0x1c`
    /// for a light touch and `0x25+` for a hard click; drops to `0`
    /// during the final lift-off frame even while contact size is
    /// still non-zero.
    pub pressure: u8,
    /// Flags byte (byte 6 of the slot). Empirically mixes touch state
    /// with quadrant hints; not fully decoded, surfaced raw.
    pub flags: u8,
}

impl TouchEvent {
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
                "touch released header=0x{:02x} seq=0x{:04x}",
                self.header, self.seq,
            );
        }
        let mut out = format!("touch fingers={fingers}");
        for (idx, slot) in self.points.iter().enumerate() {
            if let Some(f) = slot {
                let slot_no = idx + 1;
                let _ = write!(
                    out,
                    " [{slot_no}: x={} y={} pressure={} flags=0x{:02x} \
                     major={} minor={}]",
                    f.x, f.y, f.pressure, f.flags, f.major, f.minor,
                );
            }
        }
        let _ = write!(out, " header=0x{:02x} seq=0x{:04x}", self.header, self.seq);
        out
    }
}

/// Parser for the touchpad HID Input report (id `0xFC`).
///
/// Stateless — kept as a struct only because callers thread it through
/// the session loop alongside [`InputDecoder`].
pub struct TouchDecoder;

impl Default for TouchDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl TouchDecoder {
    pub fn new() -> Self {
        Self
    }

    /// Try to parse one touchpad HID Input report. Returns `None` for
    /// payloads that are not well-formed `0xFC` reports (wrong length
    /// or missing the `0x32` marker); the caller should fall back to a
    /// raw-hex dump in that case.
    ///
    /// Slot presence is gated on the per-slot (major, minor, pressure)
    /// triplet — any non-zero byte keeps the slot active so we still
    /// surface position during the lift-off frame when pressure has
    /// already decayed to zero but the contact ellipse hasn't.
    pub fn parse(&mut self, payload: &[u8]) -> Option<TouchEvent> {
        let len = payload.len();
        if (len != TOUCH_REPORT_LEN_1F && len != TOUCH_REPORT_LEN_2F)
            || payload[0] != TOUCH_MARKER
        {
            return None;
        }
        let header = payload[3];
        let seq = u16::from_le_bytes([payload[1], payload[2]]);
        let points = [
            decode_slot_if_valid(&payload[4..11]),
            if len == TOUCH_REPORT_LEN_2F {
                decode_slot_if_valid(&payload[11..18])
            } else {
                None
            },
        ];
        Some(TouchEvent {
            header,
            seq,
            points,
        })
    }
}

/// Decode one slot's 7-byte slice if it carries a contact (any of
/// `major`, `minor`, `pressure` non-zero); otherwise emit `None`.
fn decode_slot_if_valid(b: &[u8]) -> Option<FingerData> {
    if b[3] == 0 && b[4] == 0 && b[5] == 0 {
        return None;
    }
    Some(decode_slot(b))
}

/// 12-bit signed coordinate pair extracted from a slot trailer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Coord {
    pub x: i16,
    pub y: i16,
}

/// Sign-extend a 12-bit unsigned value into an `i16`.
fn sign_extend_12(v: u16) -> i16 {
    if v & 0x0800 != 0 {
        (v | 0xf000) as i16
    } else {
        v as i16
    }
}

/// Decode the three coordinate bytes of a slot trailer into a signed
/// `(x, y)` pair.
///
/// Wire layout (gen-3 trackpad, model DNDJ22MG2330):
///
/// - `byte0`           = X bits 0..7   (X low byte)
/// - `byte1` low nib   = X bits 8..11  (X high nibble)
/// - `byte1` high nib  = Y bits 0..3   (Y low nibble)
/// - `byte2`           = Y bits 4..11  (Y high byte)
///
/// Equivalently, treating the three bytes as a little-endian 24-bit
/// word `packed = b0 | (b1 << 8) | (b2 << 16)`:
///
/// - `x_u12 = packed & 0x0fff`
/// - `y_u12 = (packed >> 12) & 0x0fff`
///
/// Each 12-bit field is sign-extended into `-2048..=2047`.
pub fn decode_coord(coord_bytes: [u8; 3]) -> Coord {
    let packed = (coord_bytes[0] as u32)
        | ((coord_bytes[1] as u32) << 8)
        | ((coord_bytes[2] as u32) << 16);
    let x_u12 = (packed & 0x0fff) as u16;
    let y_u12 = ((packed >> 12) & 0x0fff) as u16;
    Coord {
        x: sign_extend_12(x_u12),
        y: sign_extend_12(y_u12),
    }
}

/// Decode the 7-byte per-slot finger payload by delegating to
/// [`decode_coord`] for bytes 0..3.
fn decode_slot(b: &[u8]) -> FingerData {
    let Coord { x, y } = decode_coord([b[0], b[1], b[2]]);
    FingerData {
        x,
        y,
        major: b[3],
        minor: b[4],
        pressure: b[5],
        flags: b[6],
    }
}

/// Compose a full event line — timestamp, source label, identifier slot, raw
/// hex dump, and decoded description.
///
/// `identifier` is rendered as-is into the line; callers supply something
/// like `"uuid=0000xxxx-…"`. (Python used `handle=0xHHHH`; btleplug does not
/// expose attribute handles, so the plan substitutes the UUID for the same
#[allow(dead_code)]
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
        assert!(TouchDecoder::new().parse(&[]).is_none());
        // Right length, wrong marker.
        assert!(TouchDecoder::new().parse(&[0x00; 11]).is_none());
        // Right marker, wrong length.
        assert!(TouchDecoder::new().parse(&[0x32, 0x00, 0x00]).is_none());
    }

    #[test]
    fn touch_parse_decodes_active_touch() {
        // Captured from a real DNDJ22MG2330 mid-swipe:
        // 32 14 a6 01 c0 1e e6 9e 8e 1a a4
        // Slot trailer bytes 4..11: c0 1e e6 9e 8e 1a a4
        let bytes = [
            0x32, 0x14, 0xa6, 0x01, 0xc0, 0x1e, 0xe6, 0x9e, 0x8e, 0x1a, 0xa4,
        ];
        let ev = TouchDecoder::new().parse(&bytes).expect("valid touch packet");
        assert_eq!(ev.header, 0x01);
        assert_eq!(ev.finger_count(), 1);
        assert_eq!(ev.seq, 0xa614);
        let f = ev.points[0].expect("slot 1 present while touching");
        // packed = 0xc0 | (0x1e<<8) | (0xe6<<16) = 0xe61ec0
        // x_u12 = packed & 0xfff = 0xec0 → sign-ext → -320.
        assert_eq!(f.x, -320);
        // y_u12 = (packed >> 12) & 0xfff = 0xe61 → sign-ext → -415.
        assert_eq!(f.y, -415);
        assert_eq!(f.major, 0x9e);
        assert_eq!(f.minor, 0x8e);
        assert_eq!(f.pressure, 0x1a);
        assert_eq!(f.flags, 0xa4);
        assert!(ev.points[1].is_none(), "slot 2 must be empty in 11-byte payload");
    }

    #[test]
    fn touch_parse_decodes_canonical_report_example() {
        // Worked example from the trackpad raw-byte decoding report:
        // 32 fc e7 01 13 bc e4 33 07 03 62
        let bytes = [
            0x32, 0xfc, 0xe7, 0x01, 0x13, 0xbc, 0xe4, 0x33, 0x07, 0x03, 0x62,
        ];
        let ev = TouchDecoder::new().parse(&bytes).expect("valid touch packet");
        assert_eq!(ev.header, 0x01);
        assert_eq!(ev.seq, 0xe7fc);
        let f = ev.points[0].expect("slot 1 active");
        assert_eq!(f.x, -1005);
        assert_eq!(f.y, -437);
        assert_eq!(f.major, 51);
        assert_eq!(f.minor, 7);
        assert_eq!(f.pressure, 3);
        assert_eq!(f.flags, 0x62);
    }

    #[test]
    fn touch_parse_decodes_release_with_zeroed_point() {
        // Trailing release frame from the same swipe:
        // 32 e6 a6 00 c3 ee e6 00 00 00 87
        // d3=d4=d5=0 ⇒ slot 1 is released.
        let bytes = [
            0x32, 0xe6, 0xa6, 0x00, 0xc3, 0xee, 0xe6, 0x00, 0x00, 0x00, 0x87,
        ];
        let ev = TouchDecoder::new().parse(&bytes).expect("valid release packet");
        assert_eq!(ev.header, 0x00);
        assert_eq!(ev.finger_count(), 0);
        assert!(ev.points.iter().all(|p| p.is_none()));
        assert_eq!(ev.seq, 0xa6e6);
    }

    #[test]
    fn touch_parse_decodes_two_finger_report() {
        // Captured from a real DNDJ22MG2330 with two fingers on the
        // touchpad: 18 bytes, header byte 0x11.
        // 32 a0 89 11 77 60 f8 47 5a 0f 8b 41 0d e2 41 55 0e 83
        let bytes = [
            0x32, 0xa0, 0x89, 0x11, 0x77, 0x60, 0xf8, 0x47, 0x5a, 0x0f, 0x8b, 0x41, 0x0d, 0xe2,
            0x41, 0x55, 0x0e, 0x83,
        ];
        let ev = TouchDecoder::new().parse(&bytes).expect("valid two-finger packet");
        assert_eq!(ev.header, 0x11);
        assert_eq!(ev.finger_count(), 2);
        assert_eq!(ev.seq, 0x89a0);
        let f1 = ev.points[0].expect("slot 1 active");
        // packed = 0x77 | (0x60<<8) | (0xf8<<16) = 0xf86077
        // x_u12 = packed & 0xfff = 0x077 = 119 (sign bit clear).
        assert_eq!(f1.x, 119);
        // y_u12 = (packed >> 12) & 0xfff = 0xf86 → sign-ext → -122.
        assert_eq!(f1.y, -122);
        assert_eq!(f1.major, 0x47);
        assert_eq!(f1.minor, 0x5a);
        assert_eq!(f1.pressure, 0x0f);
        assert_eq!(f1.flags, 0x8b);
        let f2 = ev.points[1].expect("slot 2 active");
        // packed = 0x41 | (0x0d<<8) | (0xe2<<16) = 0xe20d41
        // x_u12 = packed & 0xfff = 0xd41 → sign-ext → -703.
        assert_eq!(f2.x, -703);
        // y_u12 = (packed >> 12) & 0xfff = 0xe20 → sign-ext → -480.
        assert_eq!(f2.y, -480);
        assert_eq!(f2.major, 0x41);
        assert_eq!(f2.minor, 0x55);
        assert_eq!(f2.pressure, 0x0e);
        assert_eq!(f2.flags, 0x83);
    }

    #[test]
    fn touch_parse_decodes_slot1_when_header_byte_zero() {
        // Real-capture frame whose header byte (payload[3]) is `0x00`
        // even though a finger is still on the pad. Slot presence is
        // gated on (major, minor, pressure), not on the header byte.
        let bytes = [
            0x32, 0x7a, 0xfc, 0x00, 0x7b, 0xdf, 0x00, 0x84, 0x8a, 0x15, 0x6c,
        ];
        let ev = TouchDecoder::new().parse(&bytes).expect("valid touch packet");
        assert_eq!(ev.header, 0x00, "header byte stays surfaced as-is");
        assert_eq!(ev.finger_count(), 1, "major|minor|pressure ≠ 0 ⇒ slot active");
        let f = ev.points[0].expect("slot 1 must decode despite header=0x00");
        // packed = 0x7b | (0xdf<<8) | (0x00<<16) = 0x00df7b
        // x_u12 = packed & 0xfff = 0xf7b → sign-ext → -133.
        assert_eq!(f.x, -133);
        // y_u12 = (packed >> 12) & 0xfff = 0x00d = 13 (sign bit clear).
        assert_eq!(f.y, 13);
        assert_eq!(f.major, 0x84);
        assert_eq!(f.minor, 0x8a);
        assert_eq!(f.pressure, 0x15);
        assert_eq!(f.flags, 0x6c);
        assert!(ev.points[1].is_none(), "slot 2 stays empty on 11-byte payload");
    }

    #[test]
    fn touch_parse_keeps_slot_active_when_pressure_zero_but_size_nonzero() {
        // Final mid-swipe frame from a real capture: slot-1 pressure
        // has dropped to 0 but `major` (0x2f) and `minor` (0x08) are
        // still non-zero — the contact ellipse hasn't fully decayed
        // yet, so the slot stays active.
        let bytes = [
            0x32, 0x44, 0x81, 0x01, 0x26, 0x41, 0xe2, 0x2f, 0x08, 0x00, 0x6e,
        ];
        let ev = TouchDecoder::new().parse(&bytes).expect("valid touch packet");
        assert_eq!(ev.header, 0x01);
        assert_eq!(ev.finger_count(), 1);
        let f = ev.points[0].expect("slot 1 must stay active while size>0");
        assert_eq!(f.major, 0x2f);
        assert_eq!(f.minor, 0x08);
        assert_eq!(f.pressure, 0x00);
        assert_eq!(f.flags, 0x6e);
        assert!(ev.points[1].is_none());
    }

    #[test]
    fn touch_parse_releases_slot1_when_all_size_pressure_zero() {
        // 18-byte capture where slot 1's (major, minor, pressure) are
        // all zero — the finger has fully left the pad. Slot 2 still
        // carries a contact and must be reported.
        let bytes = [
            0x32, 0x24, 0x09, 0x00, 0xcb, 0xed, 0xea, 0x00, 0x00, 0x00, 0x68,
            0x71, 0x3e, 0x00, 0x8b, 0x63, 0x03, 0x01,
        ];
        let ev = TouchDecoder::new().parse(&bytes).expect("valid touch packet");
        assert_eq!(ev.header, 0x00);
        assert_eq!(ev.finger_count(), 1);
        assert!(ev.points[0].is_none(), "slot 1 fully released");
        let f2 = ev.points[1].expect("slot 2 must remain active");
        // packed = 0x71 | (0x3e<<8) | (0x00<<16) = 0x003e71
        // x_u12 = packed & 0xfff = 0xe71 → sign-ext → -399.
        assert_eq!(f2.x, -399);
        // y_u12 = (packed >> 12) & 0xfff = 0x003 = 3 (sign bit clear).
        assert_eq!(f2.y, 3);
        assert_eq!(f2.major, 0x8b);
        assert_eq!(f2.minor, 0x63);
        assert_eq!(f2.pressure, 0x03);
        assert_eq!(f2.flags, 0x01);
    }

    #[test]
    fn touch_format_active_includes_position_and_size() {
        let ev = TouchDecoder::new().parse(&[
            0x32, 0x14, 0xa6, 0x01, 0xc0, 0x1e, 0xe6, 0x9e, 0x8e, 0x1a, 0xa4,
        ])
        .unwrap();
        let line = ev.format();
        assert!(
            line.contains("touch fingers=1"),
            "missing fingers tag: {line}",
        );
        assert!(
            line.contains("[1: x=-320 y=-415 pressure=26"),
            "missing decoded slot-1 position: {line}",
        );
        assert!(line.contains("flags=0xa4"), "missing flags: {line}");
        assert!(line.contains("major=158"), "missing major: {line}");
        assert!(line.contains("minor=142"), "missing minor: {line}");
        assert!(line.contains("header=0x01"), "missing header: {line}");
        assert!(line.contains("seq=0xa614"), "missing seq: {line}");
    }

    #[test]
    fn touch_format_two_fingers_emits_both_slots() {
        let ev = TouchDecoder::new().parse(&[
            0x32, 0xa0, 0x89, 0x11, 0x77, 0x60, 0xf8, 0x47, 0x5a, 0x0f, 0x8b, 0x41, 0x0d, 0xe2,
            0x41, 0x55, 0x0e, 0x83,
        ])
        .unwrap();
        let line = ev.format();
        assert!(line.contains("touch fingers=2"), "{line}");
        assert!(line.contains("[1: x=119 y=-122 pressure=15"), "{line}");
        assert!(line.contains("[2: x=-703 y=-480 pressure=14"), "{line}");
        assert!(line.contains("header=0x11"), "{line}");
    }

    #[test]
    fn touch_format_release_uses_released_label() {
        let ev = TouchDecoder::new().parse(&[
            0x32, 0xe6, 0xa6, 0x00, 0xc3, 0xee, 0xe6, 0x00, 0x00, 0x00, 0x87,
        ])
        .unwrap();
        let line = ev.format();
        assert!(line.starts_with("touch released"), "expected released prefix: {line}");
        assert!(!line.contains(" x="), "released frame must not advertise x/y: {line}");
        assert!(line.contains("header=0x00"));
    }

    #[test]
    fn decode_coord_covers_sign_extension_cases() {
        // All zero → (0, 0).
        assert_eq!(decode_coord([0x00, 0x00, 0x00]), Coord { x: 0, y: 0 });
        // Both sign bits clear: x_u12 = 0x123, y_u12 = 0x456.
        // packed = 0x123 | (0x456 << 12) = 0x456123
        // bytes: b0 = 0x23, b1 = 0x61, b2 = 0x45.
        assert_eq!(
            decode_coord([0x23, 0x61, 0x45]),
            Coord { x: 0x123, y: 0x456 },
        );
        // X sign bit set, Y clear: x_u12 = 0xfff (-1), y_u12 = 0x001.
        // packed = 0xfff | (0x001 << 12) = 0x001fff
        // bytes: b0 = 0xff, b1 = 0x1f, b2 = 0x00.
        assert_eq!(
            decode_coord([0xff, 0x1f, 0x00]),
            Coord { x: -1, y: 1 },
        );
        // Y sign bit set, X clear: x_u12 = 0x001, y_u12 = 0xfff (-1).
        // packed = 0x001 | (0xfff << 12) = 0xfff001
        // bytes: b0 = 0x01, b1 = 0xf0, b2 = 0xff.
        assert_eq!(
            decode_coord([0x01, 0xf0, 0xff]),
            Coord { x: 1, y: -1 },
        );
        // Both sign bits set: x_u12 = 0x800 (-2048), y_u12 = 0x800 (-2048).
        // packed = 0x800 | (0x800 << 12) = 0x800800
        // bytes: b0 = 0x00, b1 = 0x08, b2 = 0x80.
        assert_eq!(
            decode_coord([0x00, 0x08, 0x80]),
            Coord { x: -2048, y: -2048 },
        );
    }
}
