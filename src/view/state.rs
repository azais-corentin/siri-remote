//! Live state mutated by [`crate::session::DeviceEvent`]s and rendered by
//! [`super::ui::draw`]. No IO; pure data + the `on_event` dispatcher.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::time::Duration;

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

    pub events: VecDeque<EventLine>,
}

impl AppState {
    pub fn new(selection: Selection) -> Self {
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
            events: VecDeque::with_capacity(EVENT_LOG_CAP),
        }
    }

    pub fn set_connection(&mut self, state: ConnState) {
        self.connection = state;
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
                    for (idx, slot) in event.points.iter().enumerate() {
                        if let Some(f) = slot
                            && let Some((nx, ny)) = normalize_finger(f)
                        {
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

/// Normalize a raw [`FingerData`] sample into canvas-local `0..=1`
/// coordinates. The firmware reports a cyclic X (the decoder unwraps it
/// into an extended monotonic value); for drawing we want the raw cyclic
/// position. Y is already in the firmware's `~0..=106` range.
///
/// Returns `None` only for impossibly-bad samples (`Y` outside the wire
/// range, which the decoder already filters). Out-of-disc corners are
/// kept so edge taps aren't dropped — clipping is left to the renderer.
pub fn normalize_finger(f: &FingerData) -> Option<(f64, f64)> {
    use crate::decoder::TOUCH_X_PERIOD;
    let raw_x = f.x.rem_euclid(TOUCH_X_PERIOD) as f64;
    let nx = raw_x / TOUCH_X_PERIOD as f64;
    // Y nominally lives in `0..=106`. Allow ±20 worth of overshoot so
    // corner taps don't clamp visibly.
    let ny = (f.y as f64 / 106.0).clamp(-0.2, 1.2);
    Some((nx, ny))
}
