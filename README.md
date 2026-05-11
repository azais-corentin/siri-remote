# Apple TV Siri Remote (3rd gen) on Linux

Notes, scripts, and a protocol reference for talking to a 3rd-generation
Apple TV Siri Remote over BLE on Linux. This repo currently gets as far
as **pairing the remote and dumping its GATT tree** from `bleak`; the
HID-input layer that turns button presses into `/dev/uinput` events
is not wired up yet (see [Roadmap](#roadmap)).

The protocol details below cover only what was verified against a 3rd-gen
remote in this session. Older generations are documented by the upstream
[`SiriRemote-Linux/`](SiriRemote-Linux/README.md) project vendored in this
repo, which we lean on for the HID layer.

## Scripts

- **`pair.py`** — find a Siri Remote that is currently in pairing mode,
  bond it through BlueZ, dump its GATT tree, hold the link open
  briefly, then disconnect. Re-runnable: subsequent runs require
  removing the bond first (see [Re-pairing](#re-pairing-a-remote)).
- **`discover.py`** — diagnostic BLE scanner. Logs every advertisement
  with full payload (name, service UUIDs, manufacturer data, RSSI,
  TX power) and prints a fingerprint summary at the end. Used to
  reverse-engineer the identification rule that `pair.py` then encodes.
- **`scan.py`** — RSSI-vs-time plot of every BLE device in range
  (matplotlib WebAgg). Useful for figuring out which of several
  Apple-prefix addresses is *your* remote when several are nearby.

## Dev environment

The repo ships a Nix flake (`python3` + `uv`) and `direnv` integration:

```sh
# One-time:
direnv allow                    # picks up flake automatically
# Or, without direnv:
nix develop

# Run anything:
uv run python pair.py
uv run python discover.py 45
```

`uv` resolves the lock file (`uv.lock`) on first invocation. Python 3.13+
is required (set in `pyproject.toml`). The only runtime dependency is
[`bleak`](https://github.com/hbldh/bleak); `dbus-fast` comes in as a
bleak transitive dep on Linux and is reused by `pair.py` for the BlueZ
agent.

You also need BlueZ on the host (`bluetoothd` running) and your user
must have access to it — usually membership in the `bluetooth` group is
enough; no `sudo` is needed for `pair.py`.

## Usage

### Pairing a remote

1. Put the remote into pairing mode: hold `MENU` + `Volume Up` for about
   five seconds. The status light starts pulsing.
2. Run:

   ```sh
   uv run python pair.py
   ```

3. The script scans for ~5 s, locks onto the strongest matching candidate
   (see [Identifying the remote](#identifying-the-remote-among-other-ble-peripherals)),
   registers a BlueZ agent, and connects with `pair=True`. Successful
   output ends with a `GATT services:` dump and an "Identity address:
   `XX:XX:XX:XX:XX:XX`" line — that is the stable handle to use from now on.

4. Verify the bond independently:

   ```sh
   bluetoothctl info <identity-address>
   ```

   The entry should show `Paired: yes` and `Bonded: yes`.

### Re-pairing a remote

BlueZ refuses to re-pair an already-bonded device. To start over:

```sh
bluetoothctl remove <identity-address>
```

Then put the remote back in pairing mode and run `pair.py` again.

### Just scanning

To watch raw advertisements (useful when something is wrong and you want
to see what the remote is actually broadcasting):

```sh
uv run python discover.py 45     # scan for 45 s, default
```

Each detection prints one line. At the end, a `=== SUMMARY ===` groups
advertisements by `(local_name, service_uuids, manufacturer_company_ids)`
so the ~20 s address rotation collapses back into a single fingerprint.

## How the remote works

### Identifying the remote among other BLE peripherals

The remote does not put its name in advertisements while in pairing
mode, and the BLE address rotates (see next section), so address- and
name-based selection are both unreliable. The three robust signals are:

1. **HID service UUID** advertised:

   ```
   00001812-0000-1000-8000-00805f9b34fb
   ```

2. **Apple manufacturer data** present (company ID `0x004C`).

3. **Manufacturer-data prefix** `07 0d` — Apple's HID-over-GATT
   continuity-style packet. This eliminates the AirPods, AppleTV proper,
   iPhones, and so on that also broadcast under `0x004C`.

`pair.py` requires all three, plus `RSSI >= -55 dBm`, to keep stray
Siri Remotes elsewhere in the house from being picked up.

An observed manufacturer-data blob (15 bytes):

```
07 0d 02 15 03 02 10 b9 c4 01 a3 c0 4f 50 50
└───── opaque ────┘ └─── identity ────┘ └ opaque ┘
[0]              [5][6]              [11][12]  [14]
```

The only part that has been confirmed to mean anything definite is
**bytes `[6:12]`**: those six bytes, read big-endian, are the identity
address that BlueZ ends up bonding the device under. The rest of the
prefix and the trailing `4f 50 50` are treated as opaque here — they're
consistent across rotations but their semantics weren't reverse-engineered.

### BLE address randomization

In pairing mode the remote rotates its public-random BLE address roughly
every **20 seconds**. `discover.py` makes this visible: a single physical
remote produces a new `XX:XX:XX:XX:XX:XX` every rotation, all sharing the
same advertised payload.

This has two consequences for `pair.py`:

- We can't key candidates by `BLEDevice.address`. Instead, candidates are
  keyed by the **identity address** extracted from bytes `[6:12]` of the
  manufacturer data. Multiple `BLEDevice` instances collapse into one
  `Candidate`.
- Once a rotation happens, the old random address stops being advertised
  and `connect()` against it returns `Page Timeout`. `pair.py` therefore
  drops any candidate whose most recent advert is older than 5 seconds,
  well under the 20-second rotation, and always reconnects against the
  most recent BLEDevice for that identity.

After pairing, BlueZ exchanges an IRK with the remote and resolves all
subsequent random addresses back to the identity address automatically.
At that point the rotation stops mattering — you can reconnect against
the identity address from `bluetoothctl info`.

### Pairing on Linux (bleak + BlueZ)

Two non-obvious traps; `pair.py` handles both.

**Trap 1: bond before service discovery.** The Siri Remote's services
all require an encrypted link. The default bleak flow
(`async with BleakClient(d): ...`) connects first and discovers services
second, with bonding happening implicitly somewhere in between — and the
remote disconnects mid-discovery before the bond completes. The fix is
to ask bleak to bond up front:

```python
async with BleakClient(device, pair=True) as client:
    ...
```

**Trap 2: register a BlueZ agent.** The remote initiates pairing as
numeric-comparison ("Just Works" in spirit, but it sends a passkey
anyway). Without a registered `org.bluez.Agent1`, BlueZ logs:

```
src/device.c:new_auth() No agent available for request type 2
device_confirm_passkey: Operation not permitted
```

and the pair fails with `AuthenticationFailed`. `pair.py` registers a
**`NoInputNoOutput`** agent over D-Bus (via `dbus_fast`) for the
duration of the pair attempt. `NoInputNoOutput` forces the association
model down to genuine Just Works; the agent's `RequestConfirmation`
handler is a no-op, which BlueZ interprets as "confirmed".

After pairing, BlueZ persists the bond under the identity address in
`/var/lib/bluetooth/<adapter>/<identity>/`. `pair.py` confirms this by
shelling out to `bluetoothctl devices Bonded` before declaring success;
if BlueZ reports a successful link but the device isn't actually in the
bonded list, the pair didn't stick and the script bails with retry
instructions.

### GATT layout (observed)

A successful `pair.py` run prints the full service / characteristic /
descriptor tree. The shape matches the upstream-documented dump in
[`SiriRemote-Linux/README.md`](SiriRemote-Linux/README.md):

| Service                                                    | UUID         | Notes                          |
|------------------------------------------------------------|--------------|--------------------------------|
| Generic Access                                             | `0x1800`     | Device name, appearance        |
| Generic Attribute                                          | `0x1801`     | Service Changed                |
| Device Information                                         | `0x180A`     | Serial, FW, HW, PnP ID         |
| Human Interface Device                                     | `0x1812`     | Reports + report map (HID)     |
| Battery Service                                            | `0x180F`     | Battery level + charging state |
| Bond Management                                            | `0x181E`     | BlueZ uses this on unpair      |
| `8341f2b4-c013-4f04-8197-c4cdb42e26dc` (Apple, vendor)     | vendor       | Siri / audio path (opus)       |

The vendor service at the bottom is where the microphone data shows up
when Siri is invoked — see [Audio / Siri](#audio--siri-not-working) below.
The numeric handles used by the HID layer are documented per-handle in
the next section and match upstream.

## HID-over-GATT input protocol

> **Attribution**: everything in this section is reproduced from the
> upstream [`SiriRemote-Linux/README.md`](SiriRemote-Linux/README.md)
> project (which itself credits [Jack-R1](https://github.com/Jack-R1)
> for the original protocol reverse engineering). None of it was
> independently re-verified in this session — `pair.py` does not yet
> subscribe to any HID notifications.

### Enabling input

Write `0xAF` to handle `0x001D`, then enable notifications on the report
characteristic at handle `0x0022` by writing `0x01 0x00` to its CCCD at
handle `0x0024`. The remote then pushes notifications on `0x0023` with
payloads of length **2, 13, 20, or 101** bytes (see [Audio / Siri](#audio--siri-not-working)
for the 101-byte case).

### Buttons

Button reports are 2 bytes. The second byte is a bitfield (multi-press
supported):

| Value  | Button       |
|--------|--------------|
| `0x00` | all released |
| `0x01` | AirPlay      |
| `0x02` | Volume Up    |
| `0x04` | Volume Down  |
| `0x08` | Play / Pause |
| `0x10` | Siri         |
| `0x20` | Menu         |
| `0x80` | Touchpad     |

### Touchpad

Touch events arrive as a **13-byte** payload (one finger) or **20-byte**
payload (two fingers — second finger occupies bytes `13..19`). Layout:

| Index | Meaning                                 |
|-------|------------------------------------------|
| 0     | Finger count                             |
| 1     | Button bitfield (same as above)          |
| 2     | Always `50`                              |
| 3..5  | Unknown (`[5]` increases monotonically)  |
| 6, 7  | X coordinate (see below)                 |
| 8     | Y coordinate (see below)                 |
| 9, 10 | Unknown (both `0` when released)         |
| 11    | Pressure                                 |
| 12    | Unknown                                  |

**X coordinate** is split across two bytes; the touchpad has 8 vertical
"zones" and only the low 3 bits of byte `[7]` are needed for the zone:

```
x = data[6] + 255 * (data[7] & 0x07)
```

**Y coordinate** is a signed byte in `data[8]`. Per upstream, the value
(bottom → top) ranges 188..255 then 0..38. Resolution is noticeably lower
than X.

### Battery level

Handle `0x0027` (notification source), CCCD at handle `0x0029`. Enable
with `0x01 0x00` → CCCD. Values arrive as a single byte `0x00`..`0x64`
(0–100, percentage).

### Charging state

Handle `0x002A` (notification source), CCCD at handle `0x002C`. Enable
the same way. Values:

| Byte   | Meaning      |
|--------|--------------|
| `0xAB` | charging     |
| `0xAF` | discharging  |
| `0xBB` | plugged in   |

### Audio / Siri (not working)

Holding Siri produces 101-byte notifications containing opus-encoded
audio. Per upstream, BlueZ truncates these to 20 bytes — the full
payload is visible in Wireshark but never delivered to userspace. The
upstream README does not have a workaround, and we have not investigated
this path.

## Troubleshooting

| Symptom                                                          | Cause / fix                                                                                          |
|------------------------------------------------------------------|-------------------------------------------------------------------------------------------------------|
| `bleak ... AuthenticationFailed`                                 | No BlueZ agent registered. Use `pair.py` (not raw `BleakClient`), or register your own `Agent1`.      |
| `Page Timeout` / `org.bluez.Error.UnknownObject` mid-connect     | The remote's random BLE address rotated between scan and connect. Re-run; `pair.py` already retries.  |
| `AuthenticationCanceled`, repeatedly                             | BlueZ pairing state is stuck. `systemctl restart bluetooth` and try again.                            |
| `pair.py` says "BlueZ does not list `XX:XX:...` as bonded"       | Pair handshake completed at the link layer but BlueZ didn't persist the bond. Restart `bluetoothd`.   |
| Re-pair attempt fails immediately                                | Already bonded. `bluetoothctl remove <identity>` before re-running `pair.py`.                         |
| `pair.py` locks onto someone else's remote                       | Two Siri Remotes in range. Move closer to yours; the `MIN_RSSI = -55` filter prefers nearby remotes.  |

## Roadmap

- Wire the HID notification stream (`0x0023`) through to `/dev/uinput`.
  The upstream `SiriRemote-Linux/main.py` does this with `bluepy` +
  `evdev`; port the same idea to `bleak` + `python-evdev` so we stay on
  the same stack as `pair.py`.
- Investigate the Siri / audio path. The 101-byte truncation is a BlueZ
  ATT MTU issue per upstream; check whether modern BlueZ + a manually
  negotiated higher MTU lets the full opus frame through.
- Persist the last bonded identity address so subsequent runs can
  reconnect directly instead of re-scanning.

## References

- [`SiriRemote-Linux/`](SiriRemote-Linux/README.md) — upstream project
  vendored in-tree. Source for the HID protocol section above.
- [Jack-R1](https://github.com/Jack-R1) — original protocol reverse
  engineering (credited by upstream).
- [bleak](https://github.com/hbldh/bleak) — async BLE client used here.
- [BlueZ D-Bus API](https://github.com/bluez/bluez/blob/master/doc/org.bluez.Agent.rst)
  — `org.bluez.Agent1` reference for the pairing-agent contract.
