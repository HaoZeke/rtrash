# Architecture and safety model

## What rtrash is

A **native** FreeDesktop trash implementation: one Rust library driving the `rtrash` multi-call CLI and optional Python bindings.
Files are **moved** into the same trash layout used by desktop file managers, not unlinked.

## FreeDesktop layout

| Location | Role |
|----------|------|
| `$XDG_DATA_HOME/Trash` | Home trash (default `~/.local/share/Trash`) |
| `$top/.Trash/$uid` | Per-mount shared trash (sticky, non-symlink) |
| `$top/.Trash-$uid` | Per-mount private trash |
| `files/` + `info/*.trashinfo` | Payload + `Path=` / `DeletionDate=` |

Atomic reservation: create-new on `.trashinfo` **before** moving the payload.

## Platform backends

| OS | Backend |
|----|---------|
| Linux | Full FreeDesktop (home + volume trash) |
| macOS (experimental) | FreeDesktop **home** trash only — **not** Finder Trash |
| Windows | System **Recycle Bin** (shell APIs) — **not** FreeDesktop on-disk layout |

Linux remains the primary multi-call / musl-release niche.

## Safer than permanent delete (`os.remove` / `rm`)

| Risk | `os.remove` / `Path.unlink` / `rm` | rtrash put |
|------|-------------------------------------|------------|
| Data gone immediately | Yes | No — recoverable until empty |
| Shared with DE trash UI | No | Yes |
| Accidental tree delete | Permanent | `restore` |
| Cross-device | Unlink only | Home-trash copy fallback |

rm-shaped fail-safes: refuse `.` / `..` / `/` (preserve-root), require `-r` for directories, last-wins `-f`/`-i`/`-I`, exit codes 0/1/2.

## Versus Python trash-cli

| Axis | trash-cli | rtrash |
|------|-----------|--------|
| Runtime | CPython scripts | Native binary / lib |
| Suite put/list/empty/restore/rm | Yes | Yes |
| rm-compatible put flags | Partial | Strong |
| In-process Python API | No (subprocess) | Yes (`import rtrash`) |
| Parallel / bulk empty | Sequential Python | Rayon + unlinkat; btrfs subvol when applicable |

## Where permanent delete is still preferable

Secrets wipe, non-Linux platforms, policies that forbid recoverability.

## Library layout

```
src/lib.rs main.rs put.rs empty.rs fastdelete.rs list.rs restore.rs rm.rs
src/trashdir.rs info.rs util.rs python.rs (feature = python)
```

CLI and Python share the same entry points.
