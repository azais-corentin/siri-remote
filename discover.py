"""Capture BLE advertisements with full payload to identify a Siri Remote.

Run with the remote in pairing mode (hold MENU + Volume Up for a few seconds).
The remote's BLE address is randomized every ~20s until paired, so we log
*every* advertisement and look for a stable signal: name, service UUIDs,
manufacturer data, appearance, etc.

Output: every detection prints a single line; addresses are grouped at the
end with a stability summary so we can spot the moving target.
"""

import asyncio
import sys
from collections import defaultdict
from datetime import datetime

from bleak import BleakScanner
from bleak.backends.device import BLEDevice
from bleak.backends.scanner import AdvertisementData


# All advertisements seen, keyed by current address.
adverts: dict[str, list[tuple[float, AdvertisementData]]] = defaultdict(list)
start = datetime.now()


def fmt_mfr(mfr: dict[int, bytes]) -> str:
    if not mfr:
        return "-"
    return " ".join(f"{cid:04x}:{data.hex()}" for cid, data in mfr.items())


def fmt_svc_data(svc: dict[str, bytes]) -> str:
    if not svc:
        return "-"
    return " ".join(f"{u}:{d.hex()}" for u, d in svc.items())


def cb(device: BLEDevice, adv: AdvertisementData) -> None:
    ts = (datetime.now() - start).total_seconds()
    adverts[device.address].append((ts, adv))
    print(
        f"[{ts:7.2f}s] {device.address} rssi={adv.rssi:>4} "
        f"name={adv.local_name!r:<20} "
        f"svcs={list(adv.service_uuids) or '-'} "
        f"mfr={fmt_mfr(adv.manufacturer_data)} "
        f"svc_data={fmt_svc_data(adv.service_data)} "
        f"tx={adv.tx_power}"
    )


async def main(duration: float) -> None:
    print(f"Scanning for {duration:.0f}s. Hold MENU + Volume Up on the remote now.", file=sys.stderr)
    async with BleakScanner(detection_callback=cb):
        await asyncio.sleep(duration)

    # Summary: group by (sorted service_uuids, name, manufacturer-company-ids)
    # so randomized addresses that share a "fingerprint" collapse together.
    print("\n=== SUMMARY (grouped by fingerprint) ===", file=sys.stderr)
    groups: dict[tuple, list[str]] = defaultdict(list)
    for addr, hits in adverts.items():
        # Use the most recent advert as the representative.
        _, adv = hits[-1]
        fp = (
            adv.local_name or "",
            tuple(sorted(adv.service_uuids or [])),
            tuple(sorted(adv.manufacturer_data.keys())),
        )
        groups[fp].append(addr)

    for fp, addrs in sorted(groups.items(), key=lambda kv: -len(kv[1])):
        name, svcs, mfr_ids = fp
        print(
            f"  name={name!r} svcs={list(svcs) or '-'} "
            f"mfr_companies={[f'0x{c:04x}' for c in mfr_ids] or '-'} "
            f"-> {len(addrs)} addresses",
            file=sys.stderr,
        )
        for a in addrs[:5]:
            print(f"      {a}", file=sys.stderr)
        if len(addrs) > 5:
            print(f"      ... ({len(addrs) - 5} more)", file=sys.stderr)


if __name__ == "__main__":
    duration = float(sys.argv[1]) if len(sys.argv) > 1 else 45.0
    asyncio.run(main(duration))
