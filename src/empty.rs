use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use rayon::prelude::*;

use crate::info;
use crate::list;
use crate::trashdir::{self, TrashDir};
#[cfg(feature = "tui")]
use crate::util::stdin_is_tty;
use crate::util::url_decode;

const HELP: &str = "\
Usage: {prog} [OPTION]... [DAYS]
Purge trashed items. With DAYS (or an age such as 2h), only items trashed
more than that long ago.
An item that cannot be removed stays; each one is listed at the end, with
the reason, and the exit status is 1.
On a TTY without DAYS, opens the interactive empty browser (multi-select
permanent delete). Use --plain to skip the TUI.

  -n, --dry-run    report what would be removed without removing anything;
                     also prints approximate reclaimable disk space (fast
                     in-process walk, like du of the victims)
  -v, --verbose    print each removed item by its original path
  -i, --interactive  list the trash directories and ask before emptying
                     them (as trash-cli); skips the TUI
  -f, --force      do not ask (cancels an earlier -i)
      --plain      skip TUI; non-interactive empty (also used when stdin is not a TTY)
      --all        allow an empty with no DAYS, --max-size, --until-free or
                     --trash-dir to remove items other puts left; without
                     it such an empty asks on a terminal and refuses
                     elsewhere unless every item came from the latest put
      --older-than=AGE  same as positional DAYS; AGE may also be a duration
                     such as 2h, 30m or 1d12h
      --max-size=SIZE  remove items until the trash (all roots in scope
                     together) fits in SIZE, such as 500M or 10G; with
                     DAYS, the age cut runs first
      --prune=oldest|largest  which items --max-size removes first
                     (default oldest)
      --until-free=SPEC  on each file system under SPEC free (a percent
                     such as 10%, a size such as 20G, and ,inodes=PCT),
                     remove the oldest items of every trash there (with
                     --home-only or --trash-dir, only file systems those
                     hold) until
                     it is back at SPEC; exits 1 when one stays short;
                     --json lists each short file system
      --dangling   remove only the .trashinfo records whose payload is gone
                     (they list as items that cannot be restored); takes
                     no DAYS or --max-size
      --json       emit a JSON summary on stdout ({dry_run, removed, bytes, days,
                     older_than_secs, max_size})
      --trash-dir=PATH  empty only this trash directory (repeatable);
                     default is the home trash plus every mounted volume
      --home-only  only the home trash (skip volume trash discovery)
      --all-users  every user's trash this account can write (root: all)
      --help       display this help and exit
      --version    output version information and exit
";

struct Opts {
    /// The age cut in seconds (`--older-than`, positional DAYS).
    days: Option<i64>,
    dry_run: bool,
    verbose: bool,
    home_only: bool,
    trash_dirs: Vec<PathBuf>,
    plain: bool,
    json: bool,
    max_size: Option<u64>,
    until_free: Option<(String, crate::reap::Floor)>,
    prune: crate::retention::Policy,
    dangling: bool,
    /// `-i`: ask once before emptying, as trash-cli does.
    interactive: bool,
    /// `--all`: an unscoped empty may take items other puts left.
    all: bool,
    all_users: bool,
}

/// The `.trashinfo` names in `dir` whose payload in `files/` is gone,
/// sorted.
fn dangling_records(dir: &TrashDir) -> Vec<std::ffi::OsString> {
    let files = dir.files();
    let mut out: Vec<std::ffi::OsString> = fs::read_dir(dir.info())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            // Raw bytes, so a name that is not UTF-8 still finds its
            // payload and is never taken for dangling.
            let fname = e.file_name();
            let stem = fname.as_bytes().strip_suffix(b".trashinfo")?;
            if stem.is_empty() || stem.contains(&b'/') {
                return None;
            }
            let payload = files.join(std::ffi::OsStr::from_bytes(stem));
            payload.symlink_metadata().is_err().then_some(fname)
        })
        .collect();
    out.sort();
    out
}

/// `empty --dangling`: remove the records that name no payload, under
/// each trash's lock, and nothing else.
fn empty_dangling(prog: &str, dirs: &[TrashDir], opts: &Opts) -> i32 {
    let mut removed = 0u64;
    let mut status = 0;
    for dir in dirs {
        let _lock = match trashdir::TrashLock::acquire(&dir.root) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("{prog}: cannot lock trash '{}': {e}", dir.root.display());
                status = 1;
                continue;
            }
        };
        // A total current before keeps current: each record's size and
        // its one count come off.
        let mut total = (!opts.dry_run)
            .then(|| crate::trash_total::cached(dir))
            .flatten();
        for fname in dangling_records(dir) {
            let path = dir.info().join(&fname);
            if opts.dry_run {
                if !opts.json {
                    println!("would remove {}", crate::util::term_path(&path));
                }
                removed += 1;
                continue;
            }
            let sz = fs::symlink_metadata(&path).map_or(0, |m| {
                use std::os::unix::fs::MetadataExt;
                m.blocks().saturating_mul(512)
            });
            match fs::remove_file(&path) {
                Ok(()) => {
                    if opts.verbose && !opts.json {
                        println!("removed {}", crate::util::term_path(&path));
                    }
                    crate::trash_total::forget(dir);
                    crate::trash_total::touched(dir);
                    if let Some((b, n)) = total {
                        let next = (b.saturating_sub(sz), n.saturating_sub(1));
                        crate::trash_total::store(dir, next.0, next.1);
                        total = Some(next);
                    }
                    removed += 1;
                }
                Err(e) => {
                    eprintln!("{prog}: cannot remove '{}': {e}", path.display());
                    status = 1;
                }
            }
        }
    }
    if opts.json {
        println!(
            "{{\"dry_run\":{},\"removed\":{removed},\"dangling\":true}}",
            opts.dry_run
        );
    } else {
        let verb = if opts.dry_run {
            "Would remove"
        } else {
            "Removed"
        };
        eprintln!(
            "{verb} {}",
            crate::util::plural(
                removed,
                "record without a payload",
                "records without a payload"
            )
        );
    }
    status
}

/// trash-cli's `-i` prompt: name the trash directories, read a y or n.
/// Whether this empty would take everything: no age, size or free-space
/// cut, no `--trash-dir` pin, no `--all`, and not a dry run.
fn unscoped(opts: &Opts) -> bool {
    opts.days.is_none()
        && opts.max_size.is_none()
        && opts.trash_dirs.is_empty()
        && !opts.all
        && !opts.dry_run
}

fn confirm(dirs: &[TrashDir]) -> bool {
    eprintln!("Would empty the following trash directories:");
    for d in dirs {
        let (size, n) = holds(&d.files());
        eprintln!("    - {} ({size}, {n})", d.root.display());
    }
    eprint!("Proceed? (y/N) ");
    let mut line = String::new();
    if io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim(), "y" | "Y" | "yes" | "Yes")
}

/// What a trash's payload directory takes on disk and how many items sit
/// at its top, as `12.0 KiB` and `3 items`, for the `-i` prompt.
fn holds(files: &std::path::Path) -> (String, String) {
    let n = fs::read_dir(files).map_or(0, |d| d.flatten().count()) as u64;
    let size = crate::util::format_bytes(crate::fastdelete::disk_usage(files));
    (size, crate::util::plural(n, "item", "items"))
}

fn usage_err(prog: &str, msg: &str) -> i32 {
    eprintln!("{prog}: {msg}");
    eprintln!("Try '{prog} --help' for more information.");
    2
}

pub fn run(prog: &str, args: &[String]) -> i32 {
    let snap = crate::finder_total::snapshot();
    let code = run_inner(prog, args);
    crate::trash_total::log_touched("empty");
    crate::finder_total::log("empty", &snap);
    code
}

fn run_inner(prog: &str, args: &[String]) -> i32 {
    let mut opts = Opts {
        days: None,
        dry_run: false,
        verbose: false,
        home_only: false,
        trash_dirs: Vec::new(),
        plain: false,
        json: false,
        max_size: None,
        until_free: None,
        prune: crate::retention::Policy::Oldest,
        dangling: false,
        interactive: false,
        all_users: false,
        all: false,
    };
    for arg in args {
        match arg.as_str() {
            "-n" | "--dry-run" => opts.dry_run = true,
            "-v" | "--verbose" => opts.verbose = true,
            "-f" | "--force" => opts.interactive = false,
            "-i" | "--interactive" => opts.interactive = true,
            "--all-users" => opts.all_users = true,
            "--plain" => opts.plain = true,
            "--all" => opts.all = true,
            "--json" => opts.json = true,
            "--dangling" => opts.dangling = true,
            "--home-only" => opts.home_only = true,
            "--help" => {
                print!("{}", HELP.replace("{prog}", prog));
                return 0;
            }
            "--version" => {
                println!("{prog} (rtrash) {}", env!("CARGO_PKG_VERSION"));
                return 0;
            }
            a if a.starts_with("--trash-dir=") => {
                opts.trash_dirs
                    .push(crate::util::arg_path(&a["--trash-dir=".len()..]));
            }
            a if a.starts_with("--max-size=") => {
                match crate::retention::parse_size(&a["--max-size=".len()..]) {
                    Some(n) => opts.max_size = Some(n),
                    None => return usage_err(prog, &format!("invalid SIZE in '{a}'")),
                }
            }
            a if a.starts_with("--prune=") => {
                match crate::retention::Policy::parse(&a["--prune=".len()..]) {
                    Some(p) => opts.prune = p,
                    None => return usage_err(prog, "--prune takes oldest or largest"),
                }
            }
            a if a.starts_with("--until-free=") => {
                let v = &a["--until-free=".len()..];
                match crate::reap::Floor::parse(v) {
                    Some(f) => opts.until_free = Some((v.to_string(), f)),
                    None => {
                        return usage_err(
                            prog,
                            &format!("invalid SPEC in '{a}' (a percent such as 10%, a size such as 20G, and ,inodes=PCT)"),
                        )
                    }
                }
            }
            "--max-size" | "--prune" | "--until-free" => {
                return usage_err(prog, &format!("{arg} needs a value ({arg}=VALUE)"));
            }
            a if a.starts_with("--older-than=") => {
                match crate::filters::parse_age_secs(&a["--older-than=".len()..]) {
                    Ok(secs) => opts.days = Some(secs),
                    Err(e) => return usage_err(prog, &e),
                }
            }
            "--older-than" => {
                return usage_err(prog, "--older-than requires an AGE (use --older-than=AGE)");
            }
            a if !a.starts_with('-') => match crate::filters::parse_age_secs(a) {
                Ok(secs) => opts.days = Some(secs),
                Err(_) => return usage_err(prog, &format!("invalid DAYS argument '{a}'")),
            },
            a => return usage_err(prog, &format!("unrecognized option '{a}'")),
        }
    }

    if let Some((spec, floor)) = &opts.until_free {
        if opts.days.is_some() || opts.max_size.is_some() || opts.dangling || opts.all_users {
            return usage_err(
                prog,
                "--until-free takes no DAYS, --max-size, --dangling or --all-users",
            );
        }
        return until_free(prog, spec, *floor, &opts);
    }

    // macOS empties the Finder Trash unless a pin or the setting says
    // otherwise.
    if crate::finder::is_default(!opts.trash_dirs.is_empty() || opts.all_users) {
        if opts.dangling {
            return usage_err(
                prog,
                "--dangling does not apply to the Finder Trash; pin --trash-dir or set RTRASH_MACOS_TRASH=freedesktop",
            );
        }
        if !opts.interactive && unscoped(&opts) {
            let (total, foreign) = crate::finder_shared::outside_latest_put();
            if !crate::budget::guard_ok(prog, total, foreign) {
                return 2;
            }
        }
        if opts.interactive && !opts.dry_run {
            let (mut bytes, mut n) = (0u64, 0u64);
            for t in crate::finder::trashes() {
                bytes += crate::fastdelete::disk_usage(&t);
                n += fs::read_dir(&t).map_or(0, |d| d.flatten().count()) as u64;
            }
            let size = crate::util::format_bytes(bytes);
            let items = crate::util::plural(n, "item", "items");
            eprint!("Would empty the Finder Trash ({size}, {items}). Proceed? (y/N) ");
            let mut line = String::new();
            let _ = io::stdin().read_line(&mut line);
            if !matches!(line.trim(), "y" | "Y" | "yes" | "Yes") {
                return 0;
            }
        }
        let cutoff = opts.days.map(|secs| info::now_epoch() - secs);
        return crate::finder::default_empty(
            prog,
            &crate::finder::EmptyOpts {
                cutoff,
                max_size: opts.max_size,
                prune: opts.prune,
                dry_run: opts.dry_run,
                verbose: opts.verbose,
                json: opts.json,
            },
        );
    }
    if opts.all_users && (opts.home_only || !opts.trash_dirs.is_empty()) {
        return usage_err(prog, "--all-users takes no --home-only or --trash-dir");
    }
    let dirs: Vec<TrashDir> = if opts.all_users {
        trashdir::all_users()
    } else {
        trashdir::resolve_dirs_opts(&opts.trash_dirs, opts.home_only)
    };
    if dirs.is_empty() && !opts.trash_dirs.is_empty() {
        eprintln!(
            "{prog}: no valid --trash-dir pins (need non-symlink files/ and info/ directories)"
        );
        return 2;
    }

    if opts.dangling {
        if opts.days.is_some() || opts.max_size.is_some() {
            return usage_err(prog, "--dangling takes no DAYS or --max-size");
        }
        return empty_dangling(prog, &dirs, &opts);
    }

    if opts.interactive && !opts.dry_run {
        if !confirm(&dirs) {
            return 0;
        }
        opts.plain = true;
    }

    // TTY empty browser when no age filter (full interactive review).
    // --json implies non-interactive (scripts).
    #[cfg(feature = "tui")]
    if !opts.plain && !opts.json && opts.days.is_none() && opts.max_size.is_none() && stdin_is_tty()
    {
        let mut entries = list::collect(&dirs);
        // Orphans too, so the browser can clear everything a plain empty would.
        entries.extend(list::orphans(&dirs));
        let refs: Vec<&list::Entry> = entries.iter().collect();
        return crate::empty_tui::run(prog, refs, opts.dry_run);
    }

    if !opts.interactive && unscoped(&opts) {
        let entries = list::collect(&dirs);
        let latest = crate::undo::batches(&entries).first().map(|b| b.id.clone());
        let foreign = entries
            .iter()
            .filter(|e| latest.is_none() || e.batch != latest)
            .count();
        if !crate::budget::guard_ok(prog, entries.len(), foreign) {
            return 2;
        }
    }

    let cutoff = opts.days.map(|secs| info::now_epoch() - secs);
    let removed = AtomicU64::new(0);
    let bytes = AtomicU64::new(0);
    let errors = AtomicU64::new(0);
    let failures: Failures = Default::default();

    // A size budget alone removes nothing by age.
    let age_pass = !(opts.max_size.is_some() && cutoff.is_none());
    for dir in dirs.iter().filter(|_| age_pass) {
        // Exclusive lock for this trash root so put cannot interleave a half pair.
        let _lock = match trashdir::TrashLock::acquire(&dir.root) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("{prog}: cannot lock trash '{}': {e}", dir.root.display());
                errors.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        };
        empty_one(
            prog, dir, cutoff, &opts, &removed, &bytes, &errors, &failures,
        );
    }

    if let Some(max) = opts.max_size {
        let done = crate::retention::prune(
            prog,
            &dirs,
            &crate::retention::Prune {
                max,
                policy: opts.prune,
                dry_run: opts.dry_run,
                verbose: opts.verbose,
                protect_batch: None,
                gone_before: cutoff,
                free_inodes: 0,
            },
        );
        removed.fetch_add(done.removed, Ordering::Relaxed);
        bytes.fetch_add(done.bytes, Ordering::Relaxed);
        errors.fetch_add(done.errors, Ordering::Relaxed);
    }

    let n = removed.load(Ordering::Relaxed);
    let b = bytes.load(Ordering::Relaxed);
    if opts.json {
        // Whole days as before; an age in hours or minutes gives days as a
        // fraction, and older_than_secs gives it exactly.
        let days = match opts.days {
            Some(s) if s % 86_400 == 0 => (s / 86_400).to_string(),
            Some(s) => format!("{}", s as f64 / 86_400.0),
            None => "null".into(),
        };
        let older_secs = opts.days.map_or("null".into(), |s| s.to_string());
        let max_size = match opts.max_size {
            Some(m) => m.to_string(),
            None => "null".into(),
        };
        println!(
            "{{\"dry_run\":{},\"removed\":{},\"bytes\":{},\"days\":{},\"older_than_secs\":{},\"max_size\":{}}}",
            if opts.dry_run { "true" } else { "false" },
            n,
            b,
            days,
            older_secs,
            max_size
        );
    } else {
        let verb = if opts.dry_run {
            "Would remove"
        } else {
            "Removed"
        };
        let noun = if n == 1 { "item" } else { "items" };
        if opts.verbose || opts.dry_run || n > 0 {
            if opts.dry_run {
                // Always show reclaim estimate on dry-run (the main reason to use -n).
                eprintln!(
                    "{verb} {n} {noun} ({}, approximately reclaimable)",
                    crate::fastdelete::format_bytes(b)
                );
            } else if opts.max_size.is_some() {
                eprintln!("{verb} {n} {noun} ({})", crate::fastdelete::format_bytes(b));
            } else {
                eprintln!("{verb} {n} {noun}");
            }
        }
    }
    let failures = failures.into_inner().unwrap_or_else(|p| p.into_inner());
    eprint!("{}", failure_report(prog, &failures));
    if errors.load(Ordering::Relaxed) > 0 {
        1
    } else {
        0
    }
}

/// `--until-free`: on each file system that holds a trash in scope and has less
/// free than `floor`, remove the oldest trash items there (the Finder
/// Trash first where it is the default) until it is back at `floor`.
fn until_free(prog: &str, spec: &str, floor: crate::reap::Floor, opts: &Opts) -> i32 {
    // One path per file system, keyed as reap keys them.
    let mut roots: Vec<PathBuf> = trashdir::resolve_dirs_opts(&opts.trash_dirs, opts.home_only)
        .into_iter()
        .map(|d| d.root)
        .collect();
    #[cfg(unix)]
    if crate::finder::is_default(!opts.trash_dirs.is_empty()) {
        roots.extend(crate::finder::trashes());
    }
    crate::reap::free_until(
        prog,
        &roots,
        spec,
        floor,
        opts.dry_run,
        opts.verbose,
        opts.json,
    )
}

/// Older-than-cutoff check. An entry with a missing or unreadable
/// DeletionDate is no older than the cutoff as far as anyone can tell, so
/// an age cut keeps it; a full empty still removes it.
fn is_old(date: Option<&str>, cutoff: i64) -> bool {
    date.and_then(info::parse_local_to_epoch)
        .is_some_and(|t| t <= cutoff)
}

/// The end-of-run list of what stayed in the trash, one line per item;
/// empty when everything went.
fn failure_report(prog: &str, failures: &[(PathBuf, String)]) -> String {
    if failures.is_empty() {
        return String::new();
    }
    let mut out = format!(
        "{prog}: could not remove {}:\n",
        crate::util::plural(failures.len() as u64, "item", "items")
    );
    for (path, why) in failures {
        out.push_str(&format!("  {}: {why}\n", crate::util::err_path(path)));
    }
    out
}

/// Items that could not be removed: where, and why.
type Failures = std::sync::Mutex<Vec<(PathBuf, String)>>;

fn fail(failures: &Failures, errors: &AtomicU64, path: PathBuf, e: &io::Error) {
    errors.fetch_add(1, Ordering::Relaxed);
    failures
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .push((path, e.to_string()));
}

#[allow(clippy::too_many_arguments)]
fn empty_one(
    prog: &str,
    dir: &TrashDir,
    cutoff: Option<i64>,
    opts: &Opts,
    removed: &AtomicU64,
    bytes: &AtomicU64,
    errors: &AtomicU64,
    failures: &Failures,
) {
    let info_dir = dir.info();
    let files_dir = dir.files();
    if !opts.dry_run {
        crate::trash_total::touched(dir);
    }

    // Full empty (no age filter): real wipe, or dry-run that inventories the
    // same two roots (every child of files/ and info/), not only .trashinfo rows.
    if cutoff.is_none() {
        if opts.dry_run {
            full_empty_dry_run(dir, opts, removed, bytes);
        } else {
            let before = errors.load(Ordering::Relaxed);
            full_empty_wipe(prog, dir, opts, removed, errors, failures);
            // An empty that took everything leaves a known total; one that
            // failed somewhere leaves it stale for the next reader.
            if errors.load(Ordering::Relaxed) == before {
                crate::trash_total::store(dir, 0, 0);
            } else {
                crate::trash_total::forget(dir);
            }
        }
        return;
    }

    let mut kept_names: Vec<String> = Vec::new();
    let mut victims: Vec<(PathBuf, Option<PathBuf>, PathBuf)> = Vec::new(); // (target, info, display)

    if let Ok(entries) = fs::read_dir(&info_dir) {
        for entry in entries.flatten() {
            let info_path = entry.path();
            let fname = entry.file_name();
            let fname = crate::util::str_from_os_ref(&fname);
            let Some(name) = crate::util::trash_item_name(&fname) else {
                continue;
            };
            let take = match cutoff {
                None => true,
                Some(cut) => {
                    let parsed = info::read_bounded(&info_path, info::RECORD_CAP)
                        .map(|c| info::parse_bytes(&c))
                        .ok();
                    is_old(
                        parsed.as_ref().and_then(|i| i.deletion_date.as_deref()),
                        cut,
                    )
                }
            };
            if take {
                // -v names the original path, as trash-cli users asked.
                let shown = if opts.verbose {
                    original_of(dir, &info_path)
                } else {
                    None
                };
                victims.push((
                    crate::util::join_name(&files_dir, name),
                    Some(info_path),
                    shown.unwrap_or_else(|| crate::util::join_name(&dir.root.join("files"), name)),
                ));
            } else {
                kept_names.push(name.to_string());
            }
        }
    }

    // Orphans in files/ with no .trashinfo are stale debris; purge them on a
    // full empty only (a days filter has no date to compare against).
    if cutoff.is_none() {
        if let Ok(entries) = fs::read_dir(&files_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let mut info_name = name;
                info_name.push(".trashinfo");
                let has_info = info_dir.join(info_name).exists();
                if !has_info {
                    let p = entry.path();
                    victims.push((p.clone(), None, p));
                }
            }
        }
    }

    let sizes = trashdir::DirSizes::load(dir);
    // A total current before this pass loses what the pass takes.
    let total = (!opts.dry_run)
        .then(|| crate::trash_total::cached(dir))
        .flatten();
    let gone = AtomicU64::new(0);
    let gone_items = AtomicU64::new(0);
    victims.par_iter().for_each(|(target, info_path, display)| {
        if opts.dry_run {
            // Prefer FreeDesktop directorysizes for directory payloads; fall back
            // to a disk_usage walk. Always add the .trashinfo size when present.
            let name = target
                .file_name()
                .map(crate::util::str_from_os_ref)
                .unwrap_or_default();
            let sz = if info_path.is_some() && !name.is_empty() {
                sizes.entry_bytes(&name)
            } else {
                crate::fastdelete::disk_usage(target)
                    + info_path
                        .as_ref()
                        .map(|p| crate::fastdelete::disk_usage(p))
                        .unwrap_or(0)
            };
            bytes.fetch_add(sz, Ordering::Relaxed);
            if opts.verbose {
                println!(
                    "would remove {} ({})",
                    display.display(),
                    crate::fastdelete::format_bytes(sz)
                );
            }
            removed.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let sz = match (&total, info_path) {
            (Some(_), Some(_)) => target
                .file_name()
                .map_or(0, |n| sizes.entry_bytes(&crate::util::str_from_os_ref(n))),
            _ => 0,
        };
        match crate::permanent_guard::remove(target) {
            Ok(()) => {
                if info_path.is_some() {
                    gone.fetch_add(sz, Ordering::Relaxed);
                    gone_items.fetch_add(1, Ordering::Relaxed);
                }
                if let Some(ip) = info_path {
                    match fs::remove_file(ip) {
                        Ok(()) => {}
                        // A concurrent empty may have won the race.
                        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                        Err(e) => fail(failures, errors, ip.clone(), &e),
                    }
                }
                if opts.verbose {
                    println!("removed {}", crate::util::term_path(display));
                }
                removed.fetch_add(1, Ordering::Relaxed);
            }
            Err(e) => fail(failures, errors, display.clone(), &e),
        }
    });

    if opts.dry_run {
        sizes.write_back_locked();
    } else {
        prune_directorysizes(dir, &kept_names);
        if let Some((b, n)) = total {
            crate::trash_total::store(
                dir,
                b.saturating_sub(gone.into_inner()),
                n.saturating_sub(gone_items.into_inner()),
            );
        } else {
            crate::trash_total::forget(dir);
        }
    }
}

/// The original path a `.trashinfo` records, resolved against its trash.
fn original_of(dir: &TrashDir, info_path: &std::path::Path) -> Option<PathBuf> {
    let raw = info::read_bounded(info_path, info::RECORD_CAP).ok()?;
    let rec = info::parse_bytes(&raw);
    (!rec.path.as_os_str().is_empty()).then(|| dir.resolve_original(&rec.path))
}

/// Dry-run inventory matching full wipe: every top-level child of `files/` and
/// non-counted-as-item names under `info/` still contribute reclaim size; item
/// count is top-level `files/` children (same as live wipe `n_files`).
fn full_empty_dry_run(dir: &TrashDir, opts: &Opts, removed: &AtomicU64, bytes: &AtomicU64) {
    let files_dir = dir.files();
    let info_dir = dir.info();
    let mut n_files = 0u64;
    let mut total = 0u64;
    let sizes = trashdir::DirSizes::load(dir);
    if let Ok(entries) = fs::read_dir(&files_dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            // Directory payloads: use validated directorysizes when present.
            let sz = sizes.payload_bytes(&crate::util::str_from_os_ref(&entry.file_name()));
            total = total.saturating_add(sz);
            n_files += 1;
            if opts.verbose {
                let mut info = entry.file_name();
                info.push(".trashinfo");
                let shown = original_of(dir, &info_dir.join(info)).unwrap_or(p);
                println!(
                    "would remove {} ({})",
                    crate::util::term_path(&shown),
                    crate::fastdelete::format_bytes(sz)
                );
            }
        }
    }
    if let Ok(entries) = fs::read_dir(&info_dir) {
        for entry in entries.flatten() {
            total = total.saturating_add(crate::fastdelete::disk_usage(&entry.path()));
        }
    }
    sizes.write_back_locked();
    removed.fetch_add(n_files, Ordering::Relaxed);
    bytes.fetch_add(total, Ordering::Relaxed);
}

/// Full empty: wipe `files/` and `info/`. Skip expensive pre-scans unless
/// verbose (count comes from the wipe itself).
fn full_empty_wipe(
    prog: &str,
    dir: &TrashDir,
    opts: &Opts,
    removed: &AtomicU64,
    errors: &AtomicU64,
    failures: &Failures,
) {
    let info_dir = dir.info();
    let files_dir = dir.files();

    // Verbose still lists intended removals before delete (stable UX),
    // by original path where a record names one.
    if opts.verbose {
        if let Ok(entries) = fs::read_dir(&files_dir) {
            for entry in entries.flatten() {
                let mut info = entry.file_name();
                info.push(".trashinfo");
                let shown = original_of(dir, &info_dir.join(info)).unwrap_or_else(|| entry.path());
                println!("removed {}", crate::util::term_path(&shown));
            }
        }
    }

    // A payload that cannot go stays, with its .trashinfo; the rest of the
    // trash still empties, and the failures are reported at the end.
    if let Err(e) = crate::permanent_guard::gate() {
        eprintln!("{prog}: {e}");
        errors.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let (wf, wi) = crate::fastdelete::wipe_two_parallel(&files_dir, &info_dir);
    let mut kept: Vec<String> = Vec::new();
    match wf {
        Ok(w) => {
            // Top-level `files/` children ≈ trashed items (incl. orphans).
            removed.fetch_add(w.removed, Ordering::Relaxed);
            for (name, e) in w.failed {
                kept.push(crate::util::str_from_os_ref(&name));
                fail(failures, errors, files_dir.join(&name), &e);
            }
        }
        Err(e) => {
            eprintln!(
                "{prog}: cannot wipe trash under '{}': {e}",
                dir.root.display()
            );
            errors.fetch_add(1, Ordering::Relaxed);
            return;
        }
    }
    match wi {
        Ok(w) => {
            for (name, e) in w.failed {
                fail(failures, errors, info_dir.join(&name), &e);
            }
        }
        Err(e) => fail(failures, errors, info_dir.clone(), &e),
    }

    prune_directorysizes(dir, &kept);
}

/// Rewrite the spec's `directorysizes` cache keeping only surviving entries;
/// drop the file outright when nothing survives.
fn prune_directorysizes(dir: &TrashDir, kept: &[String]) {
    let path = dir.root.join("directorysizes");
    let Ok(content) = trashdir::read_sizes(&path) else {
        return;
    };
    if kept.is_empty() {
        let _ = fs::remove_file(&path);
        return;
    }
    let keep: std::collections::HashSet<&str> = kept.iter().map(|s| s.as_str()).collect();
    let filtered: String = content
        .lines()
        .filter(|line| {
            line.rsplit(' ').next().is_some_and(|enc| {
                let decoded = url_decode(enc);
                let name = crate::util::str_from_os_ref(std::ffi::OsStr::from_bytes(&decoded));
                keep.contains(name.as_str())
            })
        })
        .map(|l| format!("{l}\n"))
        .collect();
    let _ = trashdir::atomic_write(&path, filtered.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_report_lists_each_item_with_its_reason() {
        assert_eq!(failure_report("rtrash", &[]), "");
        let f = vec![
            (
                PathBuf::from("/t/files/a"),
                "/t/files/a/x: Permission denied".to_string(),
            ),
            (
                PathBuf::from("/t/files/b"),
                "Operation not permitted".to_string(),
            ),
        ];
        assert_eq!(
            failure_report("rtrash", &f),
            "rtrash: could not remove 2 items:\n  /t/files/a: /t/files/a/x: Permission denied\n  /t/files/b: Operation not permitted\n"
        );
    }

    #[test]
    fn missing_date_is_never_old() {
        // An unknown age never passes an age cut.
        assert!(!is_old(None, i64::MAX));
        assert!(!is_old(Some("garbage"), i64::MAX));
    }

    #[test]
    fn cutoff_comparison() {
        let now = info::now_local_string();
        assert!(!is_old(Some(&now), info::now_epoch() - 3600));
        assert!(is_old(
            Some("2001-01-01T00:00:00"),
            info::now_epoch() - 3600
        ));
    }
}
