//! Live state mutated by [`crate::session::DeviceEvent`]s and rendered by
//! [`super::ui::draw`]. No IO; pure data + the `on_event` dispatcher.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::time::Instant;

use crate::decoder::{FingerData, TOUCH_X_PERIOD, TouchEvent};
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
/// Minimum X span (firmware units, 0..=`TOUCH_X_PERIOD`) before the
/// calibration is considered non-degenerate.
pub const MIN_CALIBRATION_X_SPAN: i32 = 400;
/// Minimum Y span (firmware units, nominally 0..=106).
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

/// Per-axis min/max in raw firmware coordinates. `x` is the decoder's
/// extended monotonic position (continuous across the firmware's cyclic
/// 11-bit wrap); `y` is the firmware byte (nominally `0..=106`). Default
/// matches the historical hard-coded behavior, so an uncalibrated state
/// renders identically to the pre-calibration code.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Calibration {
    pub x_min: i32,
    pub x_max: i32,
    pub y_min: i32,
    pub y_max: i32,
}

impl Default for Calibration {
    fn default() -> Self {
        Self {
            x_min: 0,
            x_max: TOUCH_X_PERIOD,
            y_min: 0,
            y_max: 106,
        }
    }
}

/// Running min/max plus a sample counter for an in-progress calibration.
#[derive(Clone, Copy, Debug)]
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

    /// Fold one finger sample into the running bounds. `raw_x` is the
    /// firmware cyclic value, `raw_y` the firmware byte.
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

    /// `true` iff the running bounds clear both the sample and span
    /// thresholds. Used by [`AppState::finish_calibration`] to decide
    /// commit vs. reject.
    pub fn is_acceptable(&self) -> bool {
        self.samples >= MIN_CALIBRATION_SAMPLES
            && self.x_max - self.x_min >= MIN_CALIBRATION_X_SPAN
            && self.y_max - self.y_min >= MIN_CALIBRATION_Y_SPAN
    }
}

/// Calibration state machine carried by [`AppState`].
#[derive(Clone, Copy, Debug)]
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
        let session = match self.calibration_mode {
            CalibrationMode::Active(s) => s,
            CalibrationMode::Idle => return FinishOutcome::Rejected,
        };
        self.calibration_mode = CalibrationMode::Idle;
        self.touch_trail.clear();
        if session.is_acceptable() {
            self.calibration = Calibration {
                x_min: session.x_min,
                x_max: session.x_max,
                y_min: session.y_min,
                y_max: session.y_max,
            };
            FinishOutcome::Committed
        } else {
            FinishOutcome::Rejected
        }
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
    pub fn calibration_session(&self) -> Option<CalibrationSession> {
        match self.calibration_mode {
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
                    let mode = self.calibration_mode;
                    let cal_for_trail = if matches!(mode, CalibrationMode::Active(_)) {
                        Calibration::default()
                    } else {
                        self.calibration
                    };
                    for (idx, slot) in event.points.iter().enumerate() {
                        if let Some(f) = slot {
                            if let CalibrationMode::Active(ref mut s) = self.calibration_mode {
                                let raw_x = f.x;
                                s.observe(raw_x, f.y as i32);
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

/// Normalize a raw [`FingerData`] sample into canvas-local coordinates
/// using `cal`. X is mapped from the cyclic firmware value through
/// `[cal.x_min..cal.x_max]`; Y is mapped from the firmware byte through
/// `[cal.y_min..cal.y_max]`. Both axes are clamped to `[-0.1, 1.1]` so
/// edge taps remain visible without painting deep into the bezel.
///
/// Returns `None` only if the calibration is degenerate
/// (`x_min == x_max` or `y_min == y_max`); production code constructs
/// these from valid sessions or `Calibration::default()`, so the `None`
/// branch is just a hard safety guard against divide-by-zero.
pub fn normalize_finger(f: &FingerData, cal: &Calibration) -> Option<(f64, f64)> {
    let x_span = cal.x_max - cal.x_min;
    let y_span = cal.y_max - cal.y_min;
    if x_span == 0 || y_span == 0 {
        return None;
    }
    let raw_x = f.x;
    let nx = (raw_x - cal.x_min) as f64 / x_span as f64;
    let ny = (f.y as i32 - cal.y_min) as f64 / y_span as f64;
    Some((nx.clamp(-0.1, 1.1), ny.clamp(-0.1, 1.1)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decoder::FingerData;
    use crate::session::DeviceEvent;

    fn finger(x: i32, y: i16) -> FingerData {
        FingerData {
            x,
            y,
            pressure: 0x20,
            status: 0,
            aux: [0, 0],
            byte1_high: 0,
        }
    }

    fn touch_event(f: FingerData) -> TouchEvent {
        TouchEvent {
            finger_mask: 0,
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
    fn calibration_default_matches_pre_calibration_bounds() {
        let cal = Calibration::default();
        // Centre of nominal touchpad → centre of canvas.
        let (nx, ny) = normalize_finger(&finger(TOUCH_X_PERIOD / 2, 53), &cal).unwrap();
        assert!((nx - 0.5).abs() < 1e-9);
        assert!((ny - (53.0 / 106.0)).abs() < 1e-9);
    }

    #[test]
    fn normalize_finger_uses_calibration_extremes() {
        let cal = Calibration { x_min: 100, x_max: 1900, y_min: 10, y_max: 90 };
        let (lo_x, lo_y) = normalize_finger(&finger(100, 10), &cal).unwrap();
        let (hi_x, hi_y) = normalize_finger(&finger(1900, 90), &cal).unwrap();
        assert!((lo_x - 0.0).abs() < 1e-9, "lo_x={lo_x}");
        assert!((lo_y - 0.0).abs() < 1e-9, "lo_y={lo_y}");
        assert!((hi_x - 1.0).abs() < 1e-9, "hi_x={hi_x}");
        assert!((hi_y - 1.0).abs() < 1e-9, "hi_y={hi_y}");
    }

    #[test]
    fn normalize_finger_clamps_out_of_range_samples() {
        // Narrow bounds so a 0 sample falls well below -0.1.
        let cal = Calibration { x_min: 1000, x_max: 1100, y_min: 50, y_max: 60 };
        let (nx, ny) = normalize_finger(&finger(0, 0), &cal).unwrap();
        assert!((nx + 0.1).abs() < 1e-9, "nx={nx}");
        assert!((ny + 0.1).abs() < 1e-9, "ny={ny}");
        // Way above x_max / y_max — should clamp to 1.1.
        let (nx, ny) = normalize_finger(&finger(TOUCH_X_PERIOD - 1, 100), &cal).unwrap();
        assert!((nx - 1.1).abs() < 1e-9, "nx={nx}");
        assert!((ny - 1.1).abs() < 1e-9, "ny={ny}");
    }

    #[test]
    fn normalize_finger_does_not_wrap_at_period_boundary() {
        // f.x == TOUCH_X_PERIOD used to fold to the left edge via rem_euclid.
        let cal = Calibration::default();
        let (nx, _) = normalize_finger(&finger(TOUCH_X_PERIOD, 53), &cal).unwrap();
        assert!((nx - 1.0).abs() < 1e-9, "nx={nx}");
    }

    #[test]
    fn normalize_finger_returns_none_for_degenerate_bounds() {
        let cal = Calibration { x_min: 0, x_max: 0, y_min: 0, y_max: 106 };
        assert!(normalize_finger(&finger(0, 0), &cal).is_none());
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
    fn touch_during_calibration_grows_running_bounds() {
        let mut s = state();
        s.start_calibration();
        dispatch(&mut s, finger(200, 20));
        dispatch(&mut s, finger(1800, 90));
        dispatch(&mut s, finger(1000, 55));
        let session = s.calibration_session().expect("session active");
        assert_eq!(session.samples, 3);
        assert_eq!(session.x_min, 200);
        assert_eq!(session.x_max, 1800);
        assert_eq!(session.y_min, 20);
        assert_eq!(session.y_max, 90);
    }

    #[test]
    fn finish_calibration_commits_when_thresholds_met() {
        let mut s = state();
        s.start_calibration();
        // Spread along both axes, more than MIN_CALIBRATION_SAMPLES frames.
        for i in 0..MIN_CALIBRATION_SAMPLES {
            let frac = i as f64 / (MIN_CALIBRATION_SAMPLES - 1) as f64;
            let x = (200.0 + frac * 1700.0) as i32;
            let y = (20.0 + frac * 70.0) as i16;
            dispatch(&mut s, finger(x, y));
        }
        let outcome = s.finish_calibration();
        assert_eq!(outcome, FinishOutcome::Committed);
        assert!(!s.is_calibrating());
        assert_eq!(s.calibration.x_min, 200);
        assert_eq!(s.calibration.x_max, 1900);
        assert_eq!(s.calibration.y_min, 20);
        assert_eq!(s.calibration.y_max, 90);
    }

    #[test]
    fn finish_calibration_rejects_too_few_samples() {
        let mut s = state();
        s.start_calibration();
        // Wide span, only a handful of samples.
        for _ in 0..3 {
            dispatch(&mut s, finger(200, 20));
            dispatch(&mut s, finger(1800, 90));
        }
        let outcome = s.finish_calibration();
        assert_eq!(outcome, FinishOutcome::Rejected);
        // Previous (default) calibration retained.
        assert_eq!(s.calibration, Calibration::default());
    }

    #[test]
    fn finish_calibration_rejects_degenerate_span() {
        let mut s = state();
        s.start_calibration();
        // Plenty of samples, but barely any spread.
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
        let prior = Calibration { x_min: 50, x_max: 1950, y_min: 5, y_max: 100 };
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
        let prior = Calibration { x_min: 50, x_max: 1950, y_min: 5, y_max: 100 };
        let mut s = AppState::new(selection(), prior);
        s.clear_calibration();
        assert_eq!(s.calibration, Calibration::default());
        assert!(!s.is_calibrating());
    }
}
