# Getting started

Docs: <https://rtrash.rgoswami.me>

## When to use what

| Need | Use |
|------|-----|
| Interactive shell / scripts | `rtrash` CLI (or multi-call `trash-put` etc.) |
| Safer than `os.remove` in Python | `import rtrash; rtrash.unlink(path)` |
| Drop-in mental model for `rm` | `rtrash -rf path` or symlink `rm` → put |
| Permanent purge of trash entries | `rtrash empty` / `rtrash rm PATTERN` |
| Windows / macOS system trash | **Not supported** (Linux FreeDesktop only) |

- [architecture.md](architecture.md) — FreeDesktop placement, fail-safes
- [benchmarks.md](benchmarks.md) — measured comparison vs trash-cli
- [bindings.md](bindings.md) — Python API

## Install

Published on **[crates.io](https://crates.io/crates/rtrash)** and **[PyPI](https://pypi.org/project/rtrash/)**.

### CLI (Rust)

```shell
cargo install rtrash          # from crates.io (needs Rust toolchain)
# or prebuilt musl on x86_64 / aarch64 Linux:
cargo binstall rtrash
rtrash setup
```

`cargo binstall` pulls GitHub Release musl assets (`rtrash-<version>-{x86_64,aarch64}-unknown-linux-musl.tar.gz`).
Metadata remaps typical glibc hosts (`*-unknown-linux-gnu`) on those arches to the matching musl tarball.
Always finish with **`rtrash setup`** (multi-call links, bash/zsh/fish completions, man under `~/.local`).

**Manual tarball:** download the Release asset or run `./scripts/package-release.sh`, then `rtrash setup`.

**Tip of main:**

```shell
cargo install --git https://github.com/HaoZeke/rtrash
rtrash setup
```

### Python

```shell
pip install rtrash
python -c "import rtrash; print(rtrash.version())"
```

Linux **x86_64** wheels for CPython **3.10–3.14**. Dev checkout: `pip install maturin && maturin develop --features python`.

## Shortest CLI path

```shell
echo data > scratch.txt
rtrash scratch.txt
rtrash list
rtrash restore scratch.txt          # by original path
# or: rtrash restore   # TUI picker on a TTY
rtrash scratch.txt
rtrash empty --trash-dir="$XDG_DATA_HOME/Trash"
```

### Interactive restore (first-class)

On a TTY, bare `rtrash restore` opens the **ratatui restore browser** — intentional
product UX, not a script-only fallback.

| Key | Action |
|-----|--------|
| `↑` `↓` / `j` `k` | Move selection |
| `Space` | Toggle mark (multi-select) |
| `a` / `A` | Mark all visible / clear marks |
| `/` | **Fuzzy** filter by original path |
| `Enter` | Restore marked items (or cursor if none marked) |
| `f` | Toggle force overwrite |
| `y` / `n` | Confirm overwrite / bulk restore |
| `q` / `Esc` | Quit |

Also: **`rtrash empty`** (TTY) multi-select permanent delete; **`rtrash put`** (TTY, no files) multi-select put from the current directory. Use `--plain` to force classic CLI behavior.

For automation: `--plain` or pipe an index (`printf '0\n' | rtrash restore`).

```shell
rtrash a.txt b.txt
rtrash restore              # TUI on TTY
rtrash restore --cwd-only   # only originals under $PWD
rtrash restore --plain      # line mode even on TTY
```


## Shortest Python path

```python
from pathlib import Path
import rtrash

p = Path("scratch.txt")
p.write_text("data")
rtrash.unlink(p)          # was: p.unlink() / os.remove(p)
rtrash.restore_path(p)
```

## Changelog

Release notes: [CHANGELOG.md](../CHANGELOG.md). For unreleased work, add a towncrier fragment under `docs/newsfragments/` (see README *Contributing and releases*).
