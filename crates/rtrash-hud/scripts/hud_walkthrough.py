#!/usr/bin/env python3
"""rtrash-hud visual walkthrough harness (anqa-shaped).

Does not invent screenshots. --dry-run writes the step map and exits 0
so CI can lock the contract without Xephyr.
"""

from __future__ import annotations

import argparse
import json
from pathlib import Path

STEPS = [
    {"id": "00-boot", "action": "start rtrash-hud --offline or window mode", "pane": "list"},
    {"id": "01-summon", "action": "show window (RTRASH_HUD_WINDOW=1)", "pane": "list"},
    {"id": "02-list", "action": "catalog rows", "pane": "list"},
    {"id": "03-restore", "action": "digit 2 restore pane", "pane": "restore"},
    {"id": "04-empty", "action": "digit 3 empty pane", "pane": "empty"},
    {"id": "05-put", "action": "digit 4 put pane", "pane": "put"},
    {"id": "06-status", "action": "digit 5 status pane", "pane": "status"},
    {"id": "07-help", "action": "? help overlay", "pane": "help"},
]


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--out", default="tmp/hud-walk/latest")
    p.add_argument("--dry-run", action="store_true")
    p.add_argument("--backend", default="xephyr")
    args = p.parse_args()
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    shots = out / "shots"
    shots.mkdir(exist_ok=True)
    timings = {
        "backend": args.backend,
        "window_mode": True,
        "dry_run": args.dry_run,
        "steps": [{"id": s["id"], "action": s["action"], "pane": s["pane"]} for s in STEPS],
    }
    (out / "timings.json").write_text(json.dumps(timings, indent=2) + "\n")
    if args.dry_run:
        print(f"out_dir={out}")
        print("dry-run: no screenshots (do not invent PNGs)")
        return 0
    print("Xephyr walk not run; pass --dry-run or provide DISPLAY + rtrash-hud")
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
