"""Stream real-time events from an Apple TV Siri Remote over BLE.

The script prefers an already-bonded Siri Remote and reconnects forever after
idle disconnects. If no bonded remote is available, put the remote in pairing
mode (hold MENU + Volume Up) and this reuses the working BlueZ pairing path from
``pair.py``.
"""

from __future__ import annotations

import argparse
import asyncio
import contextlib
import re
import sys
from dataclasses import dataclass
from datetime import UTC, datetime
from typing import Iterable, Sequence

from bleak import BleakClient, BleakScanner
from bleak.backends.characteristic import BleakGATTCharacteristic
from bleak.backends.device import BLEDevice
from bleak.backends.scanner import AdvertisementData

from pair import (
    SCAN_SETTLE_SECONDS,
    STALE_AFTER_SECONDS,
    Candidate,
    _AgentSession,
    extract_identity_address,
    is_siri_remote,
)


# Observed Siri Remote GATT value handles.
INPUT_ENABLE_HANDLE = 0x001D
INPUT_NOTIFY_HANDLE = 0x0023
BATTERY_NOTIFY_HANDLE = 0x0028
POWER_NOTIFY_HANDLE = 0x002B
BATTERY_LEVEL_UUID = "00002a19-0000-1000-8000-00805f9b34fb"
BATTERY_POWER_UUID = "00002a1a-0000-1000-8000-00805f9b34fb"
HID_REPORT_UUID = "00002a4d-0000-1000-8000-00805f9b34fb"
REPORT_REFERENCE_UUID = "00002908-0000-1000-8000-00805f9b34fb"

DEVICE_INFO_CHARS: tuple[tuple[str, str], ...] = (
    ("serial", "00002a25-0000-1000-8000-00805f9b34fb"),
    ("hardware", "00002a27-0000-1000-8000-00805f9b34fb"),
    ("firmware", "00002a26-0000-1000-8000-00805f9b34fb"),
    ("manufacturer", "00002a29-0000-1000-8000-00805f9b34fb"),
    ("pnp_id", "00002a50-0000-1000-8000-00805f9b34fb"),
)

REMOTE_NAME_KEYWORDS = (
    "siri remote",
    "apple tv remote",
    "apple remote",
)

BUTTON_NAMES: tuple[tuple[int, str], ...] = (
    (0x01, "AirPlay"),
    (0x02, "Volume Up"),
    (0x04, "Volume Down"),
    (0x08, "Play/Pause"),
    (0x10, "Siri"),
    (0x20, "Menu"),
    (0x40, "Touchpad 2-Finger"),
    (0x80, "Touchpad"),
)

BUTTON_TOUCHPAD = 0x80
BUTTON_TOUCHPAD_2 = 0x40
TOUCH_EVENT_MARKER = 0x32
POWER_STATES = {
    0xAB: "charging",
    0xAF: "discharging",
    0xBB: "plugged-in",
}

ADDRESS_RE = re.compile(r"^[0-9A-F]{2}(?::[0-9A-F]{2}){5}$", re.IGNORECASE)
AUTH_FAILURE_MARKERS = (
    "auth",
    "encrypt",
    "not authorized",
    "not permitted",
    "insufficient authentication",
    "authenticationfailed",
)


# A Siri Remote may advertise its serial number instead of a friendly name, so
# name matching is only a display nicety. Identification comes from advertising
# data via pair.is_siri_remote().


@dataclass
class Selection:
    address: str
    name: str
    device: BLEDevice | None = None
    identity_address: str | None = None
    requires_pairing: bool = False
    rssi: int | None = None



def normalize_address(address: str) -> str:
    normalized = address.strip().upper()
    if not ADDRESS_RE.match(normalized):
        raise ValueError(f"invalid Bluetooth address: {address!r}")
    return normalized


def likely_remote_name(name: str | None) -> bool:
    if not name:
        return False
    folded = name.lower()
    return any(keyword in folded for keyword in REMOTE_NAME_KEYWORDS)


async def scan_for_nearest_remote(settle: float) -> Selection | None:
    """Scan for Siri Remote advertisements and choose the strongest identity.

    This is intentionally scanner-only: Bleak does not expose a portable bonded
    device registry, and this script must not shell out to bluetoothctl.
    """

    candidates: dict[str, Candidate] = {}
    loop = asyncio.get_running_loop()

    def cb(device: BLEDevice, adv: AdvertisementData) -> None:
        if not is_siri_remote(adv):
            return
        identity = extract_identity_address(adv) or device.address.upper()
        candidate = candidates.setdefault(
            identity, Candidate(identity_address=identity)
        )
        candidate.rssis.append(adv.rssi)
        candidate.last_device = device
        candidate.last_address = device.address
        candidate.last_seen = loop.time()
        display_name = adv.local_name or device.name or identity
        print(
            f"  identity={identity} addr={device.address} name={display_name!r} "
            f"rssi={adv.rssi} hits={candidate.hits} "
            f"mean_rssi={candidate.mean_rssi:.1f}",
            file=sys.stderr,
        )

    print(f"Scanning {settle:.0f}s for Siri Remote advertisements...", file=sys.stderr)
    async with BleakScanner(detection_callback=cb):
        await asyncio.sleep(settle)

    now = loop.time()
    fresh = [
        candidate
        for candidate in candidates.values()
        if candidate.last_device is not None
        and (now - candidate.last_seen) <= max(STALE_AFTER_SECONDS, settle + 1.0)
    ]
    if not fresh:
        return None

    ranked = sorted(fresh, key=lambda candidate: candidate.mean_rssi, reverse=True)
    best = ranked[0]
    assert best.last_device is not None
    selected = Selection(
        address=best.last_address or best.last_device.address,
        name=best.last_device.name or best.identity_address,
        device=best.last_device,
        identity_address=best.identity_address,
        rssi=best.last_rssi,
    )
    if len(ranked) > 1:
        print(
            f"  ({len(ranked) - 1} other Siri Remote(s) also in range; "
            "picked the strongest signal)",
            file=sys.stderr,
        )
    return selected


async def choose_initial_selection(address: str | None, scan_seconds: float) -> Selection:
    if address is not None:
        requested = normalize_address(address)
        selected = Selection(address=requested, name="requested address")
        print_selected(selected, "requested address; connecting directly")
        return selected

    selected = await scan_for_nearest_remote(scan_seconds)
    if selected is None:
        raise asyncio.TimeoutError
    print_selected(selected, "strongest currently advertising Siri Remote")
    return selected


def print_selected(selection: Selection, reason: str) -> None:
    rssi = f" rssi={selection.rssi}" if selection.rssi is not None else ""
    identity = (
        f" identity={selection.identity_address}"
        if selection.identity_address is not None
        else ""
    )
    print(
        f"Selected {selection.name!r} address={selection.address}{identity}{rssi} ({reason}).",
        file=sys.stderr,
    )


def now_stamp() -> str:
    return datetime.now(UTC).isoformat(timespec="milliseconds").replace("+00:00", "Z")


def raw_hex(data: bytes | bytearray | memoryview) -> str:
    return bytes(data).hex(" ")


def button_names(mask: int) -> list[str]:
    return [name for bit, name in BUTTON_NAMES if mask & bit]


def button_list(mask: int) -> str:
    names = button_names(mask)
    return "+".join(names) if names else "none"


def decode_finger(data: bytes | bytearray | memoryview) -> tuple[int, int, int]:
    if len(data) != 7:
        raise ValueError(f"finger payload must be 7 bytes, got {len(data)}")
    view = bytes(data)
    x = int((view[0] + 255 * (view[1] & 0x07) - 230) / 15)
    y = (view[2] if view[2] & 0x80 else view[2] + 255) - 188
    pressure = view[5]
    return x, y, pressure


def format_battery(data: bytes | bytearray | memoryview) -> str:
    if not data:
        return "empty battery packet"
    return f"battery={int(data[0])}%"


def format_power(data: bytes | bytearray | memoryview) -> str:
    if not data:
        return "empty power packet"
    value = int(data[0])
    state = POWER_STATES.get(value, f"unknown(0x{value:02x})")
    return f"power={state}"


class InputDecoder:
    def __init__(self) -> None:
        self._last_button = 0

    @staticmethod
    def _normalized_button(data: bytes) -> int | None:
        if len(data) < 2:
            return None
        button = data[1]
        if data[0] == 2 and button & BUTTON_TOUCHPAD:
            button = (button & ~BUTTON_TOUCHPAD) | BUTTON_TOUCHPAD_2
        return button

    def format(self, payload: bytes | bytearray | memoryview) -> str:
        data = bytes(payload)
        parts: list[str] = []

        button = self._normalized_button(data)
        if button is not None and button != self._last_button:
            pressed = button & ~self._last_button
            released = self._last_button & ~button
            parts.append(
                "buttons="
                f"{button_list(button)} pressed={button_list(pressed)} "
                f"released={button_list(released)}"
            )
            self._last_button = button

        if len(data) >= 3 and data[2] == TOUCH_EVENT_MARKER:
            touch = self._format_touch(data)
            if touch is not None:
                parts.append(touch)

        if parts:
            return "; ".join(parts)
        return f"unknown HID packet len={len(data)}"

    @staticmethod
    def _format_touch(data: bytes) -> str | None:
        if len(data) not in (13, 20):
            return f"touch marker with unsupported len={len(data)}"

        fingers = [decode_finger(data[6:13])]
        if len(data) == 20:
            fingers.append(decode_finger(data[13:20]))

        pressed = bool(data[1] & BUTTON_TOUCHPAD)
        expected_count = data[0]
        rendered = ", ".join(
            f"finger{index}:x={x},y={y},pressure={pressure}"
            for index, (x, y, pressure) in enumerate(fingers, start=1)
        )
        return f"touch pressed={pressed} count={expected_count} {rendered}"


def format_event(
    source: str,
    handle: int,
    payload: bytes | bytearray | memoryview,
    decoder: InputDecoder | None = None,
) -> str:
    data = bytes(payload)
    if source == "battery":
        decoded = format_battery(data)
    elif source == "power":
        decoded = format_power(data)
    elif source == "input":
        if decoder is None:
            decoder = InputDecoder()
        decoded = decoder.format(data)
    else:
        decoded = f"len={len(data)}"
    return (
        f"{now_stamp()} {source} handle=0x{handle:04x} "
        f"raw={raw_hex(data)} | {decoded}"
    )


def iter_characteristics(client: BleakClient) -> Iterable[BleakGATTCharacteristic]:
    for service in client.services:
        yield from service.characteristics


def find_characteristic(
    client: BleakClient, spec: int | str
) -> BleakGATTCharacteristic | None:
    spec_text = str(spec).lower()
    for char in iter_characteristics(client):
        if isinstance(spec, int) and char.handle == spec:
            return char
        if char.uuid.lower() == spec_text:
            return char
    return None


async def read_optional(client: BleakClient, spec: int | str) -> bytes | None:
    try:
        return bytes(await client.read_gatt_char(spec))
    except Exception:
        return None


def decode_text_or_hex(data: bytes) -> str:
    try:
        text = data.rstrip(b"\x00").decode("utf-8")
    except UnicodeDecodeError:
        return raw_hex(data)
    return text if text else raw_hex(data)


async def print_device_info(client: BleakClient, selection: Selection) -> None:
    print("\nConnected Siri Remote", file=sys.stderr)
    print(f"  selected_address: {selection.address}", file=sys.stderr)
    if selection.identity_address is not None:
        print(f"  identity_address: {selection.identity_address}", file=sys.stderr)
    print(f"  selected_name: {selection.name}", file=sys.stderr)
    print(f"  backend: {type(client).__module__}.{type(client).__name__}", file=sys.stderr)
    print(f"  mtu: {getattr(client, 'mtu_size', 'unknown')}", file=sys.stderr)

    services = list(client.services)
    chars = list(iter_characteristics(client))
    descriptor_count = sum(len(char.descriptors) for char in chars)
    print(
        f"  services: {len(services)} chars: {len(chars)} descriptors: {descriptor_count}",
        file=sys.stderr,
    )
    for service in services:
        print(f"  Service {service.uuid} ({service.description})", file=sys.stderr)
        for char in service.characteristics:
            props = ",".join(char.properties)
            print(
                f"    Char handle=0x{char.handle:04x} {char.uuid} [{props}] "
                f"({char.description})",
                file=sys.stderr,
            )

    for label, uuid in DEVICE_INFO_CHARS:
        data = await read_optional(client, uuid)
        if data is not None:
            print(f"  {label}: {decode_text_or_hex(data)}", file=sys.stderr)

    battery = await read_optional(client, BATTERY_NOTIFY_HANDLE)
    if battery is not None:
        print(f"  current_{format_battery(battery)}", file=sys.stderr)
    power = await read_optional(client, POWER_NOTIFY_HANDLE)
    if power is not None:
        print(f"  current_{format_power(power)}", file=sys.stderr)


async def report_reference(
    client: BleakClient, char: BleakGATTCharacteristic
) -> bytes | None:
    descriptor = char.get_descriptor(REPORT_REFERENCE_UUID)
    if descriptor is None:
        return None
    try:
        return bytes(await client.read_gatt_descriptor(descriptor))
    except Exception:
        return None


def hid_report_characteristics(client: BleakClient) -> list[BleakGATTCharacteristic]:
    return [
        char
        for char in iter_characteristics(client)
        if char.uuid.lower() == HID_REPORT_UUID
    ]


async def start_optional_notify(
    client: BleakClient,
    char: BleakGATTCharacteristic | None,
    source: str,
    callback,
) -> bool:
    if char is None:
        return False
    if "notify" not in char.properties and "indicate" not in char.properties:
        print(
            f"warning: {source} characteristic 0x{char.handle:04x} is not notifiable",
            file=sys.stderr,
        )
        return False
    await client.start_notify(char, callback(source, char.handle))
    print(f"Enabled {source} notifications on handle 0x{char.handle:04x}.", file=sys.stderr)
    return True


async def write_enable_candidate(
    client: BleakClient, char: BleakGATTCharacteristic
) -> bool:
    if "write-without-response" in char.properties:
        try:
            await client.write_gatt_char(char, b"\xAF", response=False)
            return True
        except Exception as exc:
            print(
                f"input enable write-without-response failed on "
                f"0x{char.handle:04x}: {exc!r}",
                file=sys.stderr,
            )
    if "write" in char.properties:
        try:
            await client.write_gatt_char(char, b"\xAF", response=True)
            return True
        except Exception as exc:
            print(
                f"input enable write-with-response failed on "
                f"0x{char.handle:04x}: {exc!r}",
                file=sys.stderr,
            )
    return False


async def enable_input(client: BleakClient) -> None:
    reports = hid_report_characteristics(client)
    candidates: list[tuple[int, BleakGATTCharacteristic, bytes | None]] = []
    for char in reports:
        if "write" not in char.properties and "write-without-response" not in char.properties:
            continue
        ref = await report_reference(client, char)
        report_type = ref[1] if ref is not None and len(ref) >= 2 else None
        if report_type == 2:
            rank = 0
        elif char.handle == INPUT_ENABLE_HANDLE:
            rank = 1
        elif "notify" not in char.properties:
            rank = 2
        else:
            rank = 3
        candidates.append((rank, char, ref))

    if not candidates:
        fallback = find_characteristic(client, INPUT_ENABLE_HANDLE)
        if fallback is not None:
            candidates.append((9, fallback, None))

    for _, char, ref in sorted(candidates, key=lambda item: (item[0], item[1].handle)):
        ref_text = raw_hex(ref) if ref is not None else "unknown"
        print(
            f"Sending input-enable byte to report handle 0x{char.handle:04x} "
            f"report_ref={ref_text}.",
            file=sys.stderr,
        )
        if await write_enable_candidate(client, char):
            return

    raise RuntimeError("could not send Siri Remote input-enable byte to any HID report")


async def configure_notifications(client: BleakClient) -> InputDecoder:
    decoder = InputDecoder()

    def print_callback(source: str, handle: int):
        def cb(char: BleakGATTCharacteristic, data: bytearray) -> None:
            actual_handle = getattr(char, "handle", handle)
            print(format_event(source, actual_handle, data, decoder), flush=True)

        return cb

    battery_char = find_characteristic(client, BATTERY_LEVEL_UUID) or find_characteristic(
        client, BATTERY_NOTIFY_HANDLE
    )
    power_char = find_characteristic(client, BATTERY_POWER_UUID) or find_characteristic(
        client, POWER_NOTIFY_HANDLE
    )
    battery_ok = await start_optional_notify(
        client, battery_char, "battery", print_callback
    )
    power_ok = await start_optional_notify(client, power_char, "power", print_callback)

    input_chars = [
        char
        for char in hid_report_characteristics(client)
        if "notify" in char.properties or "indicate" in char.properties
    ]
    if not input_chars:
        fallback = find_characteristic(client, INPUT_NOTIFY_HANDLE)
        if fallback is not None:
            input_chars = [fallback]

    input_count = 0
    for char in input_chars:
        if await start_optional_notify(client, char, "input", print_callback):
            ref = await report_reference(client, char)
            ref_text = raw_hex(ref) if ref is not None else "unknown"
            print(
                f"  input report handle 0x{char.handle:04x} report_ref={ref_text}",
                file=sys.stderr,
            )
            input_count += 1

    if not battery_ok:
        print("warning: battery notifications were not enabled", file=sys.stderr)
    if not power_ok:
        print("warning: power notifications were not enabled", file=sys.stderr)
    if input_count == 0:
        raise RuntimeError("no HID input notification reports were discovered")

    await enable_input(client)
    print("Notifications enabled; waiting for events...", file=sys.stderr)
    return decoder


async def wait_until_disconnected(client: BleakClient) -> None:
    while getattr(client, "is_connected", False):
        await asyncio.sleep(0.5)


async def stream_connected_client(client: BleakClient, selection: Selection) -> None:
    await print_device_info(client, selection)
    await configure_notifications(client)
    await wait_until_disconnected(client)


async def connect_once(selection: Selection) -> Selection:
    target: BLEDevice | str = (
        selection.device if selection.device is not None else selection.address
    )
    if selection.requires_pairing:
        print(f"Connecting to {selection.address} with pair=True...", file=sys.stderr)
        async with _AgentSession(), BleakClient(
            target, pair=True, timeout=60.0
        ) as client:
            await stream_connected_client(client, selection)
    else:
        print(f"Connecting to {selection.address}...", file=sys.stderr)
        try:
            async with BleakClient(target, timeout=60.0) as client:
                await stream_connected_client(client, selection)
        except TimeoutError:
            print(
                "Direct connect timed out; retrying with Bleak pair=True "
                "(uses existing bond if already paired).",
                file=sys.stderr,
            )
            async with _AgentSession(), BleakClient(
                target, pair=True, timeout=60.0
            ) as client:
                await stream_connected_client(client, selection)

    return Selection(
        address=selection.identity_address or selection.address,
        name=selection.name,
        identity_address=selection.identity_address,
    )


def is_auth_failure(exc: BaseException) -> bool:
    detail = repr(exc).lower().replace(" ", "")
    return any(marker.replace(" ", "") in detail for marker in AUTH_FAILURE_MARKERS)


async def switch_to_pairing_scan() -> Selection:
    print(
        "Connection failed because the link is not bonded/authenticated. Put the "
        "remote in pairing mode (hold MENU + Volume Up) and keep it nearby.",
        file=sys.stderr,
    )
    selected = await scan_for_nearest_remote(SCAN_SETTLE_SECONDS)
    if selected is None:
        raise asyncio.TimeoutError
    selected.requires_pairing = True
    print_selected(selected, "pairing-mode remote")
    return selected


async def run_forever(selection: Selection, reconnect_delay: float) -> None:
    while True:
        try:
            selection = await connect_once(selection)
            print(
                f"Disconnected from {selection.address}; reconnecting automatically.",
                file=sys.stderr,
            )
        except RuntimeError as exc:
            if selection.requires_pairing:
                raise
            if is_auth_failure(exc):
                selection = await switch_to_pairing_scan()
                continue
            print(f"Connection/setup failed: {exc!r}; retrying.", file=sys.stderr)
        except Exception as exc:
            if not selection.requires_pairing and is_auth_failure(exc):
                selection = await switch_to_pairing_scan()
                continue
            print(f"Connection failed: {exc!r}; retrying.", file=sys.stderr)
        await asyncio.sleep(reconnect_delay)


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description=(
            "Pair/connect an Apple TV Siri Remote and print real-time battery, "
            "power, button, and touch events."
        )
    )
    parser.add_argument(
        "--address",
        help="Specific bonded Siri Remote Bluetooth identity address to use.",
    )
    parser.add_argument(
        "--scan-seconds",
        type=float,
        default=5.0,
        help="Seconds to scan for current bonded remotes before falling back.",
    )
    parser.add_argument(
        "--reconnect-delay",
        type=float,
        default=0.5,
        help="Delay before reconnect attempts after disconnect/failure.",
    )
    return parser


async def async_main(argv: Sequence[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    if args.scan_seconds < 0:
        parser.error("--scan-seconds must be non-negative")
    if args.reconnect_delay < 0:
        parser.error("--reconnect-delay must be non-negative")

    try:
        selection = await choose_initial_selection(args.address, args.scan_seconds)
    except ValueError as exc:
        print(str(exc), file=sys.stderr)
        return 2
    except asyncio.TimeoutError:
        print(
            "Timed out waiting for a Siri Remote. If it is unpaired, hold MENU + "
            "Volume Up for pairing mode and keep it close to this host.",
            file=sys.stderr,
        )
        return 1

    try:
        await run_forever(selection, args.reconnect_delay)
    except KeyboardInterrupt:
        print("Interrupted.", file=sys.stderr)
        return 130
    except RuntimeError as exc:
        print(str(exc), file=sys.stderr)
        return 2


if __name__ == "__main__":
    with contextlib.suppress(KeyboardInterrupt):
        raise SystemExit(asyncio.run(async_main()))
