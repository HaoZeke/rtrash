# rtrash: SOTA for the Linux FreeDesktop / rm-compatible trash-cli niche

**Verdict (rtrash 0.1.x, research refresh 2026-07-07):** rtrash remains
**state of the art for its niche** — a single native Linux binary that is
FreeDesktop-correct, GNU-rm-compatible on put, and a full multi-call
replacement for the everyday trash-cli suite (`trash-put` / `trash-empty` /
`trash-list` / `trash-restore` / `trash-rm`), including mount-aware trash
placement, atomic `.trashinfo` reservation, `--trash-dir` pinning on
list/empty/restore/rm, FreeDesktop `directorysizes` **writes**, parallel empty,
EXDEV-safe restore, `status`, dry-run reclaim, `--home-only`, and
`rtrash setup` for links/completions/man.

It is **not** competing on trashy/gtrash UX polish (colored tables, fuzzy TUI)
or non-Linux system trash backends; those remain explicit non-goals.

**Remaining-issues conclusion:** There is **no material open defect** that
invalidates the niche SOTA claim (suite coverage, FreeDesktop put/restore
layout, script pins, native empty). Open work is **performance/docs/polish**:
prefer reading the `directorysizes` cache in status/dry-run paths; optional
fish completions; release-artifact adoption path; dated benchmark refresh.
See vault issues under `Software/rtrash/issues.org` (IDs in the table below).

## Niche definition

| In scope | Out of scope |
| -------- | ------------ |
| FreeDesktop home + per-mount trash | Windows Recycle Bin / macOS Trash |
| rm-compatible put flags and exit codes | Colored tables / fuzzy restore UIs |
| Multi-call trash-cli command names | Private soft-delete stores |
| Selective permanent delete (`trash-rm`) | Desktop-session agents (GVFS-only) |
| Script-friendly `--trash-dir` pins | Distro packaging / crates.io as a *correctness* gate |

## Primary competitors (live research 2026-07-07)

| Tool | Stack | What it still claims to own | Relation to this niche |
| ---- | ----- | --------------------------- | ---------------------- |
| [trash-cli](https://github.com/andreafrancia/trash-cli) | Python | Canonical FreeDesktop **CLI suite** (`trash-put` … `trash-rm`); interactive restore; age empty; glob `trash-rm` | De-facto suite reference; rtrash targets the same multi-call surface without CPython startup |
| [trashy](https://github.com/oberblastmeister/trashy) | Rust | Fast put/list; **tables/colors**; Linux **and Windows** system trash; own CLI (`put`/`list`/`restore`/`empty`) | UX + Windows axes; not trash-cli multi-call identity (v2.0.0 still latest public release tag as of research) |
| [gtrash](https://github.com/umlx5h/gtrash) | Go | **Full FreeDesktop** claim; **TUI restore**; directory size cache for filters; `summary`; co-delete restore-group | Owns interactive/TUI FreeDesktop management — explicit rtrash non-goal |
| [trash-d](https://github.com/rushsteve1/trash-d) | D | Near drop-in **`rm` → trash**; FreeDesktop layout; stable/feature-complete posture | rm-compat put focus; not the full trash-cli list/restore/empty/rm product center |
| `gio trash` | GLib/GVFS | Desktop-correct trash when GVFS/session plumbing is present | Not a multi-call rm/trash-cli replacement |

Sources consulted this refresh: peer GitHub READMEs, FreeDesktop trash-spec
latest (`directorysizes` SHOULD), Arch Wiki trash management page, package
indexes (e.g. FreshPorts trash-cli version class). See implementer
`research-notes.md` for the full URL list.

### Does any peer close the niche?

No single peer simultaneously ships: FreeDesktop fidelity **and** full
trash-cli multi-call suite **and** GNU-rm-shaped put **and** selective
`trash-rm` **and** script `--trash-dir` pins **and** a native empty path,
without pivoting product identity to TUI/Windows/tables. trash-cli remains the
Python suite reference; rtrash remains the **native single-binary** answer for
that suite shape on Linux.

## Suite surface (shipped)

| Role | Subcommand | Multi-call `argv[0]` |
| ---- | ---------- | -------------------- |
| Put (rm-shaped) | `put` / bare args | `rm`, `trash`, `trash-put` |
| List | `list` | `trash-list` |
| Restore | `restore` | `trash-restore` |
| Empty | `empty` | `trash-empty` |
| Selective permanent delete | `rm PATTERN…` | `trash-rm` |
| Status summary | `status` | — |
| Install assets | `setup` / `completions` / `man` | — |

Note: multi-call name **`rm`** means *put into trash* (shell `rm` replacement).
Subcommand **`rtrash rm`** / multi-call **`trash-rm`** means *permanently delete
matching trash entries* (trash-cli `trash-rm`).

## Strengths that establish the niche claim

1. **FreeDesktop placement** — `$XDG_DATA_HOME/Trash`, sticky `$top/.Trash/$uid`,
   `$top/.Trash-$uid`, home-trash copy fallback; atomic create-new on
   `.trashinfo` before payload move; collision names `name.2`, …
2. **rm put semantics** — `-rf`, `-d`, `-i`/`-I`/`--interactive`, preserve-root,
   last-wins force vs interactive, exit 0/1/2.
3. **Full suite** — list/restore/empty/rm with shared entry scan; `trash-rm`
   shell globs on original path, basename, and trash name.
4. **Script pins** — `--trash-dir=PATH` (repeatable) on **empty, list, restore,
   and rm** so sandboxes never walk host mounts.
5. **directorysizes** — directory put writes `size mtime encoded-name`; empty
   and selective delete prune the cache (**read path for status still walks** —
   see remaining issues).
6. **Restore robustness** — refuse overwrite without `-f`; force removes
   blocking destinations; EXDEV relocate after cross-device put fallback.
7. **Durability/concurrency** — trashinfo fsync path, per-root flock, longest
   mount-prefix topdir inference.
8. **Performance class** — native binary, rayon-parallel empty; no interpreter
   startup (see `docs/benchmarks.md` for **historical** measurements — refresh
   tracked as a task issue).
9. **Operator setup** — `rtrash setup` embeds and installs multi-call links,
   bash/zsh completions, and man under a user prefix.

## Remaining issues (audit 2026-07-07)

| Severity | Gap | Must-fix for niche SOTA? | Vault issue |
| -------- | --- | -------------------------- | ----------- |
| **B** (perf fidelity) | `directorysizes` written/pruned but **not read** by `status` / reclaim-size paths (spec *SHOULD* use cache for size) | **No** (correctness intact; gtrash-class size-cache *use* incomplete) | `rtrash-3jlp` |
| **C** (polish) | No fish completions (bash/zsh shipped) | No (explicit optional) | `rtrash-rlnw` |
| **C** (adoption) | No first-class binary release / crates.io path | No (packaging non-goal as correctness gate) | `rtrash-693p` |
| **C** (docs evidence) | Competitor microbenches not re-run in this refresh | No (qualitative peer roles reaffirmed live) | `rtrash-b02c` |

### Explicit non-issues (do **not** treat as open must-fix work)

- trashy/gtrash **TUI**, fuzzy pickers, co-delete restore-group, colored tables
- Windows / macOS **native** system trash APIs
- Replacing FreeDesktop on-disk layout with a private store
- Perfect byte-identical list formatting vs trash-cli
- Requiring crates.io publish for the niche claim

## Explicit non-goals (do not block the niche SOTA claim)

- trashy/gtrash colored tables, fuzzy pickers, TUI restore flows
- Windows / macOS system trash APIs
- crates.io or distro packaging as a requirement for correctness
- Replacing FreeDesktop on-disk layout

## Method

Implementation and CLI tests in-tree under isolated `XDG_DATA_HOME`; remote
`cargo test` on the project build host. Competitor framing from **live public
project READMEs and FreeDesktop trash-spec (2026-07-07)**, plus historical
in-repo benchmarks with date caveats — not a re-benchmark of every upstream
release on this analysis pass.
