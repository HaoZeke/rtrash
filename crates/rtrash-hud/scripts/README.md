# rtrash-hud visual walkthrough

Same idea as anqa `.grok/skills/hud-visual-walkthrough`: Xephyr + shots, do not invent PNGs.

```
python3 crates/rtrash-hud/scripts/hud_walkthrough.py --dry-run --out tmp/hud-walk/latest
```

`--dry-run` writes `timings.json` and the step map. A live walk needs `Xephyr`, `rtrash-hud`, and `RTRASH_HUD_WINDOW=1`.
