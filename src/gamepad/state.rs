//! Pure `DeviceEvent` → `InputEvent` translator. No IO, so the whole
//! mapping is unit testable (same split as [`crate::view::state`]).
//!
//! Callers own the output `Vec<InputEvent>` and `clear()` it per frame, so
//! the ~66 Hz touch stream allocates nothing after warm-up. Each method
//! only pushes components that actually changed, so a stationary finger or
//! a repeated button mask produces an empty batch and no kernel write.

use evdev::{AbsoluteAxisCode, AbsoluteAxisEvent, InputEvent, KeyCode, KeyEvent};

use super::config::{GamepadConfig, HatDir, StickMode, Target};
use super::pad;
use crate::calibration::Calibration;
use crate::decoder::{FingerData, TouchEvent};

pub struct PadState {
    cfg: GamepadConfig,
    cal: Calibration,
    /// Relative-mode virtual centre, captured on touch-down; `None` while released.
    origin: Option<(f64, f64)>,
    /// Last emitted values, so unchanged frames produce an empty batch.
    axes: (i32, i32),
    hat: (i32, i32),
    keys: [bool; pad::PAD_KEYS.len()],
}

/// `(nx, ny)` in `0.0..=1.0`; `nx` grows rightward, `ny` grows toward the
/// top of the remote. `None` on a degenerate calibration (non-positive span).
///
/// Deliberately not [`crate::view::state::normalize_finger`]: that clamps to
/// `-0.1..=1.1` so edge taps still paint into the TUI bezel, which here
/// would push the stick past full scale.
fn normalize(f: &FingerData, cal: &Calibration) -> Option<(f64, f64)> {
    let xs = cal.x_max - cal.x_min;
    let ys = cal.y_max - cal.y_min;
    if xs <= 0 || ys <= 0 {
        return None;
    }
    Some((
        ((f.x as i32 - cal.x_min) as f64 / xs as f64).clamp(0.0, 1.0),
        ((f.y as i32 - cal.y_min) as f64 / ys as f64).clamp(0.0, 1.0),
    ))
}

/// Radial dead zone, rescaled so the usable range still reaches full scale.
/// `dz == 0.0` needs no special case (`s` reduces to `1.0 / m * m`), and
/// `m == 0.0` is caught by the `m <= dz` early return.
fn apply_deadzone(x: f64, y: f64, dz: f64) -> (f64, f64) {
    let m = (x * x + y * y).sqrt();
    if m <= dz {
        return (0.0, 0.0);
    }
    let s = ((m - dz) / (1.0 - dz)) / m;
    ((x * s).clamp(-1.0, 1.0), (y * s).clamp(-1.0, 1.0))
}

fn to_axis(v: f64) -> i32 {
    (v * pad::STICK_MAX as f64)
        .round()
        .clamp(pad::STICK_MIN as f64, pad::STICK_MAX as f64) as i32
}

/// Index of `k` in [`pad::PAD_KEYS`]. Linear scan over 11 compile-time
/// known entries — a map would cost more than it saves.
fn index_of(k: KeyCode) -> Option<usize> {
    pad::PAD_KEYS.iter().position(|c| *c == k)
}

impl PadState {
    pub fn new(cfg: GamepadConfig, cal: Calibration) -> Self {
        Self {
            cfg,
            cal,
            origin: None,
            axes: (0, 0),
            hat: (0, 0),
            keys: [false; pad::PAD_KEYS.len()],
        }
    }

    /// Fold one report-0xFB button mask into `out`.
    ///
    /// The full desired state is recomputed from `mask`: `DeviceEvent::Buttons`
    /// also carries `pressed` / `released`, but both are `0` on a state-refresh
    /// packet, so only `mask` is reliable.
    pub fn on_buttons(&mut self, mask: u16, out: &mut Vec<InputEvent>) {
        let mut want = [false; pad::PAD_KEYS.len()];
        let mut hx = 0i32;
        let mut hy = 0i32;

        for (bit, target) in self.cfg.buttons {
            if mask & bit == 0 {
                continue;
            }
            match target {
                // OR, not assign: two bits may share a key.
                Target::Key(k) => {
                    if let Some(i) = index_of(k) {
                        want[i] = true;
                    }
                }
                Target::Hat(HatDir::Up) => hy -= 1,
                Target::Hat(HatDir::Down) => hy += 1,
                Target::Hat(HatDir::Left) => hx -= 1,
                Target::Hat(HatDir::Right) => hx += 1,
                Target::None => {}
            }
        }
        // Opposite clicks cancel; the clamp also guards a config that maps
        // three bits to one direction.
        let (hx, hy) = (hx.clamp(-1, 1), hy.clamp(-1, 1));

        for (i, (&w, k)) in want.iter().zip(pad::PAD_KEYS).enumerate() {
            if w != self.keys[i] {
                out.push(*KeyEvent::new(k, i32::from(w)));
                self.keys[i] = w;
            }
        }
        self.push_hat(hx, hy, out);
    }

    /// Fold one report-0xFC touch frame into `out`.
    pub fn on_touch(&mut self, ev: &TouchEvent, out: &mut Vec<InputEvent>) {
        // Slot 2 is ignored entirely. A hover frame is not contact: taking
        // it as one would capture the relative origin before the finger lands.
        match ev.points[0].filter(|f| !f.hover) {
            Some(f) => {
                let Some((nx, ny)) = normalize(&f, &self.cal) else {
                    // Degenerate calibration: hold the last emitted value.
                    return;
                };
                let r = self.cfg.stick_radius;
                // Linux convention is ABS_Y negative = up, while raw pad `y`
                // grows upward, so `ny` is inverted here.
                let (ax, ay) = match self.cfg.stick_mode {
                    StickMode::Absolute => (nx * 2.0 - 1.0, 1.0 - ny * 2.0),
                    StickMode::Relative => {
                        let (ox, oy) = *self.origin.get_or_insert((nx, ny));
                        ((nx - ox) / r, (oy - ny) / r)
                    }
                };
                let (ax, ay) =
                    apply_deadzone(ax.clamp(-1.0, 1.0), ay.clamp(-1.0, 1.0), self.cfg.deadzone);
                self.push_axes(to_axis(ax), to_axis(ay), out);
            }
            None => {
                self.origin = None;
                self.push_axes(0, 0, out);
            }
        }
    }

    /// Release everything and centre the sticks (link dropped).
    pub fn neutral(&mut self, out: &mut Vec<InputEvent>) {
        for (i, k) in pad::PAD_KEYS.iter().enumerate() {
            if self.keys[i] {
                out.push(*KeyEvent::new(*k, 0));
                self.keys[i] = false;
            }
        }
        self.push_hat(0, 0, out);
        self.push_axes(0, 0, out);
        self.origin = None;
    }

    fn push_axes(&mut self, x: i32, y: i32, out: &mut Vec<InputEvent>) {
        if x != self.axes.0 {
            out.push(*AbsoluteAxisEvent::new(AbsoluteAxisCode::ABS_X, x));
            self.axes.0 = x;
        }
        if y != self.axes.1 {
            out.push(*AbsoluteAxisEvent::new(AbsoluteAxisCode::ABS_Y, y));
            self.axes.1 = y;
        }
    }

    fn push_hat(&mut self, x: i32, y: i32, out: &mut Vec<InputEvent>) {
        if x != self.hat.0 {
            out.push(*AbsoluteAxisEvent::new(AbsoluteAxisCode::ABS_HAT0X, x));
            self.hat.0 = x;
        }
        if y != self.hat.1 {
            out.push(*AbsoluteAxisEvent::new(AbsoluteAxisCode::ABS_HAT0Y, y));
            self.hat.1 = y;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gamepad::config::{DEFAULT_BUTTONS, GamepadConfig, Target};

    fn cfg(stick_mode: StickMode, deadzone: f64) -> GamepadConfig {
        let mut buttons = [(0u16, Target::None); 13];
        for (slot, (bit, _, target)) in buttons.iter_mut().zip(DEFAULT_BUTTONS) {
            *slot = (bit, target);
        }
        GamepadConfig {
            stick_mode,
            stick_radius: 0.35,
            deadzone,
            buttons,
        }
    }

    fn st(stick_mode: StickMode) -> PadState {
        PadState::new(cfg(stick_mode, 0.0), CAL)
    }

    fn finger(x: i16, y: i16) -> FingerData {
        FingerData {
            x,
            y,
            major: 0x10,
            minor: 0x10,
            pressure: 0x18,
            flags: 0,
            hover: false,
            angle_idx: 0,
        }
    }

    fn touch(points: [Option<FingerData>; 2]) -> TouchEvent {
        TouchEvent {
            header: 0,
            seq: 0,
            points,
        }
    }

    fn one(x: i16, y: i16) -> TouchEvent {
        touch([Some(finger(x, y)), None])
    }

    fn axis(batch: &[InputEvent], code: AbsoluteAxisCode) -> Option<i32> {
        batch
            .iter()
            .filter(|e| e.event_type() == evdev::EventType::ABSOLUTE && e.code() == code.0)
            .map(|e| e.value())
            .next_back()
    }

    fn key(batch: &[InputEvent], code: KeyCode) -> Option<i32> {
        batch
            .iter()
            .filter(|e| e.event_type() == evdev::EventType::KEY && e.code() == code.0)
            .map(|e| e.value())
            .next_back()
    }

    /// Spelled out so the axis assertions below can compute exact pad
    /// coordinates. Pinned to [`Calibration::default`] by
    /// `cal_const_tracks_default`.
    const CAL: Calibration = Calibration {
        x_min: -2029,
        x_max: 1984,
        y_min: -1010,
        y_max: 270,
    };

    #[test]
    fn cal_const_tracks_default() {
        assert_eq!(CAL, Calibration::default());
    }

    fn mid(lo: i32, hi: i32) -> i16 {
        ((lo + hi) / 2) as i16
    }

    #[test]
    fn absolute_centre_is_centred() {
        let mut s = st(StickMode::Absolute);
        let mut b = Vec::new();
        s.on_touch(
            &one(mid(CAL.x_min, CAL.x_max), mid(CAL.y_min, CAL.y_max)),
            &mut b,
        );
        // Integer midpoint rounding leaves at most one LSB of the span.
        let ax = axis(&b, AbsoluteAxisCode::ABS_X).unwrap_or(0);
        let ay = axis(&b, AbsoluteAxisCode::ABS_Y).unwrap_or(0);
        assert!(ax.abs() <= 32, "ABS_X {ax}");
        assert!(ay.abs() <= 32, "ABS_Y {ay}");
    }

    #[test]
    fn absolute_bottom_right_is_max_on_both_axes() {
        let mut s = st(StickMode::Absolute);
        let mut b = Vec::new();
        s.on_touch(&one(CAL.x_max as i16, CAL.y_min as i16), &mut b);
        assert_eq!(axis(&b, AbsoluteAxisCode::ABS_X), Some(pad::STICK_MAX));
        assert_eq!(axis(&b, AbsoluteAxisCode::ABS_Y), Some(pad::STICK_MAX));
    }

    #[test]
    fn absolute_top_of_pad_is_negative_y() {
        let mut s = st(StickMode::Absolute);
        let mut b = Vec::new();
        s.on_touch(&one(mid(CAL.x_min, CAL.x_max), CAL.y_max as i16), &mut b);
        let ay = axis(&b, AbsoluteAxisCode::ABS_Y).expect("ABS_Y emitted");
        assert!((-32767..=-32766).contains(&ay), "ABS_Y {ay}");
    }

    #[test]
    fn relative_first_frame_captures_origin_without_deflecting() {
        let mut s = st(StickMode::Relative);
        let mut b = Vec::new();
        s.on_touch(&one(1500, 100), &mut b);
        // Nothing changed from the centred initial state, so nothing is emitted.
        assert!(b.is_empty(), "{b:?}");
    }

    #[test]
    fn relative_displacement_of_one_radius_is_full_scale() {
        let mut s = st(StickMode::Relative);
        let mut b = Vec::new();
        let x0 = mid(CAL.x_min, CAL.x_max);
        s.on_touch(&one(x0, 100), &mut b);
        b.clear();
        let span = (CAL.x_max - CAL.x_min) as f64;
        let dx = (span * 0.35).round() as i16;
        s.on_touch(&one(x0 + dx, 100), &mut b);
        assert_eq!(axis(&b, AbsoluteAxisCode::ABS_X), Some(pad::STICK_MAX));
    }

    #[test]
    fn release_recentres_and_reorigins() {
        let mut s = st(StickMode::Relative);
        let mut b = Vec::new();
        let x0 = mid(CAL.x_min, CAL.x_max);
        s.on_touch(&one(x0, 100), &mut b);
        b.clear();
        s.on_touch(&one(x0 + 800, 100), &mut b);
        assert!(axis(&b, AbsoluteAxisCode::ABS_X).unwrap() > 0);

        // The firmware's all-zero 11-byte payload decodes to no points.
        b.clear();
        s.on_touch(&touch([None, None]), &mut b);
        assert_eq!(axis(&b, AbsoluteAxisCode::ABS_X), Some(0));

        // Fresh contact somewhere else re-origins, so its first frame is centred.
        b.clear();
        s.on_touch(&one(x0 - 1200, 200), &mut b);
        assert!(b.is_empty(), "{b:?}");
    }

    #[test]
    fn hover_frame_is_treated_as_released() {
        let mut s = st(StickMode::Relative);
        let mut b = Vec::new();
        let mut f = finger(1500, 100);
        f.hover = true;
        s.on_touch(&touch([Some(f), None]), &mut b);
        assert!(b.is_empty(), "{b:?}");

        // No origin was captured, so the first real contact still centres.
        b.clear();
        s.on_touch(&one(1500, 100), &mut b);
        assert!(b.is_empty(), "{b:?}");
    }

    #[test]
    fn slot_two_is_ignored() {
        let mut s = st(StickMode::Absolute);
        let mut b = Vec::new();
        s.on_touch(
            &touch([None, Some(finger(CAL.x_max as i16, CAL.y_min as i16))]),
            &mut b,
        );
        assert!(b.is_empty(), "{b:?}");
    }

    #[test]
    fn select_drives_btn_south_with_edge_dedup() {
        let mut s = st(StickMode::Relative);
        let mut b = Vec::new();
        s.on_buttons(0x0008, &mut b);
        assert_eq!(key(&b, KeyCode::BTN_SOUTH), Some(1));
        assert_eq!(b.len(), 1, "{b:?}");

        b.clear();
        s.on_buttons(0x0008, &mut b);
        assert!(b.is_empty(), "state-refresh packet re-emitted: {b:?}");

        b.clear();
        s.on_buttons(0x0000, &mut b);
        assert_eq!(key(&b, KeyCode::BTN_SOUTH), Some(0));
    }

    #[test]
    fn clickpad_directions_drive_the_hat() {
        let mut s = st(StickMode::Relative);
        let mut b = Vec::new();
        s.on_buttons(0x0200, &mut b);
        assert_eq!(axis(&b, AbsoluteAxisCode::ABS_HAT0Y), Some(-1));

        b.clear();
        s.on_buttons(0x0800, &mut b);
        assert_eq!(axis(&b, AbsoluteAxisCode::ABS_HAT0Y), Some(1));

        // Up + down cancel.
        b.clear();
        s.on_buttons(0x0A00, &mut b);
        assert_eq!(axis(&b, AbsoluteAxisCode::ABS_HAT0Y), Some(0));

        b.clear();
        s.on_buttons(0x0400, &mut b);
        assert_eq!(axis(&b, AbsoluteAxisCode::ABS_HAT0X), Some(1));
    }

    #[test]
    fn deadzone_suppresses_small_deflection_but_keeps_full_scale() {
        let mut s = PadState::new(cfg(StickMode::Absolute, 0.5), CAL);
        let mut b = Vec::new();
        // ~25 % right of centre, inside a 0.5 dead zone.
        let x = mid(CAL.x_min, CAL.x_max) + ((CAL.x_max - CAL.x_min) as f64 * 0.125) as i16;
        s.on_touch(&one(x, mid(CAL.y_min, CAL.y_max)), &mut b);
        assert_eq!(axis(&b, AbsoluteAxisCode::ABS_X).unwrap_or(0), 0);

        b.clear();
        s.on_touch(&one(CAL.x_max as i16, CAL.y_min as i16), &mut b);
        assert_eq!(axis(&b, AbsoluteAxisCode::ABS_X), Some(pad::STICK_MAX));
    }

    #[test]
    fn neutral_releases_and_recentres() {
        let mut s = st(StickMode::Absolute);
        let mut b = Vec::new();
        s.on_buttons(0x0208, &mut b);
        b.clear();
        s.on_touch(&one(CAL.x_max as i16, CAL.y_min as i16), &mut b);
        b.clear();

        s.neutral(&mut b);
        assert_eq!(key(&b, KeyCode::BTN_SOUTH), Some(0));
        assert_eq!(axis(&b, AbsoluteAxisCode::ABS_HAT0Y), Some(0));
        assert_eq!(axis(&b, AbsoluteAxisCode::ABS_X), Some(0));
        assert_eq!(axis(&b, AbsoluteAxisCode::ABS_Y), Some(0));

        // Idempotent: a second neutral has nothing left to release.
        b.clear();
        s.neutral(&mut b);
        assert!(b.is_empty(), "{b:?}");
    }
}
