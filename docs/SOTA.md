# rtrash vs state-of-the-art trash CLIs

Audit of **rtrash 0.1.0** against the FreeDesktop.org Trash specification and
common CLI tools people actually install as `rm` / trash-cli replacements.
Scope is **Linux FreeDesktop trash**, not macOS Finder Trash or the Windows
Recycle Bin. Verdicts are about **correctness, compatibility, and operational
fitness**, not marketing feature counts.

Primary references (read-only research, not re-benchmarked in this audit):

| Tool | Stack | Role |
| ---- | ----- | ---- |
| [trash-cli](https://github.com/andreafrancia/trash-cli) | Python | De-facto FreeDesktop CLI suite (`trash-put` / `empty` / `list` / `restore` / `rm`) |
| [trashy](https://github.com/oberblastmeister/trashy) | Rust | Fast UX-oriented manager; Linux + Windows system trash |
| [gtrash](https://github.com/umlx5h/gtrash) | Go | Modern TUI-ish restore / fuzzy find; FreeDesktop-oriented |
| [trash-d](https://github.com/rushsteve1/trash-d) | D | Drop-in `rm` focus, FreeDesktop backend |
| `gio trash` | GLib | Desktop stack integration when GNOME/GVFS is present |

The FreeDesktop [Trash specification](https://specifications.freedesktop.org/trash-spec/trashspec-latest.html)
defines home trash (`$XDG_DATA_HOME/Trash`), per-mount `$topdir/.Trash/$uid` or
`$topdir/.Trash-$uid`, `.trashinfo` with `Path=` / `DeletionDate=`, and
**atomic** reservation of the info file before moving the payload.

## What rtrash optimizes for

rtrash is a **single multi-call binary** that aims to be:

1. **FreeDesktop-correct** — same trashcan as file managers on the same machine.
2. **rm-compatible on the put path** — common GNU `rm` flags and exit codes, so
   `alias rm=rtrash` / a symlink named `rm` is plausible for interactive shells.
3. **A trash-cli command-name replacement** — `trash-put`, `trash-empty`,
   `trash-list`, `trash-restore` (and bare `trash`) dispatch from `argv[0]`,
   plus subcommands `put` / `empty` / `list` / `restore`.

It is **not** trying to be the best colored table UI, the best cross-OS trash
abstraction, or a full desktop-session agent.

## Competitive matrix (honest, not exhaustive)

| Capability | rtrash | trash-cli | trashy | gtrash | trash-d | gio trash |
| ---------- | ------ | --------- | ------ | ------ | ------- | --------- |
| FreeDesktop home + mount trash | yes | yes | yes (Linux) | yes | yes | via GVFS |
| Atomic `.trashinfo` (`O_EXCL` / create-new) | yes | yes | yes (typical) | yes (typical) | yes (typical) | yes |
| rm-style flags on put (`-rf`, `-i`/`-I`, preserve-root, …) | **strong** | partial (own CLI) | different UX | different UX | **strong** (rm focus) | no |
| Multi-call trash-cli names | put/empty/list/restore (+ `trash`) | native suite | own names | own names | rm-oriented | no |
| `trash-rm` (delete selected trash entries by pattern) | **no** | yes | restore/empty UX | own UX | n/a focus | limited |
| Parallel empty | yes (rayon) | sequential Python | fast native | fast native | native | session |
| Interactive multi-match restore | index select | index select | richer UI | fuzzy/TUI | varies | GUI |
| Cross-device put (copy into home trash) | yes | yes | yes | yes | yes | yes |
| Cross-device restore (EXDEV copy-back) | yes | yes | varies | varies | varies | yes |
| Windows / macOS system trash | **no** | no (FreeDesktop) | Windows yes | limited | no | platform APIs elsewhere |
| Pretty tables / colors | **no** | no | **yes** | **yes** | no | no |
| Interpreter / runtime | none (native) | CPython | none | none | none | libgio |

“Typical” for competitor cells means public docs and common implementations of
the same FreeDesktop layout; this audit did not re-audit every release line of
each upstream tree.

## Where rtrash is competitive (SOTA *for its niche*)

For **Linux users who want one small native binary that is both an rm-shaped
put tool and a trash-cli-shaped suite**, rtrash is in the top tier:

- **Spec placement**: home trash vs `$top/.Trash/$uid` (sticky, non-symlink) vs
  `$top/.Trash-$uid`, with home-trash fallback when the volume cannot host a
  trash dir.
- **Atomic name reservation** via create-new on `info/<name>.trashinfo` before
  moving the file; collisions become `name.2`, `name.3`, …
- **rm semantics on put**: directories need `-r` (or `-d` for empty dirs);
  `.` / `..` / `/` (with preserve-root) refused; `-f` / `-i` / `-I` /
  `--interactive=…` with **last-flag-wins** between force and interactive
  modes; exit status 0 / 1 / 2 aligned with common rm expectations.
- **Empty**: age filter (`DAYS`), dry-run, verbose, repeatable `--trash-dir`,
  orphan `files/` purge on full empty, `directorysizes` cache prune, parallel
  unlinks.
- **Restore**: path match or cwd-scoped multi-select; refuses overwrite without
  `-f`; `-f` removes a blocking destination; relocate falls back across devices.
- **Startup cost**: no Python import path; suitable for interactive shell
  aliases where process spawn dominates.

Against **trash-cli**, rtrash is competitive on put/list/empty/restore for
everyday FreeDesktop use and is typically faster at process start and large
empties (see README Performance for a dated measurement; do not treat the
table as a live benchmark).

Against **trash-d**, rtrash is in the same “native rm-compatible FreeDesktop”
class; trash-d historically emphasizes rm drop-in purity, while rtrash also
ships the full list/restore/empty multi-call surface in one binary.

## Where rtrash is behind (not SOTA overall)

rtrash is **not** the state of the art if the metric is “best trash UX app” or
“broadest platform coverage”:

| Gap | Why it matters | Status |
| --- | -------------- | ------ |
| No `trash-rm` | trash-cli users purge individual patterns without emptying everything | **Not yet** — use empty with care or delete after restore |
| No colored tables / fuzzy restore | trashy and gtrash win interactive discovery of large trashcans | **Non-goal** for now (see below) |
| Linux FreeDesktop only | trashy covers Windows system trash; macOS needs different APIs | **Non-goal** |
| No `directorysizes` writer on put | Some file managers use the cache for trash size; empty only prunes | Acceptable; size display may be slower in DEs until rewritten |
| No `--trash-dir` on list/restore | trash-cli can pin operations; empty already supports pinning | **Not yet** |
| Not on crates.io / distro packages as of this audit | install is git/`cargo install` only in-tree docs | Packaging out of scope for this audit |
| Unverified continuous benchmarks | README numbers are historical single-machine runs | Qualified in README; not re-claimed as live SOTA speed |

**Overall verdict:** rtrash is **SOTA-adjacent for the narrow niche** “native,
FreeDesktop-correct, rm-compatible multi-call trash-cli replacement on Linux.”
It is **not** SOTA as a general “trash manager product” versus trashy/gtrash UX
or as a cross-platform trash abstraction. Prefer rtrash when scriptability,
rm habits, and DE-shared FreeDesktop layout matter more than TUI polish.

## Non-goals (explicit)

- Full feature parity with gtrash/trashy UX (colors, tables, fuzzy pickers).
- Windows Recycle Bin / macOS Trash backends.
- Replacing FreeDesktop with a private soft-delete store.
- Guaranteeing the fastest empty on every hardware; only qualify measured data.

## Audit method (this revision)

- Full pass of `src/**` and `tests/cli.rs` against README claims.
- Cross-check FreeDesktop layout, atomic info creation, mount discovery via
  `/proc/self/mounts`, and multi-call dispatch in `main.rs`.
- Competitor feature framing from public project documentation (not a formal
  re-implementation review of each upstream tree).
- Material defects found and fixed in the same change set as this document:
  GNU-style last-wins for `-f`/`-i`, restore force-overwrite of directory
  destinations, and EXDEV-aware restore relocate.
