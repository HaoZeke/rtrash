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
| Parallel / bulk empty | Sequential Python | Rayon + unlinkat; btrfs subvol when applicable; optional MPI `drm` |

## Permanent-delete backends

`rm`, `empty`, and the empty TUI purge payloads directly under a trash `files/` directory through one helper (`trashdir::remove_trash_payload`).

| Backend | Chosen when | Module |
|---------|-------------|--------|
| btrfs subvolume destroy | payload is a btrfs subvolume root (ino 256) | `fastdelete.rs` |
| mpifileutils `drm` | `drm` + `mpirun`/`mpiexec`/`srun` on `PATH`; `RTRASH_DRM=always`, or `auto` with a parallel/network filesystem (Lustre, GPFS, BeeGFS, NFS, SMB, CephFS, PanFS, OrangeFS) at `RTRASH_DRM_MIN_ENTRIES`, or a local one at `RTRASH_DRM_LOCAL_MIN_ENTRIES` | `drm.rs` |
| rayon + `unlinkat` | everything else, and any drm failure | `fastdelete.rs` |

drm is spawned as `LAUNCHER -np N drm -- PATH` from an argv vector. `PATH` is the canonical path of a real (non-symlink) directory whose parent is the resolved `files/`; user paths, restore destinations, and symlink targets never reach it. The `.trashinfo` is unlinked only after drm exits 0 and the path is gone; otherwise the exit status is reported and the native walker runs. `[drm]` in `$XDG_CONFIG_HOME/rtrash/config.toml` holds the same settings as the `RTRASH_DRM*` variables (environment wins).

## Where permanent delete is still preferable

Secrets wipe, non-Linux platforms, policies that forbid recoverability.

## Library layout

```
src/lib.rs main.rs put.rs empty.rs fastdelete.rs drm.rs fastcopy.rs list.rs restore.rs rm.rs
src/trashdir.rs info.rs util.rs python.rs (feature = python)
```

CLI and Python share the same entry points.
