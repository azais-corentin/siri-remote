//! Event loop for the `view` dashboard. Multiplexes the [`Session`] event
//! stream, the crossterm input stream, and a fixed redraw tick.

use std::time::Duration;

use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::event::EventStream;
use futures::StreamExt;
use ratatui::DefaultTerminal;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::{Instant, MissedTickBehavior, interval};

use crate::logger::LogRecord;
use crate::session::{DeviceEvent, Session};
use crate::view::calibration;
use crate::view::state::{AppState, FinishOutcome};
use crate::view::ui;

/// Outcome of one connected-session run. The outer reconnect loop in
/// [`super::run_forever`] uses this to decide whether to retry the link or
/// exit the process.
#[derive(Debug)]
pub enum AppOutcome {
    /// User asked to quit (`q`, `Esc` outside calibration, or `Ctrl-C`).
    Quit,
    /// Underlying session ended (`session.next_event()` returned `None`).
    Disconnected,
}

/// Classification of a crossterm key event in the context of `view`'s
/// current calibration state. `Esc` quits when idle but cancels the
/// in-progress session otherwise, so the classifier needs to know whether
/// we are calibrating to disambiguate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KeyAction {
    None,
    Quit,
    ToggleCalibration,
    CancelCalibration,
    ClearCalibration,
}

fn classify_key(event: &Event, calibrating: bool) -> KeyAction {
    let Event::Key(KeyEvent {
        code,
        modifiers,
        kind,
        ..
    }) = event
    else {
        return KeyAction::None;
    };
    // Crossterm reports both Press and Release on some terminals (kitty
    // keyboard protocol). Treat the press as the canonical event.
    if !matches!(kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return KeyAction::None;
    }

    // Ctrl-C always quits — regardless of calibration state.
    if *code == KeyCode::Char('c') && modifiers.contains(KeyModifiers::CONTROL) {
        return KeyAction::Quit;
    }
    match code {
        KeyCode::Char('q') | KeyCode::Char('Q') => KeyAction::Quit,
        KeyCode::Esc => {
            if calibrating {
                KeyAction::CancelCalibration
            } else {
                KeyAction::Quit
            }
        }
        KeyCode::Char('c') => KeyAction::ToggleCalibration,
        KeyCode::Char('C') => KeyAction::ClearCalibration,
        _ => KeyAction::None,
    }
}

/// Outcome of `handle_key`. The waiter variants reach back into
/// [`wait_with_ui`]'s caller-controlled future via [`WaitOutcome`].
enum KeyOutcome {
    Continue,
    Quit,
}

/// Apply a classified key action to the state, performing persistence
/// side-effects (save / clear the on-disk file) and logging.
fn handle_key(state: &mut AppState, action: KeyAction) -> KeyOutcome {
    match action {
        KeyAction::None => KeyOutcome::Continue,
        KeyAction::Quit => KeyOutcome::Quit,
        KeyAction::ToggleCalibration => {
            if state.is_calibrating() {
                match state.finish_calibration() {
                    FinishOutcome::Committed => {
                        let c = state.calibration;
                        log::info!(
                            "calibration saved: x_origin={} x_span={} y=[{}..{}]",
                            c.x_origin,
                            c.x_span,
                            c.y_min,
                            c.y_max,
                        );
                        if let Err(err) = calibration::save(&c) {
                            log::warn!("calibration save failed: {err:#}");
                        }
                    }
                    FinishOutcome::Rejected => {
                        log::warn!(
                            "calibration rejected: too few samples / degenerate bounds; keeping previous"
                        );
                    }
                }
            } else {
                state.start_calibration();
                log::info!(
                    "calibration started: trace a circle on the touchpad; press c to finish, Esc to cancel"
                );
            }
            KeyOutcome::Continue
        }
        KeyAction::CancelCalibration => {
            state.cancel_calibration();
            log::info!("calibration cancelled; previous calibration retained");
            KeyOutcome::Continue
        }
        KeyAction::ClearCalibration => {
            state.clear_calibration();
            if let Err(err) = calibration::clear() {
                log::warn!("calibration clear failed: {err:#}");
            } else {
                log::info!("calibration cleared");
            }
            KeyOutcome::Continue
        }
    }
}

/// Drive one [`Session`] to completion. The outer loop is responsible for
/// re-establishing the link if this returns [`AppOutcome::Disconnected`].
pub async fn run_session(
    terminal: &mut DefaultTerminal,
    state: &mut AppState,
    session: &mut Session,
    events: &mut EventStream,
    log_rx: &mut UnboundedReceiver<LogRecord>,
) -> Result<AppOutcome> {
    let mut ticker = interval(Duration::from_millis(16));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    // Skip the immediate tick — the caller already drew the frame once
    // when transitioning to Connected.
    ticker.tick().await;

    loop {
        tokio::select! {
            ev = session.next_event() => {
                match ev {
                    None => return Ok(AppOutcome::Disconnected),
                    Some(ev) => {
                        apply_event(state, ev);
                        redraw(terminal, state)?;
                    }
                }
            }
            ev = events.next() => {
                match ev {
                    Some(Ok(event)) => {
                        let action = classify_key(&event, state.is_calibrating());
                        match handle_key(state, action) {
                            KeyOutcome::Quit => return Ok(AppOutcome::Quit),
                            KeyOutcome::Continue => {
                                if !matches!(action, KeyAction::None) {
                                    redraw(terminal, state)?;
                                }
                            }
                        }
                    }
                    Some(Err(_)) => {}
                    None => return Ok(AppOutcome::Quit),
                }
            }
            _ = ticker.tick() => {
                state.tick_afterglow(Instant::now());
                redraw(terminal, state)?;
            }
            Some(record) = log_rx.recv() => {
                state.on_log(record);
                redraw(terminal, state)?;
            }
        }
    }
}

/// Drive the UI while a connect attempt is in flight. Returns either the
/// awaited result, or [`AppOutcome::Quit`] if the user pressed a quit key
/// before the future resolved.
pub async fn wait_with_ui<T>(
    terminal: &mut DefaultTerminal,
    state: &mut AppState,
    events: &mut EventStream,
    log_rx: &mut UnboundedReceiver<LogRecord>,
    fut: impl std::future::Future<Output = T>,
) -> WaitOutcome<T> {
    let mut ticker = interval(Duration::from_millis(50));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    ticker.tick().await;

    tokio::pin!(fut);
    loop {
        tokio::select! {
            v = &mut fut => return WaitOutcome::Ready(v),
            ev = events.next() => {
                match ev {
                    Some(Ok(event)) => {
                        let action = classify_key(&event, state.is_calibrating());
                        match handle_key(state, action) {
                            KeyOutcome::Quit => return WaitOutcome::Quit,
                            KeyOutcome::Continue => {
                                if !matches!(action, KeyAction::None)
                                    && redraw(terminal, state).is_err()
                                {
                                    return WaitOutcome::Quit;
                                }
                            }
                        }
                    }
                    Some(Err(_)) => {}
                    None => return WaitOutcome::Quit,
                }
            }
            _ = ticker.tick() => {
                state.tick_afterglow(Instant::now());
                if redraw(terminal, state).is_err() {
                    return WaitOutcome::Quit;
                }
            }
            Some(record) = log_rx.recv() => {
                state.on_log(record);
                if redraw(terminal, state).is_err() {
                    return WaitOutcome::Quit;
                }
            }
        }
    }
}

pub enum WaitOutcome<T> {
    Ready(T),
    Quit,
}

fn apply_event(state: &mut AppState, ev: DeviceEvent) {
    state.on_event(ev);
}

fn redraw(terminal: &mut DefaultTerminal, state: &AppState) -> Result<()> {
    terminal.draw(|frame| ui::draw(frame, state))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventState, KeyModifiers};

    fn press(code: KeyCode, mods: KeyModifiers) -> Event {
        Event::Key(KeyEvent {
            code,
            modifiers: mods,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        })
    }

    #[test]
    fn ctrl_c_quits_in_any_mode() {
        let ev = press(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(classify_key(&ev, false), KeyAction::Quit);
        assert_eq!(classify_key(&ev, true), KeyAction::Quit);
    }

    #[test]
    fn esc_quits_when_idle_and_cancels_during_calibration() {
        let ev = press(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(classify_key(&ev, false), KeyAction::Quit);
        assert_eq!(classify_key(&ev, true), KeyAction::CancelCalibration);
    }

    #[test]
    fn lowercase_c_toggles_calibration() {
        let ev = press(KeyCode::Char('c'), KeyModifiers::NONE);
        assert_eq!(classify_key(&ev, false), KeyAction::ToggleCalibration);
        assert_eq!(classify_key(&ev, true), KeyAction::ToggleCalibration);
    }

    #[test]
    fn shift_c_clears_calibration() {
        // Crossterm reports Shift+C as KeyCode::Char('C') with SHIFT.
        let ev = press(KeyCode::Char('C'), KeyModifiers::SHIFT);
        assert_eq!(classify_key(&ev, false), KeyAction::ClearCalibration);
    }

    #[test]
    fn q_quits() {
        let ev = press(KeyCode::Char('q'), KeyModifiers::NONE);
        assert_eq!(classify_key(&ev, false), KeyAction::Quit);
    }

    #[test]
    fn release_events_are_ignored() {
        let ev = Event::Key(KeyEvent {
            code: KeyCode::Char('q'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Release,
            state: KeyEventState::NONE,
        });
        assert_eq!(classify_key(&ev, false), KeyAction::None);
    }
}
