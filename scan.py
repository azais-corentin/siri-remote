import os

# WebAgg serves the plot over HTTP (auto-opens a browser tab on
# http://127.0.0.1:8988/) so we don't need Qt / Tk / GTK installed.
os.environ.setdefault("MPLBACKEND", "WebAgg")
# Python's webbrowser module honours $BROWSER; force Firefox instead of
# whatever xdg-open resolves to.
os.environ["BROWSER"] = "firefox"

import asyncio
import threading
from datetime import datetime

import matplotlib.pyplot as plt
from bleak import BleakScanner
from matplotlib.animation import FuncAnimation

# Per-device (xs, ys) series. matplotlib's color cycle assigns each new
# device its own line color the first time we plot it.
data: dict[str, tuple[list[float], list[int]]] = {}
names: dict[str, str] = {}
start = datetime.now()
lock = threading.Lock()


def cb(device, adv):
    ts = (datetime.now() - start).total_seconds()
    addr = device.address
    with lock:
        if device.name:
            names[addr] = device.name
        xs, ys = data.setdefault(addr, ([], []))
        xs.append(ts)
        ys.append(adv.rssi)


async def scanner():
    async with BleakScanner(detection_callback=cb):
        while True:
            await asyncio.sleep(3600)


def scan_thread():
    asyncio.run(scanner())


fig, ax = plt.subplots()
ax.set_xlabel("Time (s)")
ax.set_ylabel("RSSI (dBm)")
ax.set_title("BLE RSSI vs Time")
ax.grid(True, alpha=0.3)
lines: dict[str, "plt.Line2D"] = {}


def update(_frame):
    with lock:
        snapshot = {a: (list(xs), list(ys)) for a, (xs, ys) in data.items()}
        names_snap = dict(names)

    for addr, (xs, ys) in snapshot.items():
        label = f"{addr} {names_snap.get(addr, '')}".strip()
        if addr not in lines:
            (line,) = ax.plot(xs, ys, marker=".", linewidth=1, label=label)
            lines[addr] = line
        else:
            lines[addr].set_data(xs, ys)
            lines[addr].set_label(label)

    if lines:
        ax.relim()
        ax.autoscale_view()
        ax.legend(loc="lower left", fontsize="x-small", ncol=2)
    return list(lines.values())


threading.Thread(target=scan_thread, daemon=True).start()
ani = FuncAnimation(fig, update, interval=500, cache_frame_data=False)
plt.show()
