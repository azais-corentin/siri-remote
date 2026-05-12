//! Live state mutated by [`crate::session::DeviceEvent`]s and rendered by
//! [`super::ui::draw`]. No IO; pure data + the `on_event` dispatcher.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::time::Instant;

use crate::decoder::{FingerData, TouchEvent};
use crate::logger::LogRecord;
use crate::session::{DeviceEvent, PowerState, Selection};

/// Maximum number of touch samples retained for the fading trail. Older
/// samples are pushed off the back of the deque, cleared on release.
pub const TRAIL_CAP: usize = 30;
/// Buttons stay highlighted for this long after release.
pub const BUTTON_AFTERGLOW: Duration = Duration::from_millis(150);
/// Event log capacity before old entries are dropped.
pub const EVENT_LOG_CAP: usize = 256;

/// Minimum finger samples a calibration session must collect before its
/// bounds are accepted. One frame ≈ 15 ms, so 30 samples ≈ half a second
/// of contact — short enough to feel responsive, long enough to weed out
/// stray taps where the user re-pressed `c` immediately.
pub const MIN_CALIBRATION_SAMPLES: usize = 30;
/// Minimum X span (firmware units, signed 12-bit) before the calibration
/// is considered non-degenerate. ~10 % of the representable range.
pub const MIN_CALIBRATION_X_SPAN: i32 = 400;
/// Minimum Y span (firmware units, signed 12-bit; active area spans
/// roughly `-1018..=+270`).
pub const MIN_CALIBRATION_Y_SPAN: i32 = 30;

/// One log line in the events panel.
#[derive(Clone, Debug)]
pub struct EventLine {
    #[allow(dead_code)]
    pub stamp: Instant,
    pub source: EventSource,
    pub text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventSource {
    Buttons,
    Battery,
    Power,
    Raw,
    System,
    Warning,
}

#[derive(Clone, Debug)]
pub enum ConnState {
    Connecting { since: Instant },
    Connected { since: Instant },
    Reconnecting { reason: String, since: Instant },
    Pairing { since: Instant },
}

/// One touchpad sample retained in the trail. Stored in remote-local
/// canvas coordinates (post-normalization) so the renderer doesn't have
/// to repeat the wrap math on every frame.
#[derive(Clone, Copy, Debug)]
pub struct TrailPoint {
    pub slot: u8,
    pub x: f64,
    pub y: f64,
    /// Time the sample was captured. Used to bucket the trail into fade
    /// levels (newer = brighter).
    pub stamp: Instant,
}

/// Touchpad-to-canvas mapping.
///
/// Both axes are linear: `x_min..=x_max` maps to canvas X `0..=1`, and
/// `y_min..=y_max` maps to canvas Y `0..=1`. The firmware encodes (x, y)
/// as 12-bit signed integers, so each bound naturally sits in
/// `-2048..=2047`.
///
/// Defaults are derived from the four `*_at_center_*.txt` swipe captures
/// in the repository; per-device variance is small enough that the
/// out-of-the-box mapping is usable without calibration, but a saved
/// calibration tightens both axes to the user's pad.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Calibration {
    pub x_min: i32,
    pub x_max: i32,
    pub y_min: i32,
    pub y_max: i32,
}

impl Default for Calibration {
    fn default() -> Self {
        // Empirical extents across the gen-3 capture corpus
        // (`*_at_center_*.txt`): signed X observed in -2029..=+1984,
        // signed Y in -1010..=+270.
        Self {
            x_min: -2029,
            x_max: 1984,
            y_min: -1010,
            y_max: 270,
        }
    }
}

/// Running bounds for an in-progress calibration session.
///
/// Tracks min/max on each axis directly. Both axes are linear now, so
/// there's no arc inference — `(x_min, x_max)` and `(y_min, y_max)`
/// drop straight into [`Calibration`] when the session commits.
#[derive(Clone, Debug)]
pub struct CalibrationSession {
    pub x_min: i32,
    pub x_max: i32,
    pub y_min: i32,
    pub y_max: i32,
    pub samples: usize,
}

impl CalibrationSession {
    fn new() -> Self {
        Self {
            x_min: i32::MAX,
            x_max: i32::MIN,
            y_min: i32::MAX,
            y_max: i32::MIN,
            samples: 0,
        }
    }

    /// Fold one finger sample into the running bounds.
    pub fn observe(&mut self, raw_x: i32, raw_y: i32) {
        if raw_x < self.x_min {
            self.x_min = raw_x;
        }
        if raw_x > self.x_max {
            self.x_max = raw_x;
        }
        if raw_y < self.y_min {
            self.y_min = raw_y;
        }
        if raw_y > self.y_max {
            self.y_max = raw_y;
        }
        self.samples += 1;
    }

    /// `true` iff sample count and per-axis spans clear the thresholds.
    /// Used by [`AppState::finish_calibration`] to decide commit vs.
    /// reject.
    pub fn is_acceptable(&self) -> bool {
        self.samples >= MIN_CALIBRATION_SAMPLES
            && self.x_max - self.x_min >= MIN_CALIBRATION_X_SPAN
            && self.y_max - self.y_min >= MIN_CALIBRATION_Y_SPAN
    }
}

/// Calibration state machine carried by [`AppState`].
#[derive(Clone, Debug)]
pub enum CalibrationMode {
    Idle,
    Active(CalibrationSession),
}

/// Outcome of [`AppState::finish_calibration`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FinishOutcome {
    /// Bounds met the thresholds and have been committed to
    /// `AppState::calibration`.
    Committed,
    /// Bounds were degenerate or sample count was too low; previous
    /// calibration retained.
    Rejected,
}

pub struct AppState {
    pub connection: ConnState,
    pub selection: Selection,

    pub battery: Option<u8>,
    pub power: Option<PowerState>,

    pub buttons_mask: u16,
    /// Per-bit afterglow deadlines. A bit is fully highlighted while held
    /// (i.e. while `buttons_mask & bit != 0`); after release the bit stays
    /// in this map until its deadline passes, allowing a fade animation.
    pub button_afterglow: HashMap<u16, Instant>,

    pub touch_active: bool,
    pub touch_trail: VecDeque<TrailPoint>,
    pub last_touch: Option<TouchEvent>,

    /// Saved per-axis bounds used to normalize touch samples into the
    /// canvas's `0..=1` space.
    pub calibration: Calibration,
    /// Live calibration session; `Idle` outside of calibration mode.
    pub calibration_mode: CalibrationMode,

    pub events: VecDeque<EventLine>,
}

impl AppState {
    pub fn new(selection: Selection, calibration: Calibration) -> Self {
        Self {
            connection: ConnState::Connecting {
                since: Instant::now(),
            },
            selection,
            battery: None,
            power: None,
            buttons_mask: 0,
            button_afterglow: HashMap::new(),
            touch_active: false,
            touch_trail: VecDeque::with_capacity(TRAIL_CAP),
            last_touch: None,
            calibration,
            calibration_mode: CalibrationMode::Idle,
            events: VecDeque::with_capacity(EVENT_LOG_CAP),
        }
    }

    pub fn set_connection(&mut self, state: ConnState) {
        self.connection = state;
    }

    /// `true` while a calibration session is active.
    pub fn is_calibrating(&self) -> bool {
        matches!(self.calibration_mode, CalibrationMode::Active(_))
    }

    /// Enter calibration mode. The trail is cleared so the user sees a
    /// blank canvas to draw on.
    pub fn start_calibration(&mut self) {
        self.calibration_mode = CalibrationMode::Active(CalibrationSession::new());
        self.touch_trail.clear();
    }

    /// Leave calibration mode. If the session meets the thresholds, the
    /// running bounds replace `self.calibration` and the returned
    /// [`FinishOutcome::Committed`] carries the new bounds; otherwise the
    /// previous calibration is retained and the outcome is `Rejected`.
    pub fn finish_calibration(&mut self) -> FinishOutcome {
        let session = match std::mem::replace(&mut self.calibration_mode, CalibrationMode::Idle) {
            CalibrationMode::Active(s) => s,
            CalibrationMode::Idle => return FinishOutcome::Rejected,
        };
        self.touch_trail.clear();
        if !session.is_acceptable() {
            return FinishOutcome::Rejected;
        }
        self.calibration = Calibration {
            x_min: session.x_min,
            x_max: session.x_max,
            y_min: session.y_min,
            y_max: session.y_max,
        };
        FinishOutcome::Committed
    }

    /// Abort the current session and keep the previous calibration.
    pub fn cancel_calibration(&mut self) {
        self.calibration_mode = CalibrationMode::Idle;
        self.touch_trail.clear();
    }

    /// Reset calibration to defaults and ensure we are out of calibration
    /// mode. Persistence (deleting the file) is the caller's job.
    pub fn clear_calibration(&mut self) {
        self.calibration = Calibration::default();
        self.calibration_mode = CalibrationMode::Idle;
        self.touch_trail.clear();
    }

    /// Snapshot of the running session, for the live readout panel.
    pub fn calibration_session(&self) -> Option<&CalibrationSession> {
        match &self.calibration_mode {
            CalibrationMode::Active(s) => Some(s),
            CalibrationMode::Idle => None,
        }
    }

    /// Apply one decoded event to the state. Returns nothing; rendering
    /// is driven separately on a tick.
    pub fn on_event(&mut self, ev: DeviceEvent) {
        let now = Instant::now();
        match ev {
            DeviceEvent::Battery { value, .. } => {
                self.battery = Some(value);
                self.push_log(EventSource::Battery, format!("battery={value}%"));
            }
            DeviceEvent::Power { state, .. } => {
                self.power = Some(state);
                self.push_log(EventSource::Power, format!("power={}", power_label(state)));
            }
            DeviceEvent::Buttons {
                mask,
                pressed,
                released,
                ..
            } => {
                // For each newly-released bit: record the afterglow deadline.
                let mut bit = 1u16;
                while bit != 0 {
                    if released & bit != 0 {
                        self.button_afterglow.insert(bit, now + BUTTON_AFTERGLOW);
                    }
                    // While held, the held-state takes priority — drop any
                    // stale afterglow entry so the bit doesn't fade midway
                    // through a long press.
                    if pressed & bit != 0 {
                        self.button_afterglow.remove(&bit);
                    }
                    bit = bit.wrapping_shl(1);
                }
                self.buttons_mask = mask;
                if pressed != 0 || released != 0 {
                    self.push_log(
                        EventSource::Buttons,
                        format!(
                            "mask=0x{mask:04X} pressed=0x{pressed:04X} released=0x{released:04X}"
                        ),
                    );
                }
            }
            DeviceEvent::Touch { event, .. } => {
                let fingers = event.finger_count();
                if fingers == 0 {
                    // Release: drop the trail and finger snapshot.
                    self.touch_active = false;
                    self.touch_trail.clear();
                    self.last_touch = Some(event);
                } else {
                    self.touch_active = true;
                    // Calibration mode: fold every active finger into
                    // the running bounds, but use the *default* mapping
                    // for the trail so the user can see where they are
                    // physically drawing.
                    let calibrating = self.is_calibrating();
                    let cal_for_trail = if calibrating {
                        Calibration::default()
                    } else {
                        self.calibration
                    };
                    for (idx, slot) in event.points.iter().enumerate() {
                        if let Some(f) = slot {
                            if let CalibrationMode::Active(ref mut s) = self.calibration_mode {
                                s.observe(f.x as i32, f.y as i32);
                            }
                            if let Some((nx, ny)) = normalize_finger(f, &cal_for_trail) {
                                if self.touch_trail.len() == TRAIL_CAP {
                                    self.touch_trail.pop_front();
                                }
                                self.touch_trail.push_back(TrailPoint {
                                    slot: idx as u8 + 1,
                                    x: nx,
                                    y: ny,
                                    stamp: now,
                                });
                            }
                        }
                    }
                    self.last_touch = Some(event);
                }
            }
            DeviceEvent::UnknownInput { report_id, payload } => {
                self.push_log(
                    EventSource::Raw,
                    format!("report_id=0x{report_id:02X} len={}", payload.len()),
                );
            }
            DeviceEvent::UnknownOther { uuid, payload } => {
                self.push_log(
                    EventSource::Raw,
                    format!("uuid={uuid} len={}", payload.len()),
                );
            }
        }
    }

    /// Drop afterglow entries past their deadline. Called on every tick.
    pub fn tick_afterglow(&mut self, now: Instant) {
        self.button_afterglow.retain(|_, deadline| *deadline > now);
    }

    /// Route a [`LogRecord`] from the shared logging facade into the events
    /// panel. `Info` → `System` (magenta), `Warn` → `Warning` (red).
    pub fn on_log(&mut self, record: LogRecord) {
        let source = match record.level {
            log::Level::Warn | log::Level::Error => EventSource::Warning,
            _ => EventSource::System,
        };
        self.push_log(source, record.message);
    }

    pub fn push_log(&mut self, source: EventSource, text: String) {
        if self.events.len() == EVENT_LOG_CAP {
            self.events.pop_front();
        }
        self.events.push_back(EventLine {
            stamp: Instant::now(),
            source,
            text,
        });
    }
}

pub fn power_label(state: PowerState) -> &'static str {
    match state {
        PowerState::Charging => "charging",
        PowerState::Discharging => "discharging",
        PowerState::PluggedIn => "plugged-in",
        PowerState::Unknown(_) => "unknown",
    }
}

/// Normalize a raw [`FingerData`] sample into canvas-local coordinates.
///
/// Both axes are linear:
///   `nx = (f.x - cal.x_min) / (cal.x_max - cal.x_min)`
///   `ny = (f.y - cal.y_min) / (cal.y_max - cal.y_min)`
/// Off-pad samples clamp to `[-0.1, 1.1]` so edge taps still draw
/// something without painting arbitrarily far into the bezel.
///
/// Returns `None` only if either span is zero (degenerate
/// calibration).
pub fn normalize_finger(f: &FingerData, cal: &Calibration) -> Option<(f64, f64)> {
    let x_span = cal.x_max - cal.x_min;
    let y_span = cal.y_max - cal.y_min;
    if x_span == 0 || y_span == 0 {
        return None;
    }
    let nx = (f.x as i32 - cal.x_min) as f64 / x_span as f64;
    let ny = (f.y as i32 - cal.y_min) as f64 / y_span as f64;
    Some((nx.clamp(-0.1, 1.1), ny.clamp(-0.1, 1.1)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decoder::FingerData;
    use crate::session::DeviceEvent;

    fn finger(x: i16, y: i16) -> FingerData {
        FingerData {
            x,
            y,
            major: 0x20,
            minor: 0x20,
            pressure: 0x20,
            flags: 0,
        }
    }

    fn touch_event(f: FingerData) -> TouchEvent {
        TouchEvent {
            header: 0,
            seq: 0,
            points: [Some(f), None],
        }
    }

    fn selection() -> Selection {
        Selection {
            address: "AA:BB:CC:DD:EE:FF".to_string(),
            name: "test".to_string(),
            peripheral_id: None,
            identity_address: None,
            requires_pairing: false,
            rssi: None,
        }
    }

    fn state() -> AppState {
        AppState::new(selection(), Calibration::default())
    }

    fn dispatch(s: &mut AppState, f: FingerData) {
        s.on_event(DeviceEvent::Touch {
            report_id: 0xFC,
            event: touch_event(f),
            raw: Vec::new(),
        });
    }

    #[test]
    fn calibration_default_maps_pad_centre_to_canvas_centre() {
        let cal = Calibration::default();
        let mid_x = ((cal.x_min + cal.x_max) / 2) as i16;
        let mid_y = ((cal.y_min + cal.y_max) / 2) as i16;
        let (nx, ny) = normalize_finger(&finger(mid_x, mid_y), &cal).unwrap();
        let x_span = (cal.x_max - cal.x_min) as f64;
        let y_span = (cal.y_max - cal.y_min) as f64;
        // Integer-rounded midpoints land within one firmware unit of
        // the true 0.5 — assert that tolerance, not exact equality.
        assert!((nx - 0.5).abs() <= 1.0 / x_span, "nx={nx}");
        assert!((ny - 0.5).abs() <= 1.0 / y_span, "ny={ny}");
    }

    #[test]
    fn calibration_default_maps_pad_edges_to_canvas_edges() {
        let cal = Calibration::default();
        let (nx_l, _) = normalize_finger(&finger(cal.x_min as i16, 0), &cal).unwrap();
        assert!(nx_l.abs() < 1e-9, "nx_l={nx_l}");
        let (nx_r, _) = normalize_finger(&finger(cal.x_max as i16, 0), &cal).unwrap();
        assert!((nx_r - 1.0).abs() < 1e-9, "nx_r={nx_r}");
        let (_, ny_b) = normalize_finger(&finger(0, cal.y_min as i16), &cal).unwrap();
        assert!(ny_b.abs() < 1e-9, "ny_b={ny_b}");
        let (_, ny_t) = normalize_finger(&finger(0, cal.y_max as i16), &cal).unwrap();
        assert!((ny_t - 1.0).abs() < 1e-9, "ny_t={ny_t}");
    }

    #[test]
    fn normalize_finger_uses_custom_bounds() {
        let cal = Calibration {
            x_min: -1000,
            x_max: 1000,
            y_min: -500,
            y_max: 500,
        };
        let (left, _) = normalize_finger(&finger(-1000, -500), &cal).unwrap();
        let (centre, _) = normalize_finger(&finger(0, 0), &cal).unwrap();
        let (right, _) = normalize_finger(&finger(1000, 500), &cal).unwrap();
        assert!(left.abs() < 1e-9, "left={left}");
        assert!((centre - 0.5).abs() < 1e-9, "centre={centre}");
        assert!((right - 1.0).abs() < 1e-9, "right={right}");
    }

    #[test]
    fn normalize_finger_clamps_off_pad_samples() {
        let cal = Calibration {
            x_min: -100,
            x_max: 100,
            y_min: -50,
            y_max: 50,
        };
        let (nx_lo, ny_lo) = normalize_finger(&finger(-1000, -1000), &cal).unwrap();
        assert!((nx_lo + 0.1).abs() < 1e-9, "nx_lo={nx_lo}");
        assert!((ny_lo + 0.1).abs() < 1e-9, "ny_lo={ny_lo}");
        let (nx_hi, ny_hi) = normalize_finger(&finger(1000, 1000), &cal).unwrap();
        assert!((nx_hi - 1.1).abs() < 1e-9, "nx_hi={nx_hi}");
        assert!((ny_hi - 1.1).abs() < 1e-9, "ny_hi={ny_hi}");
    }

    #[test]
    fn normalize_finger_returns_none_for_degenerate_bounds() {
        let zero_x = Calibration {
            x_min: 0,
            x_max: 0,
            y_min: 0,
            y_max: 100,
        };
        assert!(normalize_finger(&finger(0, 0), &zero_x).is_none());
        let zero_y = Calibration {
            x_min: -10,
            x_max: 10,
            y_min: 50,
            y_max: 50,
        };
        assert!(normalize_finger(&finger(0, 0), &zero_y).is_none());
    }

    #[test]
    fn start_calibration_clears_trail_and_marks_active() {
        let mut s = state();
        // Seed a trail point.
        dispatch(&mut s, finger(500, 50));
        assert!(!s.touch_trail.is_empty());
        s.start_calibration();
        assert!(s.touch_trail.is_empty());
        assert!(s.is_calibrating());
    }

    #[test]
    fn touch_during_calibration_records_samples() {
        let mut s = state();
        s.start_calibration();
        dispatch(&mut s, finger(-1500, -500));
        dispatch(&mut s, finger(1500, 200));
        dispatch(&mut s, finger(0, -100));
        let session = s.calibration_session().expect("session active");
        assert_eq!(session.samples, 3);
        assert_eq!(session.x_min, -1500);
        assert_eq!(session.x_max, 1500);
        assert_eq!(session.y_min, -500);
        assert_eq!(session.y_max, 200);
    }

    #[test]
    fn calibration_session_tracks_running_extremes() {
        let mut s = CalibrationSession::new();
        for x in [-100, 200, -500, 1000, 800, -50] {
            s.observe(x, 50);
        }
        assert_eq!(s.x_min, -500);
        assert_eq!(s.x_max, 1000);
        assert_eq!(s.y_min, 50);
        assert_eq!(s.y_max, 50);
        assert_eq!(s.samples, 6);
    }

    #[test]
    fn finish_calibration_commits_when_thresholds_met() {
        let mut s = state();
        s.start_calibration();
        // Sweep both axes well past the per-axis span thresholds with
        // more than MIN_CALIBRATION_SAMPLES frames.
        for i in 0..MIN_CALIBRATION_SAMPLES {
            let frac = i as f64 / (MIN_CALIBRATION_SAMPLES - 1) as f64;
            let x = (-1000.0 + frac * 2000.0) as i16;
            let y = (-100.0 + frac * 200.0) as i16;
            dispatch(&mut s, finger(x, y));
        }
        let outcome = s.finish_calibration();
        assert_eq!(outcome, FinishOutcome::Committed);
        assert!(!s.is_calibrating());
        assert_eq!(s.calibration.x_min, -1000);
        assert_eq!(s.calibration.x_max, 1000);
        assert_eq!(s.calibration.y_min, -100);
        assert_eq!(s.calibration.y_max, 100);
    }

    #[test]
    fn finish_calibration_rejects_too_few_samples() {
        let mut s = state();
        s.start_calibration();
        for _ in 0..3 {
            dispatch(&mut s, finger(-1000, -100));
            dispatch(&mut s, finger(1000, 100));
        }
        let outcome = s.finish_calibration();
        assert_eq!(outcome, FinishOutcome::Rejected);
        assert_eq!(s.calibration, Calibration::default());
    }

    #[test]
    fn finish_calibration_rejects_degenerate_span() {
        let mut s = state();
        s.start_calibration();
        for _ in 0..MIN_CALIBRATION_SAMPLES {
            dispatch(&mut s, finger(500, 50));
            dispatch(&mut s, finger(510, 51));
        }
        let outcome = s.finish_calibration();
        assert_eq!(outcome, FinishOutcome::Rejected);
        assert_eq!(s.calibration, Calibration::default());
    }

    #[test]
    fn cancel_calibration_restores_previous_and_clears_session() {
        let prior = Calibration {
            x_min: -1500,
            x_max: 1500,
            y_min: -500,
            y_max: 200,
        };
        let mut s = AppState::new(selection(), prior);
        s.start_calibration();
        dispatch(&mut s, finger(500, 50));
        s.cancel_calibration();
        assert!(!s.is_calibrating());
        assert_eq!(s.calibration, prior);
        assert!(s.touch_trail.is_empty());
    }

    #[test]
    fn clear_calibration_resets_to_default() {
        let prior = Calibration {
            x_min: -1500,
            x_max: 1500,
            y_min: -500,
            y_max: 200,
        };
        let mut s = AppState::new(selection(), prior);
        s.clear_calibration();
        assert_eq!(s.calibration, Calibration::default());
        assert!(!s.is_calibrating());
    }
}
