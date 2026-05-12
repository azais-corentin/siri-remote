# Repository Guidelines

## Overview

`siri-remote` is a single Rust binary crate (edition 2024) that pairs with and
streams from an Apple TV Siri Remote (3rd gen) over BLE. On Linux it can expose
the Opus mic as a PipeWire `Audio/Source`. BlueZ/D-Bus and PipeWire are gated
by `#[cfg(target_os = "linux")]`.

## Architecture

`scan` → `session::connect_once` (+ `connect_with_pairing` / `bluez::agent`) →
`Session` merges btleplug notifications and `bluez::hid::HidSession` (writes
input-enable `0xAF`, subscribes to `PropertiesChanged` on Input reports) via
`tokio::select!`, yielding `DeviceEvent`. Consumers: `events`, `dump` (`0xFC`
touch / `0xFA` mic raw hex), `view` (ratatui; calibration at
`$XDG_CONFIG_HOME/siri-remote/calibration.toml`), `mic` (`audio::MicDecoder` →
`Ring = Arc<Mutex<VecDeque<i16>>>` → `audio::PipeWireWorker` OS thread, 48 kHz
mono S16LE). Reconnect via `--reconnect-delay`. `HidInputEnableDenied` is fatal
— run bluetoothd with `--noplugin=input,hog`. Single multi-thread Tokio
runtime; only OS thread is the PipeWire worker. No `unsafe` in-tree.

## Layout

- `src/{main,cli,session,scan,decoder,events,dump,mic,pair,logger}.rs`
- `src/audio/{mod,decoder,pipewire}.rs`, `src/view/{mod,app,state,ui,calibration}.rs` (Linux),
  `src/bluez/{agent,device,hid}.rs` (Linux D-Bus)
- `flake.nix` pins toolchain + deps (dbus, libopus, pipewire, clang); enter via
  `direnv allow` or `nix develop`. `Cargo.lock` tracked. No `tests/`, CI, README.

## Conventions

- Subcommands: `pub async fn run(args: <Cmd>Args) -> anyhow::Result<u8>`.
  `Ok(1)` user-actionable, `Ok(2)` fatal, `130` Ctrl-C.
- `anyhow::Result` at boundaries; typed errors only for branching
  (`session::InitError`, `bluez::hid::HidInputEnableDenied`, `scan::ScanError`).
- Every module opens with a `//!` summary. HID constants inline as hex; Opus:
  `FRAME_SAMPLES = 960`, `MIC_REPORT_LEN = 99`, `RING_CAPACITY_SAMPLES = 12_000`.
- State machines pure (`view::state::AppState::on_event`); rendering reads only.
  Wire types explicitly — no DI. `log::*` via `logger::init`; `view` swaps an
  mpsc sink.

## Commands

`cargo {build [--release], fmt, clippy --all-targets -- -D warnings, test
[<pattern>]}`; `cargo run -- <pair|events|unpair|dump|mic|view>`. Tests are
colocated `#[cfg(test)] mod tests`; hotspots: `decoder.rs` (wire regression),
`audio/decoder.rs`, `scan.rs`, `session.rs`, `view/{state,app,calibration}.rs`.
Manual BLE/PipeWire QA needs a physical remote; `view` is the fastest dogfood.
