# Repository Guidelines

## Project Overview

`siri-remote` is a single Rust binary crate that pairs with, streams from, and
diagnostics an Apple TV Siri Remote (3rd gen) over Bluetooth Low Energy. It
parses the remote's button, touchpad, battery and power notifications, and on
Linux can expose its push-to-talk Opus microphone stream as a PipeWire virtual
`Audio/Source`. The non-Linux build is limited; BlueZ (D-Bus) and PipeWire
integrations are gated by `#[cfg(target_os = "linux")]`.

## Architecture & Data Flow

Pipeline (BLE → consumers):

1. **Selection** — `session::choose_initial_selection` either resolves a
   bonded `--address` or delegates to `scan::scan_for_remote` /
   `scan_for_nearest_remote`, which filter advertisements by HID service +
   Apple manufacturer prefix and rank candidates by mean RSSI.
2. **Connect** — `session::connect_once` resolves the peripheral and connects;
   `connect_with_pairing` and `switch_to_pairing_scan` handle auth failures by
   rescanning for a pairing-mode remote and registering a BlueZ pairing agent
   (`bluez::agent::AgentSession`, `NoInputNoOutput`).
3. **Session** — `Session::open` configures btleplug notifications for the
   battery + power characteristics and opens a parallel BlueZ GATT side-channel
   (`bluez::hid::HidSession`) that writes the input-enable byte to writable
   non-Input HID reports and subscribes to `PropertiesChanged` on Input
   reports. `Session::next_event` merges both sources via `tokio::select!` and
   yields typed `DeviceEvent` variants (battery, power, button mask, touch
   frame, raw `UnknownInput`). It returns `None` on disconnect.
4. **Consumers** — one per subcommand:
   - `events` — formats `DeviceEvent` to stdout, reconnects forever.
   - `dump` — filters HID input report IDs `0xFC` (touch) and `0xFA` (mic) to
     timestamped raw-hex lines. Requires `--touch` and/or `--mic`.
   - `view` — ratatui dashboard. `view::app::run_session` multiplexes session
     events, crossterm input, 16 ms redraw tick, and the log channel; renders
     via `view/ui.rs`; persists touchpad calibration to
     `$XDG_CONFIG_HOME/siri-remote/calibration.toml` (fallback
     `$HOME/.config/...`).
   - `mic` — feeds `0xFA` payloads into `audio::MicDecoder` (Opus 48 kHz mono,
     PLC for sequence-number gaps), writing PCM samples into a bounded
     `Ring = Arc<Mutex<VecDeque<i16>>>`. `audio::PipeWireWorker::spawn` runs a
     dedicated OS thread that owns the `pw::stream::Stream`, configured at
     48 kHz mono S16LE, and drains the ring in its `process` callback,
     zero-filling on underrun.

Reconnection: every long-running consumer wraps its loop with
`session::connect_once` + a `--reconnect-delay`; BlueZ HID denial surfaces as
`HidInputEnableDenied` and is treated as a fatal user-actionable error
(disable the BlueZ `hog`/`input` plugins, see _Runtime_).

Concurrency: a single multi-threaded Tokio runtime drives all BLE/control
paths and the TUI. The only OS thread is the PipeWire worker. Cross-thread
communication: `std::sync::mpsc::sync_channel` for the PipeWire init
handshake, `pw::channel` for its quit signal, and `tokio::sync::mpsc`
unbounded channels for `logger → view`. No `unsafe` blocks in-tree (FFI is
inside the `pipewire`, `libspa`, `zbus` crates).

## Key Directories

- `src/` — single crate root. Modules are flat; one subcommand per file.
- `src/audio/` — `mod.rs` exposes `Ring`/`SAMPLE_RATE`/`FRAME_SAMPLES`,
  `decoder.rs` parses `0xFA` payloads and runs libopus, `pipewire.rs` owns the
  PipeWire worker thread.
- `src/view/` — `mod.rs` (Linux-only TUI entry + reconnect loop),
  `app.rs` (event/input multiplexing), `state.rs` (pure `AppState`,
  `ConnState`, `Calibration*`), `ui.rs` (ratatui render), `calibration.rs`
  (TOML persistence).
- `src/bluez/` — Linux-only D-Bus shims: `agent.rs` (`org.bluez.Agent1` +
  `AgentManager1`), `device.rs` (`Adapter1.RemoveDevice`, `Device1.Pair`,
  `ObjectManager.GetManagedObjects`, properties), `hid.rs`
  (`GattService1`/`GattCharacteristic1`/`GattDescriptor1` + HID report
  discovery and input-enable workaround).
- `flake.nix`, `.envrc` — Nix dev shell, `direnv use flake`.
- `Cargo.lock` is tracked.

There is no `tests/`, `benches/`, `examples/`, `scripts/`, `docs/`,
`README.md`, `LICENSE`, CI config, or Python content.

## Development Commands

Enter the dev shell first (everything below assumes it):

```sh
direnv allow            # one-time; otherwise: nix develop
cargo build             # debug
cargo build --release   # release uses lto = "thin"
cargo run -- view       # or: pair | events | unpair | dump --mic | mic
cargo test              # all unit tests live in #[cfg(test)] mods
cargo test <pattern>    # e.g. cargo test touch_parse
cargo fmt
cargo clippy --all-targets -- -D warnings
```

Common invocations:

```sh
cargo run -- pair --scan-seconds 5 --hold-seconds 5
cargo run -- events --address AA:BB:CC:DD:EE:FF
cargo run -- dump --touch --mic
cargo run -- mic --node-name siri-remote --node-description "Siri Remote microphone"
cargo run -- view
cargo run -- unpair --dry-run
```

Exit codes: `0` success, `1` user-actionable failure (timeout, HID denied),
`2` invalid input or fatal error (printed via `{err:?}`), `130` Ctrl-C
(handled in `main`'s outer `tokio::select!`).

## Code Conventions & Common Patterns

- **Edition:** 2024. No `rustfmt.toml`/`clippy.toml` — defaults apply.
- **Module docs:** every module opens with a `//!` summary stating its role
  and (where relevant) the wire-level invariants it enforces. Follow this
  pattern when adding modules.
- **Naming:** modules `snake_case`, types `CamelCase`. Subcommand entry
  points are `pub async fn run(args: <Cmd>Args) -> anyhow::Result<u8>` so
  `main.rs` can match uniformly.
- **Errors:** `anyhow::Result` at command/session boundaries. Typed errors
  only when callers need to branch — see `session::InitError`
  (`Invalid`/`Timeout`), `bluez::hid::HidInputEnableDenied` (downcasted with
  `err.downcast_ref::<…>()`), `scan::ScanError`. Use `anyhow::bail!` for
  CLI validation, return `Ok(1|2)` for user-actionable exits.
- **Platform gating:** anything touching BlueZ/D-Bus is
  `#[cfg(target_os = "linux")]` at the module **and** call-site level
  (`src/main.rs`, `src/view/mod.rs`). Match this when adding D-Bus or
  PipeWire code.
- **Async:** Tokio multi-thread runtime (`#[tokio::main(flavor = "multi_thread")]`).
  Loops use `tokio::select!` for cancellation; never block the runtime on
  PipeWire or D-Bus calls. The PipeWire main loop runs on a dedicated
  `std::thread::Builder::spawn`.
- **Logging:** use `log::{info,warn,error}`; `logger::init()` installs a
  global `Router` that filters at `Info`. The `view` command installs an
  mpsc sink via `logger::set_sink` so log records render inside the TUI; the
  `TerminalGuard::Drop` clears it before restoring the terminal.
- **HID constants:** report IDs are referenced by their hex literals at use
  sites — `0xFA` (microphone, Opus payload), `0xFC` (touchpad frame, marker
  byte `0x32`), input-enable byte `0xAF`. Touch payload sizes 11 / 18 bytes.
  Opus frames are Mono/48 kHz, 20 ms (`FRAME_SAMPLES = 960`,
  `MIC_REPORT_LEN = 99`, `RING_CAPACITY_SAMPLES = 12_000`).
- **State machines:** keep them pure. `view::state::AppState::on_event` is the
  pattern — applies `DeviceEvent` deterministically; rendering reads, never
  mutates. Calibration follows the same shape via
  `start/finish/cancel/clear_calibration`.
- **No dependency injection framework** — wire types explicitly through
  function signatures (see how `Adapter`, `Selection`, `Session`,
  `AppState`, `terminal`, channels are threaded through `view::run_forever`
  → `run_session`).

## Important Files

- `src/main.rs` — entry point; subcommand dispatch + Ctrl-C handling.
- `src/cli.rs` — single source of truth for CLI surface and defaults
  (`--scan-seconds 5.0`, `--reconnect-delay 0.5`, mic node defaults).
- `src/session.rs` — `Session`, `DeviceEvent`, `PowerState`,
  `connect_once`/`connect_with_pairing`, the merged BLE/HID event stream.
  Most non-trivial behavior changes touch this file.
- `src/decoder.rs` — pure decoders for buttons, touch, battery, power; large
  test suite. Add/adjust report parsing here.
- `src/scan.rs` — advertisement filtering and ranking.
- `src/audio/decoder.rs`, `src/audio/pipewire.rs` — mic pipeline.
- `src/view/state.rs`, `src/view/ui.rs`, `src/view/app.rs` — TUI.
- `src/bluez/{agent,device,hid}.rs` — Linux D-Bus surface.
- `Cargo.toml` — `lto = "thin"` release, `opt-level = 0` dev, edition 2024;
  no `[features]` table.
- `flake.nix` — pins toolchain + system deps (dbus, libopus, pipewire, clang,
  rustfmt, clippy). `LIBCLANG_PATH` is set for `bindgen` consumers.

## Runtime / Tooling Preferences

- **Toolchain:** `rustc`/`cargo`/`clippy`/`rustfmt` from the Nix flake. Do
  not rely on a system toolchain. There is no `rust-toolchain.toml`.
- **Package manager:** Cargo. `Cargo.lock` is committed; reproducible
  builds expected.
- **System libraries (Linux):** `pkg-config`, `dbus`, `libopus`, `pipewire`,
  `clang` (for `pipewire`/`libspa` bindgen via `LIBCLANG_PATH`). All
  provided by `nix develop`.
- **BlueZ:** the `hog` and `input` plugins claim the HID service and block
  user-space HID input streaming. When a session reports
  `HidInputEnableDenied`, restart bluetoothd with
  `--noplugin=input,hog` (edit `bluetooth.service`'s `ExecStart`). This
  guidance is surfaced by `events`, `dump`, `mic`, and `view`.
- **PipeWire:** `audio::PipeWireWorker` connects via `connect_rc(None)`
  (default core / user session). No custom socket env vars required.
- **direnv:** `.envrc` is `use flake`; the Nix flake ships `python3` + `uv`
  for ad-hoc tooling but no in-repo Python code currently exists.

## Testing & QA

- All tests are colocated `#[cfg(test)] mod tests` blocks inside source
  files. No integration `tests/` crate, no `benches/`.
- Async tests use `#[tokio::test]` (see `src/logger.rs`). Pure decoders use
  plain `#[test]`.
- Coverage hotspots (where to add tests when changing the matching code):
  - `src/decoder.rs` — button mask, touch frame, battery, power, hex
    formatting (~25 tests; the de-facto regression suite for wire format).
  - `src/audio/decoder.rs` — Opus packet parse + PLC + ring push.
  - `src/scan.rs` — advertisement identity-address extraction.
  - `src/session.rs` — UUID dispatch into `DeviceEvent`.
  - `src/view/{state,app,calibration}.rs` — state-machine transitions, key
    handling, TOML round-trip.
- Run focused: `cargo test touch_parse`, `cargo test -p siri-remote
  calibration_`. Run all: `cargo test`.
- There is no CI in-tree; before pushing run `cargo fmt && cargo clippy
  --all-targets -- -D warnings && cargo test`.
- Manual QA for BLE/PipeWire paths requires a physical Siri Remote and a
  running PipeWire session; `view` is the fastest way to dogfood end-to-end.
