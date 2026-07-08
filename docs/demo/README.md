# Terminal demos

Reproducible **asciinema** casts and **GIF** previews for the README.

| File | Role |
|------|------|
| `sequence.sh` | Fixed FreeDesktop loop (put → list/status → restore → empty) |
| `record.sh` | Sandboxed recorder (`XDG_DATA_HOME` isolated); needs `asciinema`, `agg`, `rtrash` |
| `rtrash-quickstart.cast` | Asciicast v2 (regenerate with `./record.sh`) |
| `rtrash-quickstart.gif` | Animated preview for GitHub README |

```shell
# after cargo build --release (or cargo install / binstall):
./docs/demo/record.sh
./docs/demo/record.sh --dry-run   # print sequence only
```

Never hand-edit the GIF; change `sequence.sh` and re-record.
