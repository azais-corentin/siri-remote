"""Find and pair an Apple TV Siri Remote (3rd gen) over BLE.

Identification strategy (derived from running discover.py with the remote in
pairing mode):

  * HID Service UUID `00001812-0000-1000-8000-00805f9b34fb` is advertised
  * Apple manufacturer ID `0x004C` is present in manufacturer data
  * Manufacturer data starts with `07 0d ...` (Apple's HID-over-GATT prefix)
  * RSSI >= -55 dBm (the remote must be very close; avoids picking up another
    Siri Remote in the room)

Because the remote's BLE address is randomized every ~20s until paired, we
identify by *fingerprint*, not address. Once a strong candidate is locked
in, we open a BleakClient against its current address; BlueZ tracks the
device by its identity-resolving key after pairing so the rotating
public-random address stops mattering.

Pairing on Linux requires a registered BlueZ agent to confirm passkey
requests — bleak alone doesn't provide one, which is why every pair
attempt past the very first fails with `AuthenticationFailed`. We register
a `NoInputNoOutput` agent via dbus-fast (a bleak transitive dependency,
already installed) which forces the association down to Just Works and
auto-confirms.

Usage:
    uv run python pair.py

Hold MENU + Volume Up on the remote for ~5s to enter pairing mode before
launching the script.
"""

import asyncio
import sys
from dataclasses import dataclass, field

from bleak import BleakClient, BleakScanner
from bleak.backends.device import BLEDevice
from bleak.backends.scanner import AdvertisementData
from dbus_fast import BusType
from dbus_fast.aio import MessageBus
from dbus_fast.service import ServiceInterface, method


# -- Identification ------------------------------------------------------------

HID_SERVICE_UUID = "00001812-0000-1000-8000-00805f9b34fb"
APPLE_COMPANY_ID = 0x004C
APPLE_HID_MFR_PREFIX = bytes([0x07, 0x0D])  # observed prefix in pairing-mode ads
MIN_RSSI = -55
SCAN_SETTLE_SECONDS = 5.0


def is_siri_remote(adv: AdvertisementData) -> bool:
    """Return True if this advertisement matches a Siri Remote in pairing mode."""
    if adv.rssi < MIN_RSSI:
        return False
    if HID_SERVICE_UUID not in (adv.service_uuids or []):
        return False
    mfr = adv.manufacturer_data.get(APPLE_COMPANY_ID)
    if mfr is None or not mfr.startswith(APPLE_HID_MFR_PREFIX):
        return False
    return True


# -- Scan / candidate selection ------------------------------------------------


@dataclass
class Candidate:
    """One physical Siri Remote, identified by its identity address.

    Because the BLE address rotates every ~20s in pairing mode, multiple
    `BLEDevice` instances may correspond to the same remote. We group them by
    `identity_address` (extracted from the Apple manufacturer data) and
    always remember the *most recently seen* BLEDevice/address — older
    rotations stop being advertised and would time out on connect.
    """

    identity_address: str
    rssis: list[int] = field(default_factory=list)
    last_device: BLEDevice | None = None
    last_address: str | None = None
    last_seen: float = 0.0

    @property
    def hits(self) -> int:
        return len(self.rssis)

    @property
    def mean_rssi(self) -> float:
        return sum(self.rssis) / len(self.rssis) if self.rssis else float("-inf")

    @property
    def last_rssi(self) -> int | None:
        return self.rssis[-1] if self.rssis else None


def extract_identity_address(adv: AdvertisementData) -> str | None:
    """Pull the Siri Remote's identity address out of the Apple manufacturer
    data.

    Observed manufacturer data (Apple, 0x004C) in pairing mode looks like:

        07 0d 02 15 03 02 | <6-byte identity address> | 4f 50 50

    BlueZ resolves and bonds the device under this identity address (verified
    via `bluetoothctl devices Bonded` after a successful pair).
    """
    mfr = adv.manufacturer_data.get(APPLE_COMPANY_ID)
    if mfr is None or len(mfr) < 12 or not mfr.startswith(APPLE_HID_MFR_PREFIX):
        return None
    return ":".join(f"{b:02X}" for b in mfr[6:12])


# Drop candidates whose last advertisement is older than this many seconds.
# Apple rotates the BLE address every ~20s; once it rotates, the old address
# stops being advertised and connecting to it will time out. We err well
# under the rotation period.
STALE_AFTER_SECONDS = 5.0


async def scan_for_remote(settle: float) -> tuple[BLEDevice, Candidate]:
    """Scan until we have a confident lock on a Siri Remote.

    Selection rule: among candidates whose most recent advertisement is fresh
    (within `STALE_AFTER_SECONDS`) and that have >= 2 hits, pick the highest
    mean RSSI. Keyed by identity address so multiple BLE-address rotations of
    the same remote collapse into one candidate.
    """
    candidates: dict[str, Candidate] = {}
    loop = asyncio.get_running_loop()

    def cb(device: BLEDevice, adv: AdvertisementData) -> None:
        if not is_siri_remote(adv):
            return
        identity = extract_identity_address(adv)
        if identity is None:
            # Filter requires the Apple-HID mfr prefix, so identity should be
            # extractable; if not, the advert doesn't look right — skip it.
            return
        c = candidates.setdefault(identity, Candidate(identity_address=identity))
        c.rssis.append(adv.rssi)
        c.last_device = device
        c.last_address = device.address
        c.last_seen = loop.time()
        print(
            f"  identity={identity} addr={device.address} rssi={adv.rssi} "
            f"hits={c.hits} mean_rssi={c.mean_rssi:.1f}",
            file=sys.stderr,
        )

    print(
        f"Scanning for {settle:.0f}s for a Siri Remote in pairing mode...",
        file=sys.stderr,
    )
    async with BleakScanner(detection_callback=cb) as scanner:
        await asyncio.sleep(settle)

        while True:
            now = loop.time()
            fresh = [
                c
                for c in candidates.values()
                if c.hits >= 2
                and c.last_device is not None
                and (now - c.last_seen) <= STALE_AFTER_SECONDS
            ]
            ranked = sorted(fresh, key=lambda c: c.mean_rssi, reverse=True)
            if ranked:
                best = ranked[0]
                assert best.last_device is not None
                print(
                    f"\nLocked on identity {best.identity_address} via current "
                    f"address {best.last_address} "
                    f"(mean RSSI {best.mean_rssi:.1f} over {best.hits} adverts, "
                    f"last rssi {best.last_rssi})",
                    file=sys.stderr,
                )
                if len(ranked) > 1:
                    print(
                        f"  ({len(ranked) - 1} other Siri Remote(s) also in "
                        f"range; picked the one with the strongest signal)",
                        file=sys.stderr,
                    )
                return best.last_device, best

            print(
                "  no fresh qualifying candidate yet; scanning 2s more...",
                file=sys.stderr,
            )
            await asyncio.sleep(2.0)
            _ = scanner  # keep the scanner alive in this loop

# -- BlueZ NoInputNoOutput Agent ----------------------------------------------
#
# Without a registered agent, BlueZ requests passkey confirmation (numeric
# comparison) for the Siri Remote's pairing and gets back "Operation not
# permitted" because no agent is wired up — pairing then fails with
# `AuthenticationFailed`. Observed in journalctl:
#
#   src/device.c:new_auth() No agent available for request type 2
#   device_confirm_passkey: Operation not permitted
#
# A `NoInputNoOutput` agent forces the Bluetooth association model down to
# Just Works (no confirmation, no MITM protection), which is what the remote
# actually needs since it has no display or keyboard.

_AGENT_PATH = "/com/example/bleak_pair_agent"
_BLUEZ_BUS = "org.bluez"
_BLUEZ_PATH = "/org/bluez"


class _AutoConfirmAgent(ServiceInterface):
    """BlueZ Agent1 that approves every pairing request automatically.

    The interface signatures are dictated by the org.bluez.Agent1 D-Bus
    contract; this implementation auto-grants everything because we already
    decided to pair the discovered device before registering this agent.
    """

    def __init__(self) -> None:
        super().__init__("org.bluez.Agent1")

    @method()
    def Release(self) -> None:
        pass

    @method()
    def RequestPinCode(self, device: "o") -> "s":  # noqa: F821 (D-Bus type str)
        return "0000"

    @method()
    def DisplayPinCode(self, device: "o", pincode: "s") -> None:  # noqa: F821
        pass

    @method()
    def RequestPasskey(self, device: "o") -> "u":  # noqa: F821
        return 0

    @method()
    def DisplayPasskey(self, device: "o", passkey: "u", entered: "q") -> None:  # noqa: F821
        pass

    @method()
    def RequestConfirmation(self, device: "o", passkey: "u") -> None:  # noqa: F821
        # No-op return == confirmed. Raising would reject the pairing.
        print(
            f"  agent: auto-confirming pair request for {device} (passkey={passkey})",
            file=sys.stderr,
        )

    @method()
    def RequestAuthorization(self, device: "o") -> None:  # noqa: F821
        pass

    @method()
    def AuthorizeService(self, device: "o", uuid: "s") -> None:  # noqa: F821
        pass

    @method()
    def Cancel(self) -> None:
        pass


class _AgentSession:
    """Async context manager that registers the agent for the duration of
    the `async with` block and unregisters it on exit."""

    def __init__(self) -> None:
        self._bus: MessageBus | None = None
        self._agent: _AutoConfirmAgent | None = None

    async def __aenter__(self) -> "_AgentSession":
        self._bus = await MessageBus(bus_type=BusType.SYSTEM).connect()
        self._agent = _AutoConfirmAgent()
        self._bus.export(_AGENT_PATH, self._agent)

        introspection = await self._bus.introspect(_BLUEZ_BUS, _BLUEZ_PATH)
        proxy = self._bus.get_proxy_object(_BLUEZ_BUS, _BLUEZ_PATH, introspection)
        agent_mgr = proxy.get_interface("org.bluez.AgentManager1")
        await agent_mgr.call_register_agent(_AGENT_PATH, "NoInputNoOutput")
        await agent_mgr.call_request_default_agent(_AGENT_PATH)
        print(
            "Registered NoInputNoOutput agent (forces Just Works pairing).",
            file=sys.stderr,
        )
        return self

    async def __aexit__(self, *exc: object) -> None:
        if self._bus is None:
            return
        try:
            introspection = await self._bus.introspect(_BLUEZ_BUS, _BLUEZ_PATH)
            proxy = self._bus.get_proxy_object(
                _BLUEZ_BUS, _BLUEZ_PATH, introspection
            )
            agent_mgr = proxy.get_interface("org.bluez.AgentManager1")
            await agent_mgr.call_unregister_agent(_AGENT_PATH)
        except Exception as e:
            print(f"  warning: agent unregister failed: {e!r}", file=sys.stderr)
        finally:
            self._bus.disconnect()



# -- Pair + connect + dump services -------------------------------------------


async def _verify_bonded(identity: str) -> bool:
    """Return True if BlueZ has bonded the device under `identity`.

    We shell out to bluetoothctl because bleak doesn't expose a stable
    cross-platform `is_bonded` property; on Linux this is the source of
    truth for whether pairing actually completed.
    """
    proc = await asyncio.create_subprocess_exec(
        "bluetoothctl",
        "devices",
        "Bonded",
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.DEVNULL,
    )
    out, _ = await proc.communicate()
    return identity.upper() in out.decode("utf-8", "ignore").upper()


def _retry_message(detail: str) -> str:
    return (
        f"\nPair failed: {detail}\n"
        "\nThis is a known BlueZ flake with Apple HID peripherals. Once a pair\n"
        "attempt fails on Linux, BlueZ removes the device entry and the BLE\n"
        "address has likely also rotated, so an in-process retry can't recover.\n"
        "\nTo recover:\n"
        "  1. Put the remote back in pairing mode (hold MENU + Volume Up ~5s).\n"
        "  2. Re-run: uv run python pair.py\n"
        "\nIf this keeps happening, restart bluetoothd to clear any stuck\n"
        "session state:  systemctl restart bluetooth\n"
    )


async def pair_and_dump(device: BLEDevice, candidate: Candidate) -> None:
    identity = candidate.identity_address
    print(f"\nConnecting to {device.address} (with pair=True) ...", file=sys.stderr)
    # `pair=True` makes bleak request bonding *before* service discovery. The
    # Siri Remote's services require an encrypted link, so without this the
    # post-connect service discovery races the pairing handshake and the
    # device disconnects mid-discovery. With pair=True, BlueZ negotiates the
    # bond (Just Works for the remote's NoInputNoOutput capability) up front.
    #
    # The agent registered by `_AgentSession` is what makes BlueZ accept the
    # numeric-comparison request that the remote sends during pairing. With
    # `NoInputNoOutput` capability, the association procedure degrades to
    # Just Works and the agent auto-confirms it.
    try:
        async with _AgentSession(), BleakClient(device, pair=True) as client:
            print("Connected and paired.", file=sys.stderr)

            # Ground-truth check: BlueZ must actually have bonded the device
            # under its identity address. Without this, the pairing didn't
            # stick even though the link came up.
            if not await _verify_bonded(identity):
                raise RuntimeError(
                    _retry_message(
                        f"connect+pair reported success but BlueZ does not "
                        f"list {identity} as bonded"
                    )
                )

            print(
                f"Identity address (from advertisement manufacturer data): "
                f"{identity}",
                file=sys.stderr,
            )
            print(
                "BlueZ now bonds the remote under this stable address (IRK "
                "exchange). Use it for future reconnects; the random "
                "advertising address no longer matters.",
                file=sys.stderr,
            )

            print("\nGATT services:")
            for service in client.services:
                print(f"Service {service.uuid}  ({service.description})")
                for char in service.characteristics:
                    props = ",".join(char.properties)
                    print(f"  Char  {char.uuid}  [{props}]  ({char.description})")
                    for desc in char.descriptors:
                        print(f"    Desc  {desc.uuid}  ({desc.description})")

            print(
                "\nPaired and connected. Keeping the connection open for 5s...",
                file=sys.stderr,
            )
            await asyncio.sleep(5.0)
    except RuntimeError:
        # Already a formatted message from _retry_message — pass through.
        raise
    except Exception as e:
        raise RuntimeError(_retry_message(repr(e))) from e


async def main() -> int:
    try:
        device, candidate = await asyncio.wait_for(
            scan_for_remote(SCAN_SETTLE_SECONDS), timeout=60.0
        )
    except asyncio.TimeoutError:
        print(
            "Timed out waiting for a Siri Remote. Make sure it's in pairing mode "
            "(MENU + Volume Up held for ~5s) and within reach (RSSI >= -55).",
            file=sys.stderr,
        )
        return 1

    try:
        await pair_and_dump(device, candidate)
    except RuntimeError as e:
        # pair_and_dump produces a fully-formatted, multi-line message.
        print(str(e), file=sys.stderr)
        return 2
    except Exception as e:
        print(f"\nUnexpected error during pair/connect: {e!r}", file=sys.stderr)
        return 2

    return 0


if __name__ == "__main__":
    raise SystemExit(asyncio.run(main()))
