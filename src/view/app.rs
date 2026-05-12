//! Event loop for the `view` dashboard. Multiplexes the [`Session`] event
//! stream, the crossterm input stream, and a fixed redraw tick.

use std::time::Duration;

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers};
use futures::StreamExt;
use ratatui::DefaultTerminal;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::{Instant, MissedTickBehavior, interval};

use crate::logger::LogRecord;
use crate::session::{DeviceEvent, Session};
use crate::view::state::AppState;
use crate::view::ui;

/// Outcome of one connected-session run. The outer reconnect loop in
/// [`super::run_forever`] uses this to decide whether to retry the link or
/// exit the process.
#[derive(Debug)]
pub enum AppOutcome {
    /// User asked to quit (`q`, `Esc`, or `Ctrl-C`).
    Quit,
    /// Underlying session ended (`session.next_event()` returned `None`).
    Disconnected,
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
                        if should_quit(&event) {
                            return Ok(AppOutcome::Quit);
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
                        if should_quit(&event) {
                            return WaitOutcome::Quit;
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

fn should_quit(event: &Event) -> bool {
    if let Event::Key(KeyEvent {
        code, modifiers, ..
    }) = event
    {
        if matches!(code, KeyCode::Char('q') | KeyCode::Esc) {
            return true;
        }
        if *code == KeyCode::Char('c') && modifiers.contains(KeyModifiers::CONTROL) {
            return true;
        }
    }
    false
}
