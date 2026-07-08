# rtrash

<p align="center"> <img src="docs/source/_static/logo.svg" width="280" alt="rtrash logo" /> </p>

Native Rust FreeDesktop trash for Linux: an rm-compatible put path and the everyday trash-cli suite (`trash-put`, `trash-empty`, `trash-list`, `trash-restore`, `trash-rm`) as one multi-call binary.
Implements the [freedesktop.org Trash specification](https://specifications.freedesktop.org/trash-spec/trashspec-latest.html).
No interpreter startup; emptying deletes entries in parallel via [rayon](https://crates.io/crates/rayon).

## Documentation

| Doc | Contents |
| --- | --- |
| [docs/getting-started.md](docs/getting-started.md) | Install CLI + Python, shortest paths |
| [docs/architecture.md](docs/architecture.md) | FreeDesktop layout, safety vs `rm` / `os.remove` / trash-cli |
| [docs/benchmarks.md](docs/benchmarks.md) | Measured safer/better/faster vs trash-cli |
| [docs/bindings.md](docs/bindings.md) | Maturin/PyO3 API (`unlink` / `rmtree` replacements) |
| [CHANGELOG.md](CHANGELOG.md) | Release notes (towncrier fragments in `docs/newsfragments/`) |

Org-mode sources (readcon-core style): [docs/orgmode/](docs/orgmode/).

### Python: replace permanent delete

```python
import rtrash
rtrash.unlink("file.txt")    # not os.remove
rtrash.rmtree("build/")      # not shutil.rmtree
```

```console
$ pip install rtrash
```


## Docs site

Published at **https://rtrash.rgoswami.me** (Cloudflare Pages).


Project documentation is authored in **orgmode** under [`docs/orgmode/`](docs/orgmode/) and built to a **Sphinx + Shibuya** static site (readcon-style: org → RST → HTML).

```shell
# needs: pandoc (or emacs + ox-rst), python3, venv
./docs/build.sh
# HTML → docs/build/index.html
python3 -m http.server -d docs/build 8000
```

| Page | Source |
|------|--------|
| Landing | `docs/orgmode/index.org` |
| Getting started | `docs/orgmode/getting-started.org` |
| Architecture | `docs/orgmode/architecture.org` |
| Benchmarks | `docs/orgmode/benchmarks.org` |
| Python bindings | `docs/orgmode/bindings.org` |

Markdown mirrors under `docs/*.md` remain for quick GitHub reading; the site is the structured presentation of the same material.


## Platform

| OS | Backend | Notes |
| -- | ------- | ----- |
| **Linux** | FreeDesktop home + per-mount trash | Primary niche; multi-call `setup`, musl releases, full suite tests |
| **macOS** | FreeDesktop **home** trash only (experimental) | `$XDG_DATA_HOME/Trash` or `~/.local/share/Trash`. **Not** Finder / system Trash |
| **Windows** | System **Recycle Bin** (shell APIs) | put/list/restore/empty/rm/status. **Not** FreeDesktop on-disk layout. Multi-call `setup` is Unix-oriented |

On Linux/macOS TTY, bare `rtrash restore` opens the **interactive restore browser** (ratatui). Windows uses numbered restore selection (no TUI).

## Install

**Registry (primary):** published on [crates.io/crates/rtrash](https://crates.io/crates/rtrash) and [pypi.org/project/rtrash](https://pypi.org/project/rtrash/).

```console
$ cargo install rtrash          # CLI from crates.io (needs Rust toolchain)
$ cargo binstall rtrash         # CLI prebuilt musl (x86_64 / aarch64 Linux)
$ pip install rtrash            # Python bindings (Linux x86_64 wheels, CPython 3.10–3.14)
$ rtrash setup                  # multi-call links, completions, man under ~/.local
```

Always run **`rtrash setup`** after the CLI binary lands on `PATH` (override with `--prefix=DIR`).

### 1. `cargo binstall` (preferred binary install)

Uses [cargo-binstall](https://github.com/cargo-bins/cargo-binstall) to download the prebuilt **Linux musl static** tarball for your CPU (no local compile).
GitHub Releases ship **x86_64** and **aarch64** musl assets for tag `v*`.
Crate metadata remaps typical glibc hosts (`*-unknown-linux-gnu`) to the matching musl tarball, so a normal:

```console
$ cargo binstall rtrash
$ rtrash setup
```

does **not** look for a non-existent `*-linux-gnu*.tar.gz` on x86_64 or aarch64 Linux.

| Arch | Asset basename |
| ---- | -------------- |
| x86_64 | `rtrash-<version>-x86_64-unknown-linux-musl.tar.gz` |
| aarch64 (ARM64) | `rtrash-<version>-aarch64-unknown-linux-musl.tar.gz` |

Naming matches `[package.metadata.binstall]` in `Cargo.toml` / `scripts/package-release.sh`.

Prebuilt musl tarballs are **Linux-only**. macOS (experimental FreeDesktop home trash) and Windows (Recycle Bin backend) are **from-source** (`cargo install rtrash`); there are no Finder Trash or FreeDesktop-as-Recycle-Bin assets.

### 2. Manual musl tarball (no cargo at all)

When a Release exists, download the matching asset from the repository Releases page, extract, put `bin/` on `PATH`, then `rtrash setup --force` (or follow `INSTALL.txt` in the tarball).

Build the **same** artifact yourself (builder host with musl target):

```console
$ ./scripts/package-release.sh x86_64-unknown-linux-musl
$ ./scripts/package-release.sh aarch64-unknown-linux-musl
# or, on a host that can build both:
$ ./scripts/package-release.sh --all
# → dist/rtrash-<version>-{x86_64,aarch64}-unknown-linux-musl.tar.gz
```

CI: [`.github/workflows/release-musl.yml`](.github/workflows/release-musl.yml) builds both targets on `v*` tags (x86_64 and ubuntu-24.04-arm) and attaches the tarballs to the GitHub Release (what binstall downloads).

### 3. From crates.io (`cargo install`)

Requires a Rust toolchain (MSRV **1.77**).
Dynamically linked glibc binary by default (compiles on the install host).

```console
$ cargo install rtrash
$ rtrash setup
```

Tip of `main` without waiting for a release:

```console
$ cargo install --git https://github.com/HaoZeke/rtrash
$ rtrash setup
```

### 4. Python (`pip install rtrash`)

Prebuilt **manylinux** wheels for **CPython 3.10–3.14** on **Linux x86_64** (FreeDesktop only).

```console
$ pip install rtrash
$ python -c "import rtrash; print(rtrash.version())"
```

From a checkout (dev): `pip install maturin && maturin develop --features python`.

### What `rtrash setup` installs

| What | Where (default) |
| ---- | --------------- |
| Multi-call symlinks (`trash-put`, `trash-empty`, `trash-list`, `trash-restore`, `trash-rm`, `trash`) | `~/.local/bin/` → this `rtrash` |
| bash completion | `~/.local/share/bash-completion/completions/rtrash` (+ multi-call links) |
| zsh completion | `~/.local/share/zsh/site-functions/_rtrash` |
| fish completion | `~/.config/fish/completions/rtrash.fish` (+ multi-call `*.fish` links) |
| man page | `~/.local/share/man/man1/rtrash.1` |

Useful flags: `rtrash setup --dry-run`, `--force` (refresh after upgrade), `--with-rm` (also link `rm` → put into trash), `--prefix=/usr/local`.

Subcommands work without multi-call names: `rtrash put|empty|list|status|restore|rm …`.

| Multi-call name | Meaning |
| --------------- | ------- |
| `rm` / `trash` / `trash-put` | put (move paths into the trash) |
| `trash-list` | list |
| `trash-restore` | restore |
| `trash-empty` | empty |
| `trash-rm` | permanently delete matching **trash** entries |

Subcommand `rtrash rm PATTERN` is the same as multi-call `trash-rm` (not the same as multi-call `rm`, which puts).

### Packagers and custom layouts

Sources also live in-tree for packaging (`completions/`, `man/rtrash.1`).
From any installed binary:

```console
$ rtrash completions bash > rtrash.bash
$ rtrash completions zsh  > _rtrash
$ rtrash completions fish > rtrash.fish
$ rtrash man              > rtrash.1
$ rtrash setup --prefix=/usr --force    # system prefix when packaging
```



## Contributing and releases

- **Changelog:** add a towncrier fragment under `docs/newsfragments/` for user-visible changes:
  - `towncrier create -c "Short description." +slug.added.md` (or `.fixed.md`, `.changed.md`, …)
  - At release: `towncrier build --version X.Y.Z --yes` then tag `vX.Y.Z`
- **Version lockstep:** cocogitto (`cog.toml`) bumps `Cargo.toml`, `pyproject.toml`, and `docs/source/conf.py` together (`disable_changelog = true`; towncrier owns `CHANGELOG.md`).
- **Hooks:** `prek install` then `prek run -a` (builtin large-file check, yaml/toml, codespell, version lockstep). CI also runs [HaoZeke/large-file-auditor](https://github.com/HaoZeke/large-file-auditor) and [lychee](https://github.com/lycheeverse/lychee).
- **Docs PRs:** the documentation workflow uploads a `documentation` artifact; [HaoZeke/doc-previewer](https://github.com/HaoZeke/doc-previewer) comments a preview link on the PR.

## Tutorial

Trash a file, inspect the trash, restore it, then empty:

```console
$ echo data > scratch.txt
$ rtrash scratch.txt
$ rtrash list
2026-07-03 14:00:00 /home/you/scratch.txt
$ rtrash restore scratch.txt
restored '/home/you/scratch.txt'
$ rtrash scratch.txt
$ rtrash empty
Removed 1 item
```

Selective permanent delete from the trash (trash-cli `trash-rm`):

```console
$ rtrash put a.o b.c
$ rtrash rm '*.o'          # quote globs; removes a.o from trash only
$ rtrash list              # b.c still listed
```

`rtrash FILE` with no subcommand behaves like `rm`, so shell habits carry over: `rtrash -rf build/` moves `build/` to the trash instead of unlinking it.

**Scripts / tests:** pin with `--trash-dir=…` and/or isolate `XDG_DATA_HOME` so list/empty/restore/rm do not walk every mount’s trash.

## Reference

### `rtrash put` (also `rm`, `trash`, `trash-put`, or bare `rtrash`)

Accepts the common `rm(1)` flags with the same semantics, except that files move to the trash: `-f`, `-i`, `-I`, `--interactive[=WHEN]`, `-r`/`-R`, `-d`, `-v`, `--one-file-system` (accepted for rm compatibility; no-op because entries move whole and are never walked like `rm -x`), `--preserve-root` (default), `--no-preserve-root`, `--`. Directories need `-r` (or `-d` when empty), `.`/`..`/`/` are refused (unless `--no-preserve-root` for `/`), and exit codes mirror rm (0 success, 1 failure, 2 usage error).

As with GNU rm, **the last of `-f` / `-i` / `-I` (and the matching long forms) wins** for prompt vs force behavior.
`-f` also ignores missing paths.

Trash placement follows the spec: the home trash (`$XDG_DATA_HOME/Trash`, defaulting under `~/.local/share`) for same-filesystem files, `$top/.Trash/$uid` (must be a sticky non-symlink directory) or `$top/.Trash-$uid` on other mounts, with a copy-into-home-trash fallback when the volume cannot host a trash directory.
Names are reserved atomically (create-new / `O_EXCL` on the `.trashinfo`), so concurrent invocations never clobber each other; collisions get `name.2`, `name.3`, ...

Putting a **directory** updates that trash dir’s FreeDesktop `directorysizes` cache (`size mtime percent-encoded-name`).
Putting ordinary files does not add directory lines.

### `rtrash empty [DAYS]` (also `trash-empty`)

Purges every trash directory visible to the user (home trash plus mounted volumes).
With `DAYS`, only items trashed more than `DAYS` days ago go.
Entries are removed in parallel.
Orphaned `files/` entries (no `.trashinfo`) and entries with broken metadata are purged on a full empty, and the `directorysizes` cache is pruned when present.
Options: `-n`/`--dry-run` (also prints an approximate **reclaimable** size via a fast in-process walk of the victims, like a small `du` of what would go away), `-v`/`--verbose`, `--trash-dir=PATH` (repeatable), `-f` (accepted for trash-cli compatibility; emptying never prompts).

### `rtrash status`

Prints per-trash-root and total **item count** plus approximate **reclaimable size** (same disk-usage walk as empty dry-run).
Accepts `--home-only` and `--trash-dir`.

### `rtrash list` (also `trash-list`)

Prints `DELETION-DATE ORIGINAL-PATH` per item, oldest first, in the `trash-list` output format (`YYYY-MM-DD HH:MM:SS` plus the original path).
Scans the home trash and per-mount trash directories owned by the current user, or only the directories given with `--trash-dir=PATH` (repeatable).

### `rtrash restore [PATH]` (also `trash-restore`)

| Invocation | Behavior |
|------------|----------|
| `rtrash restore PATH` | Restore the item whose original path was `PATH` (exact match) |
| `rtrash restore` | **TUI** (TTY): fuzzy filter, multi-select (Space/a/A), bulk restore; session until `q` |
| `rtrash restore --plain` | Numbered line list + index (scripts / no TUI) |
| `printf '0\n' \| rtrash restore` | Piped index without TUI |
| `rtrash restore --cwd-only` | Same as bare restore, only originals under `$PWD` |
| Single match | Restores immediately without prompting |

**TUI keys (shared by restore / empty / put):** defaults `↑↓`/`jk` move · `PgUp`/`PgDn` page · `g`/`G` first/last · `Space` mark · `a`/`A` all/clear · `/` live fuzzy · `?` help · `Enter` action · `q`/`Esc` quit.

**Fully customizable:** every action can be remapped in `$XDG_CONFIG_HOME/rtrash/keys.toml` (or `RTRASH_KEYS=path`). TOML under `[keys]`; `rtrash keys --list` shows the resolved map; `rtrash keys --sample` prints a full template. Help (`?`) reflects the live map.

Browser-specific actions (also remappable): restore `toggle_force` · empty `toggle_dry_run` · put `toggle_recursive` / `toggle_force`.

**TUI empty:** bare `rtrash empty` on a TTY opens the multi-select permanent-delete browser (`--plain` or non-TTY keeps classic empty).

**TUI put:** bare `rtrash put` on a TTY opens the multi-select path browser for the current directory (`--plain` or FILE operands keep classic put).

Existing paths at the destination are preserved unless `-f` / `--force` is given; with `-f`, a blocking destination is removed first.
Same-filesystem restore uses `rename`; cross-device restore copies then deletes the trash payload (needed when put fell back to the home trash).
Options: `--trash-dir=PATH` (repeatable), `--home-only`, `--cwd-only`.

### `rtrash rm PATTERN...` (also `trash-rm`)

Permanently deletes trash entries whose original path, basename, or trash name matches a shell-style glob `PATTERN` (quote globs from the shell).
Matching `files/` payloads and `.trashinfo` files are removed; non-matches stay.
Does **not** restore.
Options: `-v`/`--verbose`, `--trash-dir=PATH` (repeatable).

## FreeDesktop durability notes

Previously deferred items are implemented in the shipped put/empty path:

- **Durable `.trashinfo`:** put `fsync`s the reserved info file (and best-effort the `info/` dir) before moving the payload.
- **EXDEV fidelity:** cross-device put/restore copies preserve content, symlink-as-link, mode, and mtime (not a bare content-only copy).
- **Put/empty exclusion:** each trash root takes an exclusive `flock` on `.rtrash.lock` for put and empty so the pair cannot tear mid-operation.
- **Btrfs multi-subvol topdir:** volume topdir is the **longest mount-point prefix** from `/proc/self/mounts`, not a pure `st_dev` parent walk.
- **Default multi-volume empty:** with no `--trash-dir`, empty/list/restore/rm discover home trash plus existing user trash on every non-pseudo mount (including `/`), matching trash-cli’s multi-volume default. Pin with `--trash-dir` in scripts.

## Limitations

- **Platform:** Linux FreeDesktop is the primary niche. macOS is experimental FreeDesktop **home** trash (not Finder). Windows uses the system Recycle Bin (not FreeDesktop layout).
- **Restore UI:** bare `rtrash restore` on a TTY opens the **ratatui restore browser** (filter, navigate, multi-restore). Use `--plain` or a piped index for non-interactive selection. Scripts and path restore stay first-class.
- **Not a general soft-delete database:** only the FreeDesktop on-disk layout.
- **EXDEV does not re-create xattrs/ACLs/hardlinks** (mode+mtime+symlink+bytes only).
- **Locks are local `flock`** (advisory on some network FS).

## Performance

Historical single-machine comparison against trash-cli **0.24.5.26** (CPython 3) on an NVMe-backed Linux host, best of warm runs.
These numbers are **not continuously re-verified in CI** and will vary by filesystem, core count, and trash layout; treat them as order-of-magnitude evidence that native startup and parallel empty help, not as a live leaderboard.

| Operation                  | trash-cli | rtrash | ratio |
| -------------------------- | --------- | ------ | ----- |
| `empty`, 100 000 entries   | 0.92 s    | 0.36 s | 2.5x  |
| `empty`, 20 000 entries    | 0.21 s    | 0.07 s | 3x    |
| `put`, one file            | 52 ms     | 1 ms   | 52x   |

Full empty was further optimized for large trashcans: no pre-scan when not verbose, `d_type`-fast `unlinkat` for regular files, and **serial** wipes of the `files/` then `info/` roots with **parallel children** inside each.
Emptying uses an in-process bulk tree delete (readdir/`unlinkat` walk inspired by empty-source `rsync --delete`, not a shell-out to rsync).
On btrfs, if a trash payload is a real subvolume root, empty uses `BTRFS_IOC_SNAP_DESTROY` instead of walking the tree.
Interactive `put` mainly wins on process startup versus trash-cli.

## Development

```console
$ cargo test
```

Integration tests isolate trash under a temporary `XDG_DATA_HOME` and pin `empty` / list / restore / `rm` with `--trash-dir=…` so they never clear the host trash.

## Citation

```bibtex
@software{rtrash,
  author = {Goswami, Rohit},
  title  = {rtrash: a fast rm-compatible freedesktop.org trash tool},
  url    = {https://github.com/HaoZeke/rtrash},
  year   = {2026}
}
```

## License

MIT
