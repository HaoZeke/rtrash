# rtrash

A single fast binary for the [freedesktop.org Trash
specification](https://specifications.freedesktop.org/trash-spec/trashspec-latest.html)
with an rm-compatible interface. One static Rust executable replaces the
`trash-cli` Python suite (`trash-put`, `trash-empty`, `trash-list`,
`trash-restore`): no interpreter startup, and emptying deletes trash entries
in parallel via [rayon](https://crates.io/crates/rayon).

## Install

```console
$ cargo install --git https://github.com/HaoZeke/rtrash
```

Optional multi-call symlinks make it a drop-in for `rm` and the trash-cli
commands (dispatch is on `argv[0]`):

```console
$ for n in trash-put trash-empty trash-list trash-restore; do
>   ln -s "$(command -v rtrash)" ~/.local/bin/$n
> done
$ ln -s "$(command -v rtrash)" ~/.local/bin/rm   # optional, shadows rm
```

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

## Reference

### `rtrash put` (also `rm`, `trash-put`, or bare `rtrash`)

Accepts the common `rm(1)` flags with the same semantics, except that files
move to the trash: `-f`, `-i`, `-I`, `--interactive[=WHEN]`, `-r`/`-R`, `-d`,
`-v`, `--preserve-root` (default), `--no-preserve-root`, `--`. Directories
need `-r` (or `-d` when empty), `.`/`..`/`/` are refused, and exit codes
mirror rm (0 success, 1 failure, 2 usage error).

Trash placement follows the spec: the home trash
(`$XDG_DATA_HOME/Trash`) for same-filesystem files, `$top/.Trash/$uid` or
`$top/.Trash-$uid` on other mounts, with a copy-into-home-trash fallback.
Names are reserved atomically (`O_EXCL` on the `.trashinfo`), so concurrent
invocations never clobber each other; collisions get `name.2`, `name.3`, ...

### `rtrash empty [DAYS]` (also `trash-empty`)

Purges every trash directory visible to the user (home trash plus mounted
volumes). With `DAYS`, only items trashed more than `DAYS` days ago go.
Entries are removed in parallel. Orphaned `files/` entries (no
`.trashinfo`) and entries with broken metadata are purged on a full empty,
and the `directorysizes` cache is pruned. Options: `-n`/`--dry-run`,
`-v`/`--verbose`, `--trash-dir=PATH` (repeatable), `-f` (accepted for
trash-cli compatibility; emptying never prompts).

### `rtrash list` (also `trash-list`)

Prints `DELETION-DATE ORIGINAL-PATH` per item, oldest first, in the
`trash-list` output format.

### `rtrash restore [PATH]` (also `trash-restore`)

Restores the item trashed from `PATH` (or picks among items trashed from
under the current directory when `PATH` is omitted). A single match restores
directly; multiple matches list with indices for interactive selection.
Existing files at the destination are preserved unless `-f` is given.

## Performance

Measured against trash-cli 0.24.5.26 (CPython 3) on an NVMe-backed Linux
machine, best of warm runs:

| Operation                  | trash-cli | rtrash | ratio |
| -------------------------- | --------- | ------ | ----- |
| `empty`, 100 000 entries   | 0.92 s    | 0.36 s | 2.5x  |
| `empty`, 20 000 entries    | 0.21 s    | 0.07 s | 3x    |
| `put`, one file            | 52 ms     | 1 ms   | 52x   |

Emptying parallelizes the unlinks (3.3 s of system time in 0.36 s of wall
time on the 100 000-entry run); `put` wins on process startup, which is what
an interactive shell feels.

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
