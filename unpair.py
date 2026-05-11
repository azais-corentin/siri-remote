"""Unpair previously bonded Apple TV Siri Remotes from BlueZ.

This uses BlueZ's D-Bus API directly through dbus-fast. It does not call
``bluetoothctl``.
"""

from __future__ import annotations

import argparse
import asyncio
import sys
from dataclasses import dataclass, field
from typing import Any, Sequence

from dbus_fast import BusType, Variant
from dbus_fast.aio import MessageBus

from pair import APPLE_COMPANY_ID, APPLE_HID_MFR_PREFIX, HID_SERVICE_UUID

BLUEZ_BUS = "org.bluez"
BLUEZ_ROOT = "/"
DEVICE_IFACE = "org.bluez.Device1"
ADAPTER_IFACE = "org.bluez.Adapter1"
OBJECT_MANAGER_IFACE = "org.freedesktop.DBus.ObjectManager"

REMOTE_NAME_KEYWORDS = (
    "siri remote",
    "apple tv remote",
    "apple remote",
)


@dataclass(frozen=True)
class RemoteDevice:
    path: str
    adapter_path: str
    address: str
    name: str
    alias: str
    paired: bool
    bonded: bool
    trusted: bool
    uuids: tuple[str, ...]
    modalias: str
    manufacturer_data: dict[int, bytes]
    reasons: tuple[str, ...] = field(default_factory=tuple)

    @property
    def display_name(self) -> str:
        return self.name or self.alias or self.address


def unwrap(value: Any) -> Any:
    while isinstance(value, Variant):
        value = value.value
    return value


def normalize_address(address: str) -> str:
    return address.strip().upper()


def decode_manufacturer_data(value: Any) -> dict[int, bytes]:
    value = unwrap(value)
    if not isinstance(value, dict):
        return {}

    result: dict[int, bytes] = {}
    for key, raw_data in value.items():
        company_id = int(unwrap(key))
        data = unwrap(raw_data)
        if isinstance(data, bytes):
            result[company_id] = data
        elif isinstance(data, bytearray):
            result[company_id] = bytes(data)
        elif isinstance(data, list):
            result[company_id] = bytes(int(byte) & 0xFF for byte in data)
    return result


def get_bool(props: dict[str, Any], key: str) -> bool:
    return bool(unwrap(props.get(key, False)))


def get_str(props: dict[str, Any], key: str) -> str:
    value = unwrap(props.get(key, ""))
    return value if isinstance(value, str) else ""


def get_uuids(props: dict[str, Any]) -> tuple[str, ...]:
    value = unwrap(props.get("UUIDs", []))
    if not isinstance(value, list):
        return ()
    return tuple(str(unwrap(uuid)).lower() for uuid in value)


def adapter_path_for_device(path: str) -> str:
    adapter_path, separator, _ = path.rpartition("/dev_")
    if not separator or not adapter_path:
        raise ValueError(f"cannot determine adapter path from BlueZ device path {path!r}")
    return adapter_path


def build_remote(path: str, props: dict[str, Any]) -> RemoteDevice:
    return RemoteDevice(
        path=path,
        adapter_path=adapter_path_for_device(path),
        address=normalize_address(get_str(props, "Address")),
        name=get_str(props, "Name"),
        alias=get_str(props, "Alias"),
        paired=get_bool(props, "Paired"),
        bonded=get_bool(props, "Bonded"),
        trusted=get_bool(props, "Trusted"),
        uuids=get_uuids(props),
        modalias=get_str(props, "Modalias"),
        manufacturer_data=decode_manufacturer_data(props.get("ManufacturerData", {})),
    )


def remote_match_reasons(device: RemoteDevice) -> tuple[str, ...]:
    reasons: list[str] = []
    name = f"{device.name} {device.alias}".lower()
    if any(keyword in name for keyword in REMOTE_NAME_KEYWORDS):
        reasons.append("remote-like name")
    if HID_SERVICE_UUID in device.uuids:
        reasons.append("HID service")
    if device.modalias.lower().startswith("bluetooth:v004c"):
        reasons.append("Apple modalias")
    apple_data = device.manufacturer_data.get(APPLE_COMPANY_ID)
    if apple_data is not None:
        reasons.append("Apple manufacturer data")
        if apple_data.startswith(APPLE_HID_MFR_PREFIX):
            reasons.append("Apple HID manufacturer prefix")

    has_name = "remote-like name" in reasons
    has_hid = "HID service" in reasons
    has_apple = any(
        reason in reasons
        for reason in (
            "Apple modalias",
            "Apple manufacturer data",
            "Apple HID manufacturer prefix",
        )
    )

    if has_hid and (has_apple or has_name):
        return tuple(reasons)
    return ()


def with_reasons(device: RemoteDevice, reasons: tuple[str, ...]) -> RemoteDevice:
    return RemoteDevice(
        path=device.path,
        adapter_path=device.adapter_path,
        address=device.address,
        name=device.name,
        alias=device.alias,
        paired=device.paired,
        bonded=device.bonded,
        trusted=device.trusted,
        uuids=device.uuids,
        modalias=device.modalias,
        manufacturer_data=device.manufacturer_data,
        reasons=reasons,
    )


async def get_managed_objects(bus: MessageBus) -> dict[str, dict[str, dict[str, Any]]]:
    introspection = await bus.introspect(BLUEZ_BUS, BLUEZ_ROOT)
    proxy = bus.get_proxy_object(BLUEZ_BUS, BLUEZ_ROOT, introspection)
    manager = proxy.get_interface(OBJECT_MANAGER_IFACE)
    objects = await manager.call_get_managed_objects()
    return objects


async def find_siri_remotes(
    bus: MessageBus, addresses: set[str] | None = None
) -> list[RemoteDevice]:
    objects = await get_managed_objects(bus)
    remotes: list[RemoteDevice] = []
    for path, interfaces in objects.items():
        props = interfaces.get(DEVICE_IFACE)
        if props is None:
            continue

        device = build_remote(path, props)
        if addresses is not None and device.address not in addresses:
            continue
        if not (device.paired or device.bonded):
            continue

        reasons = remote_match_reasons(device)
        if reasons:
            remotes.append(with_reasons(device, reasons))

    remotes.sort(key=lambda device: (device.address, device.path))
    return remotes


async def remove_device(bus: MessageBus, device: RemoteDevice) -> None:
    introspection = await bus.introspect(BLUEZ_BUS, device.adapter_path)
    proxy = bus.get_proxy_object(BLUEZ_BUS, device.adapter_path, introspection)
    adapter = proxy.get_interface(ADAPTER_IFACE)
    await adapter.call_remove_device(device.path)


def format_remote(device: RemoteDevice) -> str:
    reasons = ", ".join(device.reasons)
    states = []
    if device.paired:
        states.append("paired")
    if device.bonded:
        states.append("bonded")
    if device.trusted:
        states.append("trusted")
    state_text = "/".join(states) if states else "known"
    return (
        f"{device.address} {device.display_name!r} [{state_text}] "
        f"path={device.path} match={reasons}"
    )


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Unpair all previously paired/bonded Siri Remotes using BlueZ D-Bus."
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="List matching Siri Remotes but do not remove them.",
    )
    parser.add_argument(
        "--address",
        action="append",
        help="Only remove this Bluetooth address. May be provided multiple times.",
    )
    return parser


async def async_main(argv: Sequence[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    addresses = (
        {normalize_address(address) for address in args.address}
        if args.address is not None
        else None
    )

    bus = await MessageBus(bus_type=BusType.SYSTEM).connect()
    try:
        remotes = await find_siri_remotes(bus, addresses)
        if not remotes:
            target = " matching requested address" if addresses else ""
            print(f"No paired/bonded Siri Remote{target} found.")
            return 0

        print("Matched Siri Remote device(s):")
        for remote in remotes:
            print(f"  {format_remote(remote)}")

        if args.dry_run:
            print("Dry run: no devices removed.")
            return 0

        failures = 0
        for remote in remotes:
            try:
                await remove_device(bus, remote)
            except Exception as exc:
                failures += 1
                print(
                    f"Failed to unpair {remote.address} {remote.display_name!r}: {exc!r}",
                    file=sys.stderr,
                )
            else:
                print(f"Unpaired {remote.address} {remote.display_name!r}.")

        return 2 if failures else 0
    finally:
        bus.disconnect()


def main() -> int:
    try:
        return asyncio.run(async_main())
    except KeyboardInterrupt:
        print("Interrupted.", file=sys.stderr)
        return 130


if __name__ == "__main__":
    raise SystemExit(main())
