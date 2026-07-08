# Terminal demos

Reproducible **asciinema** casts and **GIF** previews for the README.

| Asset | Sequence | Shows |
|-------|----------|--------|
| `rtrash-quickstart.{cast,gif}` | `sequence.sh` | put → list/status → restore → empty |
| `rtrash-suite.{cast,gif}` | `sequence-suite.sh` | `-rf` put, multi-call, `rm` globs, dry-run empty, `keys` |

| Script | Role |
|--------|------|
| `record.sh` | Sandboxed recorder (`asciinema` + `agg`) |
| `sequence.sh` | Quickstart FreeDesktop loop |
| `sequence-suite.sh` | Broader CLI surface (still non-TUI) |

## Isolation (required)

`XDG_DATA_HOME` alone is **not** enough: unpinned suite commands discover
**volume** trash. `record.sh` sets:

```bash
export RTRASH_DEMO_PIN="--trash-dir=$XDG_DATA_HOME/Trash"
```

Sequences refuse to run without the pin. Cleanup uses `/bin/rm` so multi-call
`rm` → rtrash never intercepts sandbox teardown.

```shell
./docs/demo/record.sh              # both demos
./docs/demo/record.sh suite        # suite only
./docs/demo/record.sh --dry-run
```

**Not automated:** ratatui TUI browsers (need a real TTY). Covered in README text.
