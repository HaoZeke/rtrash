# Terminal demos

Reproducible **asciinema** casts and **GIF** previews for the README.

| File | Role |
|------|------|
| `sequence.sh` | Fixed FreeDesktop loop (put → list/status → restore → empty) |
| `record.sh` | Sandboxed recorder; needs `asciinema`, `agg`, `rtrash` |
| `rtrash-quickstart.cast` | Asciicast v2 |
| `rtrash-quickstart.gif` | Animated preview for GitHub README |

## Isolation (required)

`XDG_DATA_HOME` alone is **not** enough: unpinned `list` / `status` / `empty`
discover **volume** trash (`.Trash-$uid` on mounts) and can wipe real data.

`record.sh` therefore sets:

```bash
export XDG_DATA_HOME=…/xdg
export RTRASH_DEMO_PIN="--trash-dir=$XDG_DATA_HOME/Trash"
```

and `sequence.sh` refuses to run without `RTRASH_DEMO_PIN`, applying it on every
list/status/restore/empty. Re-record only via `./record.sh`.

```shell
./docs/demo/record.sh
./docs/demo/record.sh --dry-run
```
