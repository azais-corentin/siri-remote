//! `siri-remote view` — ratatui dashboard that renders a live silhouette of
//! the Apple TV Siri Remote alongside status / touch / event readouts.

#![cfg(target_os = "linux")]

use std::time::Duration;

use anyhow::Result;
use btleplug::platform::Adapter;
use crossterm::event::EventStream;
use ratatui::DefaultTerminal;
use tokio::sync::mpsc::{self, UnboundedReceiver};
use tokio::time::Instant;

#[cfg(target_os = "linux")]
use crate::bluez;
use crate::cli::ViewArgs;
use crate::logger::{self, LogRecord};
use crate::session::{self, InitError, Selection, Session};

pub mod app;
pub mod state;
pub mod ui;

use crate::view::app::{AppOutcome, WaitOutcome, run_session, wait_with_ui};
use crate::view::state::{AppState, ConnState, EventSource};

pub async fn run(args: ViewArgs) -> Result<u8> {
    if args.scan_seconds < 0.0 {
        anyhow::bail!("--scan-seconds must be non-negative");
    }
    if args.reconnect_delay < 0.0 {
        anyhow::bail!("--reconnect-delay must be non-negative");
    }

    let adapter = session::make_adapter().await?;
    let selection = match session::choose_initial_selection(
        &adapter,
        args.address.as_deref(),
        Duration::from_secs_f64(args.scan_seconds),
    )
    .await
    {
        Ok(s) => s,
        Err(InitError::Invalid(msg)) => {
            eprintln!("{msg}");
            return Ok(2);
        }
        Err(InitError::Timeout) => {
            eprintln!(
                "Timed out waiting for a Siri Remote. If it is unpaired, hold MENU + \
                 Volume Up for pairing mode and keep it close to this host."
            );
            return Ok(1);
        }
    };

    // Enter alt screen + raw mode. The guard's Drop unwinds even on panic
    // / Ctrl-C cancellation. Ratatui's `init` also installs its own panic
    // hook that calls `restore`, which complements (does not replace) the
    // guard.
    // Install the log sink BEFORE entering the alt screen would also work,
    // but the guard's Drop is the right teardown hook for `clear_sink` so
    // we install in the same step as `enter()`.
    let (log_tx, mut log_rx) = mpsc::unbounded_channel::<LogRecord>();
    let mut guard = TerminalGuard::enter()?;
    logger::set_sink(log_tx);
    let initial_calibration = crate::calibration::load().unwrap_or_default();
    run_forever(
        &adapter,
        selection,
        Duration::from_secs_f64(args.reconnect_delay),
        &mut guard.terminal,
        &mut log_rx,
        initial_calibration,
    )
    .await
}

async fn run_forever(
    adapter: &Adapter,
    mut selection: Selection,
    reconnect_delay: Duration,
    terminal: &mut DefaultTerminal,
    log_rx: &mut UnboundedReceiver<LogRecord>,
    initial_calibration: crate::calibration::Calibration,
) -> Result<u8> {
    let mut state = AppState::new(selection.clone(), initial_calibration);
    let mut events = EventStream::new();

    loop {
        state.set_connection(ConnState::Connecting {
            since: Instant::now(),
        });
        state.push_log(
            EventSource::System,
            format!("connecting to {}", selection.address),
        );
        // First paint of the connecting banner.
        terminal.draw(|f| ui::draw(f, &state))?;

        // 1. Connect / re-pair.
        let connect_fut = session::connect_once(adapter, &selection);
        let connect_result =
            match wait_with_ui(terminal, &mut state, &mut events, log_rx, connect_fut).await {
                WaitOutcome::Ready(r) => r,
                WaitOutcome::Quit => return Ok(0),
            };

        match connect_result {
            Ok((peripheral, new_sel)) => {
                selection = new_sel;
                state.selection = selection.clone();

                // 2. Open session (configure notifications).
                let open_fut = Session::open(peripheral, &selection);
                let open_result =
                    match wait_with_ui(terminal, &mut state, &mut events, log_rx, open_fut).await {
                        WaitOutcome::Ready(r) => r,
                        WaitOutcome::Quit => return Ok(0),
                    };
                match open_result {
                    Ok(mut session) => {
                        state.set_connection(ConnState::Connected {
                            since: Instant::now(),
                        });
                        state.push_log(EventSource::System, "session opened".to_string());
                        terminal.draw(|f| ui::draw(f, &state))?;
                        match run_session(terminal, &mut state, &mut session, &mut events, log_rx).await? {
                            AppOutcome::Quit => return Ok(0),
                            AppOutcome::Disconnected => {
                                state.push_log(
                                    EventSource::System,
                                    "link dropped; reconnecting".to_string(),
                                );
                                state.set_connection(ConnState::Reconnecting {
                                    reason: "link dropped".to_string(),
                                    since: Instant::now(),
                                });
                            }
                        }
                    }
                    Err(err) => {
                        if handle_hid_denied(&mut state, &err) {
                            return Ok(1);
                        }
                        state.push_log(
                            EventSource::System,
                            format!("session open failed: {err}"),
                        );
                        state.set_connection(ConnState::Reconnecting {
                            reason: format!("{err}"),
                            since: Instant::now(),
                        });
                    }
                }
            }
            Err(err) => {
                if handle_hid_denied(&mut state, &err) {
                    return Ok(1);
                }
                if !selection.requires_pairing && session::is_auth_failure(&err) {
                    state.set_connection(ConnState::Pairing {
                        since: Instant::now(),
                    });
                    state.push_log(
                        EventSource::System,
                        "auth failure; scanning for pairing-mode remote".to_string(),
                    );
                    terminal.draw(|f| ui::draw(f, &state))?;
                    let scan_fut = session::switch_to_pairing_scan(adapter);
                    match wait_with_ui(terminal, &mut state, &mut events, log_rx, scan_fut).await {
                        WaitOutcome::Ready(Ok(s)) => {
                            selection = s;
                            state.selection = selection.clone();
                            continue; // bypass reconnect_delay; pair attempt is hot
                        }
                        WaitOutcome::Ready(Err(_)) => {
                            state.push_log(
                                EventSource::System,
                                "pairing-mode scan timed out".to_string(),
                            );
                        }
                        WaitOutcome::Quit => return Ok(0),
                    }
                } else {
                    state.push_log(EventSource::System, format!("connect failed: {err}"));
                }
                state.set_connection(ConnState::Reconnecting {
                    reason: format!("{err}"),
                    since: Instant::now(),
                });
            }
        }

        // Reconnect delay — keep the UI responsive while we wait.
        terminal.draw(|f| ui::draw(f, &state))?;
        let sleep_fut = tokio::time::sleep(reconnect_delay);
        if let WaitOutcome::Quit =
            wait_with_ui(terminal, &mut state, &mut events, log_rx, sleep_fut).await
        {
            return Ok(0);
        }
    }
}

#[cfg(target_os = "linux")]
fn handle_hid_denied(state: &mut AppState, err: &anyhow::Error) -> bool {
    if let Some(denied) = err.downcast_ref::<bluez::hid::HidInputEnableDenied>() {
        state.push_log(
            EventSource::System,
            format!(
                "BlueZ refused HID input streaming on {}. Start bluetoothd with --noplugin=input,hog.",
                denied.device_path
            ),
        );
        state.set_connection(ConnState::Reconnecting {
            reason: "hog plugin owns HID service".to_string(),
            since: Instant::now(),
        });
        return true;
    }
    false
}

// --- Terminal guard ---------------------------------------------------------

/// RAII guard that enters the alt screen + raw mode on construction and
/// restores the terminal on drop. Combined with ratatui's own panic hook,
/// this ensures clean teardown on any exit path (normal return, panic,
/// Ctrl-C in `main`'s outer `tokio::select!`, etc.).
struct TerminalGuard {
    terminal: DefaultTerminal,
}

impl TerminalGuard {
    fn enter() -> Result<Self> {
        // `ratatui::init()` installs a panic hook AND switches the
        // terminal mode. The hook calls `ratatui::restore()` from the
        // unwinding path, which is the right thing on panic. For non-
        // panic exits (Ctrl-C, AppOutcome::Quit) we rely on `Drop`.
        let terminal = ratatui::init();
        Ok(Self { terminal })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // Clear the log sink BEFORE restoring the terminal: any further
        // facade calls (including from worker tasks racing teardown) must
        // fall through to stderr, not push onto a soon-to-be-dropped
        // receiver. Idempotent if no sink was installed.
        logger::clear_sink();
        ratatui::restore();
    }
}
