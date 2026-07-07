# Getting started

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

### CLI (Rust)

```shell
cargo install --git https://github.com/HaoZeke/rtrash
rtrash setup
```

`rtrash setup` installs multi-call links, bash/zsh/**fish** completions, and the
man page under `~/.local` from **embedded** assets (no clone required). Use
`--dry-run` to preview, `--force` after upgrades, `--with-rm` to also link
`rm` → put into trash. Packagers: `rtrash completions bash|zsh|fish`,
`rtrash man`, or `rtrash setup --prefix=/usr`.

**Without a full Rust toolchain:** use a musl release tarball from GitHub
Releases (tag `v*`) or build one with `./scripts/package-release.sh` (see
README). Optional: `cargo binstall --git https://github.com/HaoZeke/rtrash rtrash`
when a release asset is published.

### Python (maturin / PyO3)

```shell
pip install maturin
maturin develop --features python
python -c "import rtrash; print(rtrash.version())"
```

## Shortest CLI path

```shell
echo data > scratch.txt
rtrash scratch.txt
rtrash list
rtrash restore scratch.txt
rtrash scratch.txt
rtrash empty --trash-dir="$XDG_DATA_HOME/Trash"
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
