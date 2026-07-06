# rtrash: SOTA for the Linux FreeDesktop / rm-compatible trash-cli niche

**Verdict (rtrash 0.1.x):** rtrash is **state of the art for its niche** — a
single native Linux binary that is FreeDesktop-correct, rm-compatible on put,
and a full multi-call replacement for the everyday trash-cli suite
(`trash-put` / `trash-empty` / `trash-list` / `trash-restore` / `trash-rm`),
including mount-aware trash placement, atomic `.trashinfo` reservation,
`--trash-dir` pinning on list/empty/restore/rm, FreeDesktop `directorysizes`
updates on directory put, parallel empty, and EXDEV-safe restore.

It is **not** competing on trashy/gtrash UX polish (colored tables, fuzzy TUI)
or non-Linux system trash backends; those remain explicit non-goals.

## Niche definition

| In scope | Out of scope |
| -------- | ------------ |
| FreeDesktop home + per-mount trash | Windows Recycle Bin / macOS Trash |
| rm-compatible put flags and exit codes | Colored tables / fuzzy restore UIs |
| Multi-call trash-cli command names | Private soft-delete stores |
| Selective permanent delete (`trash-rm`) | Desktop-session agents (GVFS-only) |
| Script-friendly `--trash-dir` pins | Distro packaging / crates.io as a gate |

## Primary competitors

| Tool | Stack | Relation to this niche |
| ---- | ----- | ---------------------- |
| [trash-cli](https://github.com/andreafrancia/trash-cli) | Python | De-facto FreeDesktop CLI suite; rtrash targets drop-in command coverage without CPython |
| [trashy](https://github.com/oberblastmeister/trashy) | Rust | Faster UX-oriented manager; wins on tables/Windows, different CLI surface |
| [gtrash](https://github.com/umlx5h/gtrash) | Go | Strong interactive/fuzzy restore UX |
| [trash-d](https://github.com/rushsteve1/trash-d) | D | Native rm drop-in focus; rtrash also ships full list/restore/empty/rm suite |
| `gio trash` | GLib | Correct when GVFS is present; not a multi-call rm/trash-cli replacement |

## Suite surface (shipped)

| Role | Subcommand | Multi-call `argv[0]` |
| ---- | ---------- | -------------------- |
| Put (rm-shaped) | `put` / bare args | `rm`, `trash`, `trash-put` |
| List | `list` | `trash-list` |
| Restore | `restore` | `trash-restore` |
| Empty | `empty` | `trash-empty` |
| Selective permanent delete | `rm PATTERN…` | `trash-rm` |

Note: multi-call name **`rm`** means *put into trash* (shell `rm` replacement).
Subcommand **`rtrash rm`** / multi-call **`trash-rm`** means *permanently delete
matching trash entries* (trash-cli `trash-rm`).

## Strengths that establish the niche claim

- **Atomic `directorysizes`:** temp+rename rewrite on directory put/prune
- **`rtrash status`:** item count + reclaimable size per discovered root
- **`rtrash rm -n` / `--dry-run`:** match report + reclaim without permanent delete
- **`--home-only`:** suite commands can skip multi-volume discovery (default remains all volumes)



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
   and selective delete prune the cache.
6. **Restore robustness** — refuse overwrite without `-f`; force removes
   blocking destinations; EXDEV relocate after cross-device put fallback.
7. **Performance class** — native binary, rayon-parallel empty; no interpreter
   startup (see README for historical measurements, not a live leaderboard).

## Explicit non-goals (do not block the niche SOTA claim)

- trashy/gtrash colored tables, fuzzy pickers, TUI restore flows
- Windows / macOS system trash APIs
- crates.io or distro packaging as a requirement for correctness
- Replacing FreeDesktop on-disk layout

## Method

Implementation and CLI tests in-tree under isolated `XDG_DATA_HOME`; remote
`cargo test` on the project build host. Competitor framing from public project
roles, not a re-benchmark of every upstream release.
