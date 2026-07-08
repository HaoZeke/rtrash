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
