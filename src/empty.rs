use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use rayon::prelude::*;

use crate::info;
#[cfg(feature = "tui")]
use crate::list;
use crate::trashdir::{self, TrashDir};
#[cfg(feature = "tui")]
use crate::util::stdin_is_tty;
use crate::util::url_decode;

const HELP: &str = "\
Usage: {prog} [OPTION]... [DAYS]
Purge trashed items. With DAYS, only items trashed more than DAYS days ago.
An item that cannot be removed stays; each one is listed at the end, with
the reason, and the exit status is 1.
On a TTY without DAYS, opens the interactive empty browser (multi-select
permanent delete). Use --plain to skip the TUI.

  -n, --dry-run    report what would be removed without removing anything;
                     also prints approximate reclaimable disk space (fast
                     in-process walk, like du of the victims)
  -v, --verbose    print each removed item
  -f, --force      accepted for trash-cli compatibility (emptying never prompts)
      --plain      skip TUI; non-interactive empty (also used when stdin is not a TTY)
      --older-than=DAYS  same as positional DAYS (list/status flag style)
      --max-size=SIZE  remove items until the trash (all roots in scope
                     together) fits in SIZE, such as 500M or 10G; with
                     DAYS, the age cut runs first
      --prune=oldest|largest  which items --max-size removes first
                     (default oldest)
      --json       emit a JSON summary on stdout ({dry_run, removed, bytes, days})
      --trash-dir=PATH  empty only this trash directory (repeatable);
                     default is the home trash plus every mounted volume
      --home-only  only the home trash (skip volume trash discovery)
      --help       display this help and exit
      --version    output version information and exit
";

struct Opts {
    days: Option<i64>,
    dry_run: bool,
    verbose: bool,
    home_only: bool,
    trash_dirs: Vec<PathBuf>,
    plain: bool,
    json: bool,
    max_size: Option<u64>,
    prune: crate::retention::Policy,
}

fn usage_err(prog: &str, msg: &str) -> i32 {
    eprintln!("{prog}: {msg}");
    eprintln!("Try '{prog} --help' for more information.");
    2
}

pub fn run(prog: &str, args: &[String]) -> i32 {
    let mut opts = Opts {
        days: None,
        dry_run: false,
        verbose: false,
        home_only: false,
        trash_dirs: Vec::new(),
        plain: false,
        json: false,
        max_size: None,
        prune: crate::retention::Policy::Oldest,
    };
    for arg in args {
        match arg.as_str() {
            "-n" | "--dry-run" => opts.dry_run = true,
            "-v" | "--verbose" => opts.verbose = true,
            "-f" | "--force" => {}
            "--plain" => opts.plain = true,
            "--json" => opts.json = true,
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
            "--max-size" | "--prune" => {
                return usage_err(prog, &format!("{arg} needs a value ({arg}=VALUE)"));
            }
            a if a.starts_with("--older-than=") => {
                match a["--older-than=".len()..].parse::<i64>() {
                    Ok(d) if d >= 0 => opts.days = Some(d),
                    Ok(d) => return usage_err(prog, &format!("DAYS must be >= 0 (got {d})")),
                    Err(_) => {
                        return usage_err(prog, &format!("invalid DAYS in '{a}'"));
                    }
                }
            }
            "--older-than" => {
                return usage_err(prog, "--older-than requires DAYS (use --older-than=DAYS)");
            }
            a if !a.starts_with('-') => match a.parse::<i64>() {
                Ok(d) if d >= 0 => opts.days = Some(d),
                _ => return usage_err(prog, &format!("invalid DAYS argument '{a}'")),
            },
            a => return usage_err(prog, &format!("unrecognized option '{a}'")),
        }
    }

    let dirs: Vec<TrashDir> = trashdir::resolve_dirs_opts(&opts.trash_dirs, opts.home_only);
    if dirs.is_empty() && !opts.trash_dirs.is_empty() {
        eprintln!(
            "{prog}: no valid --trash-dir pins (need non-symlink files/ and info/ directories)"
        );
        return 2;
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

    let cutoff = opts.days.map(|d| info::now_epoch() - d * 86_400);
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
        let days = match opts.days {
            Some(d) => d.to_string(),
            None => "null".into(),
        };
        let max_size = match opts.max_size {
            Some(m) => m.to_string(),
            None => "null".into(),
        };
        println!(
            "{{\"dry_run\":{},\"removed\":{},\"bytes\":{},\"days\":{},\"max_size\":{}}}",
            if opts.dry_run { "true" } else { "false" },
            n,
            b,
            days,
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
                victims.push((
                    crate::util::join_name(&files_dir, name),
                    Some(info_path),
                    crate::util::join_name(&dir.root.join("files"), name),
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
        match trashdir::remove_any_path(target) {
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
        }
    }
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
                println!(
                    "would remove {} ({})",
                    p.display(),
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

    // Verbose still lists intended removals before delete (stable UX).
    if opts.verbose {
        if let Ok(entries) = fs::read_dir(&files_dir) {
            for entry in entries.flatten() {
                println!("removed {}", crate::util::term_path(&entry.path()));
            }
        }
    }

    // A payload that cannot go stays, with its .trashinfo; the rest of the
    // trash still empties, and the failures are reported at the end.
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
