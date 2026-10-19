# rtrash

<p align="center">
  <img src="docs/source/_static/logo.svg" width="240" alt="rtrash logo" />
</p>

<p align="center">
  <strong>Native FreeDesktop trash</strong> — rm-compatible put, full trash-cli suite, Python bindings.<br/>
  One multi-call binary · recoverable by design · parallel empty
</p>

<p align="center">
  <a href="https://crates.io/crates/rtrash"><img src="https://img.shields.io/crates/v/rtrash.svg" alt="crates.io" /></a>
  <a href="https://pypi.org/project/rtrash/"><img src="https://img.shields.io/pypi/v/rtrash.svg" alt="PyPI" /></a>
  <a href="https://rtrash.rgoswami.me"><img src="https://img.shields.io/badge/docs-rtrash.rgoswami.me-blue" alt="docs" /></a>
</p>

<!-- Absolute raw URLs so the GIF embeds on GitHub *and* crates.io (relative paths often break on crates.io). -->
<p align="center">
  <a href="https://github.com/HaoZeke/rtrash/blob/main/docs/demo/rtrash-quickstart.cast">
    <img
      src="https://raw.githubusercontent.com/HaoZeke/rtrash/main/docs/demo/rtrash-quickstart.gif"
      alt="Terminal demo: rtrash put → list → restore → empty in a pinned sandbox trash"
      width="720"
    />
  </a>
</p>

<p align="center">
  <em>put → list/status → restore → empty</em>
  · <a href="https://raw.githubusercontent.com/HaoZeke/rtrash/main/docs/demo/rtrash-quickstart.cast">asciicast</a>
  · <a href="https://rtrash.rgoswami.me/">docs site</a>
</p>

```console
$ cargo binstall rtrash && rtrash setup
$ rtrash notes.txt                 # put (also: rtrash -rf dir, multi-call trash-put)
$ rtrash list && rtrash status
$ rtrash du -n 10 ~/src             # largest dirs and files to trash
$ rtrash restore                   # TUI on a TTY; --plain for scripts
$ rtrash undo                      # bring back everything the last put trashed
$ rtrash empty --plain
```

<details>
<summary><strong>More of the surface</strong> (rm-flags, multi-call, trash-rm, dry-run, keys)</summary>

<p align="center">
  <a href="https://github.com/HaoZeke/rtrash/blob/main/docs/demo/rtrash-suite.cast">
    <img
      src="https://raw.githubusercontent.com/HaoZeke/rtrash/main/docs/demo/rtrash-suite.gif"
      alt="Terminal demo: rtrash -rf, multi-call, rm globs, empty --dry-run, keys"
      width="720"
    />
  </a>
</p>

| Feature | Demo |
| ------- | ---- |
| FreeDesktop put / list / restore / empty | quickstart GIF above |
| `rtrash -rf` rm-shaped put | suite GIF |
| Multi-call `trash-put` / `trash-list` / … | suite GIF |
| `rtrash rm` / `trash-rm` globs | suite GIF |
| `empty --dry-run` | suite GIF |
| Custom keys (`rtrash keys`, `keys.toml`) | suite GIF |
| Interactive TUI (fuzzy, multi-select, `?`) | **TTY** — not in the GIF; run `rtrash restore` |
| Python `unlink` / `rmtree` | [bindings](docs/bindings.md) |

Demos are regenerable: `./docs/demo/record.sh` (pinned `--trash-dir` sandbox — safe).
Source: [docs/demo/](docs/demo/).

</details>

## Documentation

| Doc | Contents |
| --- | --- |
| [docs/getting-started.md](docs/getting-started.md) | Install CLI + Python, shortest paths |
| [docs/demo/](docs/demo/) | Terminal demo cast/GIF + recorder |
| [docs/architecture.md](docs/architecture.md) | FreeDesktop layout, safety vs `rm` / `os.remove` / trash-cli |
| [docs/benchmarks.md](docs/benchmarks.md) | Measured safer/better/faster vs trash-cli, and `du` against dua, gdu and du |
| [docs/bindings.md](docs/bindings.md) | Maturin/PyO3 API (`unlink` / `rmtree` replacements) |
| [CHANGELOG.md](CHANGELOG.md) | Release notes (towncrier fragments in `docs/newsfragments/`) |

Site: **https://rtrash.rgoswami.me** · Org sources: [docs/orgmode/](docs/orgmode/).

### Python: replace permanent delete

```python
import rtrash
rtrash.unlink("file.txt")    # not os.remove — GIL released during I/O
rtrash.rmtree("build/")      # not shutil.rmtree
```

```console
$ pip install rtrash
```

## Platform

| OS | Backend | Notes |
| -- | ------- | ----- |
| **Linux** | FreeDesktop home + per-mount trash | Primary niche; multi-call `setup`, musl releases, full suite tests |
| **macOS** | Finder Trash with Put Back by default (experimental); FreeDesktop **home** trash with `RTRASH_MACOS_TRASH=freedesktop`, `macos.trash` or a `--trash-dir` pin | `~/.Trash`; FreeDesktop: `$XDG_DATA_HOME/Trash` or `~/.local/share/Trash` |
| **Windows** | System **Recycle Bin** (shell APIs) | put/list/restore/empty/rm/status/du. **Not** FreeDesktop on-disk layout. Multi-call `setup` is Unix-oriented |

On a TTY, bare `rtrash restore` opens the **interactive restore browser** (ratatui), on Windows too, where it lists the Recycle Bin. `--plain` keeps the numbered selection. On Windows `rtrash du -i` deletes into the Recycle Bin and `u` restores from it. `rtrash status` and the trash line of `rtrash du` show the cap of each drive (`MaxCapacity` in the registry) and how much of it is in use. `rtrash status --json` gives the Linux object there too, with `orphans` and `dangling` null (the shell does not count them) and the cap as `cap_bytes`. The age filters (`--older-than`, `--newer-than`, `empty DAYS`) and size filters (`--larger-than`, `--smaller-than`) work on the Recycle Bin too, for `list`, `status` and `rm`, and `empty` takes the age ones; sizes come from each item's `$I` record.

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

Prebuilt musl tarballs are **Linux-only**. macOS (experimental; Finder Trash by default) and Windows (Recycle Bin backend) are **from-source** (`cargo install rtrash`); there are no Finder Trash or FreeDesktop-as-Recycle-Bin assets.

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

Requires a Rust toolchain (MSRV **1.88**).
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
| Midnight Commander menu sample (optional) | `~/.local/share/rtrash/mc.menu.sample` — append into `~/.config/mc/menu` by hand; setup never overwrites your menu |

Useful flags: `rtrash setup --dry-run`, `--force` (refresh after upgrade), `--with-rm` (also link `rm` → put into trash), `--prefix=/usr/local`.

Subcommands work without multi-call names: `rtrash put|empty|list|status|du|restore|undo|rm …`.

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

Accepts the common `rm(1)` flags with the same semantics, except that files move to the trash: `-f`, `-i`, `-I`, `--interactive[=WHEN]`, `-r`/`-R`, `-d`, `-v`, `--one-file-system` (accepted for rm compatibility; no-op because entries move whole and are never walked like `rm -x`), `--preserve-root[=all]` (default), `--no-preserve-root`, `--`. Directories need `-r` (or `-d` when empty), `.`/`..`/`/` get refused, and so do your home directory and mount points (unless `--no-preserve-root`), the trash directory itself, a directory containing it, and items already in it; exit codes mirror rm (0 success, 1 failure, 2 usage error).

Safety rails beyond rm:

- **safe-rm list.** Put skips safe-rm's built-in system directories and every path or shell pattern listed in `/etc/safe-rm.conf`, `/usr/local/etc/safe-rm.conf`, `$XDG_CONFIG_HOME/safe-rm` or `~/.safe-rm`, so an existing safe-rm setup keeps working.
- **Repository warning.** Trashing a directory with a git repository at its top that has uncommitted or unpushed work prints a warning naming `rtrash undo`.

As with GNU rm, **the last of `-f` / `-i` / `-I` (and the matching long forms) wins** for prompt vs force behavior.
`-f` also ignores missing paths.

Trash placement follows the spec: the home trash (`$XDG_DATA_HOME/Trash`, defaulting under `~/.local/share`) for same-filesystem files, `$top/.Trash/$uid` (must be a sticky non-symlink directory) or `$top/.Trash-$uid` on other mounts, with a copy-into-home-trash fallback when the volume cannot host a trash directory.
When `$top/.Trash` exists but fails the check (a symlink, a plain file, or no sticky bit), put warns once and uses `$top/.Trash-$uid`, as the spec asks.
The per-user directory, `$top/.Trash/$uid` or `$top/.Trash-$uid`, must also be a real directory owned by you; one that is a symlink or belongs to another user gets a warning, put moves on to the next choice (in the end the home trash), and `list`, `empty` and `rm` skip it.
Names are reserved atomically, so concurrent invocations never clobber each other; collisions get `name.2`, `name.3`, ... A put writes the whole `.trashinfo` under a temporary name in `info/`, then hard-links it to its trash name. The link fails if that name exists, as `O_EXCL` does. Other tools never read a cut-off record. Put falls back to creating the record with `O_EXCL` on a file system with no hard links. `rtrash status --check --fix` removes the temporary of a killed put.

**Script pins:** `--trash-dir=PATH` puts into that FreeDesktop root (creates `files/` and `info/` if needed); `--home-only` always uses the home trash.

**Script output:** `--json` prints one object, `{"trashed": [...], "failed": N}`. Each trashed entry lists the original path, the name under `files/`, and the trash root, so a script can hand the names to `rtrash rm` or restore by path later.

Putting a **directory** updates that trash dir’s FreeDesktop `directorysizes` cache (`size mtime percent-encoded-name`).
Sizes count allocated blocks the way `du -B1` does, the directories' own blocks included and each hard-linked file once.
rtrash reads the lines other tools write in any encoding the spec allows, from one that encodes only `%` and newline to full percent-encoding, and each update leaves one line per directory.
Put measures the tree in parallel after the move, outside the trash lock.
When `status` or `empty --dry-run` has to walk a directory with no valid line, it writes the size back and drops lines for directories that left the trash, so the next run reads the cache.
Putting ordinary files does not add directory lines.

### `rtrash empty [DAYS]` (also `trash-empty`)

Purges every trash directory visible to the user (home trash plus mounted volumes).
With `DAYS` (or `--older-than=DAYS`), only items trashed more than `DAYS` days ago go.
Entries are removed in parallel.
Orphaned `files/` entries (no `.trashinfo`) and entries with broken metadata are purged on a full empty, and the `directorysizes` cache is pruned when present.
A directory you own but cannot list or change, such as a build tree's `pkg` at mode 0311, gets owner `rwx` back and then goes.
Nothing someone else owns is changed.
An item that still cannot go stays in the trash, with its `.trashinfo`.
The rest still goes, and `empty` exits 1 after listing each item left and why.
Options: `-n`/`--dry-run` (also prints an approximate **reclaimable** size via a fast in-process walk of the victims, like a small `du` of what would go away), `-v`/`--verbose`, `--trash-dir=PATH` (repeatable), `-f` (accepted for trash-cli compatibility; emptying never prompts), `--json` (summary: `dry_run`, `removed`, `bytes`, `days`, `max_size`).

**Retention by size:** `rtrash empty --max-size=10G` removes items, oldest first, until the trash directories in scope fit in 10 GiB together; `--prune=largest` removes the largest first, and `DAYS` runs before the size cut.
Set `RTRASH_MAX_SIZE=10G` (and optionally `RTRASH_PRUNE=largest`), or `max_size` under `[policy]` in the config file, to prune after every put: each trash directory the put used goes back under the budget, and the items that put just trashed always stay.

### `rtrash list` (also `trash-list`)

Prints `DELETION-DATE ORIGINAL-PATH` per item, oldest first, in the `trash-list` output format (`YYYY-MM-DD HH:MM:SS` plus the original path).
Scans the home trash and per-mount trash directories owned by the current user, or only the directories given with `--trash-dir=PATH` (repeatable).
Orphans (payloads in `files/` with no `.trashinfo`) come last, with a `????-??-??` date and their path inside the trash.
In JSON they carry `"orphan": true`; an age filter leaves them out.

| Flag | Meaning |
|------|---------|
| `--json` | JSON array of `{deletion_date, original, name, trash_dir, batch}` (plus `size` in bytes with `--size`) |
| `--size` | Add each item's disk usage (payload plus `.trashinfo`, as `status` counts it). As `trash-list`, trash-cli's `SIZE PATH` rows: the payload's size in bytes in place of the date |
| `--files` | `trash-list` only, as trash-cli's hidden option: add ` -> FILE`, the item in the trash, to each row |
| `--sort=date\|size\|name\|path` | Row order; `size` lists the largest first and implies `--size` |
| `--reverse` | Reverse the order |
| `--older-than=DAYS` | Only items trashed more than DAYS days ago (same cutoff as `empty DAYS`) |
| `--newer-than=DAYS` | Only items trashed within the last DAYS days |
| `--larger-than=SIZE` / `--smaller-than=SIZE` | Only items taking more (or less) than SIZE on disk, as `--size` shows it; SIZE as in `empty --max-size` (`500M`, `1.5G`). Orphans have a size, so they stay in |
| `--home-only` | Home trash only |
| `--all-users` | Every account's trash this user can read (root reads all), with an owner column and a JSON `owner` field; read-only. Root restores or removes another account's items by pinning that trash with `--trash-dir` |

### `rtrash status`

Per-root item count and reclaimable size (uses `directorysizes` when valid). Same discovery pins as list. Also accepts `--json`, `--older-than=DAYS`, `--newer-than=DAYS`, `--larger-than=SIZE`, `--smaller-than=SIZE`.
Orphans count as items, noted as `(N without .trashinfo)` per root and as `orphans` in JSON.

### `rtrash du [PATH]...`

Finds what to delete. It walks each `PATH` (default `.`) in parallel and lists the largest directories and files, with on-disk size, apparent size, and the age of the newest modification below each entry.

| Invocation | Behavior |
|------------|----------|
| `rtrash du /workspace` | 20 largest directories and 20 largest files, ranked by disk use |
| `rtrash du -n 5 -x ~` | Top 5 of each, staying on the file system of `~`. The totals match coreutils `du -sx`; a CI test checks this across a Btrfs subvolume, which has a device number of its own |
| `rtrash du --apparent` | Rank by file length; sparse files rank higher than their disk use |
| `rtrash du --sort=items` | Directories with the most files and subdirectories below them (inode hogs such as `node_modules`); `-C` adds an item count column to any listing |
| `rtrash du --types ~/data` | Lists file types instead of directories and files: disk use, apparent size and file count per extension, with case ignored, ranked by disk use, `--apparent` or `--sort=items`. JSON adds a `types` array |
| `rtrash du --unique` | Adds a UNIQUE column: the bytes in extents no other file shares, roughly what deleting a row frees where reflinks, clones or snapshots share data (Btrfs on Linux, the Apple File System on macOS). The closing summary shows the shared extents once each and as referenced. It maps every file's extents, so it runs slower; the default listing skips it |
| `rtrash du --shared` | Counts each extent that reflinks, clones or snapshots share once per row and in the totals, so copies on Btrfs or XFS no longer add up to more than they take on disk; the total also gives the size that counts each reference. Linux only; it maps every file's extents, like `--unique` |
| `rtrash du --unique` on compressed Btrfs | Btrfs maps a compressed extent at its uncompressed length and `st_blocks` counts that length too, so disk use shows what the data would take uncompressed, as `du` shows it. `--unique` says how much of the mapped data sits in compressed extents, and `--shared` counts each compressed extent once. Without root the kernel reports no compressed size; `compsize` (as root) reads it |
| `rtrash du --sort=mtime` | Most recently modified first; a directory gives way to the subdirectory that contains its newest change |
| `rtrash du --dirs --older-than=30` | Directories with nothing modified in the last 30 days |
| `rtrash du --since=2026-09-01 --until=2026-10-01` | Entries last modified in September, in local time; `YYYY-MM-DDTHH:MM[:SS]` sets an hour too |
| `rtrash du --artefacts ~/src` | Only regenerable build output and caches: cargo `target`, `node_modules`, `__pycache__`, virtualenvs, CMake build trees, `CACHEDIR.TAG` directories |
| `rtrash du --exclude='*.iso' -X ~/.duignore` | Leave out what matches (gitignore syntax); excluded entries add nothing to any size |
| `rtrash du --gitignore ~/src` | Leave out what git ignores in each repository (`.gitignore`, `.git/info/exclude`, `core.excludesFile`) |
| `rtrash du --exclude-caches` | Leave out directories tagged with a valid `CACHEDIR.TAG` |
| `rtrash du --max-depth=2 --all-dirs` | Every directory level, at most 2 below `PATH` |
| `rtrash du --json` | One object with `roots`, `dirs`, `files`, `unreadable`, `unreadable_paths` (up to 100 entries with `path` and `error`), plus `too_deep` when the walk met directories past its limits of 4096 levels and 32 KiB of path (counted without their contents), and `snapshots` naming the read-only Btrfs snapshots below `PATH` |
| `rtrash du /` on Btrfs | Each snapshot subvolume counts in full, as `du` counts it, so a root with snapper or Timeshift snapshots adds up to more than the disk holds. On Linux, stderr names each read-only snapshot met below `PATH` (the first 10). `--shared` counts the extents they share once, and `-x` leaves every subvolume below `PATH` out, since each has a device number of its own |
| `rtrash du -x /` on macOS | It walks both volumes of the Apple File System (APFS) volume group: the read-only system volume and the data volume. The two report one device number. A firmlinked folder such as `/Users` shows up at the top and under `/System/Volumes/Data`. It is one directory with one inode, so it counts once. A CI test checks this on both macOS runners |
| Ctrl-C during `rtrash du` | Stops the scan and prints what it counted, marked partial (`"partial": true` in JSON), with exit status 130; `--print0`, `--pick`, `-o` and `--stacks` print nothing then. A second Ctrl-C ends it at once |
| `rtrash du ~/src ~/src/app` | Exits with status 2, as gdu does: the walk of `~/src` counts `~/src/app` already, so the totals, `--json`, `--dupes` and the explorer would count it twice. du scans a repeated `PATH` once and notes it on stderr. Under `-x`, du keeps a `PATH` on another file system, since the outer walk stops at its mount |
| `rtrash du --no-git` | Skip the git checks |
| `rtrash du --trash-dir=PATH` / `--home-only` | Limit the trash summary to these trash directories |
| `rtrash du --open-deleted /var` | One more line under the trash line: the space that deleted files still hold while a process keeps them open, on the scanned file systems. It explains a gap between `df` and `du`, and no total counts it. Linux (from `/proc`) and macOS (from libproc); without root, other users' processes are left out and counted; `--json` adds an `open_deleted` key |
| `rtrash du --print0 … \| xargs -0 rtrash put -rf --` | Trash the listed paths |
| `rtrash du --pick` | Mark rows in the put browser and trash them; rows with git work at risk show `!` |
| `rtrash du -o base.snap ~/src` | Write a snapshot of every directory total instead of listing (`-` for stdout) |
| `rtrash du diff base.snap` | What grew or shrank since the snapshot (scans again); `du diff OLD NEW` compares two snapshots, `--json` for scripts |
| `rtrash du --devices` | Size, use and free space of every mounted file system, as `df` and `gdu -d` show them (`--json` for scripts) |
| `rtrash du --ncdu=usr.json /usr` | Write an ncdu export for `ncdu -f usr.json` (or `du -f`); `-x` and the filters apply |
| `rtrash du --ncdu-binary=usr.ncdu /usr` | The same in ncdu 2's binary format (`ncdu -O`), for ncdu 2.6 and later: about a fifth of the JSON export's size, with each directory's totals stored |
| `rtrash du -f base.snap` | List the largest directories from a snapshot without scanning; an ncdu export (`ncdu -o`, `gdu -o`, compressed `ncdu -c -o`, or ncdu 2's binary `ncdu -O`) works too, here and in `du diff` |
| `rtrash du -f base.snap -i` | Browse a snapshot or ncdu export in the explorer, read-only and with no scan (ncdu #247): its directories open from the file, so they need not exist any more |
| `rtrash du --stacks ~ \| inferno-flamegraph --countname=bytes > du.svg` | Folded stacks of each directory's own bytes for a flame graph (`flamegraph.pl`, inferno, speedscope); works with `-f FILE` |
| `rtrash du -i ~/src` | Explore one directory at a time, like ncdu (`du -i ~/src ~/work`, as `dua i a b`, opens on a view with one row per directory): `d` trashes in the background, one row at a time, with a count of the rows and bytes left and of the bytes copied of a row that crosses file systems (`q` stops it, and a row partway through its copy stays whole in place; a row that stays says why, `e` lists every one, and marks wait until the delete ends), `u` undoes, `r` rescans, and `s`, `n`, `C` and `M` sort by size, name, item count or newest change, `S` switches between disk use and apparent size, `i` shows a row's details, `/` filters the directory by name or glob, `.` hides names that start with a dot, `%` cycles each row's share between a bar, a percent, both and neither (ncdu's `g`), `L` draws the bars on a log scale so small rows still show (ncdu #75), `p` quits and prints the marked paths (with `?` for each control character when stdout is a terminal, dua #365), `y` copies them to the clipboard (OSC 52), `E` writes the directory as a `du -o` snapshot file from the scan in memory, for `du -f` and `du diff` (ncdu #174), or on the view above two or more directories all of them as one snapshot, `b` opens a shell in the directory, as in ncdu and gdu, `v` shows a file in `$PAGER`, and `o` opens a row with its default program or `$RTRASH_OPENER` (gdu's `v` and `o`, gdu #584); Ctrl-L redraws the screen and Ctrl-Z suspends to the shell until `fg`, as in dua (Windows has no job control, so Ctrl-Z does nothing there); a scan or delete of 10 s or more rings the bell when it ends (dua #347; `RTRASH_NOTIFY` or config `notify` = `off`, `bell`, `osc9` or `osc777`, `notify_after` the seconds); rows carry size bars, artefact and git badges, and `reap:NAME` on a directory a reap profile empties by age (gdu #695). Directories open at once and scan in the background, so you can browse before the sizes are in; `q` or Ctrl-C stops a running scan and keeps its partial sizes |
| `rtrash du -i --no-delete /srv` | The same explorer, read-only: `d`, `u` and `b` only show a note (config key `no_delete` makes it the default) |
| `rtrash du --si` | Sizes in powers of 1000 (kB, MB, GB), as disk vendors and `df -H` count; JSON stays in bytes (config key `si`) |

Symlinks are not followed unless `-L`/`--follow-symlinks` is given (a directory met again through a loop or a second link is walked once, and a file reached by several names counts once; reports only, not on Windows), and a file with more than one hard link counts once, in the sizes and in the file count, so files plus directories match `du --inodes -s`; the total names the extra links. On Unix a directory met a second time with the same device and inode, as a bind mount on the same file system shows it, is walked once, under the name met first (ncdu #223); coreutils `du` over one `PATH` counts such files twice. When one subdirectory accounts for 90% or more of a directory's size, only the subdirectory is listed, so each row points at the bulk itself. The scan root appears only in the closing total.

Filters use gitignore syntax. A pattern without a `/` matches a name at any depth (`--exclude='*.iso'`, `--exclude=node_modules/`); a pattern with a `/` matches the path relative to the scanned `PATH` (`--exclude=/build`, `--exclude='docs/**/*.pdf'`), and an absolute path under `PATH` works too. `-X FILE` reads one pattern per line, with `#` comments and `!` to re-include. An excluded directory is never opened, so filters also speed up a scan, and the total reports how many entries were left out. `--gitignore` applies ignore files only inside git repositories, and a nested repository does not inherit the rules of the one around it, as in git. It reads the `core.excludesFile` of git config, asked of git once per repository, or `~/.config/git/ignore` when that is unset or git is missing, and the `info/exclude` of a linked worktree's main repository. The global file ranks lowest, then `info/exclude`, then each `.gitignore`, as in git. A `!` pattern re-includes what an earlier one left out, and the last pattern that matches decides. Nothing comes back from inside an excluded directory, since git never opens it: `build/` then `!build/keep.txt` keeps nothing. A `PATH` below an excluded directory lists nothing, with a note on stderr. On Unix the match works on bytes, so a pattern with bytes that are not UTF-8 matches names that carry them, and such a byte in an ignore file no longer drops the file.

A filter leaves entries out of the sizes, but trashing a directory takes all of it. So `--print0`, `--pick` and the explorer (`-i`) never offer a directory that holds entries the filters left out: `--print0` and `--pick` drop such rows with a count on stderr, and the explorer refuses to trash one and names it. `--allow-filtered` offers them anyway, and the `--json` row of such a directory counts the entries in `filtered_out`. gdu goes further and refuses every delete while a filter is active; this keeps files and fully shown directories deletable. Age and size filters (`--older-than`, `--since`, `--min-size` and the like) only choose which rows are listed, and every size stays whole, so they hide nothing.

The NOTE column tags regenerable artefacts (`cargo-target`, `node-modules`, `python-cache`, `python-env`, `cmake-build`, `cache`, and a few more listed in `man rtrash`). An artefact is one row; nothing below it is listed on its own, so the list names the `target` directory instead of the largest `.rlib` inside it. Scanning an artefact directly (`rtrash du target`) lists what is inside it, and the total names its kind. Names that are ambiguous on their own (`target`, `build`, `venv`) need a marker file such as `.rustc_info.json`, `CMakeCache.txt` or `pyvenv.cfg`.

du also labels each row in or above a git repository with a git state: `git:ignored`, `git:clean`, `git:dirty` (uncommitted changes, untracked files or a stash), `git:unpushed` (local commits that no remote has), or `git:unknown`. A directory takes the riskiest state among its own repository and every repository below it, so a folder of clones with one unpushed branch shows `git:unpushed`. `--print0` leaves out dirty, unpushed and unknown rows unless `--include-dirty` is given, and nothing inside `.git` or a bare repository is ever listed. A bare repository is one row; it shows `git:unpushed` when no remote has its commits, since it may be the only copy.

The last line reports the trash on the scanned filesystems. Trashing a build tree frees nothing until the trash is emptied, so the summary points at `rtrash empty -n` (preview), `rtrash empty` (free it all) and `rtrash empty 30` (keep the last 30 days). A trash directory inside the scanned tree shows up as one `trash` row, and `--print0` never prints it.

A cleanup pass on a source tree:

```console
$ rtrash du --artefacts --older-than=14 ~/src          # review
$ rtrash du --artefacts --older-than=14 --print0 ~/src | xargs -0 rtrash put -rf --
$ rtrash du --artefacts --pick ~/src                   # or mark rows by hand
$ rtrash empty -n && rtrash empty 14                    # free the space
```

### `rtrash restore [PATH]` (also `trash-restore`)

| Invocation | Behavior |
|------------|----------|
| `rtrash restore PATH` | Restore the item whose original path was `PATH` (exact match) |
| `rtrash restore` | **TUI** (TTY): fuzzy filter, multi-select (Space/a/A), bulk restore; session until `q` |
| `rtrash restore --plain` | Numbered line list + index (scripts / no TUI) |
| `printf '0-2,5\n' \| rtrash restore` | Piped selection without TUI: indices, ranges, commas, or `all` |
| `rtrash restore --glob='*.rs' --all` | Restore every item whose file name matches; a pattern with a `/` matches the original path |
| `rtrash restore --regex='/src/.*\.c$' --all` | Restore every item whose original path matches the regular expression |
| `rtrash restore --to=DIR …` | Restore into `DIR`, keeping each file name |
| `rtrash restore --conflict=rename\|skip\|overwrite …` | When the destination exists: restore as `NAME.1.EXT`, leave the item in the trash, or replace it (`-f`); the default refuses |
| `rtrash restore --cwd-only` | Same as bare restore, only originals under `$PWD` |
| `rtrash restore --name=NAME --latest --json` | Restore the newest item named `NAME` (file name or trash name) without a prompt and print `{"restored": [...], "failed": N, "skipped": N}` |
| `rtrash restore --newest-first` / `--oldest-first` | Set the list order. The interactive browser lists newest first and `o` flips it; `--plain` lists oldest first, so the newest item sits next to the prompt |
| Single match | Restores immediately without prompting |

**TUI keys (shared by restore / empty / put):** defaults `↑↓`/`jk` move · `PgUp`/`PgDn` page · `g`/`G` first/last · `Space` mark · `a`/`A` all/clear · `/` live fuzzy · `?` help · `Enter` action · `q`/`Esc` quit.

**Fully customizable:** every action can be remapped in `$XDG_CONFIG_HOME/rtrash/keys.toml` (`%APPDATA%\rtrash\keys.toml` on Windows without `XDG_CONFIG_HOME`, or `RTRASH_KEYS=path`). TOML under `[keys]`; `rtrash keys --list` shows the resolved map; `rtrash keys --sample` prints a full template. Help (`?`) reflects the live map. While a filter is open, plain letters, digits and symbols always type into it; arrows, Enter, Esc and Ctrl or Alt chords keep their bindings.

Browser-specific actions (also remappable): restore `toggle_force` / `toggle_order` · empty `toggle_dry_run` · put `toggle_recursive` / `toggle_force`.

**TUI empty:** bare `rtrash empty` on a TTY opens the multi-select permanent-delete browser (`--plain` or non-TTY keeps classic empty).

**TUI put:** bare `rtrash put` on a TTY opens the multi-select path browser for the current directory (`--plain` or FILE operands keep classic put).

Existing paths at the destination are preserved unless `-f` / `--force` is given; with `-f`, a blocking destination is removed first.
Same-filesystem restore uses `rename`; cross-device restore copies then deletes the trash payload (needed when put fell back to the home trash).
Options: `--trash-dir=PATH` (repeatable), `--home-only`, `--cwd-only`.

### `rtrash undo [BATCH]`

Each put run writes one batch id into every `.trashinfo` it creates, under the extra key `X-Rtrash-Batch` (other tools ignore keys they do not know).
`rtrash undo` restores every item of the most recent run, so `rtrash -r build dist logs` comes back in one step.

| Invocation | Behavior |
|------------|----------|
| `rtrash undo` | Restore every item the most recent put run trashed |
| `rtrash undo --list` | Put runs, newest first: id, date, item count, first path |
| `rtrash undo BATCH` | Restore the run with that id (from `--list`, or `batch` in `list --json`) |
| `rtrash undo -n` | Show what would come back |
| `rtrash undo --json` | Print `{"batch": ID, "restored": [...], "failed": N}` |

Items trashed by gio, Dolphin or trash-cli carry no batch id, so undo never picks them.
It refuses to overwrite a path that exists again unless given `-f`.
The library shares one id across a process, so a Python session counts as one run.

### `rtrash finder put|list|restore` (macOS Finder Trash)

> **Experimental and untested on a real Mac.** The `.DS_Store` reader passes tests on files written by rtrash and by the Python `ds_store` package, and put compiles for macOS. Neither has run against Finder yet.

| Invocation | Behavior |
|------------|----------|
| `rtrash finder put FILE...` | Ask Finder (through `osascript`) to trash each file, so Finder's own Put Back works. macOS only; symbolic links get refused |
| `rtrash finder list` | `NAME -> ORIGINAL-PATH` for each item in `~/.Trash`, read from the Put Back records in its `.DS_Store` (`?` when none) |
| `rtrash finder restore NAME\|ORIGINAL...` | Move items back to their Put Back location; `--all`, `--conflict=…` and `--json` work as for restore |
| `--trash-dir=DIR` | Another Finder trash, such as `/Volumes/X/.Trashes/501` (Put Back paths count from the volume root) |

List and restore only read files, so they also work on Linux against a copied trash.

### `rtrash config`

Defaults live in `$XDG_CONFIG_HOME/rtrash/config.toml`. Without `XDG_CONFIG_HOME` the file is `~/.config/rtrash/config.toml`, or `%APPDATA%\rtrash\config.toml` on Windows. Environment variables and command-line options win over it. `RTRASH_CONFIG=FILE` reads another file, and an empty value turns the file off. Run `rtrash config` to see the settings in effect, with a warning for each line it cannot read. Add `--path` to see where it looks, or `--default` for a template with every key commented out.

```toml
[du]
top = 30
sort = "disk"            # disk, apparent, items or mtime
exclude = ["*.iso", "/scratch/"]
gitignore = true

[list]
sort = "size"
size = true

[policy]
max_size = "50G"         # the prune after every put (RTRASH_MAX_SIZE wins)
prune = "oldest"         # or largest (RTRASH_PRUNE wins)

[protect]
paths = ["/srv/data", "/home/*/keep"]   # put refuses these, like safe-rm entries
```

The other `[du]` keys are `count`, `one_file_system`, `threads`, `git`, `exclude_regex`, `include_regex`, `exclude_caches`, `no_delete`, `si`, `notify` and `notify_after`, and `[list]` also takes `reverse`. Unknown keys and values of the wrong type earn a warning and are skipped, and the rest of the file still applies.

### `rtrash rm PATTERN...` (also `trash-rm`)

Permanently deletes trash entries whose original path, basename, or trash name matches a shell-style glob `PATTERN` (quote globs from the shell).
Matching `files/` payloads and `.trashinfo` files are removed; non-matches stay.
An orphan matches by its name in `files/` or its path there.
Does **not** restore.
On a terminal it names the matches (the first 10, with sizes) and asks once before it deletes, as `rm -I` does and as trash-cli #383 asks of `trash-rm`. Only `y` or `yes` goes on. A script, whose stdin or stderr is not a terminal, is never asked, so pipelines and cron jobs work as before. `-i`/`--interactive` asks even then, with the reply read from stdin. `-y`/`--yes` and `-f`/`--force` never ask, and the last of the three wins.
Options: `-v`/`--verbose`, `-n`/`--dry-run`, `-i`/`--interactive`, `-y`/`--yes`, `-f`/`--force` (no question, and mass patterns), `--trash-dir=PATH` (repeatable), `--home-only`, `--older-than=DAYS` / `--newer-than=DAYS`, `--larger-than=SIZE` / `--smaller-than=SIZE`, `--json` (summary + `matches` array).

### Prompts

Every question rtrash asks on the command line (`put -i`, `put --inspect`, `empty -i`, `rm -i`, the guards before a whole-trash empty) reads its answer the same way. Only `y` or `yes`, in any case and with blanks around it, goes on. Anything else is a no: `n`, `yolo`, an empty line, and the end of input. A pipe answers like a terminal, so `echo n | rtrash empty -i` keeps the trash (trash-cli #333). Unlike `rm -i`, an answer that merely starts with `y` is a no. The numbered pick of `restore` reads its line the same way.

## FreeDesktop durability notes

Previously deferred items are implemented in the shipped put/empty path:

- **Durable `.trashinfo`:** put `fsync`s the reserved info file (and best-effort the `info/` dir) before moving the payload.
- **Cross-device fidelity:** a put or restore across file systems copies, then removes the original once the copy is whole. The copy keeps bytes, holes in sparse files, mode, owner where allowed, times, extended attributes (which carry access control lists and capabilities on Linux; on macOS the access control list is copied on its own), symlinks as links, named pipes, and hard links inside the tree. Where both sides share one btrfs file system, such as two subvolumes, it clones instead. A tree with a directory this user can't empty is refused before anything moves. A copy that fails partway is removed again and the original stays whole; once the copy is whole, it stays even when the original can't all go, so no file is lost (trash-cli #429).
- **Put/empty exclusion:** each trash root takes an exclusive `flock` on `.rtrash.lock` for put and empty so the pair cannot tear mid-operation.
- **Btrfs multi-subvol topdir:** volume topdir is the **longest mount-point prefix** from `/proc/self/mounts`, not a pure `st_dev` parent walk.
- **No mount table (Termux):** when `/proc/self/mounts` cannot be read, the volume topdir comes from the `st_dev` parent walk, and `du --devices` lists the file systems of `/`, the home directory and the current directory.
- **Names that are not UTF-8:** put keeps the bytes of such a name in `files/` and `info/`, so two such names never fold into one. A `.trashinfo` whose `Path=` carries raw bytes from another tool still lists and restores, and an age filter can empty it. Output shows each stray byte as the Unicode replacement character.
- **Terminal-safe names:** when stdout is a terminal, text output shows control characters and bidirectional overrides in names as `?`, as `ls` does, so a hostile `Path=` in a shared trash can't rewrite the screen or fake rows. Pipes, files, and `--json` keep the raw name.
- **Default multi-volume empty:** with no `--trash-dir`, empty/list/restore/rm discover home trash plus existing user trash on every non-pseudo mount (including `/`), matching trash-cli’s multi-volume default. Pin with `--trash-dir` in scripts.

## Limitations

- **Platform:** Linux FreeDesktop is the primary niche. On macOS, plain put, list, restore and empty use the Finder Trash (experimental); `RTRASH_MACOS_TRASH=freedesktop` keeps the FreeDesktop home trash. Windows uses the system Recycle Bin (not FreeDesktop layout).
- **Restore UI:** bare `rtrash restore` on a TTY opens the **ratatui restore browser** (filter, navigate, multi-restore). Use `--plain` or a piped index for non-interactive selection. Scripts and path restore stay first-class.
- **Not a general soft-delete database:** only the FreeDesktop on-disk layout.
- **Across file systems:** hard links to files outside the trashed tree become separate copies, the copy leaves out an extended attribute the target refuses, sockets fail, and device nodes need privilege.
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
The walk keeps its own stack and at most 16 open directories, so trees of any depth go.
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
