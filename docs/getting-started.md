# Getting started

Docs: <https://rtrash.rgoswami.me>

## 30-second story

| Step | Command |
|------|---------|
| Install | `cargo binstall rtrash && rtrash setup` (or `cargo install` / `pip install rtrash`) |
| Trash files | `rtrash file` or `rtrash put -rf dir` (rm-shaped flags) |
| See trash | `rtrash list` · `rtrash status` |
| Bring back | `rtrash restore` (TUI on TTY) or `rtrash restore --plain PATH` |
| Wipe | `rtrash empty` (TUI) or `rtrash empty --plain` |
| Scripts | always prefer `--plain` / non-TTY; pin `--trash-dir=` or isolate `XDG_DATA_HOME` |

Animated loop (put → list → restore → empty):

<p align="center">
  <img src="https://raw.githubusercontent.com/HaoZeke/rtrash/main/docs/demo/rtrash-quickstart.gif" width="720" alt="rtrash quickstart demo" />
</p>

Broader surface (rm flags, multi-call, `rm` globs, dry-run, keys):

<p align="center">
  <img src="https://raw.githubusercontent.com/HaoZeke/rtrash/main/docs/demo/rtrash-suite.gif" width="720" alt="rtrash suite demo" />
</p>

Regenerate: [`docs/demo/record.sh`](demo/record.sh). Platform: Linux FreeDesktop primary; macOS experimental home FDO trash; Windows system Recycle Bin.

## When to use what

| Need | Use |
|------|-----|
| Interactive shell / scripts | `rtrash` CLI (or multi-call `trash-put` etc.) |
| Safer than `os.remove` in Python | `import rtrash; rtrash.unlink(path)` |
| Drop-in mental model for `rm` | `rtrash -rf path` or symlink `rm` → put |
| Permanent purge of trash entries | `rtrash empty` / `rtrash rm PATTERN` |
| Windows system Recycle Bin | `rtrash put/list/restore/empty` (from source; not FreeDesktop layout) |
| macOS Finder Trash | **Not supported** — experimental FreeDesktop **home** trash only |

- [architecture.md](architecture.md) — FreeDesktop placement, fail-safes
- [benchmarks.md](benchmarks.md) — measured comparison vs trash-cli
- [bindings.md](bindings.md) — Python API
- [demo/](demo/) — terminal cast/GIF for README

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
Always finish with **`rtrash setup`** (multi-call links, bash/zsh/fish completions, man, and a Midnight Commander menu sample under `~/.local`).

Scripts: `rtrash list --json`, `rtrash list --older-than=30`, `rtrash status --json`.

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

Shared keys for **restore**, **empty**, and **put** TUI browsers:

| Key | Action |
|-----|--------|
| `↑` `↓` / `j` `k` | Move selection |
| `PgUp` / `PgDn` | Page by viewport |
| `g` / `G` (or Home/End) | First / last item |
| `Space` | Toggle mark (multi-select) |
| `a` / `A` | Mark all visible / clear marks |
| `/` | **Live fuzzy** filter (refilters each keystroke; Enter commit, Esc restore prior) |
| `?` | Help overlay (same core map in every browser) |
| `Enter` | Primary action: restore / purge / put (marked, or cursor if none) |
| `y` / `n` | Confirm / cancel bulk destructive actions |
| `q` / `Esc` | Quit (browse mode) |

Browser-specific: restore `f` force · empty `n` dry-run · put `r` recursive / `f` force.

### Custom keybinds

All TUI actions are remappable:

```shell
mkdir -p "${XDG_CONFIG_HOME:-$HOME/.config}/rtrash"
rtrash keys --sample > "${XDG_CONFIG_HOME:-$HOME/.config}/rtrash/keys.toml"
# edit keys.toml, e.g.:
#   [keys]
#   move_down = ["down", "j", "J"]
#   toggle_mark = ["space", "x"]
rtrash keys --list    # show resolved map
```

Override path with `RTRASH_KEYS=/path/to/keys.toml`. Unlisted actions keep defaults.

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
