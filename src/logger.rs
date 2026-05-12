//! Routing layer over the [`log`] crate facade.
//!
//! Shared connection plumbing (`session::*`, `scan::*`, `bluez::*`) emits
//! through `log::info!` / `log::warn!`. Those calls reach the [`Router`]
//! installed at process startup, which forwards each record to one of two
//! places:
//!
//! - **No sink installed** (default — `events` path): write the message to
//!   stderr, mirroring the historical `eprintln!` behaviour.
//! - **Sink installed** (`view` installs one right after entering the alt
//!   screen): push a [`LogRecord`] onto the channel; the UI loop drains it
//!   into the events panel.
//!
//! ## Usage
//!
//! ```ignore
//! use log::{info, warn};
//! info!("connecting to {}", addr);
//! warn!("battery notifications were not enabled");
//! ```

use std::sync::{Mutex, OnceLock};

use log::{Level, LevelFilter, Log, Metadata, Record, SetLoggerError};
use tokio::sync::mpsc::UnboundedSender;

/// One log line routed through the facade. Carries the original [`Level`]
/// so the view UI can map it to a colour/tag arm.
#[derive(Clone, Debug)]
pub struct LogRecord {
    pub level: Level,
    pub message: String,
}

/// Global sink. `None` (or no entry at all) means "no consumer attached —
/// fall back to stderr". Wrapped in `Mutex` so install/clear are race-free
/// across the BLE task and the UI task.
static SINK: OnceLock<Mutex<Option<UnboundedSender<LogRecord>>>> = OnceLock::new();

fn slot() -> &'static Mutex<Option<UnboundedSender<LogRecord>>> {
    SINK.get_or_init(|| Mutex::new(None))
}

/// Install a sink. Subsequent log records are sent to `tx` instead of
/// landing on stderr. Replaces any previously installed sink.
pub fn set_sink(tx: UnboundedSender<LogRecord>) {
    let mut guard = slot().lock().expect("log sink mutex poisoned");
    *guard = Some(tx);
}

/// Remove the installed sink. Subsequent log records fall back to stderr.
/// Idempotent.
pub fn clear_sink() {
    let mut guard = slot().lock().expect("log sink mutex poisoned");
    *guard = None;
}

/// The `log::Log` implementation that fans out to either the channel or
/// stderr. Holds no state of its own — the sink lives in [`SINK`] so it
/// can be swapped at runtime by `view`.
struct Router;

impl Log for Router {
    fn enabled(&self, metadata: &Metadata) -> bool {
        // We accept everything down to Info. Trace/Debug are filtered out;
        // the binary has no callers at those levels.
        metadata.level() <= Level::Info
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let message = format!("{}", record.args());
        // Clone the sender out under the mutex and drop the guard before
        // sending, so a slow consumer never holds the global lock.
        let tx = {
            let guard = slot().lock().expect("log sink mutex poisoned");
            guard.clone()
        };
        if let Some(tx) = tx {
            let log_record = LogRecord {
                level: record.level(),
                message,
            };
            match tx.send(log_record) {
                Ok(()) => return,
                // Receiver dropped; fall through to stderr with the
                // original message. We do NOT clear the sink here — the
                // owner (TerminalGuard::drop) handles teardown.
                Err(send_err) => {
                    eprintln!("{}", send_err.0.message);
                    return;
                }
            }
        }
        eprintln!("{message}");
    }

    fn flush(&self) {}
}

static ROUTER: Router = Router;

/// Install the router as the process-global `log` implementation. Idempotent:
/// safe to call repeatedly (e.g. from each test) — subsequent calls after
/// the first succeed silently because the `log` crate refuses to replace an
/// already-installed logger.
pub fn init() {
    static INSTALLED: OnceLock<Result<(), SetLoggerError>> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        let r = log::set_logger(&ROUTER);
        // `set_max_level` is independent and idempotent; bumping it on
        // every call is harmless and lets tests reset it after fiddling.
        log::set_max_level(LevelFilter::Info);
        r
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::unbounded_channel;

    /// Serialise sink-touching tests behind a single mutex: `SINK` is
    /// process-global, so two `#[tokio::test]`s racing on it would flake.
    static TEST_GATE: Mutex<()> = Mutex::new(());

    fn make_record<'a>(level: Level, args: std::fmt::Arguments<'a>) -> Record<'a> {
        Record::builder()
            .level(level)
            .args(args)
            .target("test")
            .build()
    }

    #[tokio::test]
    async fn sink_receives_emitted_record() {
        let _gate = TEST_GATE.lock().unwrap_or_else(|e| e.into_inner());
        clear_sink();

        let (tx, mut rx) = unbounded_channel::<LogRecord>();
        set_sink(tx);

        Router.log(&make_record(Level::Info, format_args!("hello")));
        Router.log(&make_record(Level::Warn, format_args!("careful")));

        let r1 = rx.recv().await.expect("first record");
        assert_eq!(r1.level, Level::Info);
        assert_eq!(r1.message, "hello");
        let r2 = rx.recv().await.expect("second record");
        assert_eq!(r2.level, Level::Warn);
        assert_eq!(r2.message, "careful");

        clear_sink();
    }

    #[tokio::test]
    async fn cleared_sink_falls_through() {
        let _gate = TEST_GATE.lock().unwrap_or_else(|e| e.into_inner());
        clear_sink();

        let (tx, mut rx) = unbounded_channel::<LogRecord>();
        set_sink(tx);
        clear_sink();

        // After clear_sink the channel is silent. We cannot assert that
        // stderr actually received the line (no capture in unit tests) but
        // we can assert that no record landed on the channel.
        Router.log(&make_record(Level::Info, format_args!("after-clear")));
        assert!(
            rx.try_recv().is_err(),
            "channel should be empty after clear_sink"
        );
    }

    #[tokio::test]
    async fn router_drops_below_info() {
        let _gate = TEST_GATE.lock().unwrap_or_else(|e| e.into_inner());
        clear_sink();

        let (tx, mut rx) = unbounded_channel::<LogRecord>();
        set_sink(tx);

        // Trace/Debug are below our Info threshold — Router::enabled()
        // returns false and the record is dropped on the floor.
        Router.log(&make_record(Level::Debug, format_args!("debug")));
        Router.log(&make_record(Level::Trace, format_args!("trace")));

        assert!(rx.try_recv().is_err(), "debug/trace must be filtered");

        clear_sink();
    }
}
