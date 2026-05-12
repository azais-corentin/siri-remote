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

/// Touchpad-to-canvas mapping.
///
/// X is **cyclic**: the firmware's encoded X is an 11-bit value in
/// `0..TOUCH_X_PERIOD` whose wrap point falls inside the physical pad.
/// We model the pad as an arc on that cycle: `x_origin` is the encoded
/// X at the **left edge**, `x_span` is the cyclic distance clockwise to
/// the right edge (always `< TOUCH_X_PERIOD`).
///
/// Y is a linear `[y_min, y_max]` window over the firmware byte
/// (nominally `0..=106`).
///
/// Defaults are calibrated against the gen-3 DNDJ22MG2330 captures in
/// the repository; per-device variance is small enough that the
/// out-of-the-box mapping is usable without calibration, but a saved
/// calibration tightens both axes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Calibration {
    pub x_origin: i32,
    pub x_span: i32,
    pub y_min: i32,
    pub y_max: i32,
}

impl Default for Calibration {
    fn default() -> Self {
        Self {
            // Empirically derived from the swipe captures: left edge
            // ≈ encoded 1040, right edge ≈ encoded 300, so the pad
            // covers a 1300-unit arc starting at 1040.
            x_origin: 1040,
            x_span: 1300,
            y_min: 0,
            y_max: 106,
        }
    }
}

/// Running bounds for an in-progress calibration session.
///
/// X collection keeps every distinct encoded value observed; on
/// [`Self::derive_x_arc`] we identify the **longest unsampled arc** on
/// the cycle as the "off-pad" region, and report the complementary arc
/// (its end as `x_origin`, the period minus the gap as `x_span`).
#[derive(Clone, Debug)]
pub struct CalibrationSession {
    pub x_samples: Vec<i32>,
    pub y_min: i32,
    pub y_max: i32,
    pub samples: usize,
}

impl CalibrationSession {
    fn new() -> Self {
        Self {
            x_samples: Vec::new(),
            y_min: i32::MAX,
            y_max: i32::MIN,
            samples: 0,
        }
    }

    /// Fold one finger sample into the running bounds. `raw_x` is the
    /// firmware cyclic value, `raw_y` the firmware byte.
    pub fn observe(&mut self, raw_x: i32, raw_y: i32) {
        self.x_samples.push(raw_x);
        if raw_y < self.y_min {
            self.y_min = raw_y;
        }
        if raw_y > self.y_max {
            self.y_max = raw_y;
        }
        self.samples += 1;
    }

    /// Infer `(x_origin, x_span)` from the collected X samples. Finds
    /// the largest cyclic gap between consecutive distinct samples;
    /// the sample immediately after that gap is the left edge, and
    /// the gap's complement is the pad span. Returns `None` when no
    /// samples have been recorded yet.
    pub fn derive_x_arc(&self) -> Option<(i32, i32)> {
        if self.x_samples.is_empty() {
            return None;
        }
        let mut sorted: Vec<i32> = self.x_samples.clone();
        sorted.sort_unstable();
        sorted.dedup();
        if sorted.len() == 1 {
            return Some((sorted[0], 0));
        }
        let n = sorted.len();
        let mut max_gap = 0;
        let mut gap_end = 0usize;
        for i in 0..n {
            let cur = sorted[i];
            let next = sorted[(i + 1) % n];
            let gap = (next - cur).rem_euclid(TOUCH_X_PERIOD);
            if gap > max_gap {
                max_gap = gap;
                gap_end = (i + 1) % n;
            }
        }
        Some((sorted[gap_end], TOUCH_X_PERIOD - max_gap))
    }

    /// `true` iff the running bounds clear both the sample and span
    /// thresholds. Used by [`AppState::finish_calibration`] to decide
    /// commit vs. reject.
    pub fn is_acceptable(&self) -> bool {
        let x_span = self.derive_x_arc().map(|(_, s)| s).unwrap_or(0);
        self.samples >= MIN_CALIBRATION_SAMPLES
            && x_span >= MIN_CALIBRATION_X_SPAN
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
        let (x_origin, x_span) = session
            .derive_x_arc()
            .expect("acceptable session must have ≥1 sample");
        self.calibration = Calibration {
            x_origin,
            x_span,
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
                                s.observe(f.x, f.y as i32);
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
/// X uses the signed shortest cyclic delta from the pad's centre
/// (`cal.x_origin + cal.x_span/2`), divided by the span and shifted to
/// `[0, 1]`. Off-pad positions on either side of the wrap produce
/// values outside `[0, 1]`, which are clamped to `[-0.1, 1.1]` so the
/// renderer still draws something at edge taps without painting
/// arbitrarily far into the bezel.
///
/// Y is the linear `[y_min, y_max]` mapping over the firmware byte.
///
/// Returns `None` only if calibration is degenerate (`x_span <= 0` or
/// `y_min == y_max`).
pub fn normalize_finger(f: &FingerData, cal: &Calibration) -> Option<(f64, f64)> {
    let y_span = cal.y_max - cal.y_min;
    if cal.x_span <= 0 || y_span == 0 {
        return None;
    }
    let half_period = TOUCH_X_PERIOD / 2;
    let centre = cal.x_origin + cal.x_span / 2;
    // Signed shortest cyclic delta from the pad's centre, in
    // `(-TOUCH_X_PERIOD/2, TOUCH_X_PERIOD/2]`. The `+ half_period`
    // before `rem_euclid` and the `- half_period` after fold the
    // wrap into a signed quantity centred on 0.
    let d = (f.x - centre + half_period).rem_euclid(TOUCH_X_PERIOD) - half_period;
    let nx = d as f64 / cal.x_span as f64 + 0.5;
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
    fn calibration_default_maps_pad_centre_to_canvas_centre() {
        let cal = Calibration::default();
        // Encoded X 1690 is exactly the centre of the default pad arc
        // (origin 1040 + span/2 = 650).
        let (nx, ny) = normalize_finger(&finger(1690, 53), &cal).unwrap();
        assert!((nx - 0.5).abs() < 1e-9, "nx={nx}");
        assert!((ny - (53.0 / 106.0)).abs() < 1e-9, "ny={ny}");
    }

    #[test]
    fn calibration_default_maps_pad_edges_to_canvas_edges() {
        let cal = Calibration::default();
        // Left edge (encoded 1040): nx ≈ 0.
        let (nx_l, _) = normalize_finger(&finger(1040, 53), &cal).unwrap();
        assert!(nx_l.abs() < 1e-9, "nx_l={nx_l}");
        // Right edge (encoded 1040 + 1300 = 2340 mod 2040 = 300): nx ≈ 1.
        let (nx_r, _) = normalize_finger(&finger(300, 53), &cal).unwrap();
        assert!((nx_r - 1.0).abs() < 1e-9, "nx_r={nx_r}");
    }

    #[test]
    fn normalize_finger_is_history_independent_across_the_wrap() {
        // Centre, slightly-left-of-centre, slightly-right-of-centre all
        // produce stable nx regardless of where the user touched down.
        let cal = Calibration::default();
        let centre_a = normalize_finger(&finger(1690, 53), &cal).unwrap().0;
        let centre_b = normalize_finger(&finger(1690, 53), &cal).unwrap().0;
        assert!((centre_a - centre_b).abs() < 1e-9);

        // Encoded values straddling the 2040↔0 wrap map continuously.
        let near_wrap_pre = normalize_finger(&finger(2039, 53), &cal).unwrap().0;
        let near_wrap_post = normalize_finger(&finger(0, 53), &cal).unwrap().0;
        assert!(
            (near_wrap_pre - near_wrap_post).abs() < 2.0 / cal.x_span as f64,
            "expected continuity across the wrap, got {near_wrap_pre} vs {near_wrap_post}",
        );
    }

    #[test]
    fn normalize_finger_uses_custom_origin_and_span() {
        // Custom pad covering encoded [200..1800]: centre at 1000.
        let cal = Calibration { x_origin: 200, x_span: 1600, y_min: 10, y_max: 90 };
        let (left, _) = normalize_finger(&finger(200, 10), &cal).unwrap();
        let (centre, _) = normalize_finger(&finger(1000, 50), &cal).unwrap();
        let (right, _) = normalize_finger(&finger(1800, 90), &cal).unwrap();
        assert!(left.abs() < 1e-9, "left={left}");
        assert!((centre - 0.5).abs() < 1e-9, "centre={centre}");
        assert!((right - 1.0).abs() < 1e-9, "right={right}");
    }

    #[test]
    fn normalize_finger_clamps_off_pad_samples() {
        // Narrow pad arc; samples in the off-pad region get clamped.
        let cal = Calibration { x_origin: 1000, x_span: 100, y_min: 50, y_max: 60 };
        // Encoded X = 1050 sits at the pad centre → nx = 0.5.
        let (nx, ny) = normalize_finger(&finger(1050, 55), &cal).unwrap();
        assert!((nx - 0.5).abs() < 1e-9, "nx={nx}");
        assert!((ny - 0.5).abs() < 1e-9, "ny={ny}");
        // Encoded X = 0 is far off-pad → clamps. With centre=1050 and
        // period 2040, the shortest cyclic delta from 1050 to 0 is 990
        // units *clockwise* (forward), so 0 sits on the "past the right
        // edge" side and nx clamps to 1.1.
        let (nx, _) = normalize_finger(&finger(0, 55), &cal).unwrap();
        assert!((nx - 1.1).abs() < 1e-9, "nx={nx}");
        // Encoded X = 200 is 850 units counter-clockwise from centre,
        // i.e. past the left edge → nx clamps to -0.1.
        let (nx, _) = normalize_finger(&finger(200, 55), &cal).unwrap();
        assert!((nx + 0.1).abs() < 1e-9, "nx={nx}");
    }

    #[test]
    fn normalize_finger_returns_none_for_degenerate_bounds() {
        let cal = Calibration { x_origin: 0, x_span: 0, y_min: 0, y_max: 106 };
        assert!(normalize_finger(&finger(0, 0), &cal).is_none());
    }

    #[test]
    fn start_calibration_clears_trail_and_marks_active() {
        let mut s = state();
        // Seed a trail point.
        dispatch(&mut s, finger(1500, 50));
        assert!(!s.touch_trail.is_empty());
        s.start_calibration();
        assert!(s.touch_trail.is_empty());
        assert!(s.is_calibrating());
    }

    #[test]
    fn touch_during_calibration_records_samples() {
        let mut s = state();
        s.start_calibration();
        dispatch(&mut s, finger(200, 20));
        dispatch(&mut s, finger(1800, 90));
        dispatch(&mut s, finger(1000, 55));
        let session = s.calibration_session().expect("session active");
        assert_eq!(session.samples, 3);
        assert_eq!(session.x_samples, vec![200, 1800, 1000]);
        assert_eq!(session.y_min, 20);
        assert_eq!(session.y_max, 90);
    }

    #[test]
    fn derive_x_arc_finds_longest_unsampled_gap() {
        let mut s = CalibrationSession::new();
        // Samples concentrated in the encoded range [1000..1500], leaving
        // [1500..1000+TOUCH_X_PERIOD] = 1540 units unsampled.
        for x in [1000, 1100, 1200, 1300, 1400, 1500] {
            s.observe(x, 50);
        }
        let (origin, span) = s.derive_x_arc().expect("samples present");
        assert_eq!(origin, 1000, "origin = sample immediately after the gap");
        assert_eq!(span, 1500 - 1000, "span = period − longest gap");
    }

    #[test]
    fn derive_x_arc_handles_samples_straddling_the_wrap() {
        let mut s = CalibrationSession::new();
        // Samples near the wrap on both sides: 1900, 2000, 100, 200.
        // Longest gap is between 200 and 1900 = 1700.
        for x in [1900, 2000, 100, 200] {
            s.observe(x, 50);
        }
        let (origin, span) = s.derive_x_arc().expect("samples present");
        assert_eq!(origin, 1900, "origin sits right after the longest gap");
        assert_eq!(span, TOUCH_X_PERIOD - 1700);
    }

    #[test]
    fn finish_calibration_commits_when_thresholds_met() {
        let mut s = state();
        s.start_calibration();
        // Spread along both axes, more than MIN_CALIBRATION_SAMPLES frames.
        // Samples sweep the encoded range [200..1900], so the longest gap
        // is between 1900 and 200 (cyclic) = 2040 − 1700 = 340 units, and
        // the inferred pad span is 1700.
        for i in 0..MIN_CALIBRATION_SAMPLES {
            let frac = i as f64 / (MIN_CALIBRATION_SAMPLES - 1) as f64;
            let x = (200.0 + frac * 1700.0) as i32;
            let y = (20.0 + frac * 70.0) as i16;
            dispatch(&mut s, finger(x, y));
        }
        let outcome = s.finish_calibration();
        assert_eq!(outcome, FinishOutcome::Committed);
        assert!(!s.is_calibrating());
        assert_eq!(s.calibration.x_origin, 200);
        assert_eq!(s.calibration.x_span, 1700);
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
        let prior = Calibration { x_origin: 50, x_span: 1900, y_min: 5, y_max: 100 };
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
        let prior = Calibration { x_origin: 50, x_span: 1900, y_min: 5, y_max: 100 };
        let mut s = AppState::new(selection(), prior);
        s.clear_calibration();
        assert_eq!(s.calibration, Calibration::default());
        assert!(!s.is_calibrating());
    }
}
