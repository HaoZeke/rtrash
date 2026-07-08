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

### CLI (Rust)

**Preferred** on **x86_64 or aarch64 Linux** when a GitHub Release for this version exists (tag `v*`, musl assets `rtrash-<version>-{x86_64,aarch64}-unknown-linux-musl.tar.gz`):

```shell
cargo binstall rtrash
# or: cargo binstall --git https://github.com/HaoZeke/rtrash rtrash
rtrash setup
```

Metadata remaps typical glibc hosts (`*-unknown-linux-gnu`) on those arches to the matching musl static tarball (no `--target` / `--pkg-url` required).
Always finish with **`rtrash setup`** (multi-call links, bash/zsh/fish completions, man under `~/.local`).

**Manual tarball:** download the Release asset or run `./scripts/package-release.sh`, then `rtrash setup`.

**From source** (no release asset yet):

```shell
cargo install --git https://github.com/HaoZeke/rtrash
rtrash setup
```

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
