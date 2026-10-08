#!/usr/bin/env python3
"""Analyze the drowse metrics log: does power-save actually save battery?

Reads ~/.local/share/drowse/metrics.jsonl and compares battery discharge rate
(watts) with power-save ON vs OFF, on battery only (the fair comparison — on
AC the battery isn't discharging). Reports mean/median draw per state, the
delta, and the estimated runtime change for a nominal battery.

Honest about the method: passive logging mixes workloads, so the delta is an
observational estimate, not a controlled experiment. More samples = more trust.
"""
import json
import os
import sys

LOG = os.path.expanduser("~/.local/share/drowse/metrics.jsonl")
BATTERY_WH = 42.7  # GPD Pocket 4 design energy; override with arg 2


def median(xs):
    s = sorted(xs)
    n = len(s)
    if n == 0:
        return None
    return s[n // 2] if n % 2 else (s[n // 2 - 1] + s[n // 2]) / 2


def summarize(rows):
    ws = [r["batt_w"] for r in rows if r.get("batt_w")]
    ses = [r["se"] for r in rows if r.get("se") is not None]
    return {
        "n": len(rows),
        "mean_w": sum(ws) / len(ws) if ws else None,
        "med_w": median(ws),
        "mean_se": sum(ses) / len(ses) if ses else None,
    }


def fmt(x, u=""):
    return f"{x:.2f}{u}" if isinstance(x, float) else "—"


def main():
    path = sys.argv[1] if len(sys.argv) > 1 else LOG
    wh = float(sys.argv[2]) if len(sys.argv) > 2 else BATTERY_WH
    if not os.path.exists(path):
        print(f"no metrics yet at {path}\nstart recording: systemctl --user enable --now drowse-record.timer")
        return 1
    rows = []
    for line in open(path):
        line = line.strip()
        if line:
            try:
                rows.append(json.loads(line))
            except json.JSONDecodeError:
                pass

    # Fair comparison: on battery, discharging.
    on_batt = [r for r in rows if r.get("on_battery") and r.get("batt_w")]
    save = [r for r in on_batt if r.get("power_save")]
    nosave = [r for r in on_batt if not r.get("power_save")]
    s, n = summarize(save), summarize(nosave)

    span_h = (rows[-1]["ts"] - rows[0]["ts"]) / 3600 if len(rows) > 1 else 0
    print(f"drowse metrics — {len(rows)} samples over {span_h:.1f}h  (battery {wh:.0f}Wh)")
    print(f"  on battery: {len(on_batt)} samples  ({len(save)} power-save, {len(nosave)} normal)\n")
    print(f"  {'state':<12}{'n':>5}{'mean W':>10}{'median W':>11}{'mean SE':>10}{'est. runtime':>15}")
    for name, g in (("power-save", s), ("normal", n)):
        rt = f"{wh / g['mean_w']:.1f} h" if g["mean_w"] else "—"
        print(f"  {name:<12}{g['n']:>5}{fmt(g['mean_w']):>10}{fmt(g['med_w']):>11}{fmt(g['mean_se']):>10}{rt:>15}")

    if s["mean_w"] and n["mean_w"]:
        dw = n["mean_w"] - s["mean_w"]
        pct = dw / n["mean_w"] * 100
        rt_save = wh / s["mean_w"]
        rt_norm = wh / n["mean_w"]
        print()
        if dw > 0:
            print(f"  => power-save draws {fmt(dw,' W')} less ({pct:.0f}%),")
            print(f"     ~{rt_save:.1f} h vs ~{rt_norm:.1f} h runtime (+{(rt_save-rt_norm)*60:.0f} min).")
        else:
            print(f"  => no measured saving yet (power-save {fmt(-dw,' W')} higher) — likely")
            print(f"     workload differences; keep logging across comparable idle periods.")
        if min(s["n"], n["n"]) < 20:
            print(f"  note: few samples in one bucket (min {min(s['n'], n['n'])}); treat as provisional.")
    else:
        need = "power-save" if not s["mean_w"] else "normal"
        print(f"\n  need battery samples in the '{need}' state — toggle it and let the timer log.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
