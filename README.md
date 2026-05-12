# siri-remote

A Linux CLI for the 3rd-generation Apple TV Siri Remote over Bluetooth Low Energy: pair it, stream its events, render its live state as a TUI, dump raw HID frames, and republish its microphone as a PipeWire `Audio/Source`.

## Status

Experimental. The HID-over-GATT protocol is reverse-engineered from a single physical remote; expect rough edges on anything Apple has not documented. **Gen-1 / gen-2 remotes are out of scope** — they use a different button layout. Linux only.

## Requirements

- Linux with BlueZ + D-Bus running.
- A BLE-capable adapter the kernel exposes through BlueZ.
- PipeWire (required for `siri-remote mic`).
- `libopus`, `libdbus`, `libclang` (all wired by the flake; see below).
- Rust 2024 toolchain (provided by the flake).

## Build

The `flake.nix` devShell pins every native dependency and exports `LIBCLANG_PATH` for the `pipewire` / `libspa` bindings.

```sh
nix develop
cargo build --release
./target/release/siri-remote --help
```


## Pairing the remote

Hold **MENU + Volume Up** for about 5 seconds until the remote starts advertising in pairing mode. Keep it within ~50 cm of the host — the scanner discards any candidate weaker than RSSI `-55 dBm` to avoid latching onto someone else's remote.

Apple rotates the BLE address roughly every 20 s, so once the pairing window starts you have about that long to land a `pair` invocation.

## Usage

```sh
# Bond a remote in pairing mode, dump its GATT tree, hold the link briefly.
siri-remote pair
siri-remote pair --scan-seconds 8 --hold-seconds 10
```

```sh
# Stream battery / power / button / touch / mic frames to stdout.
siri-remote events
siri-remote events --address AA:BB:CC:DD:EE:FF --reconnect-delay 1.0
```

```sh
# Live TUI dashboard (silhouette + status + touch + event log).
siri-remote view
siri-remote view --address AA:BB:CC:DD:EE:FF
```

```sh
# Raw HID frames. At least one of --touch / --mic is required.
siri-remote dump --touch
siri-remote dump --touch --mic
```

```sh
# Republish the remote's microphone (Siri button held) as a PipeWire node.
siri-remote mic
siri-remote mic --node-name siri-remote --node-description "Siri Remote microphone"

# In another shell, while the Siri button is held:
pw-record --target=siri-remote out.wav
```

```sh
# Remove every paired Siri Remote (or just one address) via BlueZ.
siri-remote unpair --dry-run
siri-remote unpair
siri-remote unpair --address AA:BB:CC:DD:EE:FF
```

Every connect-style subcommand (`events`, `view`, `dump`, `mic`) shares the same flags: `--address`, `--scan-seconds`, `--reconnect-delay`. With no `--address`, the binary scans for the nearest advertising remote and falls back to pairing-mode discovery on auth failure.

## `view` dashboard

`siri-remote view` renders a `ratatui` two-pane dashboard. The left pane is a `Canvas` silhouette of the remote drawn in remote-local coordinates (100 × 300, ≈ 1:3 aspect), with live button highlights and a fading touch trail. The right pane shows connection status, decoded touch geometry (X / Y / pressure / hover / ellipse), and a scrollable event log. Minimum terminal size is **70 × 24** cells.

## Protocol notes

Constants and offsets here match `src/decoder.rs`, `src/session.rs`, `src/scan.rs`, and `src/audio/mod.rs` — treat the source as authoritative if anything below drifts.

- **Input enable**. The remote stays silent until userspace writes `0xAF` to every HID Output report (`session::INPUT_ENABLE_BYTE`).
- **HID reports**:
  - `0xFA` — microphone audio (Opus, 99-byte payload, emitted only while the Siri button is held).
  - `0xFB` — system buttons (2-byte mask; 13 bits assigned; gen-3 layout only).
  - `0xFC` — touchpad frames (11 bytes for one finger, 18 bytes for two; 7-byte per-slot trailer).
- **GATT extras**:
  - Battery level — characteristic `0x2A19`.
  - Power status — characteristic `0x2A1A`, with `0xAB` = charging, `0xAF` = discharging, `0xBB` = plugged-in.
- **Advertisement fingerprint**. Apple manufacturer prefix `07 0D 02 15 03 02 <6-byte identity address> 4F 50 50` plus the HID service UUID `0x1812`. The scanner ranks candidates by mean RSSI with `MIN_RSSI = -55` and discards entries older than `STALE_AFTER = 5 s` because the BLE address rotates ~every 20 s.
- **Microphone**. Opus CELT-only WB (TOC `0xB8`), 20 ms frames at 48 kHz mono → 960 samples / frame. `MicDecoder` tracks sequence numbers and emits up to 4 frames of libopus packet-loss concealment on gaps.
- **Why we bypass btleplug for HID**. The HID service has eight `Report` characteristics that all share UUID `0x2A4D` and differ only by their Report Reference descriptor. `btleplug` collapses same-UUID characteristics within a service, so the input-enable write and per-report `StartNotify` are issued directly via `org.bluez.GattCharacteristic1` (`src/bluez/hid.rs`).

### Touch ellipse: axes and rotation

Each contact slot ends with a 7-byte trailer. Bytes 3 (`major`) and 4 (`minor`) carry the long and short axis of the contact ellipse as `0..=255`. The high three bits of byte 6 (`flags`) carry an orientation index `angle_idx ∈ 0..=7`:

- `angle_deg = ((angle_idx + 1) mod 8) × 22.5` — 8 quantization buckets at `0°, 22.5°, …, 157.5°`. The `+1` is an empirical shift (verified against live touches); `angle_idx == 7` wraps back to `0°` because the ellipse is 180°-symmetric.
- `θ` is the orientation of the **major** axis, measured CCW from touchpad +X. The renderer uses the standard rotation
  `dx = a·cos u·cos θ − b·sin u·sin θ`, `dy = a·cos u·sin θ + b·sin u·cos θ` (`src/view/ui.rs`).
- Byte → canvas scaling in `view` is empirical: `semi-axis = byte × 0.8 × R / 256`, where `R = 30` canvas units is the touchpad disc radius. A mid-swipe contact (`major ≈ 0x70`, `minor ≈ 0x60`) spans roughly a quarter of the touchpad ring.

<p align="center">
  <img src="docs/touch-ellipse.svg" alt="Touch ellipse major/minor axes and rotation diagram" width="560">
</p>

## Architecture

| Path | Purpose |
|------|---------|
| `src/main.rs` | Tokio entry point; CLI dispatch; Ctrl-C → exit 130. |
| `src/cli.rs` | `clap` definitions for every subcommand and flag. |
| `src/logger.rs` | Process-wide log sink, shared by all subcommands. |
| `src/scan.rs` | BLE scan loop, Apple-HID advertisement matching, RSSI ranking. |
| `src/pair.rs` | `pair` subcommand: pairing-mode scan, bond, GATT dump, hold. |
| `src/unpair.rs` | `unpair` subcommand: enumerate + `Adapter1.RemoveDevice`. |
| `src/session.rs` | Shared connect / configure-notifications / typed `DeviceEvent` stream. |
| `src/decoder.rs` | Pure parsing: button mask, touch trailer geometry, battery, power. |
| `src/hid.rs` | btleplug-side GATT helpers (battery, power, find-by-UUID). |
| `src/bluez/` | Linux-only D-Bus side: pairing `agent`, `device`, HID `hid` channel. |
| `src/events.rs` | `events` subcommand: format `DeviceEvent`s to stdout. |
| `src/dump.rs` | `dump` subcommand: raw HID `0xFA` / `0xFC` frames to stdout. |
| `src/view/` | `view` subcommand: ratatui `app` loop, `state`, `ui` render, `calibration`. |
| `src/mic.rs` | `mic` subcommand: wire `DeviceEvent::UnknownInput` 0xFA → `MicDecoder` → PipeWire ring. |
| `src/audio/` | Opus `decoder` + PipeWire `pipewire` worker + shared `Ring` definition. |

## Troubleshooting

- **`pair` times out**. Remote is not in pairing mode, RSSI is weaker than `-55 dBm`, or the BLE address rotated. Re-hold MENU + Volume Up and rerun within ~20 s.
- **`BlueZ refused every HID input-enable write … org.bluez.Error.NotAuthorized`**. The BlueZ `hog` (HID-over-GATT) plugin is owning the HID service and locking userspace out of the Report characteristics. Disable it:

  ```ini
  # /etc/bluetooth/main.conf
  [General]
  DisablePlugins=hogp
  ```

  Then `systemctl restart bluetooth.service` and re-pair.
- **No events after a successful pair**. The input-enable byte did not land. Confirm the remote is bonded (`bluetoothctl info <addr>` is fine for diagnostics — but do **not** use `bluetoothctl` to pair; this project relies on its own BlueZ agent so the gen-3 Just-Works flow completes cleanly).
- **`agent` registration fails**. The current user cannot talk to BlueZ over the system bus. Add the user to the `bluetooth` group (or install an equivalent polkit rule) and log back in.
- **`pw-record --target=siri-remote` finds nothing**. Either `siri-remote mic` is not running, or the consumer is talking to ALSA only. Confirm the node is present with `pw-cli ls Node`; if the consumer cannot see PipeWire nodes, route it via `pipewire-pulse` or the PipeWire ALSA shim.

## License

See [`LICENSE`](LICENSE).
