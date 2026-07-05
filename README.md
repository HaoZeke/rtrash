# rtrash

A single native binary for the [freedesktop.org Trash
specification](https://specifications.freedesktop.org/trash-spec/trashspec-latest.html)
with an rm-compatible put interface. One Rust executable covers the
`trash-cli` command names (`trash-put`, `trash-empty`, `trash-list`,
`trash-restore`, and bare `trash`): no interpreter startup, and emptying
deletes trash entries in parallel via [rayon](https://crates.io/crates/rayon).

**Platform:** Linux FreeDesktop trash only (home trash + per-mount trash
dirs). Not a Windows/macOS system-trash wrapper.

**Competitive position:** for that niche (native, FreeDesktop-correct,
rm-compatible multi-call replacement), rtrash is strong; it is not a
full UX competitor to trashy/gtrash. See [docs/SOTA.md](docs/SOTA.md).

## Install

```console
$ cargo install --git https://github.com/HaoZeke/rtrash
```

Requires a Rust toolchain (MSRV **1.77**). The default `cargo install`
build is a normal dynamically linked Linux binary (glibc), not a musl
static link; use a musl target yourself if you need a fully static
artifact.

Optional multi-call symlinks make it a drop-in for `rm` and the trash-cli
commands (dispatch is on `argv[0]`):

```console
$ for n in trash trash-put trash-empty trash-list trash-restore; do
>   ln -s "$(command -v rtrash)" ~/.local/bin/$n
> done
$ ln -s "$(command -v rtrash)" ~/.local/bin/rm   # optional, shadows rm
```

Subcommands also work without symlinks: `rtrash put|empty|list|restore …`.

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

`rtrash FILE` with no subcommand behaves like `rm`, so shell habits carry
over: `rtrash -rf build/` moves `build/` to the trash instead of unlinking
it.

**Scripts / tests:** prefer `rtrash empty --trash-dir="$XDG_DATA_HOME/Trash"`
(or an isolated `XDG_DATA_HOME`) so a bare `empty` does not walk every
mounted volume’s trash on the machine.

## Reference

### `rtrash put` (also `rm`, `trash`, `trash-put`, or bare `rtrash`)

Accepts the common `rm(1)` flags with the same semantics, except that files
move to the trash: `-f`, `-i`, `-I`, `--interactive[=WHEN]`, `-r`/`-R`, `-d`,
`-v`, `--one-file-system` (accepted for rm compatibility; no-op because
entries move whole and are never walked like `rm -x`), `--preserve-root`
(default), `--no-preserve-root`, `--`. Directories need `-r` (or `-d` when
empty), `.`/`..`/`/` are refused (unless `--no-preserve-root` for `/`), and
exit codes mirror rm (0 success, 1 failure, 2 usage error).

As with GNU rm, **the last of `-f` / `-i` / `-I` (and the matching long
forms) wins** for prompt vs force behavior. `-f` also ignores missing paths.

Trash placement follows the spec: the home trash
(`$XDG_DATA_HOME/Trash`, defaulting under `~/.local/share`) for same-filesystem
files, `$top/.Trash/$uid` (must be a sticky non-symlink directory) or
`$top/.Trash-$uid` on other mounts, with a copy-into-home-trash fallback when
the volume cannot host a trash directory. Names are reserved atomically
(create-new / `O_EXCL` on the `.trashinfo`), so concurrent invocations never
clobber each other; collisions get `name.2`, `name.3`, ...

### `rtrash empty [DAYS]` (also `trash-empty`)

Purges every trash directory visible to the user (home trash plus mounted
volumes). With `DAYS`, only items trashed more than `DAYS` days ago go.
Entries are removed in parallel. Orphaned `files/` entries (no
`.trashinfo`) and entries with broken metadata are purged on a full empty,
and the `directorysizes` cache is pruned when present. Options: `-n`/`--dry-run`,
`-v`/`--verbose`, `--trash-dir=PATH` (repeatable), `-f` (accepted for
trash-cli compatibility; emptying never prompts).

### `rtrash list` (also `trash-list`)

Prints `DELETION-DATE ORIGINAL-PATH` per item, oldest first, in the
`trash-list` output format (`YYYY-MM-DD HH:MM:SS` plus the original path).
Scans the home trash and per-mount trash directories owned by the current
user. No filter flags in this version.

### `rtrash restore [PATH]` (also `trash-restore`)

Restores the item trashed from `PATH` (or picks among items trashed from
under the current directory when `PATH` is omitted). A single match restores
directly; multiple matches list with indices for interactive selection
(requires a TTY on stdin). Existing paths at the destination are preserved
unless `-f` / `--force` is given; with `-f`, a blocking destination is
removed first. Same-filesystem restore uses `rename`; cross-device restore
copies then deletes the trash payload (needed when put fell back to the home
trash).

## Limitations and non-goals

- **No `trash-rm`:** cannot permanently delete selected trash entries by
  pattern the way trash-cli’s `trash-rm` does; use a careful `empty` or
  restore-then-delete.
- **No pretty TUI:** no colored tables or fuzzy restore (see trashy/gtrash).
- **Linux FreeDesktop only:** no Windows Recycle Bin / macOS Trash backends.
- **No `directorysizes` updates on put:** empty may prune an existing cache;
  desktop “trash size” may recompute until something else rewrites the cache.
- **list/restore have no `--trash-dir`:** only `empty` pins trash directories
  today.
- **Not a general soft-delete database:** only the FreeDesktop on-disk layout.

Details and competitor framing: [docs/SOTA.md](docs/SOTA.md).

## Performance

Historical single-machine comparison against trash-cli **0.24.5.26**
(CPython 3) on an NVMe-backed Linux host, best of warm runs. These numbers
are **not continuously re-verified in CI** and will vary by filesystem,
core count, and trash layout; treat them as order-of-magnitude evidence that
native startup and parallel empty help, not as a live leaderboard.

| Operation                  | trash-cli | rtrash | ratio |
| -------------------------- | --------- | ------ | ----- |
| `empty`, 100 000 entries   | 0.92 s    | 0.36 s | 2.5x  |
| `empty`, 20 000 entries    | 0.21 s    | 0.07 s | 3x    |
| `put`, one file            | 52 ms     | 1 ms   | 52x   |

Emptying parallelizes the unlinks (about 3.3 s of system time in 0.36 s of
wall time on that 100 000-entry run); interactive `put` mainly wins on
process startup.

## Development

```console
$ cargo test
```

Integration tests isolate trash under a temporary `XDG_DATA_HOME` and pin
`empty --trash-dir=…` so they never clear the host trash.

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
