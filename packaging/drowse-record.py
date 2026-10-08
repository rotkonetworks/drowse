#!/usr/bin/env python3
"""Append one drowse metrics sample to a JSONL log.

Queries drowsed over its unix socket for the current scorecard (battery and
package power, Sleep Efficiency) and power-save state, and appends one line:

  {"ts": 1699999999, "on_battery": true, "power_save": false,
   "batt_w": 8.2, "pkg_w": 3.1, "se": 61.0, "grade": "C"}

Run it on a timer (see drowse-record.timer). Analyze with drowse-metrics.
Never raises on a transient daemon hiccup — it just skips that sample.
"""
import json
import os
import socket
import sys
import time

SOCK = "/run/drowsed/sock"
LOG = os.path.expanduser("~/.local/share/drowse/metrics.jsonl")


def rpc(conn, method, params=None, _id=1):
    req = {"jsonrpc": "2.0", "id": _id, "method": method}
    if params:
        req["params"] = params
    conn.sendall((json.dumps(req) + "\n").encode())
    buf = b""
    while b"\n" not in buf:
        chunk = conn.recv(65536)
        if not chunk:
            break
        buf += chunk
    return json.loads(buf.split(b"\n")[0])


def main():
    try:
        c = socket.socket(socket.AF_UNIX)
        c.settimeout(5)
        c.connect(SOCK)
        score = rpc(c, "score.get", None, 1).get("result", {})
        status = rpc(c, "power.status", None, 2).get("result", {})
    except Exception as e:
        # Transient: daemon restarting, socket gone. Skip this sample quietly.
        print(f"drowse-record: skip ({e})", file=sys.stderr)
        return 0

    batt = score.get("battery") or {}
    sc = score.get("score") or {}
    row = {
        "ts": int(time.time()),
        "on_battery": bool(status.get("on_battery", batt.get("discharging", False))),
        "power_save": bool(status.get("power_save_active", False)),
        "batt_w": round(batt.get("power_w"), 3) if batt.get("power_w") is not None else None,
        "pkg_w": round(score.get("package_power_w"), 3) if score.get("package_power_w") is not None else None,
        "se": round(sc.get("sleep_efficiency"), 1) if sc.get("sleep_efficiency") is not None else None,
        "grade": sc.get("grade"),
        "soc": status.get("battery_pct"),  # charge %, for the real discharge curve
    }
    os.makedirs(os.path.dirname(LOG), exist_ok=True)
    with open(LOG, "a") as f:
        f.write(json.dumps(row) + "\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
