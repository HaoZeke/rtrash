# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

This project uses [*towncrier*](https://towncrier.readthedocs.io/).
Unreleased fragments live in [`docs/newsfragments/`](docs/newsfragments/).
Name fragments `+slug.<type>.md` (no issue) or `<issue>.<type>.md` (links to GitHub).

<!-- towncrier release notes start -->

## [0.1.5](https://github.com/HaoZeke/rtrash/tree/v0.1.5) - 2026-11-19

### Added

- A put or restore across devices now refuses a copy that would leave the target under 5% of its bytes or inodes free, or under `RTRASH_MIN_FREE` bytes.
  Pass `--no-space-check` to copy anyway.
- All six completion scripts complete `rtrash wsl restore` with the Recycle Bin items, read from `rtrash wsl list --format`.
- All six completion scripts complete `rtrash wsl rm` with the Recycle Bin items and `rtrash wsl --name=` with their file names.
- Bare `rtrash list PATTERN` keeps items whose name contains PATTERN, or matches it as a glob, and `rtrash restore '*.log'` works as `--glob`.
- CI now checks `rtrash du -x` against coreutils `du -s -x` on Btrfs. Across a subvolume, disk use, apparent size and the inode count match, with and without `-x`. Under `-x` a directory on another device adds nothing, as in `du -x`.
- Ctrl-C during `rtrash du` now stops the scan and prints what it counted, with a line saying the sizes are lower bounds, and exits 130.
  JSON output gains `"partial": true` in that case, while `--print0`, `--pick`, `-o` and `--stacks` print nothing, since a partial list would pass for a whole one.
  A second Ctrl-C ends the process at once.
  In `rtrash du -i`, `q` or Ctrl-C while a directory scans stops that scan and keeps its partial sizes; pressed again, it quits.
- Ctrl-C now stops `rtrash empty` and `trash-empty` cleanly, with or without an age or `--max-size`. The items in hand finish. The rest stay in the trash with their records. A line on stderr says how many went, and the run exits 130. Each record goes right after its payload. rtrash waits for that record when Ctrl-C comes in between. No stopped empty leaves a record whose payload is gone. A second Ctrl-C quits at once.
- Ctrl-C now stops `rtrash reap` after the entry in hand, as it stops `empty` and `rm`. A line on stderr counts what was reaped and what was not reached, JSON output still prints, and the run exits 130. An entry reap trashes keeps its record. `reap --watch` also ends at a Ctrl-C between passes, which it checks for every 200 ms while it waits. A second Ctrl-C quits at once. On Windows, Ctrl-C keeps its usual meaning.
- Ctrl-C now stops `rtrash rm`, `trash-rm` and `status --check --fix` after the entry in hand, as it stops a put or restore. A line on stderr counts what went and what was not reached, and the run exits 130. In `rm`, a Ctrl-C after a payload is gone waits until its record goes too, so no record is left behind without its payload. A second Ctrl-C quits at once.
- Ctrl-C now stops a put, restore or undo after the item in hand, as it already stopped a `du` scan. This covers `rtrash put`, `trash-put`, `rtrash restore` and `rtrash undo`. A line on stderr counts what was done and what was not reached, JSON output still prints, and the run exits 130. Ctrl-C stops a copy to another file system at once. Its copied part goes, so that item stays where it was. A second Ctrl-C quits at once, as in gdu 5.38. Only one step waits: writing a trash record and moving its payload. A Ctrl-C there, in any command, takes effect once both are whole, so no half-written `.trashinfo` and no payload without a record is left. `put -i` and `put -I` keep the usual Ctrl-C at their questions.
- Du can now count only files with given extensions (`--type=yaml,json`) or leave them out (`--exclude-type`), as gdu can.
- Du can now keep a directory folded with `--collapse=name`: one row for it, as in dust and gdu.
- Du can now print each table in reverse with the top row last (`--reverse`), as dust and gdu can.
- Du can now print only the totals (`-s`, `--summarize`) or only the rows (`--skip-total`), as gdu and dust can.
- Du can now read the paths to scan from a file or stdin, one per line (`--files-from`) or NUL-separated (`--files0-from`), as dust can.
- Du can now show each row as a percent of its scan total (`--show-relative-size`), as gdu can.
- Du can now show when each entry last changed, as a local date and time (`--show-mtime`), as gdu can.
- Du can now skip entries named with a leading dot (`--no-hidden`), as gdu can.
- Du explorer: the header shows the item count below the directory, as in ncdu.
- Each put, empty and reap that changes a trash appends its size and item count to `rtrash-history` in the trash root, and `trash-stats --history` shows those lines (a `history` list with `--json`).
- Each trash keeps a running total of its size in `rtrash-total`, so the checks after a put are cheap, and a put now warns by default when a trash takes more than 20% of its disk (`RTRASH_WARN_SIZE` or `policy.warn_size` change it; empty turns it off).
- In `rtrash du -i`, `%` cycles what each row shows of its share of the directory: the bar, the share in percent such as ` 12.5%`, both, or neither, as ncdu's `g` does.
  The bar stays the default.
- In `rtrash du -i`, `.` hides names that start with a dot in every view until pressed again, and the title says so.
  The name filter then works on the rows that remain.
- In `rtrash du -i`, `/` filters the current directory by name as you type, with a part of the name or a glob, and case ignored.
  Enter keeps the filter and Esc clears it.
  Marking all and trashing cover only the rows shown, and the title reads `N of M entries`.
  Opening or leaving the directory ends the filter.
- In `rtrash du -i`, `L` draws the size bars on a log scale, so a row with 1% of the directory still fills 6 of the 10 cells (ncdu #75). `L` again goes back to the linear bars.
- In `rtrash du -i`, `S` switches sizes, bars and totals between disk use and apparent size, and back.
  A size order follows the switch and keeps its direction.
  `a` stays mark-all, so the key differs from the `a` of ncdu and gdu.
- In `rtrash du -i`, `i` opens the details of the cursor row: full path, kind, disk use and its share of the directory, apparent size, item count, newest change and git state.
  On Unix the panel adds the mode, owner, group and link count.
  Any key closes it.
- In `rtrash du -i`, `p` quits and prints the marked paths, or the cursor row, on stdout after the screen is restored.
  `y` copies the same paths to the clipboard through the terminal's clipboard escape (`OSC 52`), in terminals that allow it.
- In `rtrash du -i`, a symlink is now a row of its own size, marked `@` in the flag column as ncdu marks it.
  It never opens, `d` trashes the link and leaves its target alone, and `i` names the target.
  The table and `--json` count links as before without listing them.
- In the `du -i` explorer and the restore, empty and put browsers, Ctrl-L now draws the screen again and Ctrl-Z suspends rtrash, as in dua 2.44. Before it stops, the terminal leaves raw mode and the alternate screen, so the shell works as usual; `fg` brings the browser back in raw mode and redraws it.
  Windows has no job control, so there Ctrl-Z is a no-op, and so it is in a process started with SIGTSTP ignored, as under `nohup`.
- In the `rtrash du -i` explorer, `v` shows the cursor file in `$PAGER` and `o` opens the cursor row with its default program, a file in its application or a directory in the file manager, as gdu's `v` and `o` do.
- In the explorer, `E` now works on the view above two or more directories (`du -i a b`). It writes one snapshot with a root record for each directory, the one `du -o FILE --max-depth=1 a b` writes. It comes from the scans in memory, with no second walk. Before, `E` asked to open one of the directories first.
- In the explorer, `E` writes the current directory to a file as a snapshot (ncdu #174). The snapshot is the one `rtrash du -o FILE --max-depth=1` writes, so `du -f` and `du diff` read it. It comes from the scan in memory, with no second walk. A file that exists is left alone, and a view whose sizes changed since its scan asks for `r` first.
- List can now print the contents of each trashed directory, N levels down (`--depth=N`), as gtrash can.
- New `--max-count=N` for rm and empty, and the `RTRASH_MAX_PERMANENT` environment variable, cap how many items one run may delete for good.
  A run past the cap exits 2 and deletes nothing, and `-n` lists what it would have deleted.
- New `RTRASH_ALLOW_ROOTS` environment variable, a path list, scopes rtrash to those folders: put trashes only paths under a root, restore and undo write only under one, and rm deletes only items trashed from one.
  empty, reap and wsl rm and empty refuse while it is set, and a run that would touch a path outside exits 2 and changes nothing.
  Paths compare on disk, so a symlinked folder that leads out of a root stays outside, and on Windows short 8.3 names and letter case do not matter.
- New `RTRASH_NO_PERMANENT` environment variable makes rtrash delete nothing for good: rm and empty (and their trash-cli names) exit 2 before they start, while dry runs, `--help` and `--version` still work.
  Set it for agents and shared accounts, so that trash stays the only way to remove files.
- New `du --strict` exits 1 when any entry could not be read, as coreutils du does, after the usual output; it covers the table, `--json` and the ncdu exports.
  In the NOTE column and in `--tree`, a folder du could not read shows `unreadable`, and a folder with an unreadable entry below it shows `incomplete`, since its sizes are low.
- New `rm --regex`: PATTERNs are regular expressions searched in the original path, as trashy's `--match=regex`, on every trash.
- New `rm -m`/`--match=KIND` reads PATTERNs as glob, regex, substring or exact, the kinds of trashy's `-m`.
- New `rtrash du --shared` counts each extent that reflinks, clones or snapshots share once per row and in the totals, on copy-on-write file systems for Linux such as Btrfs, so copies no longer add up to more than they take on disk.
  It maps extents like `--unique`, so the plain listing stays as fast as before.
- New `rtrash du --unique` adds a UNIQUE column with the bytes of each row that no other file shares, roughly what deleting it would free on copy-on-write file systems such as Btrfs and the Apple File System.
  On Linux it maps each file's extents with `FS_IOC_FIEMAP` and leaves out the ones flagged shared; on macOS it reads the private size kept for each clone.
  The totals gain a unique figure, and a closing line gives the shared extents once each and as the files reference them.
  JSON output carries `unique` on rows and roots and a `shared` object, only on these scans.
  The plain listing stays as fast as before, since only `--unique` maps extents.
- New `rtrash du -i PATH` explores a tree one directory at a time, largest first, with a size bar on each row.
  Each row carries its artefact kind and git state, and a `!` marks git work at risk.
  Pressing `d` moves rows to the trash after a confirmation, `u` restores the last deletion, and `r` rescans the current directory.
  Parent totals follow every change without a rescan, and the explorer reuses the keymap of the other browsers.
- New `rtrash du -o FILE` writes a snapshot of every directory total, and `rtrash du -f FILE` lists the largest directories from one without scanning.
  Snapshots keep one record per line with percent-encoded paths, so grep and text diff tools read them too.
  The new `rtrash du diff` lists the directories that grew or shrank most between two snapshots.
  Given only the older snapshot, it scans the same roots again and compares against that.
  Keeping thousands of rows no longer costs quadratic time: listing all 11,881 directories under /usr dropped from 6 seconds to 0.19.
- New `rtrash empty --max-size=10G` removes items until the trash fits in 10 GiB, oldest first or with `--prune=largest` largest first.
  An age argument still applies: the age cut runs first and the size cut takes the rest.
  Setting `RTRASH_MAX_SIZE` prunes each trash directory a put used back under that budget, and the items of that put always stay.
- New `rtrash status --check` checks each FreeDesktop trash record and lists records whose payload is gone, records that cannot serve a restore, bad deletion dates, payloads with no record and stray names in `info/`, with exit 1 when it finds any.
  `--fix` removes the dangling and bad records but keeps each payload, as an orphan. `--fix -n` shows the plan.
  `RTRASH_NO_PERMANENT`, `RTRASH_MAX_PERMANENT` and `RTRASH_ALLOW_ROOTS` apply to `--fix`.
- New `rtrash undo` restores every item one put run trashed, in one step.
  Each put run now writes a batch id into its `.trashinfo` files under the extra key `X-Rtrash-Batch`, which other tools ignore.
  Without an argument, undo takes the most recent run; `undo --list` shows past runs and `undo BATCH` picks one.
  Items from gio, Dolphin or trash-cli carry no id and never match.
  JSON from `list --json` now includes the `batch` of each item.
- New `rtrash wsl empty` deletes Windows Recycle Bin items from inside WSL by age (`--older-than=AGE` or DAYS), or all of them with `--all`; a whole empty asks on a terminal and otherwise refuses, like the guard of `rtrash empty`.
- New `rtrash wsl list` and `rtrash wsl restore PATH` list and restore Windows Recycle Bin items from inside WSL by reading the `$I` records under the automount root, so items the Recycle Bin route sent stay within reach.
- New config file at `$XDG_CONFIG_HOME/rtrash/config.toml` sets defaults for `du` and `list`, the trash budget after every put (`[policy]` with `max_size` and `prune`), and extra paths that put refuses (`[protect]` with `paths`).
  Environment variables and command-line options still win, `RTRASH_CONFIG` points at another file, and an empty `RTRASH_CONFIG` turns the file off.
  The new `rtrash config` prints the settings in effect, `--path` prints where the file lives, and `--default` prints a template.
- New du flags filter by a regex on the whole path: `--exclude-regex=RE` and `--include-regex=RE`, as in dust and gdu.
- New reap profile in the `pkg` group: `node-gyp` (header downloads for old Node versions, after 90 days).
- New reap profiles in the `ai` group: `torch` (partial hub downloads and TorchInductor caches in the temp dir) and `vllm` (torch.compile caches).
- New reap profiles in the `ai` group: `triton` (kernel caches) and `jax` (persistent compilation caches).
- New reap profiles: `huggingface` (partial downloads and the xet chunk cache, in the `ai` group), `mamba` (micromamba and mamba package tarballs) and `pre-commit` (pre-commit and prek hook environments), both in the `pkg` group.
- On WSL, `RTRASH_WSL_RECYCLE=1` (or `[wsl] recycle = true`) sends files on Windows drives (`/mnt/c/...`) to the Windows Recycle Bin, where Explorer restores them.
- On WSL, put reads the Windows drive mount root from `/etc/wsl.conf` and, until `RTRASH_WSL_RECYCLE` is set either way, notes once per run that a Windows-drive file could go to the Recycle Bin.
- On Windows, `RTRASH_MAX_SIZE` and `policy.max_size` now trim the Recycle Bins after each put, sparing the items that put moved.
- On Windows, `put` warns when the volume of the Recycle Bin it used has under 10% free, since trashing frees nothing there.
- On Windows, `rtrash list` and `rtrash restore` take `--directory=DIR` as on Linux; DIR counts as given and as resolved, so a volume mounted on a folder still matches.
- On Windows, `rtrash list` now says on stderr how many Recycle Bin entries it could not show: a record whose payload is gone, a record it cannot read, or a payload with no record. The line names `rtrash status --check`, which lists each one. The rows on stdout do not change.
- On Windows, `rtrash list` takes `--last=N` and prints only the N most recently deleted items.
- On Windows, `rtrash list` takes `--size` and `--format=TEMPLATE`.
- On Windows, `rtrash list` takes `--sort=KEY` (date, size, name or path) and `--reverse`, as on Linux; `--last` cuts after them.
- On Windows, `rtrash put --dry-run` prints what would move without moving it, and `rtrash list` takes `--glob`, `--regex`, `--name` and `--cwd-only`.
- On Windows, `rtrash put -v` names the Recycle Bin each item went to by its volume root, so an item on a volume mounted on a folder shows that folder.
- On Windows, `rtrash reap --install-timer` writes a Task Scheduler XML file and passes it with `schtasks /XML` when the command is longer than the 261 characters `/TR` takes, so many `-p` options no longer make the install fail.
- On Windows, `rtrash reap --timer-status` shows the full command of a task made from a task XML file, read from that file, and lists the file among the job's files.
- On Windows, `rtrash restore --batch=last` restores every item the latest put moved.
- On Windows, `rtrash restore` opens the restore browser on a terminal, and `rtrash du -i` explores a tree and deletes into the Recycle Bin, with `u` to undo.
  `rtrash status` lists each drive's Recycle Bin with its cap from `MaxCapacity` in the registry and the share in use, then the total.
  The trash line of `rtrash du` adds the cap of the scanned drives, and `--json` carries it as `cap` when known.
  The keymap file falls back to `%APPDATA%\rtrash\keys.toml` on Windows, next to the config file.
- On Windows, `rtrash rm --batch=last` permanently removes the items the latest put moved, from the last-put record.
- On Windows, `rtrash status --json` now prints the same object as on Linux, with one root for each Recycle Bin volume in the summary, and an age or size filter works with it too. The shell counts no orphans or dangling records, so `orphans` and `dangling` are null; `status --check` still finds them. Each root also gives `cap_bytes`, the `MaxCapacity` cap (null when unset), and `skips_bin`, which is true where deletes skip the bin. The inode fields are null, since Windows counts no inodes.
- On Windows, `rtrash status` and `rtrash du --devices` also list volumes mounted on folders, each with its own free space.
- On Windows, `status` shows each drive's free space, and `restore` refuses a copy onto another drive that would leave under 5% or `RTRASH_MIN_FREE` free unless given `--no-space-check`.
- On Windows, `who`, `put --in-use` and `reap` ask the Restart Manager which processes have files open, and the reap profiles cover `%TEMP%` and the `%LOCALAPPDATA%` caches.
- On Windows, list now takes `--depth=N` too and prints the contents of each deleted folder, N levels down.
- On Windows, put now takes `--inspect` too: it asks before each item and shows its size and a directory's file count.
- On Windows, restore now keeps to an age range on request. It takes the same age bounds as list.
- On Windows, trash-restore asks before it restores a single match. A blank answer restores nothing.
- On Windows, trash-restore keeps to the current directory. It says so when nothing came from there.
- On Windows, trash-restore lists matches in the trash-cli layout and asks with the trash-cli prompt.
- On Windows, trash-restore takes a path as a scope. It restores the item there and the items trashed from under it.
- On btrfs, `rtrash du` now names on stderr the read-only snapshots it met below `PATH` and counted in full, the way `du` counts them, and points at `--shared` and `-x`. The JSON object lists them in `snapshots`. With `--shared` the note stays quiet, since shared extents then count once.
- On macOS, `RTRASH_MAX_SIZE` and `policy.max_size` now trim the Finder Trash after each put, sparing the items that put moved.
- On macOS, a put to the Finder Trash now gives the `RTRASH_WARN_SIZE` warning, from the stored Finder total plus what the put moved.
- On macOS, a put to the Finder Trash now warns when that trash sits on a file system under 10% free, as FreeDesktop puts do.
- On the Finder Trash, trash-restore asks before it restores, even for one match. A blank answer restores nothing.
- On the Finder Trash, trash-restore keeps to the current directory. It says so when nothing came from there.
- On the Finder Trash, trash-restore shows the trash-cli prompt only at a terminal. With the all flag and nothing from the current directory, it prints the trash-cli line.
- On the Finder Trash, trash-restore takes a path as a scope. It restores the item there and the items trashed from under it.
- On the macOS Finder Trash, `list --depth=N` now prints the contents of each trashed folder too.
- Put can now show what it is about to move, its size and a directory's file count, and ask first (`--inspect`), as rip2 can.
- Put now skips paths on the safe-rm list (its built-in system directories plus `/etc/safe-rm.conf`, `~/.config/safe-rm` and `~/.safe-rm`), refuses your home directory and mount points unless given `--no-preserve-root`, accepts `--preserve-root=all` as coreutils spells it, and warns before it trashes a git repository with uncommitted or unpushed work.
- Restore can now order its numbered list by size, largest first, on every platform.
- Restore gains `--to=DIR`, `--glob=PATTERN` and `--regex=REGEX` matching, `--all`, and `--conflict=refuse|overwrite|rename|skip`; the numbered prompt takes ranges such as `0-2,5` or `all`. The restore JSON adds `skipped` and each item's `restored_to`. List gains `--size`, `--sort=date|size|name|path` and `--reverse`.
- Restore on the Finder Trash now honours the current directory scope and leaves items from other folders in the trash.
- Restore on the Finder Trash now orders its numbered list by date, by path or as read, oldest or newest first.
- Restore on the Finder Trash now takes a directory scope and restores only the items whose Put Back place sits under it.
- Restore takes `-m`/`--match=KIND` to read PATH as a glob, regex, substring or exact pattern, as in trashy.
- Restore takes age bounds, as list does, so you can bring back only what you trashed in the last hour, or only older items.
- Restore takes size bounds on every platform, as list does, so you can bring back only the large items or only the small ones.
- Row badges in the interactive du browser now show `unreadable` on a folder du could not read and `incomplete` on a folder with an unreadable entry below it, as the table and `--tree` do.
- Shell completion for `restore` now offers the original paths in the trash, and the trashed names after `--name=`, in all six scripts.
  The zsh script adds them through `compadd`, so a `matcher-list` style applies (trash-cli #363).
- Shell completion for `rm` and `trash-rm` now offers the original paths in the trash as patterns, in all six scripts.
- Sorted by items (`C`), the `rtrash du -i` explorer now measures each row's share bar and percent by item count instead of size, as gdu does. A row's items are its files and folders, itself included, out of the item total in the title. The size column still shows bytes, and the other orders measure sizes as before.
- The Recycle Bin backend on Windows now filters as Linux and macOS do: `list`, `status` and `rm` take `--older-than`, `--newer-than`, `--larger-than` and `--smaller-than`, and `empty` takes a day count or `--older-than`.
- The Windows `empty --json` now prints the Unix summary (dry_run, removed, bytes, days, older_than_secs, max_size) without `--until-free` too.
- The Windows `empty` now takes `--max-size` and `--prune`, trimming the Recycle Bins of every drive together.
- The Windows build takes `rtrash restore -n/--dry-run` too: each Recycle Bin item prints as would restore or would fail, and nothing moves.
- The `[du]` table of the config file now takes `exclude_regex` and `include_regex`, lists of path regexes that `--exclude-regex` and `--include-regex` add to. A regex that does not compile earns a warning naming it, and the rest still apply.
- The `du -i` explorer now trashes in the background, as dua 2.45 does. The status line counts the rows done and the bytes left, and each row leaves the list as it goes, so the explorer stays usable during a large delete.
  A delete in progress keeps queued directories closed. `d`, `u` and `r` wait for it. For a row that crosses file systems, the status line also shows the bytes copied so far. On `q` or Ctrl-C, the explorer stops the delete and then quits. A row partway through its copy goes back whole, with the part copied removed, and stays in place with the rows not yet started.
  The explorer used to crash when you trashed the directory on screen through a mark from a view above. It now shows the parent instead.
- The `du -i` explorer rings the terminal bell when a scan or delete that took 10 seconds or more ends, so a user in another window or tmux pane learns of it, as dua #347 asks. `RTRASH_NOTIFY` or the config key `du.notify` picks `off`, `bell`, `osc9` or `osc777` (a desktop notification in the terminals that take one), and `du.notify_after` sets the seconds.
- The `du -i` explorer tags a directory that a reap profile empties by age with `reap:NAME`, such as `reap:npm` on `~/.npm/_logs`, as gdu #695 asks. It reads the profile paths that end in `/*` with no other wildcard, so it needs no extra walk.
- The `du -i` title now ends with the room left on the file system of the current directory and its share, such as `48.2 GiB free (9%)`, as ncdu #77 asks. It shows only when the whole path fits beside it. None shows for a snapshot, for the view above two or more directories, or on a network file system.
- The `du` explorer has a marks panel (`m`) listing every mark from all directories, to unmark one, jump to it, or trash the batch.
- The `du` explorer keeps marks while you browse other directories, so one `d` trashes marks from several directories as one batch (one confirmation, one `u` to undo); `p` and `y` take them all too.
- The `empty -i` prompt now gives the size and item count of each trash it would empty.
- The `rtrash du -i` explorer sorts rows by size with `s`, by name with `n`, by item count with `C` and by newest change with `M`.
  Pressing the key of the current order reverses it.
  The cursor and marks stay on their rows, and the window title names the order.
- The manual says what `rtrash du -x /` covers on macOS. There one device number covers the system and data volumes of the Apple File System (APFS) volume group, so it walks both. A firmlinked folder such as `/Users`, met at the top and under `/System/Volumes/Data`, counts once. A macOS CI test checks this with `getattrlistbulk` reads and with plain `stat`.
- The restore and empty browsers and the numbered restore list end the row of a record whose payload is gone with `[payload gone]`.
- The trash history now also records restore and `rtrash rm`, on Linux, macOS and Windows.
- The trash history records `purge` instead of `reap` when `rtrash reap --purge-trash` empties trash items, on Unix and in the Windows Recycle Bin history.
- When `du -f` reads an ncdu export (JSON or binary), it now counts the entries marked `read_error` as unreadable and keeps the first 100 paths. That gives them the NOTE mark and the stderr lines, makes `--strict` fail on them, and lets `du diff` report a change in the count. Each unreadable directory now gets its own row, as in a scan. An export with no `read_error` counts 0, so `--strict` passes it. Exports from gdu still read as unknown, since gdu marks nothing.
- Windows `rm --json` now prints the same summary as the other trashes, and Finder `rm --regex` matches a pattern typed decomposed.
- Windows `rtrash restore --cwd-only` now offers only items deleted from the current directory or below, as on FreeDesktop; it used to act as `--plain` alone.
- Windows `rtrash restore --sort=date|path|none` orders the numbered list, as trash-restore does.
- Windows `rtrash restore --to=DIR` restores into DIR, keeping each file name, under the conflict policy.
- Windows `rtrash restore` takes `--conflict=refuse|overwrite|rename|skip` and `--json`, as on FreeDesktop; a taken destination now fails or skips that item alone.
- Windows `rtrash restore` takes `--name`, `--glob` and `--regex`, and `--latest` or `--all` when several items match.
- Windows keeps a history too: put, empty and reap append a line for each drive whose Recycle Bin changed, and `trash-stats --history` shows it.
- Windows now takes ranges and lists such as 0-2,4 in the numbered restore pick.
- Without `--older-than`, `rtrash reap` takes each directory's age from its tmpfiles.d `d`, `D`, `e`, `q`, `Q` or `v` line, so `/var/tmp` keeps its usual 30 days, and falls back to 10 days.
- Without a DIR, `rtrash reap` turns the directories a tmpfiles.d glob age line matches inside the temp dirs into roots with that age, and never judges a root as an entry of an outer root.
- `RTRASH_OPENER` names the program, with arguments split at spaces, that `o` in `du -i` runs on a row instead of `xdg-open`, `open` or `explorer.exe`, as gdu #584 asks; a blank value keeps the default.
- `RTRASH_WARN_SIZE` (or `policy.warn_size`) makes a put warn when a trash it used holds more than a size such as `20G` or a share of its disk such as `10%`.
- `b` in the `rtrash du -i` explorer opens a shell in the current directory, as in ncdu and gdu, and rescans the directory once the shell exits.
  Ctrl-C in that shell stops only the programs it runs, and `--no-delete` turns the key off.
- `du --clone-dupes` works on Windows ReFS volumes, including Dev Drives, through block cloning (`FSCTL_DUPLICATE_EXTENTS_TO_FILE`).
- `du -f FILE -i` now browses a snapshot or an ncdu export in the explorer, with no scan (ncdu #247). Each directory the file records opens from it, so a saved scan can be looked through after its directories are gone. The explorer is read-only there: `d`, `u`, `b`, `r` and `E` explain instead of acting. Before, `-f` refused `-i` because a snapshot can be stale. A new precheck rule makes every explorer walk go through the check that refuses on a snapshot.
- `du -o` snapshots now record how many entries the scan could not read, and the first 100 with the reason. `du -f` marks and names them as a scan does, `--strict` applies to `-o` and `-f`, and `du diff` reports a change in the count. Snapshots from older releases load with the count shown as unknown (null in the JSON), and older releases still read the new snapshots, skipping the new records.
- `empty --until-free --json` lists each short file system with what it lacked and what it still lacks.
- `empty --until-free=SPEC` removes the oldest trash items on each file system under SPEC free (such as 10% or 20G, with `,inodes=PCT`), Finder's Trash first on macOS, until it is back at SPEC.
- `empty --until-free` now works on Windows, per drive on the Recycle Bins, with the same JSON.
- `put` warns when a live process has an item open, mapped or as its working directory, and `--in-use=refuse`, `RTRASH_IN_USE` or `policy.in_use` makes it refuse instead.
- `reap --install-timer --watch` works on Windows: it makes a Task Scheduler task that starts at logon, has no time limit and restarts after a failure.
- `reap --install-timer` now works on Windows too: it makes a Task Scheduler task for the current user with `schtasks`, and `-n` prints the command instead.
- `reap --remove-timer` stops and removes the job `--install-timer` made, on Linux (systemd), macOS (launchd) and Windows (Task Scheduler); `-n` prints what it would run and remove.
- `reap --timer-status --json` prints the timer's state as an object (`installed`, `manager`, `files`, `state`).
- `reap --timer-status` says whether the reap timer is installed and shows what systemd, launchd or Task Scheduler reports about it; it exits 1 when there is none.
- `reap --when-below --purge-trash` now works on Windows: it empties the oldest items of the Recycle Bin on each drive that is short.
- `rtrash completions elvish` prints an elvish module that completes rtrash, its subcommands and the multi-call names; save it under `~/.config/elvish/lib` and `use rtrash`.
- `rtrash completions nu` prints a nushell script that declares rtrash, each subcommand and the multi-call names as externs with their options, so nushell completes them; source it from `config.nu`.
- `rtrash completions powershell` (or `pwsh`) prints a PowerShell script that registers a native argument completer for rtrash and its multi-call names.
- `rtrash du --devices` lists every mounted file system with its size, use, free space and use percent, as `df` and `gdu -d` do, with `--json` for scripts.
- `rtrash du --dupes --clone-dupes` replaces every extra copy with a reflink clone of the kept file on btrfs, XFS or APFS, so each path keeps its own mode and times while the blocks are shared.
- `rtrash du --dupes --link-dupes` replaces every extra copy with a hard link to the kept file, so one copy stays on disk and every path still works.
- `rtrash du --dupes` finds files with the same contents, grouped by size, edge hash and full hash.
  Hard links count once and shared reflink extents free nothing.
  `--trash-dupes` trashes every copy but the oldest, `--min-size` skips small files, and `--json` and `--print0` work for scripts.
- `rtrash du --filter-from=FILE` reads rsync filter rules and leaves out what they exclude. The first rule that matches an entry decides, as in rsync, so a `+` line keeps an entry from the `-` lines below it. A leading `/` anchors a pattern at PATH, a pattern with another `/` matches the end of the path, and `dir/***` takes the whole folder. Wildcards work as in rsync, not as in `.gitignore`: `*` stops at a `/` and `**` crosses it, even inside a name, so `docs/**/draft.md` needs a folder between `docs` and `draft.md`. Spaces at the end of a line belong to the pattern. A merge rule or a rule with modifiers is refused with its line number. The flag takes no `-f` snapshot and no `--devices` list.
- `rtrash du --inodes` (or `--sort=inodes`) ranks directories by inodes, each hard-linked file once.
  The total line and the JSON roots now give the inodes each path uses.
- `rtrash du --min-size=SIZE` now works outside `--dupes`, as `dust --min-size` does: it drops every row below that size from the table, `--json`, `--tree` and `--print0` of a scan, and from a listing read with `-f`. The floor measures disk use, or the apparent size under `--apparent`. A row of exactly that size stays, the totals still count everything, and `-n` still caps each list. A folder is at least as large as its contents, so in a tree the folder of each kept row stays too. A directory left out because one subdirectory makes up 90% or more of it comes back when that subdirectory falls below the floor. Snapshots keep every total for `du diff`, so `-o` refuses the option with exit status 2. The option also exits 2 with `-i`, `--pick`, `--stacks`, `--ncdu`, `--types`, `--devices` and the `items` and `mtime` sort keys.
- `rtrash du --ncdu-binary=FILE PATH` writes ncdu 2's binary export (`ncdu -O`), which ncdu 2.6 and later browse with `ncdu -f FILE`. It is about a fifth of the size of the JSON export, and it stores each directory's totals, with hard links counted the way ncdu counts them.
- `rtrash du --ncdu=FILE PATH` writes an ncdu export that `ncdu -f FILE` browses and `rtrash du -f FILE` lists.
  It streams as it walks, honours `-x` and the filters, and marks what it could not read.
- `rtrash du --open-deleted` adds one line under the trash line. It shows how much space deleted files take while a process keeps them open, on the scanned file systems. It explains a gap between df and du, and it is in no total. Linux reads it from /proc and macOS from libproc. On Windows a note says it is not available.
- `rtrash du --ratio` adds a `RATIO` column: the disk use of each row as a percent of its apparent size (gdu #294 and #265). A folder that compression or holes keep small shows below 100%, so Btrfs compression, Windows file compression and Compact show without compsize. Each Total line gives the ratio of its PATH, and `--json` gives each row and root a `ratio` key.
- `rtrash du --si` shows sizes in powers of 1000 (kB, MB, GB) instead of 1024, in the listing and in the explorer.
  JSON output stays in bytes, and the `si` key under `[du]` in the config file makes it the default.
- `rtrash du --since=DATE` and `--until=DATE` keep entries last modified in a span of local dates, given as `YYYY-MM-DD` or `YYYY-MM-DDTHH:MM[:SS]`.
  They combine with `--older-than` and `--newer-than`, and the stricter limit wins.
- `rtrash du --sort=items` ranks directories by the files and directories below them, to find inode hogs such as `node_modules`; `-C` adds an item count column to any listing, and JSON rows carry `items`.
  `rtrash du --sort=mtime` lists the most recently modified entries first, and a directory gives way to the subdirectory that contains its newest change.
  Short options now take an attached value, as in `rtrash du -n5`.
- `rtrash du --stacks` prints folded stacks: each directory's path from the scanned root, then the bytes it uses outside the directories below it. Flame graph tools such as `flamegraph.pl`, `inferno-flamegraph`, or speedscope turn that into a picture of where the space goes. It weighs by disk use or `--apparent` size, honors `--max-depth` and the filters, and reads a snapshot with `-f FILE` without scanning again.
- `rtrash du --types` lists file types instead of directories and files: disk use, apparent size and file count per extension, with case ignored.
  It shows the first `--top` types and sums the rest on one line, and JSON adds a `types` array.
- `rtrash du -L --show-symlink-target` shows each row that is itself a symlink as `PATH -> TARGET`, with the target as the link stores it, as gdu's `--show-symlink-target` and `ls -l` do. It works in the table and in `--tree`, and `--json` adds a `target` key to such rows. Without `-L` the scan lists no links, so the option refuses there, and with `-f` too, since a snapshot keeps no links.
- `rtrash du -L`/`--follow-symlinks` follows symlinks for reports, as `du -L`, gdu and dua do.
  A directory met again through a loop or a second link is walked once, and a file reached by several names counts once.
- `rtrash du -f` and `du diff` now read ncdu 2's binary exports (`ncdu -O FILE`, ncdu 2.6 and later) as well as the JSON ones. Both forms of one scan give the same totals.
- `rtrash du -f` and `du diff` now read ncdu JSON exports compressed with Zstandard (`ncdu -c -o FILE`, ncdu 2.7 and later). The file's first bytes pick the reader, whatever its name, and a compressed export lists the same rows and totals as its plain form.
- `rtrash du -f` and `rtrash du diff` read ncdu exports (`ncdu -o FILE`, `gdu -o FILE`) as well as snapshots, so a scan saved by ncdu can be listed, folded with `--stacks` or compared.
- `rtrash du -i --no-delete` opens the explorer read-only, so `d` and `u` only show a note.
  The `no_delete` key under `[du]` in the config file makes every `-i` read-only.
- `rtrash du -i a b` explores more than one directory at once, as `dua i a b` does. The explorer opens on a view with one row per directory and goes back to it from each of them. Trashing and undo carry size changes up to that view.
- `rtrash du -i` opens each directory at once and scans it in the background.
  The entries show right away, with file sizes and a placeholder for each directory, and the title counts files and bytes so far.
  You can move around, open other directories, and go up while scans run; trashing waits until a view has its sizes.
- `rtrash du -o` snapshots now end with a record of their line count and the CRC-32 of the lines above it. `du -f` and `du diff` refuse a snapshot whose end record is missing or does not match, and say which line and why, instead of listing or diffing a file that was cut off or edited by hand. A snapshot that names a checksum other than CRC-32 is refused rather than read unchecked. The checksum catches accidents, not forgery.
  Snapshots from older rtrash versions still load. A record cut short is refused in any snapshot, and so is a file with no root or directory record. Older rtrash versions read the new snapshots and skip the two new records.
- `rtrash du` names the paths it could not read, with the reason, instead of only counting them.
  stderr shows the first 10 and counts the rest, and `--json` adds `unreadable_paths` with up to 100 entries.
- `rtrash du` now reads `RTRASH_BLOCK_SIZE`, `DU_BLOCK_SIZE`, `BLOCK_SIZE` and `BLOCKSIZE` as coreutils `du` does, and the first one set picks the units of its printed sizes. `BLOCK_SIZE=1K` prints counts of 1024-byte blocks, rounded up, and `K` adds the unit to each count. `MB` counts in powers of 1000, and `si` or `human-readable` pick those units. The `--si` and `--no-prefix` flags still win, and the variables win over the `si` config key. JSON, `--print0` and the explorer keep their units. A value rtrash cannot read keeps the default units; only a bad `RTRASH_BLOCK_SIZE` gets a warning.
- `rtrash du` takes filters in gitignore syntax: `--exclude=PATTERN` (repeatable), `-X FILE` for a pattern file, `--exclude-caches` for directories with a valid `CACHEDIR.TAG`, and `--gitignore` to leave out what git ignores inside each repository.
  Excluded entries add nothing to any size or count, an excluded directory is never opened, and the total reports how many entries were left out (`excluded` in the JSON roots).
- `rtrash empty --dangling` removes only the `.trashinfo` records whose payload is gone, which list as items but cannot be restored, as gtrash's `metafix` does; real items and payloads without a record stay.
- `rtrash list --all-users` shows the trash of every account the caller can read, so root can review a shared machine. It covers each home trash at the default location and every per-user trash on mounted volumes. An owner column follows the date, and JSON rows gain an `owner` field. The listing changes nothing on disk; root restores or removes another account's items by pinning that trash with `--trash-dir`.
- `rtrash list --format=TEMPLATE` prints each item through a template such as `{path}\0` for scripts, and the `--json` field names are now documented as stable.
- `rtrash list --last=N` prints only the last N rows of the listing, the N newest by default, like `gtrash find -n`.
- `rtrash list --last=N` works on the macOS Finder Trash and keeps the N most recently trashed items.
- `rtrash list` and `rtrash restore` take `--directory=DIR`, like `--cwd-only` for another directory (gtrash `-d`); DIR need not exist any more.
- `rtrash list` and `rtrash status` take `--older-than`/`--newer-than` durations such as `2h`, `30m` and `1d12h` as well as whole days.
- `rtrash list` filters by `--glob`, `--regex`, `--name` and `--cwd-only`, as restore does, on the FreeDesktop and Finder trashes.
- `rtrash list` now says on stderr how many trash records it could not show: a record it cannot read, such as a folder by that name or one past the size cap, or one whose relative `Path=` climbs out of its volume. The line names `rtrash status --check`, which lists each one, the same as the Windows hint. The rows on stdout do not change, and `trash-list` prints nothing new.
- `rtrash list`, `status` and `rm` take `--larger-than=N` and `--smaller-than=N`, with N a size such as `500M` or `1.5G`. They keep only items that take more or less than N on disk, as `list --size` measures them, so `rtrash rm --larger-than=1G --force '*'` drops only the big items.
- `rtrash put --dry-run` runs every check put makes and prints `would trash PATH` for each item that would move, moving nothing.
- `rtrash put -v` names the trash directory each item went to, so an item on another file system shows that file system's trash.
- `rtrash put` warns when the trash it used sits on a file system with less than 10% of its bytes or inodes free, since trashing frees nothing there until the trash is emptied.
- `rtrash reap --install-timer --watch=EVERY --when-below=SPEC` installs a user service that keeps `reap --watch` running (systemd on Linux, launchd KeepAlive on macOS) instead of a timer.
- `rtrash reap --install-timer[=EVERY]` writes a user systemd service and timer on Linux or a launchd agent on macOS that runs the same reap on a schedule, without root; with `-n` it prints them.
- `rtrash reap --profile NAME` reaps tool caches and temp files from built-in profiles (`ai`, `compilers`, `pkg` and their members such as `claude`, `cargo-target`, `pip`) or `[reap.profiles.NAME]` tables in the config file, each with its own age and action; `--list-profiles` lists them, `--tool` hands a profile to its tool's own cleanup, and the dry run and JSON give each profile's total.
- `rtrash reap --roots=DIR`, `RTRASH_REAP_ROOTS` and a profile's `roots` key point `{roots}` globs such as the `cargo-target` profile's at directories outside home, and `--depth` or the `depth` key sets how far `**` reaches.
- `rtrash reap --shared` gives what a delete would free next to the apparent size, counting reflinked and snapshot extents once, and extent maps are now read only for files of at least 64 KiB (`RTRASH_EXTENT_MIN`) on file systems that share extents, which keeps `du --unique` and `du --shared` fast elsewhere.
- `rtrash reap --watch[=EVERY]` stays running and reaps again every EVERY (default 1m) while a `--when-below` target is short.
- `rtrash reap --when-below=SPEC --purge-trash` first empties the oldest items of your trash on a file system that is short of space, since a put there frees nothing, and reaps only if that is not enough.
- `rtrash reap --when-below=SPEC` exits at once while free space is above a size, a percent or an inode percent, and otherwise reaps oldest first by profile until `--until` (or the same target) is met, so a frequent timer costs almost nothing.
- `rtrash reap [DIR]... --older-than=AGE` trashes or deletes stale entries in `$TMPDIR`, `/tmp` and `/var/tmp`, and keeps anything a live process holds, sockets, pipes, mount points, live lock files, other users' files and tmpfiles.d exclusions; `--dry-run` and `--json` show the plan.
- `rtrash reap` honours the tmpfiles.d age modifiers (`~` reaps one level down, `LETTERS:` picks the times that count) and glob paths in age lines.
- `rtrash reap` reads user `user-tmpfiles.d` files as well as the system ones, fills in specifiers such as `%h`, `%C` and `%t` for ages and exclusions, honours `/usr/local/lib/tmpfiles.d`, and lets a file mask a same-named one in a later directory for exclusions too.
- `rtrash restore --batch=ID` (or `--batch=last`) restores one whole put run, as `undo` does, matching `rm --batch`.
- `rtrash restore -n/--dry-run` prints where each chosen item would go, or why it would be skipped or fail, and moves nothing; `--json` marks the summary with `"dry_run": true`.
- `rtrash restore -n/--dry-run` works on the macOS Finder Trash without a `--trash-dir` pin.
- `rtrash rm --batch=ID` (or `--batch=last`) permanently removes only the items of one put run, on the FreeDesktop and Finder trashes; with no PATTERN it takes the whole run, a scoped alternative to emptying the trash.
- `rtrash rm --older-than/--newer-than` and `rtrash empty --older-than` (and positional DAYS) take durations such as `2h`, `30m` and `1d12h` as well as whole days; `empty --json` adds `older_than_secs`.
- `rtrash setup --powershell-profile[=PATH]` adds a block that loads the PowerShell completer to the profile `pwsh` reports, or to PATH; it is opt-in, runs once, and is the one thing setup writes on Windows.
- `rtrash setup` installs the elvish module into `PREFIX/share/elvish/lib`, on elvish's module path; add `use rtrash` to `rc.elv`.
- `rtrash setup` installs the nushell completions into `PREFIX/share/nushell/vendor/autoload`, which nushell sources at startup.
- `rtrash status --check` on Windows now checks the Recycle Bin: it reports `$I` records whose `$R` payload is gone, `$I` records that cannot be parsed, and `$R` payloads with no record. `--fix` removes the dangling and bad records under `RTRASH_NO_PERMANENT`, `RTRASH_MAX_PERMANENT` and `RTRASH_ALLOW_ROOTS`, and `-n` previews it.
- `rtrash status` notes records whose payload is gone and hints at `rtrash empty --dangling` on stderr; its JSON roots carry a `dangling` count, and each `rtrash list --json` item carries `missing`.
- `rtrash status` now gives the free bytes and free inodes of each trash's file system, and warns when either is under 10%.
- `rtrash trash-stats --history --json` gives each history line `delta_bytes` and `delta_items`, the signed change from the line before (`null` on the first), so scripts need not work them out.
- `rtrash trash-stats --history` draws a trend line of bars under each trash's history, from its smallest size to its largest.
- `rtrash trash-stats --history` shows, on each text line after the first, how much the trash grew or dropped since the line before.
- `rtrash trash-stats --total --space` adds each trash's file system free bytes and inodes, with a low flag under 10%, from one statfs per trash.
- `rtrash trash-stats --total [--json]` reads each trash's size and item count from the stored running total without walking it, so a disk guard gets an answer at once on trashes with huge file counts; `--measure` stores a total where none is current.
- `rtrash trash-stats --total` gives the inodes each trash holds, from tree counts a put keeps in `rtrash-dirinodes`, so the answer needs no walk.
- `rtrash trash-stats` summarizes each trash directory (size, items, inodes, orphans, oldest and newest items, the `--top=N` largest), as text or with `--json`.
- `rtrash undo` now adds an `undo` line to `rtrash-history` in each trash it brings items back from. Restore already logs its line there, and undo on the Finder Trash already logged one. No line comes from a dry run, a refused run or a run that brings nothing back. `trash-stats --history` shows the new lines.
- `rtrash undo` takes `--conflict=refuse|overwrite|rename|skip`, as restore does, for items whose original location is taken.
  With `-n`, each line says what the policy would do.
  The undo JSON adds `skipped`, and each restored item carries `restored_to`.
- `rtrash who PATH` lists the live processes that hold a path or anything below it, by open descriptor, memory map, working or root directory, or executable.
- `rtrash wsl list` takes `--format=TEMPLATE` with the fields of `rtrash list --format`, so scripts and completion can read Recycle Bin items one per line.
- `rtrash wsl list` takes `--json` and the `--glob`, `--regex` and `--name` matchers, and the new `rtrash wsl rm` deletes matching Recycle Bin items for good from inside WSL, with `--dry-run`.
- `status --check --fix` now adds a `fix` line to the trash history when it removes records. The line goes to `rtrash-history` in each trash it changed; on Windows it goes to the history of each volume whose Recycle Bin it changed. A check, a dry run (`-n`) or a refused fix adds no line. `trash-stats --history` shows the new lines.
- `trash-stats --history --since=AGE` shows only the history lines from the last AGE (such as 12h or 7d).
- `trash-stats --total --exit-low` gives each trash's file system as `--space` does and exits with status 3 when any of them is under 10% free bytes or inodes.
- `trash-stats` now works on Windows, with one summary for each drive's Recycle Bin in the same text and JSON shapes.
- docs: a parity page compares rtrash with trash-cli, gtrash, gio and the macOS Finder Trash, from live runs on one shared trash.
- du: `--pick` opens the put browser on the listed rows, so marked rows go to the trash after one confirmation. Rows with git work at risk show `!`, and the confirmation counts them.
- du: `--threads N` sets the number of walker threads. The default stays at one per CPU; 24 threads cut a cold-cache scan of `~/.rustup` on 8 CPUs from 409 to 339 ms.
- du: each row inside or containing a git repository shows its git state (ignored, clean, dirty, unpushed, unknown), and `--print0` leaves out rows with work at risk unless `--include-dirty` is given. Paths inside `.git` are never listed; `--no-git` skips the checks.
- du: new subcommand that lists the largest directories and files under one or more paths, with on-disk and apparent sizes and the age of the newest change. It walks in parallel, never follows symlinks, counts hard links once, and takes `-x`, `--max-depth`, age filters, `--json` and `--print0`.
- du: tag regenerable build artefacts and caches (cargo `target`, `node_modules`, `__pycache__`, virtualenvs, CMake build trees, `CACHEDIR.TAG` directories) in a NOTE column and the JSON `artefact` field. Each artefact is one row, and `--artefacts` lists nothing else.
- du: the summary reports the trash on the scanned filesystems and how to empty it, since trashed files keep their space until then. A trash directory found during the walk is one `trash` row that `--print0` never prints. `--trash-dir` and `--home-only` limit the summary.
- du: works on Windows. One `GetFileInformationByHandleEx` call lists many entries with sizes and file IDs, junctions and symlinks are never followed, and the closing summary reports the Recycle Bin of each scanned drive.
- macOS: `empty --max-size` and `--prune` now trim the Finder Trash, by disk usage, and the Finder `empty --json` summary gains `bytes`.
- macOS: `rtrash rm PATTERN` (and `trash-rm`) permanently deletes matching items from the Finder trashes, matched by Put Back path, its file name or the name in the trash.
- macOS: `trash-stats --total` and `--history` now work on the Finder Trash, from a total and history per trash kept under `$XDG_STATE_HOME/rtrash/finder-totals` and updated by put, restore, rm, empty and undo.
- macOS: `undo`, `status`, `trash-stats` and the restore browser now cover the Finder Trash. Plain put logs each run's items in `$XDG_STATE_HOME/rtrash/finder-undo.tsv`, since Finder keeps no `.trashinfo`, and undo restores a run from that log.
- macOS: on the Finder Trash, `list`, `status` and `rm` now take the age and size filters (ages count from the move into the trash), and `restore` takes `--name`, `--glob`, `--regex`, `--latest` and `--to`.
- macOS: plain put sends an item on an external drive to that drive's `.Trashes/UID`, as Finder does, and list, restore, empty, undo, status and trash-stats read every mounted volume's Finder trash with `~/.Trash`. `RTRASH_FINDER_VOLUMES` adds volume roots the mount table misses.
- macOS: the Finder total and history files of a trash gone for over 90 days, such as a drive not plugged in since, are now removed at the next logged change.
- put/empty/rm: --json summaries; put --trash-dir/--home-only pin; empty --older-than; rm age filters.
- put: `--json` prints `{"trashed": [...], "failed": N}` with the original path, trash name and trash root of each moved item. It skips the put browser and replaces `-v` lines on stdout.
- put: on Windows `--json` prints the same `{"trashed": [...], "failed": N}` summary as on Linux, with each item's name and folder in the Recycle Bin. `--trash-dir` and `--home-only` parse there too; a pin draws a warning, since the Recycle Bin cannot move.
- reap: a new `jetbrains` profile (in the `compilers` group) takes the caches of older JetBrains IDE versions and keeps the newest of each IDE; `keep_newest` also splits names such as `IntelliJIdea2024.3` without a dash.
- reap: a new `puppeteer` profile (in the `ai` group) takes every Puppeteer browser build but the newest of each browser, and `keep_newest` now compares dotted versions such as `131.0.6778.85`.
- reap: a tmpfiles.d `r` or `R` removal line now keeps its path out of reap, which leaves that removal to systemd-tmpfiles at boot.
- reap: the `ollama` profile also takes partial model downloads (`blobs/sha256-*-partial*`), the new `playwright` profile (in the `ai` group) takes every Playwright browser build but the newest of each browser, and the new `pip-build` profile (in the `pkg` group) takes the build dirs pip leaves in the temp dir.
  A config profile takes `keep_newest = true` to spare the highest version of each `NAME-VERSION` entry.
- restore: in the browser, `o` (the remappable `toggle_order` action) flips the list between newest and oldest first. Marks, the filter and the cursor stay on the same items, and the title shows the current order.
- restore: scripts can now restore without the browser. `--name=NAME` picks items by file name or by name inside the trash, and `--latest` restores the newest match without a prompt. `--json` prints `{"restored": [...], "failed": N}` on stdout.
- rtrash gains `rtrash finder put|list|restore` for the macOS Finder Trash with Put Back. Put asks Finder to trash files, so Finder's own Put Back works. List and restore read the Put Back records from the trash's `.DS_Store` with a bounds-checked parser. Experimental: tests cover crafted `.DS_Store` files and one from the Python `ds_store` package. Nothing has run against Finder on a real Mac yet.
- trash-cli command lines now work under the trash-* names: `--trash-dir DIR` as two words, `--print-completion bash|zsh`, `trash-list --volumes` and `--trash-dirs`, and `trash-empty -i` and `--all-users`. `trash-put` exits 74 when a file cannot be trashed, and `trash-list --all-users` prints trash-cli's rows without the owner column.

### Developer

- A Linux test checks that a put and restore across file systems keep the POSIX access and default ACLs of a file and a directory tree.
- A Linux test now puts a file into a home trash on another file system (glib #883). It sets `XDG_DATA_HOME` to a directory on `/dev/shm`. The file's volume has no trash of ours, so put copies the file into the home trash; `list` must show it and `restore` must bring it back. It skips if the volume's top directory has a trash of ours or would take one.
- A Unix test of the `du -i` explorer now covers ncdu #237 in the view itself: a trashed row leaves the other rows in order and the cursor on its line, and the manual says so.
- A Windows CI step compares the PowerShell completion tables with the Windows `--help` of put, list, empty, restore, status and rm, and completes through pwsh.
- A Windows test checks that `rtrash reap --install-timer --dry-run` prints a task XML file for a command too long for `schtasks /TR`, and writes nothing.
- A Windows test pins that every `du` filter (`--filter-from`, `--exclude-from`, `--gitignore`, `--exclude`, both regex flags and both type flags) takes a file name with an unpaired surrogate, as NTFS allows: the file counts when a filter misses it and drops out when one matches it. dua #408 panicked on such a name.
- A debug-only `RTRASH_TEST_FAIL_DELETE=NAME` seam makes a full empty refuse one payload, and the same-tick history test now covers an empty that fails partway.
- A debug-only `RTRASH_TEST_STAMP_TICK` seam makes trash total stamps as coarse as a given tick, and a test runs put, restore, rm and an empty by age in one tick and checks each history count.
- A fixed-seed test runs odd and non-UTF-8 names, and payloads removed under their records, through put, list, status, restore, rm and empty.
- A new Unix unit test pins the blocks a directory itself adds in `rtrash du`. Its `st_blocks` count toward disk use and add nothing to the apparent size, as in `du`, whether the directory is empty, wide or unreadable.
- A property test builds trashes full of forged records, odd names, symlinked and piped records and payloads, and a garbled `directorysizes`. It runs `list`, `status`, `rm`, `empty` and `restore` against them under a deadline, and checks that no command hangs, that no file outside the trash changes unless a record names it and `-f` was given, and that every real item stays whole. Two fuzz loops check that record names and relative `Path=` values never point outside their trash.
- A report-only "du cold cache" workflow times `rtrash du` against dua, gdu and du with the page cache dropped, on each push to main.
- A shared test builder, `common::rtrash(home)`, pins the home, data and config folders and keeps macOS off the Finder Trash. Every test file uses it, or names its reason for the real environment with `common::rtrash_real`: the Finder tests, the Recycle Bin tests, or output that reads no trash. Precheck fails on any bare rtrash run in a test.
- A small fuzzer under `fuzz/` feeds mutated `.trashinfo`, `directorysizes` and `.DS_Store` bytes to the parsers. It is not part of the default build. `cargo run --manifest-path fuzz/Cargo.toml -- --seconds 60` runs each one.
- CI mounts a VHD on a folder of drive C on windows-latest and runs `tests/windows_mount.rs`, which checks that the folder is a volume of its own and that `reap --purge-trash` takes only its Recycle Bin.
- CI now parses the bash, zsh and fish completions and loads the zsh and fish ones. codespell also covers the completions and the man page.
- CI now runs clippy for the x86_64 musl target with default features and with `--no-default-features`, and `scripts/cross-clippy.sh` adds that target. On musl, libc deprecates the `time_t` and `suseconds_t` type names, so `src/info.rs` no longer names `time_t`, and a precheck rule refuses either name in `src/` and `tests/`.
- CI now runs the shared extent tests on btrfs and XFS loop images, so `du --unique`, `du --shared` and `reap --shared` are checked on real reflink copies.
- CI now runs the tests on Windows and on macOS (Apple silicon and Intel). On each platform a smoke test compares `rtrash du` totals with an independent reference: `du -sk` and a Python walk on Linux and macOS, and PowerShell file lengths plus a hard-link fixture on Windows. macOS runs it with both directory readers through `RTRASH_DU_BULK`. The scripts are `scripts/du-smoke.sh` and `scripts/du-smoke.ps1`.
- CI scans `/` with `rtrash du -x` now, on Linux and macOS. It checks the total against the used space of the file systems that `-x` walks, as `df` gives it. Those are the mounts that share the root's device number, so on macOS both the system and the data volume count. On Linux the two agree within 1% (or 512 MiB). The macOS bound is wide, from 50% to 150%, since Apple File System clones and local snapshots count differently. First the script prints every `df` row and device number. It stops with an error when an expected row is missing.
- CI: a new interop job has rtrash, trash-cli, gtrash and gio each trash a file and every tool restore it, through `scripts/interop-smoke.sh`.
- CI: the interop job also has gio delete one rtrash item (`gio remove trash:///NAME`) and empty the rest (`gio trash --empty`); the empty step needs `RTRASH_INTEROP_GIO_EMPTY=1`, since gio also empties every volume trash it finds.
- CI: the interop job also has trash-cli and gtrash remove one rtrash item and empty the rest, then checks that rtrash lists and counts nothing left.
- Debug builds let `RTRASH_TEST_FS` drive `du --devices` too (it lists the one fake file system), and new tests use it for `status` and `du --devices`.
- Each system now gets a unit test that builds one folder of plain, empty, sparse, hard-linked, linked and packed files. It checks each entry: the stat du takes must give the file type, length and disk use of a stat by path. A precheck rule names every directory listing that hands du a stat, so a new one cannot skip the test.
- Every du ranking now orders through one function, du::rank: the table, the tree, the explorer, the type list, du diff and the dupe groups. Precheck refuses a comparator of its own in du that puts the larger first.
- Every hand-off to the OS trash now goes through `allow_roots::trash_delete`, which checks the scope first, and `scripts/precheck.py` refuses any other call of `trash::delete`.
- Fixed-seed odd-name tests now also run on the Finder Trash and on a mounted Windows volume.
- In debug builds, the `RTRASH_TEST_FS` seam reads a Windows directory such as `D:\a\fs` whole, so `du --devices` and the space checks see the test file system there too instead of the real drives.
- Linux and macOS helpers now carry the cfg of their callers instead of `cfg(unix)`: the device table, the holders scan, the clone error mapping, and the `stale_file` and `json_path` helpers of the reap tests. `util::Look::There` boxes its metadata. Clippy for x86_64-unknown-freebsd runs in `scripts/cross-clippy.sh` and in a new CI job, with default features and with none. Precheck now evaluates each cfg on every cross-clippy target, so it finds a fn, const or static compiled where nothing uses it.
- Precheck flags a test helper with no `#[cfg(...)]` when every caller shares one cfg. Such a helper turns into dead code on the other targets and fails cross-clippy there.
- Precheck now compares the string literals of `src` at the base with those in the tree before it warns about a stale test string. A message that moved, or that another string still contains, no longer warns. Each warning names the test file and line of a string that no message in `src` contains any more.
- Precheck now fails a read of an `RTRASH_TEST_*` variable in `src` unless a release build ignores it. Such a read needs `cfg!(debug_assertions)` before it in its function, or `#[cfg(debug_assertions)]` on that function or on a module around it. Precheck also fails a const set to a seam name, because it must see each name. The tree already passes: all nine reads of the eight seams check for a debug build.
- Precheck now fails when code outside the tests calls a removal primitive without going through `permanent_guard`, unless a `// scratch:` comment says why the data is no user data.
- Precheck now flags a function that copies a tree across devices and then removes the original, unless it calls `tree_removable` before the copy. A tree that cannot wholly go is then refused before anything moves. AGENTS.md now lists this rule and round 224's dot-operand rule.
- Precheck now requires every call of a removal primitive outside `permanent_guard` to go through `permanent_guard::remove` or `remove_record`, or to sit in a function that its `PERMANENT_ALLOW` list names with a call count and a reason. A `gate()` call written next to the removal no longer counts. The two Windows restores that overwrite, and the reap on Unix, now call `permanent_guard::remove`, with the same checks, messages, and exit codes as before.
- Skip notes now reach the CI log. `common::skip` prints the reason and also appends it to the file `RTRASH_SKIP_LOG` names. The Linux check, Windows and macOS jobs print that file after the tests, even when every test passes.
- Tests no longer reach the Windows Recycle Bin. `common::rtrash(home)` now points debug builds at a scratch bin under `home` through `RTRASH_TEST_BIN_DIR`: a put moves the item there as a `$R` file with its `$I` record, and the du trash summary counts that folder. A test that compares two `du --json` outputs goes through `common::du_scan`, which leaves out the live trash summary. Precheck fails when either rule is broken. Round 231 went red on Windows when a parallel `du --trash-dupes` filled the real bin that another test's summary read.
- Tests that expect a refusal to change nothing share `common::assert_refused_unchanged`. It snapshots the watched folders and files, contents included, right before the call, and precheck flags any copy written by hand.
- Tests that mount volumes, look at a real mount's top, put across devices without a pin or run a bare `rtrash empty` now hold one file lock, so they run one at a time under both `cargo test` and `cargo nextest`. A precheck rule makes each such test take it.
- The CI Linux and macOS test jobs install pinned elvish 0.21.0, fish 4.9.3 and nu 0.116.1 through `scripts/ci-shells.sh` (checked by SHA-256 and cached) and set `RTRASH_REQUIRE_SHELLS=1`, so the elvish, fish and nu completion tests run there instead of skipping.
- The CI macOS test jobs build pinned rsync 3.5.1 through `scripts/ci-rsync.sh` (checked by SHA-256 and cached) and set `RTRASH_REQUIRE_RSYNC=1`, so the rsync half of `filter_from_leaves_out_what_rsync_leaves_out` runs there; Windows CI stays without rsync, since the msys2 and Cygwin ports read a `C:\` path as a remote host.
- The `cargo-dist plan` workflow now uses dist 0.33.1, which runs on `ubuntu-22.04` instead of the retired `ubuntu-20.04` image, so the PR job starts again.
  `dist-workspace.toml` sets `allow-dirty = ["ci"]`, since `scripts/dist-pr-only-release-yml.sh` rewrites the generated `release.yml` and `dist plan` would otherwise reject it as out of date.
- The `next_trash_second` test helper now waits on two clocks: the system clock and the file system's clock. A Finder Trash item's date is its change time, and Linux sets that time from a coarse clock a few milliseconds behind the system clock. A put just after the tick could then land in the second before and tie with the previous put, which made `list_last_keeps_the_newest_finder_items` flaky. Precheck now refuses a test loop that waits on the system clock by hand.
- The interop smoke test and a CLI test now cover the records that a put killed between its record and its move leaves behind (trash-cli #414): an empty record, one cut off before `Path=`, and a whole one whose payload never came. `status --check --fix` removes all three and leaves the good entry, and trash-cli's trash-list then prints no parse errors.
- The interop smoke test probes gio restores by name shape and prints the GLib and gvfs versions. Any failed probe fails the run, the plain `x 100%` name included. `RTRASH_INTEROP_NO_GIO=1` leaves gio out.
- The interop smoke test runs gio under a UTF-8 locale, and the Finder smoke test resolves its binary path once.
- The macOS Finder smoke job now runs `reap --purge-trash` and `empty --until-free` against a short disk-image volume.
- The man page now covers trashed symbolic links in its empty section (trashy #115). Each goes as the link, and empty removes a link into an unmounted volume like any other item. A new Linux test checks this with links into a tmpfs volume, before and after an unmount.
- The man page now says that one `rtrash empty` covers the home trash and this user's trash on every mounted volume, and that a volume mounted inside another keeps a trash at its own top folder. A Linux test mounts three nested tmpfs volumes and checks put, list, empty and restore across them. The du man page notes that the macOS system du counts firmlinked folders twice.
- The minimum-compiler CI job builds the locked dependencies instead of only checking them, so a change that cannot be linked fails there.
- The musl build no longer warns that `libc::time_t` is deprecated: the restore code that copies timestamps now lets the compiler pick the width of `tv_sec`.
- The precheck script now checks call arity. When a free fn gets a new parameter count, the check reads each call of its name in src and tests, on every cfg branch. It flags a call that passes a count no fn of that name takes. Round 270 missed one caller of a changed fn, and no CI target built. On that range the check names the missed call and nothing else.
- The precheck script now checks that every config key is documented. Each key in the KEYS table of `src/config.rs` must be named in the README's `rtrash config` section and have a line under its table in the man page's config example. The README had left out the `[policy]` keys `min_free`, `warn_size` and `in_use`, as well as `[macos]` `trash` and `[wsl]` `recycle`; it now names them. The man example now has a `[wsl]` table.
- The precheck script now flags a slice search written as iter().any with a deref of its argument against a name, which clippy rejects as manual_contains. Round 274 sent one in the du parser, and clippy failed in CI.
- The real-Finder smoke job now trashes and restores a name with a newline and logs what Finder did with it.
- The rule that a put's budget trim spares the items of that put now has a unit test on every platform.
- The status cache test now reads only the Total line, so a process id with 741 in the sandbox path no longer fails it.
- The test suite now compiles and passes on Windows. Tests that drive the FreeDesktop trash, the shell completions or the terminal browsers stay Unix-only; the du tests handle Windows paths and set file times through the Windows API, and a new hard-link test runs on every platform.
- Two Linux tests with names that are not UTF-8 now pin list, rm and restore to the home trash, so items in a volume trash no longer break them.
- Unix tests now pin that put refuses a directory or file it cannot move, on one file system and across devices, with every file kept and no trash record left (trash-cli #427 and #434).
- `benches/compare_du.sh` times `rtrash du` against dua, gdu and du with hyperfine; the measured results are in the benchmarks page.
- `du_filter::Spec::is_active` now names every field of `Spec`, with no `..`, so a new filter field does not build until it is listed there. A unit test sets each filter field alone and wants the spec active, and wants the default and a lone base inactive.
- `scripts/precheck.py --base origin/main` now fails on a commit after that base whose subject carries a conventional-commit prefix such as `feat(du):`, and `prek install` adds a commit-msg hook that runs the same check through `precheck.py --commit-msg`. Subjects in this repository are plain sentences.
- precheck `--base` now runs codespell on the files changed since the base, with the version and arguments of the pre-commit hook in prek.toml.

### Changed

- restore: the interactive browser now opens on the most recently trashed item. The numbered `--plain` table keeps oldest first, so its newest row stays above the index prompt. `--newest-first` and `--oldest-first` override either default. ([#3](https://github.com/HaoZeke/rtrash/issues/3))
- After a delete or an undo deep in the `rtrash du -i` explorer, the views above keep their row order, as ncdu #237 asks. Going back up finds the row walked out of in its place, with the cursor on it, even when it is now smaller than the row below. A sort key or a rescan puts the rows in order again. With `.` hiding dot names, going back used to sort the view.
- An `empty` with no DAYS, `--max-size`, `--until-free` or `--trash-dir` now asks on a terminal, and exits 2 elsewhere, when the trash has items from other put runs; `--all` removes everything as before.
- An age cut (`rtrash empty DAYS`, `--older-than` on `rm`, `list` and `status`) now keeps an item whose `DeletionDate` is missing or unreadable, as trash-cli's `trash-empty DAYS` does. Such an item used to count as old, so the first age cut deleted it, however recently it was trashed. A full `rtrash empty`, or `rm` without an age bound, still removes it.
- Commands `list`, `status` and `restore` read a big trash faster: they read the trashinfo files in parallel and size the entries in parallel.
  Reading dates now costs one time-zone lookup per hour instead of one per item, and each lookup used to stat the zone file.
  On a trash of 2,050 items, `list` went from 19.7 to 10.1 milliseconds and `status` from 38.9 to 17.2.
- Du skips kernel file systems such as /proc on Linux, as ncdu can. The `--include-kernfs` flag walks them.
- In `rtrash du`, the table of directories already walked, and the tables of hard links, hash their device and inode numbers with two multiplies in place of SipHash.
- Kernel file systems on Linux now stay out of the `du --ncdu` export and `du --dupes` too. The `--include-kernfs` flag walks them.
- On Linux, `rtrash du` counts the entries of each small or mid-sized directory read before storing them, and makes room for them all at once. Its lists of names no longer grow a doubling at a time, which took most of the allocator's work on a source tree.
- On Linux, `rtrash du` gives back the claim-path pages a shard no longer names, so a listing of many hard-linked files uses less memory. Other systems keep those bytes in the path buffer. Totals and rows are unchanged.
- On Linux, `rtrash du` starts the stats of a directory with tens of thousands of entries while the rest of the directory is still being read, so a huge folder no longer waits for its whole listing first. This is quickest when the cache is cold.
- On Unix, `rtrash du` compares the paths of a hard-linked file met more than once as bytes, with no parse of either path. Totals and rows are unchanged.
- On Unix, `rtrash du` cuts a lone subdirectory's name off the path at a known length when it leaves it, with no parse of the path. It also reads each subdirectory's name from the end of its path.
- On Windows, `list`, `rm` and `restore` now read each `$I` record the way `rtrash status --check` does. A `$I` that is a link or a junction, or one larger than any record, used to show up as an item. Now `list` leaves it out and counts it in its hint. The hint and the check now give the same number. `rm` no longer matches such an entry, so it deletes nothing for it. `restore` skips it and says "no trashed files match" when nothing else matches. The Windows Subsystem for Linux reads Recycle Bin items on Windows drives the same way. Run `rtrash status --check` to see them.
- On Windows, `reap --permanent` (and a reap profile set to delete) now removes a read-only entry past its age. It clears the read-only attribute and tries again. A forced restore and `status --check --fix` already work this way. Before, such an entry stayed and the run exited 1 with "Access is denied". An entry that is gone by the time reap reaches it now counts as removed, as on Unix. `RTRASH_NO_PERMANENT` and `RTRASH_MAX_PERMANENT` refuse it as before.
- On Windows, an `empty` with no DAYS, `--max-size` or `--until-free` asks on a terminal, and exits 2 elsewhere, when the Recycle Bin has items the latest put did not move; `--all` empties everything as before.
- On Windows, restore shows its prompt only when someone types the answer at a terminal.
- On Windows, the PowerShell completions offer the options of the Windows put, list, empty, restore, status and rm, which differ from the Unix ones.
- On Windows, the trash history follows each volume mounted on a folder in a file of its own, `vol-SERIAL.txt`, and `trash-stats` groups the Recycle Bin items of such a volume under its folder instead of under the drive above it.
- On Windows, trash-restore shows local times. Trash-cli does the same.
- On a tree with many hard-linked files, `rtrash du` keeps a smaller record of where it met each one when it lists rows, with each path kept from below the folder it scans, so the scan needs less memory.
- On a tree with many hard-linked files, `rtrash du` keeps each claim path in one buffer per shard instead of a separate allocation, so a listing (`-n`, the default, `--types`) uses less memory. With `-s` alone or `-n 0` it keeps no claims and is unchanged there. Totals and rows are unchanged.
- On every platform, `rtrash du` no longer allocates a list for the one batch of stats of a directory of 256 entries or fewer.
- On macOS, `reap --purge-trash` now empties the oldest items of the Finder Trash on a short file system before the FreeDesktop trashes, since a plain put goes there.
- One parser now reads every prompt answer. A `y` or `yes` in any case goes on, and anything else is a no, from a pipe too, so `echo n | rtrash empty -i` keeps the trash (trash-cli #333). For `put -i` and `put --inspect`, an answer that merely starts with `y` is now a no. A precheck rule keeps prompts from reading stdin or matching a yes themselves.
- Release builds for musl link mimalloc as the allocator, which brings `rtrash du` on musl level with the glibc build.
- Restore reads `--older-than 30` like `--older-than=30`, for every age and size filter, as list does.
- Size arguments now take their unit in any case. `5mb`, `10kib` and `1.5gb` work, where before only `5M`, `5MB` and `5MiB` did. All of them stay powers of 1024, as before. For powers of 1000, give the bytes (`5000000` or `5e6`); an exponent such as `1e3` keeps working. A whole number of bytes past 2^53 is now read exactly, and `P` (PiB) joins K, M, G and T. A precheck rule keeps every option on the one parser (dust #629).
- The low-space warnings of `put` and `status` now suggest `empty --until-free=10%`, which removes only the oldest items needed.
- The minimum supported Rust version is 1.88.
  `rust-version` said 1.77, but the locked crates need 1.88, and CI checks that minimum.
- The nushell completions now declare `--interactive`, `--preserve-root` and `--install-timer` as flags with an optional value, so nushell completes them and passes both the bare and the `=VALUE` form through.
- The trash history no longer scans every mounted trash before and after each put, empty and reap; only the trashes a command changed are read.
- Under `rtrash du -L`, the total names repeats as repeated names, since a file can come back through a symlink as well as a hard link.
- When `rtrash du` lists the largest files, a folder with hundreds of thousands of files now keeps only the best few of each batch it stats. It no longer keeps one candidate per file until the folder is done, so it needs less memory and picks its list on every thread.
- When two `du` flags do not go together, the message now always names both: it says what the first flag does and lists every flag it refuses, as in `--stacks prints folded stacks; it takes no -o, --json, --print0, --pick or -i`. All such refusals come from one table, and one test runs every pair in it.
- Windows restore now names the index range to give after a blank pick, as on FreeDesktop.
- `RTRASH_NO_PERMANENT` now covers every permanent removal, not just rm and empty: reap with `--permanent` or `--purge-trash`, a reap profile set to delete, the size cap after a put, wsl rm and empty, the Finder Trash and Recycle Bin purges, and a restore that overwrites an existing file.
  A refused put still moves the file and keeps the older items, and a refused restore leaves both copies in place.
- `empty --dangling` now keeps the same limits as `status --check --fix`. `RTRASH_MAX_PERMANENT` and `--max-count` now cap it. Past the cap it removes no record and exits 2; before, it removed them all. With `RTRASH_ALLOW_ROOTS` set, it no longer refuses outright. It removes the records when each one names a path under a root. If one does not, or cannot be read, it refuses the whole run and removes nothing. `RTRASH_NO_PERMANENT` refuses it as before.
- `rtrash du --gitignore` now reads the global ignore file that git config names in `core.excludesFile` (often `~/.gitignore_global`), asked of git once per repository so repository config and includes count. When that is unset, or git is missing, it reads `$XDG_CONFIG_HOME/git/ignore` or `~/.config/git/ignore` as before. A linked worktree or a submodule now reads the `info/exclude` of the git directory its `.git` file names. The global file ranks lowest, then `info/exclude`, then each `.gitignore`, as in git.
- `rtrash du -f` now refuses each filter flag and exits 2. The list covers `--exclude`, `-X`, `--filter-from`, `--type`, `--exclude-type`, the two regex flags, `--no-hidden`, `--gitignore` and `--exclude-caches`. A snapshot or export records totals and no walk, so du used to print the whole table as if no filter was given. Filters set in the configuration file are defaults, and `-f` still reads the file with them.
- `rtrash du -s` and `-n 0` walk a tree with hard links once. With no rows to show, a linked file may count at any of its paths. The second walk, which pins each file to its first path in name order, no longer runs there. On the bench1 tree of 200,000 files and 200,000 hard links, that walk made half the stat calls.
- `rtrash du` listings of a tree with hard links stat each linked file once. The second walk keeps each linked file at its first path in name order. It now takes the stats the first walk read, found by the inode numbers that the Linux listing gives on `ext4`, `xfs`, `btrfs`, `tmpfs` and `f2fs`. A file bind mount below the PATH, or a mount table du cannot read, keeps the old stat of every file.
- `rtrash du` merges a walked subdirectory in the walk, keeps the sort heading in the printer, and filters roots in the list they already occupy. Totals and rows are unchanged.
- `rtrash du` needs less memory on deeply nested trees: it keeps the path of each folder it walks into at that path's own length, and a chain of folders with one subfolder each shares a single path and, past 64 levels, keeps no list of subfolders per level.
- `rtrash du` no longer copies the subdirectory list of a directory once it has stated its entries, on every platform.
- `rtrash du` opens the subdirectories of a deep chain relative to their parent. It keeps parents open while its share of the open-file limit lasts. The old cutoff was level 64. Past that level each one used to open by full path, and the kernel walked the path again. Once the share runs out, du opens by path as before. A low `ulimit -n` no longer makes it skip directories as unreadable. With no hard limit, as on macOS, du raises the soft limit to 10240.
- `rtrash du` splits each batch of 8 or more hard-linked files into one run per table lock by counting the files under each lock, in place of a sort.
- `rtrash du` stats a directory of many entries in a few jobs per thread rather than in jobs of 64 entries. It counts the linked files of each job in one batch, so a directory full of hard links takes each lock once a job rather than once a file.
- `rtrash du` takes about 2 MiB less memory on a tree of 200,000 hard-linked files when it lists rows or file types, as it does by default and with `--types`, even beside `-s`. With `-s` alone or `-n 0` it keeps no record of where each linked file was met, so it is unchanged there. A linked file on the device of the PATH it is under is now filed by its inode number alone, and one on any other device by both numbers as before. Totals and rows are unchanged.
- `rtrash du` uses less memory on a large directory. It keeps eight bytes of index for each name instead of twenty-four. Totals and rows are unchanged.
- `rtrash du` with rows (`-n`, `--types`) looks up each hard-linked file once a sighting rather than twice, builds a linked file's path only when it keeps it, and hands its claims to the second walk without copying them, so a tree full of hard links lists faster and in less memory.
- `rtrash empty -v` names each removed item by its original path on every backend (FreeDesktop, Finder, Windows), not by its path inside the trash.
- `rtrash list` and `trash-list` no longer show a trash record that has no `Path=` line (or an empty one). Such a row had no original path, so it showed a blank path or the volume's top folder. trash-cli's `trash-list` leaves such records out too, so the `trash-list` rows now match it. `rtrash list` counts them in its stderr hint, and `rtrash status --check` names each one as a bad record. `rm`, `restore`, `empty` and the totals see these records as before; `rtrash status --check --fix` removes them.
- `rtrash list` now prints one stderr line per trash with records it left out, naming the trash, in place of one total for all of them (trash-cli #428). The line also counts names in `info/` that are not records, such as a `.DS_Store`. The trash-list name still leaves such names unmentioned, as trash-cli 0.26.9.29 does.
- `rtrash reap --watch` checks the last pass's file systems with one statfs each before a full pass and skips it while they are above the target, with a full pass at least every 10 ticks.
- `rtrash rm` (and `trash-rm`) now asks once before it deletes trashed items for good, as `rm -I` does and as trash-cli #383 asks. It names the matches first. It asks only when stdin and stderr are both terminals, so pipelines and cron jobs keep working. The new `-i`/`--interactive` asks even off a terminal, reading the reply from stdin; the new `-y`/`--yes` and the existing `-f`/`--force` skip the question.
- `trash-list --size` now prints trash-cli's rows: the payload's size in bytes in place of the date, then the original path. A directory shows its own entry size, as trash-cli reads it, and a symlink whose target is gone shows 0. `trash-list` also takes trash-cli's hidden `--files`, which adds ` -> FILE`, the item in the trash, to each row. Where trash-cli stops with a traceback on a payload that is gone, `trash-list --size` names it on stderr, lists the rest and exits 1. `rtrash list --size` keeps its date and disk usage columns, and the interop job now compares these rows with trash-cli's own.
- `trash-list` now prints trash-cli's stderr line for each trash record trash-cli reports: `Parse Error: FILE: Unable to parse Path.` for a record with no `Path=` line, and `[Errno N] TEXT: 'FILE'` for one it cannot open, such as a directory or a dangling symlink. The rows on stdout stay the same and the exit code stays 0. After the rows, `trash-list` also prints the `rtrash status --check` hint, as it already did on Windows, and counts only the records left out that trash-cli would have listed, so no record gets reported twice. `rtrash list` prints only the hint, which counts every record it leaves out.
- `trash-restore` now behaves as trash-cli's command: without PATH it offers only items trashed from under the current directory, PATH also matches items trashed from under it, a single match still asks, and the table columns take one space. `restore` gains `--sort=date|path|none` and `--overwrite` under every name.
- du picks more threads when the tree looks uncached: without `--threads` or `RAYON_NUM_THREADS`, it times a few stats under the first PATH, and when they wait on the disk it walks with three threads per CPU, up to 64. A cold scan of the rustup home went from about 470 ms to 370 to 384 ms; cached scans keep one thread per CPU.
- du starts the git probes for a repository as soon as the walk meets it, so git runs beside the walk instead of after it. On a folder of eleven clones the git part of a run dropped from about 32 ms to 17 ms (49 ms to 36 ms in total).
- du: directories with more than 256 entries spread their stats over all threads. A flat directory of 200000 files scans 2.3 times faster.
- du: git now answers each repo in one batched ignore check, with repos probed in parallel. A default run over 11 clones dropped from 104 to 66 ms.
- du: on macOS one `getattrlistbulk` call returns the names and sizes of many entries at once. The walk falls back to `readdir` and parallel stats when a file system lacks the call or the reads wait on the disk; `RTRASH_DU_BULK=0` or `1` forces either path.
- du: the walk opens directories relative to their parent and stats entries against the directory descriptor. The musl release build scans about three times faster.
- macOS: plain `put`, `list`, `restore` and `empty` now use the Finder Trash (`~/.Trash`), and put goes through Finder so its Put Back works. `RTRASH_MACOS_TRASH=freedesktop`, `macos.trash = "freedesktop"` in the config file or a `--trash-dir` pin keeps the FreeDesktop home trash. A new CI job runs the commands against Finder on a macOS runner.
- macOS: put, rm and empty now carry the Finder Trash total by the sizes of the items they move instead of walking the trash after each change.
- macOS: restore and undo now carry the Finder Trash total too, and the total keeps the inode count, so `trash-stats --total` shows inodes on the Finder Trash without `--measure`.
- macOS: the Finder total of a trash on an external drive is now keyed by the volume UUID, so it survives the drive being mounted again at another path.
- reap: the `pip-build` profile now keeps pip scratch dirs while a process named pip, pip3 or uv works in the temp dir.

### Fixed

- A `.trashinfo` record or a `directorysizes` cache that is a named pipe no longer hangs `rtrash list`, `status`, `rm`, `restore` or `empty`, and one that leads to an endless device such as `/dev/zero` no longer fills the memory. Records and the cache are read only from regular files, up to 1 MiB for a record and 64 MiB for the cache. A record that fails this is skipped, and a cache that fails it is walked around.
- A `DeletionDate` that is not a real `YYYY-MM-DDTHH:MM:SS` day is unreadable, the same as any other bad date. `2023-02-29` and `2026-00-00` used to be shifted onto another day, so an age filter could delete the item from that day. A date or batch value longer than 128 bytes is dropped. A time that cannot be shown in local time is `????-??-??T??:??:??` instead of a year taken from a failed conversion.
- A `directorysizes` line longer than 64 KiB is ignored, so a run of zeros the size of the cache is not taken as a directory of size 0. Shorter lines keep the same size, mtime and name, including numbers with leading zeros.
- A command that waits for the lock of a trash another process has now says which trash it waits for after a second. After `RTRASH_LOCK_WAIT` seconds (120 by default; 0 does not wait) it gives up with an error and changes nothing there. Before, put and empty could wait without a word for good behind a process stuck on a dead mount (trash-cli #354).
- A copy across devices on macOS no longer stops with "File name too long" on a deep tree.
  The walk measures the resolved path, so `/var` counts as `/private/var`, and switches to directory descriptors before that path would exceed the limit.
- A copy across devices on macOS reads file times from the seconds and nanoseconds fields.
  The previous names are not on `libc::stat`, so that build failed.
- A dead network share no longer hangs `list`, `empty`, `status` or `du --devices` (glib #605). Those commands look at every mount. Take a hard mount of a Network File System (NFS) share. When its server is gone, a look there waits forever. Each look at a network mount (NFS, Samba, `sshfs` or another such type) now runs on a thread of its own. A mount that does not answer within `RTRASH_MOUNT_WAIT` seconds (3 by default) is skipped with a warning.
- A file with two or more hard links now counts in du toward the directory of the first of its paths in sorted order, in a scan and in an ncdu export read with `du -f`. Before, it counted wherever a walk thread met it first, so on a tree of rsnapshot links the folder sizes changed from run to run (gdu #439).
- A gitignore `**` between other parts of a pattern no longer walks the rest of the path once per name. A long chain of them against a deep directory matches in time proportional to the table, and a table over two million cells still does not match.
- A home trash that is a symlink to a folder on another disk now works as one trash (glib #1640). Put sends a file from that disk to the home trash by a rename. It used to make a second trash at the top of that disk, because it took the device of the link and not of its target. Put also refuses to trash the `Trash` link itself, or the folder around it. Either would have moved the link into the trash it points at, and the next put would have started an empty home trash.
- A path that cannot be reached no longer counts as missing (trash-cli #377). On Windows, `rtrash put` names the real error for a file in a folder that denies access, and `-f` no longer hides it. When files/ in a trash lacks search permission, `rtrash empty --dangling` keeps the records of its payloads. Restore then says that the payload cannot be read.
- A put into a home trash whose `files` or `info` folder refuses writes now fails before moving anything, with an error that names the folder, since the home trash has no fallback.
- A put or restore across file systems no longer waits forever at a named pipe; the old copy opened the pipe and waited for a writer. The copy now keeps holes in sparse files, so a disk image with large empty runs keeps its small size in the trash. It also keeps extended attributes, the owner where allowed, and hard links inside the tree, and it clones within one btrfs file system. A tree with a directory this user can't empty gets refused before anything moves. A directory now gets its mode after its children, so a read-only tree copies too.
- A put or restore that copies across devices no longer crashes on a directory tree thousands of levels deep.
  The check that the tree can be removed, the copy, and cleanup of a half-made copy keep their own stack and a fixed number of open directories.
- A record in the trash named `.trashinfo`, `..trashinfo` or `...trashinfo` no longer counts as an item. Its payload in `files/` would be `files/` itself or the trash root, so `rtrash rm` on such an item, or `rtrash empty DAYS` once it looked old enough, deleted every other item in the trash and reported one removed. `rtrash list` no longer shows these records, and a full `rtrash empty` clears them.
- A restore from a trash on another file system no longer loses files when removing the trashed item fails partway. Restore used to delete its whole copy then, along with the files the removal had already taken. It now keeps the copy, finishes the restore, and warns that what is left stays in the trash with no record. A copy that fails partway is removed again on every path, Finder put-back included, so no half copy stays at the destination (trash-cli #429).
- A size total in `rtrash du` now stops at the largest 64-bit value (ncdu #278). Coreutils `du` stops the same way. Before, a total could wrap around, or panic in a debug build. Sizes that large come from a corrupt or crafted ncdu export or a broken stat. A plain sum added up the explorer totals, the `--dupes` reclaim, the shared extent count and the Recycle Bin bytes.
- A volume trash that two mounts show, such as a bind mount of a disk, now counts once in list, restore and empty, by its device and inode. Before, list showed each of its items twice, and restore offered both rows (trash-cli #301 and #341, gtrash #20).
- An age drawn from a timestamp near either end of the 64-bit range no longer overflows. `du` shows it as a large age, or as no age when the time is in the future, instead of aborting a debug build.
- Arguments and trashed names that are not UTF-8 now work with every command, `du` included. Before, any such argument made the program abort in argument parsing. A `.trashinfo` whose `Path=` has raw bytes from another tool used to vanish from list and `empty DAYS`; it now lists and restores, and an age filter can empty it. Put keeps the bytes of such a name in the trash. Two names that differ only in those bytes no longer fold into one name made of replacement characters. The Python bindings pass such paths through too.
- Btrfs compressed extents now count once each by where they start, in `du --shared` and in the shared extent summary of `du --unique`. Btrfs maps a compressed extent at its uncompressed length. Those mapped ranges overlap, and rtrash used to join them into a size no tool reports: 380 KiB for 8 MiB of zstd text and its reflink copy. Now it counts 8 MiB, the size `st_blocks` gives. `du --unique` also says how much of the mapped data sits in compressed extents. Without root, the kernel reports no compressed size.
- Errors raised while a browser is open, such as a put that hits a read-only directory, no longer draw over the screen and vanish at the next redraw. The browser keeps stderr aside and prints it after it exits.
- Every command now rebuilds path operands and path option values with their exact bytes, so names that are not valid text (stray bytes on Unix, unpaired surrogates on Windows) reach put and restore on Windows, `du diff`, `finder restore`, `setup --prefix`, `--bin-dir` and `--powershell-profile`.
- Every command that changes a trash now updates or drops its stored total: the interactive `empty` also writes a history line, and put, restore and `empty DAYS` no longer leave an old total that a coarse clock could make look current.
- In `rtrash du -i`, a long path no longer pushes the sizes, the counts and the `(partial)` marker out of the title.
  The path now gives way from the left, marked with `…`, as in ncdu.
- In the du explorer (`du -i`), a row a background delete leaves in place now says why in the status line, and `e` lists every such row with its reason; before, the reason waited in the held stderr until the explorer closed. Marks wait while a delete runs, so a new mark can no longer move onto another row as the trashed rows leave, and `v` and `o` refuse a row that is queued for the trash.
- In the explorer, a rescan (`r`) now moves the apparent size of the directories above in its own direction. Before, it moved only the way disk use went. If disk use shrank while the apparent size grew, for example when a sparse file replaced a dense one, the parents kept their old apparent size until they were rescanned too. Disk use, apparent size and item count now move together through one function, and a precheck rule keeps it that way.
- Messages now show the control characters of a file name as `?` on a terminal, in put, restore, rm, empty, du and the rest. That includes the operand put was given. Before, a file name could move the cursor or fake a line on the screen through such a message (dua #365).
- On Unix, `rtrash du --exclude` and `-X` patterns with bytes that are not UTF-8 now match names with the same bytes, since the match works on raw bytes.
  An ignore file or pattern file holding such a byte used to fail to read and drop every rule in it; now each rule still applies.
- On Unix, `rtrash du` walks a directory met a second time with the same device and inode once, under the name it met first. A bind mount on the same file system shows a directory that way, and its files used to count twice (ncdu #223). The totals now match coreutils `du` with the second name left out; `du` over one `PATH` still counts them twice.
- On Windows the trash-cli names match in any case, so TRASH-PUT.exe runs as trash-put.
- On Windows, `list --format={name}` prints the real file name with its extension, not the shell display name.
- On Windows, `reap --when-below` now measures each drive on its own, and `--purge-trash` purges the Recycle Bin of each drive that is short, instead of treating every path as one file system.
- On Windows, `reap` now counts the space a file takes on disk, as `du` does. A compressed, Compact, sparse or online-only file used to count its full length, so reap overstated what a cache would free.
- On Windows, `rtrash completions SHELL` and `rtrash man` now print the embedded scripts and man page; before, they fell through to the setup note, so the PowerShell completer could not be loaded there.
- On Windows, `rtrash du` never enters a reparse point that names another folder. That covers junctions, volume mount points and symlinks, and also Windows Subsystem for Linux (WSL) symlinks, container links and any other such tag. Each counts as a link, so no folder counts twice and no loop forms (diskus #42, gdu #441). Before, the scan took only symlinks and junctions for links.
- On Windows, `rtrash put` asks the Recycle Bin before it moves an item. The shell deletes an item for good, without asking, when the drive has no bin, when the bin is turned off, or when the item is over the bin's cap. Put now refuses such an item and leaves it in place, and `--allow-permanent` lets it go. The du explorer and `reap --trash` refuse it the same way.
- On Windows, `rtrash reap --purge-trash` empties the Recycle Bin of the volume that holds the path, so a volume mounted on a folder gives up its own items instead of those of the drive above it.
- On Windows, `rtrash reap --when-below` tells volumes apart by serial number, so a volume mounted on a folder is measured on its own instead of as part of the drive that holds the folder.
- On Windows, `rtrash status` with an age or size filter counts each item under the volume of its Recycle Bin, so items on a volume mounted on a folder show under that folder, as `trash-stats` shows them.
- On Windows, `trash-stats --history` now lists a drive whose Recycle Bin is empty when it has a history, so a history that ends in an empty still shows.
- On Windows, a Recycle Bin item whose name holds an unpaired surrogate (valid on NTFS) now restores to its exact name instead of the shell display name.
- On Windows, a copy of `rtrash.exe` named `trash-put.exe`, `trash-list.exe` and so on runs as that trash-cli command; the `.exe` suffix no longer hides the name.
- On Windows, du no longer counts a file that Compact packed (`compact /exe`) as 0 bytes on disk. The folder listing can show such a file as a plain one with no clusters. du now asks the file itself for its packed size, and keeps the listed size if that fails. Sparse files still count what the listing gives.
- On Windows, du now gives a compressed file its real disk use, the "Size on disk" of Explorer. That covers `compact /c` and Compact, the Windows Overlay Filter (gdu #407).
- On Windows, list, restore, rm, empty and `reap --purge-trash` now show and match the full original path from the `$I` record, so a file such as `notes.txt` no longer shows as `notes` when Explorer hides known extensions.
- On Windows, put, reap and the du explorer now show a Recycle Bin refusal as the system's own message, such as a file in use by another process. They no longer print the debug dump of the `trash` crate.
- On Windows, restore by path and `restore --batch=last` now match Recycle Bin items on a folder-mounted volume, comparing paths without case after mapping a volume GUID path back to its mount point, and `--batch=last` fails when an item it restored is not where it should be.
- On Windows, restore by path now finds an item when the path is given in the short 8.3 form (such as a `RUNNER~1` temp folder) while the Recycle Bin recorded the long one.
- On Windows, restore now moves each Recycle Bin item back to its original path itself, so a restored file keeps its extension instead of taking the shell display name.
- On Windows, restoring a Recycle Bin item that another process has open no longer copies it out and then fails to remove it, which left the item both restored and still in the bin. restore now copies only across volumes. Any other failed move is reported, and the item and its record stay in the bin. When the file is in use, the message names the processes as put does (`cannot restore 'PATH': in use by pid N (NAME, open)`), as trashy #114 asks. `rtrash` under the Windows Subsystem for Linux follows the same rule when it restores a Windows bin item.
- On Windows, rm and empty now remove a read-only file whose path runs past 260 characters. Every Win32 call that takes a path now gets the long form of a deep path, so the Recycle Bin and disk-space lookups work there too (dua #410 and #406).
- On Windows, rtrash now handles file names that hold an unpaired surrogate: arguments and names pass through WTF-8 and round-trip exactly, and output shows the bad unit as U+FFFD.
- On Windows, rtrash now makes each junction and symbolic link in an item again when it restores the item to another volume. It used to copy through a link, or stop half way on a junction. Any other kind of link stops the restore before it copies anything.
- On Windows, rtrash now prints and records every path in its plain form (`C:\...` or `\\server\share\...`), as dua 2.45.1 and 2.46 do. That covers a path typed with the verbatim `\\?\` prefix and one that `canonicalize` gives back that way, across put, list, restore, rm and du. rtrash reads a verbatim path in a Recycle Bin record as the plain path. list shows that form, and restore and rm match it. `list --directory` and `restore --to` take a verbatim directory. The du explorer shows its roots plainly too. A volume path such as `\\?\Volume{...}\` has no plain form and stays whole. A long `\\.\` device path no longer gets the network-share form of the verbatim prefix. The Windows runner now tests paths past 260 characters through put, list, rm, restore and du.
- On Windows, rtrash now reads the Recycle Bin index records itself instead of asking the shell, so one item whose name holds an unpaired surrogate no longer makes list, restore, empty and the rest fail for every item.
- On Windows, the `rtrash du -i` explorer now shows a file's allocated size in the rows it lists before the scan ends. It used to show the file length there, so an online-only OneDrive file (gdu #688), a sparse file or a compressed file looked as big as its length until the scan's sizes came in. The scan already counted the allocated size as disk use and the length as the apparent size.
- On Windows, the free space check before a restore picks the volume of each path by its mount, so a restore onto a volume mounted on a folder checks that volume instead of the drive above it, and one that moves across such volumes is counted as the copy it is.
- On Windows, the original path from a version 2 `$I` record is read in full: its length field counts characters, not bytes, so names such as `rtrash-purge-1.txt` no longer come out cut short.
- On Windows, the plain `rtrash status` lists the Recycle Bin of each volume mounted on a folder on a line of its own, as the filtered status already did.
- On macOS, Finder Trash locations drop the `/System/Volumes/Data` firmlink prefix when the short path names the same folder, and restore matches a path spelled through a linked folder such as `/var` for `/private/var`.
- On macOS, a put or restore across volumes now keeps the access control list of each file and directory; the copy took only the extended attributes, which on macOS leave the list out.
- On macOS, plain list, restore and rm find the Finder trash of every drive under /Volumes, not only those named in RTRASH_FINDER_VOLUMES.
- On the Finder Trash, list and restore show and use an item's original location from rtrash's put log until Finder writes its Put Back record, and `rtrash finder put` logs its items too.
- On the Finder Trash, list, restore and rm match names, paths and patterns in Unicode NFC, so a decomposed name such as one Finder kept matches its composed spelling.
- Permanent delete no longer walks into a mount on another device, and a directory you own is opened up and retried when the kernel returns permission denied.
  A symbolic link is removed as a link, and whatever still cannot be removed is reported.
  On Windows the same delete clears the read-only attribute and retries once.
- Put warns, with the system error, when it cannot create or write a volume trash (a fresh partition whose root only root may write) before falling back to the home trash.
- Put, list, empty and rm only use a volume's `.Trash/$uid` or `.Trash-$uid` when it is a real directory that belongs to you.
  One that is a symlink or belongs to another user gets a warning: put falls back to the next choice, in the end the home trash, and the others skip it.
- Removing an orphan (a payload without a `.trashinfo`) with `rtrash rm` keeps the running total, which never counted it.
- Removing items in the `rtrash empty` TUI and `rtrash empty --dangling` keep a current running total current, so `trash-stats --total` stays fresh after them.
- Restore now follows trash-cli 0.26.9 on a volume trash whose record leads out of its volume. When a `.trashinfo` on a USB stick has a `Path=` naming an absolute path elsewhere, such as `/home/you/.bashrc`, restore leaves it alone and list drops it, as for one that climbs out with `..`. `rtrash status --check` reports it. An absolute path inside the volume still restores. `put --trash-dir` now refuses to fill such a volume trash with an item from outside its volume.

  Under the `trash-list` name, rows print paths as trash-cli 0.26.9 does. A pipe gets each path as is, with a byte that is not UTF-8 as `\xNN`. A terminal gets `ls`-style quoting, such as `'/t/new'$'\n''line'`. Unlike trash-cli, rtrash also quotes bidirectional overrides there and ignores `TRASH_RAW_OUTPUT`.
- Restore on the Finder Trash with the JSON flag now prints an empty summary when nothing matches, as on FreeDesktop.
- Restore, empty and reap now keep the `rtrash-total` running total current, so the next put after them reads it instead of measuring the trash again.
- Shell completions now know the `keys` subcommand and offer `fish` after `rtrash completions`. Bash no longer offers put options after `rtrash keys` or `rtrash setup`. The bash script also runs under bash 3.2, the macOS `/bin/bash`.
- Shell completions now offer every option that `--help` lists: restore gains `--cwd-only` and `--plain` in bash, zsh and fish, and the fish `trash-*` names pick up the age, JSON and pin flags. `put --help` and the man page now document `--plain`.
- Subcommand help and error hints now name the subcommand, so `rtrash du --help` shows `Usage: rtrash du` and a bad option points at `rtrash du --help`. Counts read `1 item`, `1 file` and `1 directory`, and on Windows the `du` table ends directory paths with a backslash instead of a forward slash.
- Text output on a terminal now shows control characters and bidirectional overrides in names as `?`, as `ls` does.
  A trashed name, or a `Path=` line another user wrote into a shared trash, could carry escape sequences that move the cursor, recolor the screen, or fake extra rows.
  Pipes, files, and `--json` still get the raw name.
- The `directorysizes` cache now records allocated blocks the way `du -B1` counts them, with directory blocks included and each hard-linked file counted once.
  Put sizes a trashed directory in parallel after releasing the trash lock, so a 40,000-file tree goes to the trash in 91 milliseconds instead of 148.
  Commands `status` and `empty --dry-run` write sizes they had to walk back to the cache and drop lines for directories no longer in the trash, so a repeat run drops from 53 to 1.4 milliseconds.
  A `$top/.Trash` that fails the sticky-directory check now prints one warning before put falls back to `$top/.Trash-$uid`.
- The `du` explorer keeps marks through a rescan, shows their fresh sizes, and drops marks on entries that are gone.
- The busy check of `reap` profiles, which keeps a cargo target dir while cargo or rustc works in its project, now also matches the executable and first argument of each process, so names longer than the 15-byte Linux command name count.
- The du explorer (`du -i`) now cuts a long path in its title and marks panel, and wraps the info panel, by terminal columns rather than characters, as dua 2.46 does. A path with Chinese, Japanese or Korean characters, or with emoji, stays inside its frame.
- The empty browser (`empty -i`) and the put browser now show in their status line why an entry stayed, as the restore browser and the du explorer do. Before, the reason waited in the held stderr until the browser closed. The empty browser still prints every reason once it has closed.
- The explorer's item counts now match a rescan (gdu #691). After a delete or an undo, the rows of the parent directories kept their old item counts, so the title total disagreed with `du --json`. With a name filter or hidden dot names, the title counted only the rows shown, while its size covered the whole directory. Both now follow each change. A new test reads the same tree four ways: the walk with `getattrlistbulk`, the walk without it, and both ncdu exports read back. All four give the same counts.
- The filter in the restore, empty and put browsers takes every letter.
  Before, `j` and `k` moved the cursor instead of typing, as did any letter bound to a move.
- The first `DeletionDate` or `X-Rtrash-Batch` in a trashinfo file wins even when that value is too long to keep or grows past the limit as invalid bytes are replaced. A later copy of the key no longer replaces it, so an age cut keeps an item whose first date cannot be read.
- The inode count in `trash-stats` no longer serves a stale cached answer when another tool adds a record within the same clock tick as the last count; an `info/` stamp from the last two seconds is recounted.
- The man page now documents `rtrash keys` (the browsers' key bindings, `keys.toml` and its `--list`, `--sample` and `--path`), `RTRASH_KEYS`, the `keys.toml` path under FILES, and `RTRASH_DU_BULK` on macOS. A precheck rule now fails when a command the completions offer has no man section, or when `src/` reads an `RTRASH_*` variable that ENVIRONMENT does not list.
- The paths that `p` in `du -i` prints on quit, and those `du --print0` and `du --dupes --print0` list, show each control character as `?` when stdout is a terminal, as dua #365 asks, so a crafted file name cannot act on the screen. Down a pipe they stay the exact bytes, for `xargs`.
- The put, empty, and restore browsers now show their status line. The footer had room for the key hints only, so confirm prompts such as "overwrite existing file?" never appeared on screen.
- The restore browser no longer prints `restored '...'` lines over its own screen. It prints them, and any errors, after it exits, and the status line names the last error.
- The sizes `put` records in `directorysizes` and the totals `empty --dry-run` shows now count directories past `PATH_MAX`, which they skipped before.
- Trash directories now resolve symlinked parents the same way original paths do, so `put --json` reports `original` and `trash_dir` in one form (on macOS both start with `/private/var` instead of mixing it with `/var`). A `--trash-dir` pin such as `/link/vol/.Trash-1000` reached through a symlink still writes a `Path=` relative to its volume, and `restore PATH` finds a file when PATH is typed through a symlinked directory.
- When `rtrash restore` cannot put an item in place, the item goes back into the trash under its old name and the error says so. It used to stay outside the trash, beside the destination, under a hidden `.rtrash-restore-` name, while its record pointed at nothing; a `Path=` with `%00` was enough to cause this.
- Windows `rtrash list --cwd-only` matches the current directory as given and as resolved, so it finds items on a volume mounted on a folder.
- With `--gitignore`, `rtrash du` now lists nothing for a `PATH` below a directory that git ignores, and says why on stderr, since git ignores all of it. Before, it counted the whole `PATH`, and a `!` pattern for a file in there seemed to bring that file back. Inside the scan, `!` patterns already matched git, including that nothing comes back from inside an excluded directory; a new test checks each file against `git check-ignore`.
- With `RTRASH_ALLOW_ROOTS` set, the du browser on Windows no longer trashes a row outside the roots, and wsl restore checks every destination before it changes anything, so `-f` cannot remove a file outside the scope.
  A delete in the du browser with any row outside trashes nothing.
- `empty` no longer stops at a directory you own but cannot list or change, such as a makepkg `pkg` tree at mode 0311.
  It gives the owner `rwx` back and removes it, and it never changes what another account owns.
  An item that still cannot go stays in the trash with its `.trashinfo`, while the rest goes.
  `empty` then lists each item left with the reason and exits 1.
  `list`, `status`, and `rm` now show and match payloads that have no `.trashinfo`.
- `list`, `empty`, `rm` and `put` now treat a `ramfs` mount as a volume with its own `.Trash-$uid`, like `tmpfs`. Mounts that only mirror files kept elsewhere or are kernel interfaces are skipped: `fuse.lxcfs`, `fuse.snapfuse`, `selinuxfs`, `smackfs` and `nfsd` join the list. Mounts on VMs, containers and network shares, such as `virtiofs`, `9p`, `fuse.sshfs` and `overlay`, were already listed and still are; trash-cli's `trash-list` leaves them out (trash-cli #412).
- `put` and `restore` across file systems now check that the whole item can go before they copy it, and refuse it otherwise. Before, the copy ran first, and the removal stopped partway and left the original half gone. The new checks cover a parent directory that does not allow deleting, another user's file in a sticky directory, and an immutable or append-only file or directory (`chattr +i` or `+a` on Linux, `chflags` on macOS). A read-only directory deep inside was already refused (trash-cli #427).
- `put` now refuses `.` and `..` however the operand spells them, as `rm` does: `./`, `d/.`, `d/./`, `d/..` and `../` get the same "refusing to remove '.' or '..' directory" message as `.`. Before, `put -r d/.` trashed `d`, and `./` or `d/..` failed with an unclear error.
- `put` now refuses `link/` when link is a symlink to a directory, and leaves both the link and the directory's files in place. That operand names the directory, while the move would take the link. Give `link` to trash the link itself. `d/` and `d//` trash `d` with a record that names `d`, and `f/` on a file is an error that keeps the file (trash-cli #434).
- `put` sends a file on a mount that volume discovery skips, such as a document portal (`fuse.portal`) or the `gvfs` mount under `/run/user/$uid`, to the home trash. Before, it made a `.Trash-$uid` on the mount around it, which is another device, and copied the file into it.
  Discovery now goes by the later of two mounts on one mount point, the one in view.
- `rtrash config --default` showed `trash = ""` for `macos.trash`, which names neither choice. It now shows the value the system uses when the key is unset: `"finder"` on macOS, `"freedesktop"` elsewhere, so uncommenting the line changes nothing.
- `rtrash du --dupes --trash-dupes` now passes `--home-only` and a single `--trash-dir` on to the put, so the extra copies go to the trash the du summary reads. Before, both options changed only the summary. Two `--trash-dir` options with `--trash-dupes` are refused.
- `rtrash du --dupes` no longer downloads cloud placeholders while it hashes. On Windows it leaves unread a file marked `RECALL_ON_DATA_ACCESS` (a OneDrive online-only file), `OFFLINE` or, in the listing, `RECALL_ON_OPEN`. On macOS it skips a file with the `SF_DATALESS` flag (an iCloud Drive file not yet on the Mac). A line on stderr counts the placeholders that shared a size with another file, and `--json` and `--print0` output stay as before. Linux has no standard mark for such files, so nothing changes there.
- `rtrash du --dupes` walks a tree one level at a time. A chain of directories thousands of levels deep used to overflow a walk thread's stack and abort the run. On Windows, a restore of a deep folder from the Recycle Bin to another volume no longer recurses either.
- `rtrash du -i --sort=items` lists files again; before, every directory view left them out.
- `rtrash du -o out.snap` now writes the snapshot to a new file beside `out.snap` and renames it into place once it is whole and on disk, as dua 2.44 does. `--ncdu` and `--ncdu-binary` exports work the same way, and one written inside the directory it scans leaves that new file out of the listing. A full disk, a crash or a failed scan used to leave a cut-off file that `du -f` and `du diff` could then read as if whole; now the old file stays. A pipe, a device or a symbolic link is still written in place, and a file written over keeps its mode.
- `rtrash du` counts directories past 64 levels whose paths exceed `PATH_MAX`, which it used to skip without saying so, and no longer crashes with a stack overflow on trees about 2000 levels deep.
  The walk stops at 4096 levels or 32 KiB of path, counts what lies past that with its own blocks only, and reports how many such directories there were.
- `rtrash du` names an empty or missing PATH on stderr and scans the other paths, with exit status 1, as GNU du does. Before, one such PATH (`du a "" b`, or a missing line in `--files-from`) ended the whole run with no output, as dust #585 reports for dust.
- `rtrash du` no longer aborts with a stack overflow on a deep tree when the system refuses its walk threads their full stacks, as under `ulimit -v`. It now picks stacks that fit the cap and enters fewer levels on smaller ones; the directories past that depth count with their own size and are reported, as those past 4096 levels already were.
- `rtrash du` no longer opens a folder that the filters leave out. Opening an excluded folder such as `~/Library` brought up macOS privacy prompts (dust #546). The cold-cache probe at the start of a scan used to open it, and `--dupes` ignored `--exclude`, `--gitignore` and the other filters. Both now skip what the filters leave out.
- `rtrash du` no longer reads a large directory twice before the walk: the check for an uncached tree now lists only the 48 entries it times.
  A directory of 200000 files took about 110 milliseconds again instead of 160, with 10 MiB less memory.
- `rtrash du` no longer tries every split of every `**` when it matches a gitignore path. A long chain of them against a deep path gives the same answer quickly, and a pattern whose match table would pass two million cells does not match. An rsync wildcard over a long pattern and a long text remembers failed positions without allocating a table the size of their product.
- `rtrash du` on macOS now reads `getattrlistbulk` records the way the kernel packs them. The reader expected the error slot after the file ID and file attributes on every record, but macOS packs the error slot first and leaves file attributes off directory records. Any directory with a subdirectory stopped listing early, and file sizes came from the wrong fields.
- `rtrash du` refuses a `PATH` inside another one, as gdu 5.38 does, since the walk of the outer `PATH` counted it already and the totals, `--json`, `--dupes` and the explorer counted it twice. A `PATH` given twice is scanned once, with a note on stderr, and under `-x` a `PATH` on another file system than the one around it stays.
- `rtrash finder list` and `restore` read the `.DS_Store` of a trash only when it is a regular file of at most 64 MiB. A trash on a foreign drive with a named pipe there hung both commands, and a link to `/dev/zero` took memory until the system killed the process.
- `rtrash put -r` on a directory that contains the trash it would move into now says so, where it used to fail with "Invalid argument". Putting an item that is already in that trash is refused too, since moving it would lose its record there.
- `rtrash put` no longer trashes into a trash whose `files` or `info` folder is a symbolic link, as trash-cli 0.26.9 refuses one too. Such a home trash or `--trash-dir` pin fails the item with "is a symbolic link" and leaves it in place. A volume trash like that gets a warning, and put falls back as for any other unusable volume trash. Before, the item went wherever the link pointed.
- `rtrash put` now writes each record under a temporary name in `info/` and links it to its trash name once it is whole. Other tools then never meet an empty or cut-off record while they read the trash, and a killed put leaves none behind. On a file system with no hard links, put makes the record in place as before. `rtrash status --check` reports a temporary that a killed put left as a put leftover, and `--fix` removes it.
- `rtrash reap --purge-trash` counts the inodes each trash item takes when a file system is short of inodes, and stops once enough are free instead of emptying the whole trash.
- `rtrash reap --when-below` measures free space again before each entry instead of trusting the entries' sizes, so hard links, shared extents and other writers no longer make it stop early or late; a dry run still estimates.
- `rtrash restore` errors and warnings, and the lines the restore browser prints when it closes, now show control characters as `?` on a terminal, as `list` does. A `Path=` with an escape sequence could clear or redraw the screen through a "refusing to overwrite" error.
- `rtrash restore` keeps a running total that another process stored while the restore ran, sizing the item where it now lives.
- `rtrash restore` no longer deletes a file that already has the hidden name it stages an item under (`.rtrash-restore-` with its process number and the item's name). It picks a free name instead, so an item that an interrupted restore left under such a name is not lost.
- `rtrash restore` refuses a destination that is, contains or sits inside the item's trash or the home trash, whatever the flags. A forged or corrupt `.trashinfo` naming such a place used to let `restore -f` replace a real trashed item with another payload, or delete the whole data directory around the trash.
- `rtrash restore` refuses an item whose record in the home trash has a relative or empty `Path=`, and says that `--to` restores it into a chosen directory. Such a path used to resolve against the directory restore ran in, so `restore -f` there could overwrite an unrelated file of the same name.
- `rtrash rm` and `rtrash empty` no longer abort with a stack overflow on a tree some thousands of levels deep; they remove trees of any depth.
  The walk keeps its own stack and at most 16 open directories, reopening parents through `..` after checking they are still the same directory.
- `rtrash rm` drops the stored trash total after it removes items, so the trash history and the size checks after it no longer read a total from before the rm when it ran within the same clock tick.
- `rtrash rm` keeps a current running total current, and it sizes trashed directories from the `directorysizes` cache, so `rm -n` no longer walks every trashed tree.
- `rtrash rm` matches an absolute pattern typed through a symlinked directory, such as `/var/folders` on macOS, where `/var` is a symlink to `/private/var`. The directory part up to the first glob resolves the way put records original paths; the pattern as typed still matches too.
- `rtrash status --check --fix` no longer deletes a folder, with everything in it, that sits in `info/` under a name ending in `.trashinfo`. Only a regular file counts as a record now. A folder, link or other file with such a name is reported as `stray:` and kept, and the record removal refuses anything but a regular file. `empty --dangling` also leaves such names alone, where it used to remove a link and fail on a folder.
- `rtrash status --check --fix` scans each trash again once it has taken the trash lock, and removes only what that scan still finds. Before, it could remove the record of a put in flight, such as one moving a large folder across file systems. That record looked dangling in the first scan, and the fix removed it once the put let go of the lock.
- `rtrash` now refuses a `.DS_Store` at once when its block table, table of contents or B-tree record count cannot fit in the bytes it points at. A block count of `u32::MAX` used to overflow the padded table size on a 32-bit host and panic a debug build.
- du counts a file with more than one hard link once in its file count, as it already did in the sizes, so files plus directories match `du --inodes -s`. The total names the extra links ("1 file (+2 hard links)"), and the JSON root has a new `links` field.
- du lists what is inside a regenerable artefact when that artefact is the PATH being scanned (`rtrash du target`), instead of only printing a total. The total and the JSON root name its kind. A trash directory named as the PATH still keeps its contents unlisted.
- du no longer drops a directory whose bulk sits in a subdirectory it cannot list. With `--max-depth`, an age filter, or a large `.git`, the parent row used to vanish along with the hidden child; it now stays.
- du no longer lets you trash a directory that the filters show only in part. Its trash would take the unseen entries too. `--print0` and `--pick` drop such rows and count them on stderr, and the explorer refuses to trash one and names it. The new `--allow-filtered` lets them through. The `--json` row of such a directory counts the hidden entries in `filtered_out`. Age and size filters hide nothing, and a scan without filters works as before.
- du no longer merges every file into one when the filesystem reports no file IDs. On Windows a directory listing can return file ID 0, for instance on volumes without IDs; du then counted only the first file of the whole tree. It now counts such files one by one.
- du treats a bare repository like a `.git` directory: it is one row with a git state (`unpushed` when no remote has its commits), and nothing inside it, such as `objects/`, is listed or printed by `--print0`.
- empty: an item put into the trash after the count, or after the yes to `empty -i`, now stays. The run takes only what it counted and says how many it left.
- empty: naming one trash twice with `--trash-dir`, even spelled two ways, no longer hangs a `--max-size` prune. The trash is emptied once.
- macOS: Finder rewriting `.DS_Store` in a trash no longer marks the Finder total stale; the total now tracks the names in the trash instead of its modification time.
- restore --name, --glob and --regex compare composed (NFC) forms on every trash, and undo on the Finder Trash finds an item whose name Finder stored decomposed.
- rm, restore and empty now check each Finder Trash or Recycle Bin item again right before acting on it, on macOS, on Windows and in the Windows Subsystem for Linux. An item that took the name of a listed one while rtrash waited for an answer stays put. The run names it, and rm and restore exit 1.
- rm, restore, the empty browser and the undo of the du explorer now check each item again under the trash lock, just before they act. They used to act on the trash as they had read it, which could be long before: after the rm question, or after a pick in the restore or empty browser. By then the item could have left the trash, and a later put could have given its name to another item, which was then deleted or restored in its place. Such an item now stays, and the message says it changed in the trash. A restore also keeps the lock until the item is out, so an rm or empty of the same trash waits for it.
- rtrash exits quietly when a reader closes stdout early, as `| head` or `| grep -q` do, instead of panicking.
- rtrash now compares `directorysizes` names as decoded bytes. It reads a line from another tool that used a different percent-encoding: a raw space, lower-case hex, bytes that are not UTF-8. Put, rm, restore and empty replace or drop that line instead of adding a second one or leaving it stale. rtrash ignores a line whose name has a slash in it, even as `%2F`, as the spec requires.
- rtrash now skips a volume's `.Trash/$uid` or `.Trash-$uid` when your group or other users can write to it. Anyone who can write there could swap its `files/` or `info/` for a symlink (trash-cli #433).
  Put warns and falls back to the next trash, in the end the home trash; list, empty and rm warn and skip it. Run `chmod go-w` on the folder to use it again.
- rtrash sizes a trashed directory after the put lets go of the trash lock, so other puts need not wait for the walk. During that walk, the item could leave the trash and another item could take its name. The cache of directory sizes then got the old size for the new item. Now the size goes into the cache only when the record is still the one that was sized.
- rtrash works where the mount table cannot be read, as on Termux (trash-cli #348). A trash command then finds the volume of a path by its device. Before, it took every path for one on `/`. Without the table, `rtrash du --devices` lists the file systems of `/`, the home directory and the current directory. One note says that the list is partial.
- rtrash writes each whole file to a temporary beside it, syncs it and renames it over the old file. A full disk or a crash leaves the old file, never a cut-off one. `rtrash setup` wrote its completion scripts in place, and so did the reap timer units and the last-put record on Windows. The Finder undo log and the trimmed history used a temporary. They skipped the sync, though, and kept the temporary when a write failed. A file the user asked for, such as a completion script that is a link, is still written through the link. rtrash replaces a link at the path of its own caches and logs.


## [0.1.4](https://github.com/HaoZeke/rtrash/tree/v0.1.4) - 2026-07-09

### Added

- README terminal demo (asciinema cast + GIF) with regenerable docs/demo/record.sh.
- list/status: --json and --older-than/--newer-than age filters (empty DAYS cutoff model).
- setup installs an optional Midnight Commander user-menu sample (share/rtrash/mc.menu.sample).


## [0.1.3](https://github.com/HaoZeke/rtrash/tree/v0.1.3) - 2026-07-08

### Added

- Add a ratatui restore browser on TTY (`rtrash restore`): filter, navigate, multi-restore session, force/confirm overwrite; `--plain` keeps line-mode selection.
- Interactive restore lists the full trash (trash-cli style) when PATH is omitted; add `--cwd-only` for the previous cwd-scoped pick; accept piped index selection without a TTY.
- Restore TUI: multi-select and fuzzy filter; add empty and put TUI browsers on TTY (multi-select, confirm); `--plain` skips TUI.
- TUI keybinds fully customizable via TOML (`keys.toml` / `RTRASH_KEYS`); `rtrash keys` CLI.
- Windows: system Recycle Bin backend for put/list/restore/empty (not FreeDesktop on-disk fiction).
- macOS: experimental FreeDesktop home trash (`$XDG_DATA_HOME/Trash`), not Finder Trash.

### Developer

- Refresh dated trash-cli comparison in docs/benchmarks.md (2026-07-08); harden benches/compare_trash_cli.py to reject multi-call rtrash as trash-cli.

### Changed

- TUI: live fuzzy filter, shared keys + ? help, viewport paging across restore/empty/put.

### Fixed

- Python bindings release the GIL around put/empty/restore/list I/O (PyO3 `detach`); force `--plain` (no TUI).


## [0.1.2](https://github.com/HaoZeke/rtrash/tree/v0.1.2) - 2026-07-08

### Fixed

- Bump optional PyO3 dependency to 0.29 (RUSTSEC-2026-0176 / RUSTSEC-2026-0177); cargo-deny CI green on GitHub Actions.


## [0.1.1](https://github.com/HaoZeke/rtrash/tree/v0.1.1) - 2026-07-08

### Developer

- Add CI quality bar (`cargo fmt --check`, `clippy -D warnings`, `test --locked`), commit `Cargo.lock`, and `cargo-deny` policy (`deny.toml`) with matching CONTRIBUTING/`scripts/quality.sh` docs.
- Add towncrier (`CHANGELOG.md` / `docs/newsfragments/`), cocogitto (`cog.toml` version lockstep), `prek.toml` (large-file guard, toml/yaml hygiene), lychee link check, `large-file-auditor` CI, and HaoZeke/doc-previewer PR comments for the Sphinx site.
- Adopt cargo-nextest (CI profile), cargo-dist (Linux musl releases + shell installer), and ASV PR benchmarks with asv-spyglass + HaoZeke/asv-perch comments (eOn-style).


## [0.1.0](https://github.com/HaoZeke/rtrash/tree/v0.1.0) - 2026-07-08

### Added

- Initial release: FreeDesktop trash CLI with rm-compatible put, multi-call suite (`trash-put` / `trash-list` / `trash-restore` / `trash-empty` / `trash-rm`), parallel empty via rayon, and `rtrash setup` for links, completions, and man page.
- Publish **rtrash 0.1.0** to [crates.io](https://crates.io/crates/rtrash) and [PyPI](https://pypi.org/project/rtrash/) (maturin/PyO3 bindings; Linux x86_64 wheels for CPython 3.10–3.14).
- GitHub Release musl static assets for **x86_64** and **aarch64** Linux with `cargo-binstall` metadata remapping glibc hosts to musl tarballs.
- Use FreeDesktop `directorysizes` cache for `status` and `empty --dry-run` reclaim estimates when the cache is valid.
- Embedded bash/zsh/fish completions and man page (`rtrash completions`, `rtrash man`, installed by `rtrash setup`).

### Developer

- Docs site at https://rtrash.rgoswami.me (Sphinx + Shibuya); orgmode sources under `docs/orgmode/`.
