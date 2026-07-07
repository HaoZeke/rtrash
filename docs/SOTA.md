# rtrash: SOTA for the Linux FreeDesktop / rm-compatible trash-cli niche

**Verdict (rtrash 0.1.x):** rtrash is **state of the art for its niche** — a
single native Linux binary that is FreeDesktop-correct, GNU-rm-compatible on
put, and a full multi-call replacement for the everyday trash-cli suite
(`trash-put` / `trash-empty` / `trash-list` / `trash-restore` / `trash-rm`),
including mount-aware trash placement, atomic `.trashinfo` reservation,
`--trash-dir` pinning on list/empty/restore/rm, FreeDesktop `directorysizes`
updates on directory put, parallel empty, EXDEV-safe restore, `status`, dry-run
reclaim, `--home-only`, and `rtrash setup` for multi-call links, completions,
and man pages.

It is **not** competing on trashy/gtrash UX polish (colored tables, fuzzy TUI)
or non-Linux system trash backends; those remain explicit non-goals.

## Niche definition

| In scope | Out of scope |
| -------- | ------------ |
| FreeDesktop home + per-mount trash | Windows Recycle Bin / macOS Trash |
| rm-compatible put flags and exit codes | Colored tables / fuzzy restore UIs |
| Multi-call trash-cli command names | Private soft-delete stores |
| Selective permanent delete (`trash-rm`) | Desktop-session agents (GVFS-only) |
| Script-friendly `--trash-dir` pins | Distro packaging / crates.io as a *correctness* gate |

## Primary competitors

| Tool | Stack | Relation to this niche |
| ---- | ----- | ---------------------- |
| [trash-cli](https://github.com/andreafrancia/trash-cli) | Python | De-facto FreeDesktop CLI suite; rtrash targets drop-in multi-call coverage without CPython |
| [trashy](https://github.com/oberblastmeister/trashy) | Rust | UX tables/colors and Windows system trash; different CLI surface |
| [gtrash](https://github.com/umlx5h/gtrash) | Go | Strong interactive/TUI restore UX on FreeDesktop layout |
| [trash-d](https://github.com/rushsteve1/trash-d) | D | Native rm drop-in focus; not the full trash-cli suite as product center |
| `gio trash` | GLib | Correct when GVFS is present; not a multi-call rm/trash-cli replacement |

No single peer simultaneously owns FreeDesktop fidelity, the full trash-cli
multi-call suite, GNU-rm-shaped put, selective `trash-rm`, script pins, and a
native empty path without pivoting to TUI/Windows/tables as the product identity.

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

Note: multi-call name **`rm`** means *put into trash*. Subcommand **`rtrash rm`**
/ multi-call **`trash-rm`** permanently deletes matching *trash entries*.

## Strengths that establish the niche claim

1. **FreeDesktop placement** — home + volume trash, atomic `.trashinfo`,
   collision names, home-trash copy fallback on unusable volume trash.
2. **rm put semantics** — `-rf`, `-d`, interactive/force last-wins, preserve-root,
   exit 0/1/2.
3. **Full suite** — list/restore/empty/rm with shared entry scan and shell globs
   on selective permanent delete.
4. **Script pins** — `--trash-dir` (repeatable) and `--home-only` on suite
   commands that discover trash roots.
5. **directorysizes** — written and pruned on directory put / empty / selective
   delete (status/dry-run may still walk payloads for size).
6. **Restore robustness** — no overwrite without `-f`; EXDEV relocate after
   cross-device put fallback.
7. **Durability/concurrency** — trashinfo fsync path, per-root flock, mount-aware
   topdir inference.
8. **Performance class** — native binary, rayon-parallel empty (see
   `docs/benchmarks.md` for historical measurements).
9. **Operator setup** — `rtrash setup` installs multi-call links, bash/zsh
   completions, and man from embedded assets.

## Explicit non-goals (do not block the niche SOTA claim)

- trashy/gtrash colored tables, fuzzy pickers, TUI restore flows
- Windows / macOS system trash APIs
- crates.io or distro packaging as a requirement for correctness
- Replacing FreeDesktop on-disk layout

## Method

Implementation and CLI tests in-tree under isolated `XDG_DATA_HOME`. Competitor
framing from public project roles and the FreeDesktop trash specification;
benchmark numbers in-repo are historical unless re-dated.
