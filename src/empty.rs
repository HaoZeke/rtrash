use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use rayon::prelude::*;

use crate::info;
use crate::list;
use crate::trashdir::{self, TrashDir};
use crate::util::{stdin_is_tty, url_decode};

const HELP: &str = "\
Usage: {prog} [OPTION]... [DAYS]
Purge trashed items. With DAYS, only items trashed more than DAYS days ago.
Orphan files/ payloads (no .trashinfo) are always purged: they have no
DeletionDate, so an age filter cannot keep them.
On a TTY without DAYS, opens the interactive empty browser (multi-select
permanent delete). Use --plain to skip the TUI.

  -n, --dry-run    report what would be removed without removing anything;
                     also prints approximate reclaimable disk space (fast
                     in-process walk, like du of the victims)
  -v, --verbose    print each removed item
  -f, --force      accepted for trash-cli compatibility (emptying never prompts)
      --plain      skip TUI; non-interactive empty (also used when stdin is not a TTY)
      --older-than=DAYS  same as positional DAYS (list/status flag style)
      --max-bytes=N  drop largest items first until remaining reclaimable
                     bytes <= N (N, Nk/K, Nm/M, Ng/G; 1024-based).
                     Omit for a full empty (unchanged default)
      --json       emit a JSON summary on stdout
                     ({dry_run, removed, bytes, days, max_bytes})
      --trash-dir PATH  empty only this trash directory (repeatable;
                     `--trash-dir=PATH` also accepted);
                     default is the home trash plus every mounted volume
      --home-only  only the home trash (skip volume trash discovery)
      --help       display this help and exit
      --version    output version information and exit
";

struct Opts {
    days: Option<i64>,
    max_bytes: Option<u64>,
    dry_run: bool,
    verbose: bool,
    home_only: bool,
    trash_dirs: Vec<PathBuf>,
    plain: bool,
    json: bool,
}

fn usage_err(prog: &str, msg: &str) -> i32 {
    eprintln!("{prog}: {msg}");
    eprintln!("Try '{prog} --help' for more information.");
    2
}

pub fn run(prog: &str, args: &[String]) -> i32 {
    let mut opts = Opts {
        days: None,
        max_bytes: None,
        dry_run: false,
        verbose: false,
        home_only: false,
        trash_dirs: Vec::new(),
        plain: false,
        json: false,
    };
    let mut i = 0usize;
    while i < args.len() {
        let arg = &args[i];
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
            a if crate::util::is_trash_dir_flag(a) => {
                match crate::util::take_trash_dir_value(args, &mut i) {
                    Ok(p) => opts.trash_dirs.push(p),
                    Err(e) => return usage_err(prog, e),
                }
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
            a if a == "--max-bytes" || a.starts_with("--max-bytes=") => {
                match crate::util::take_eq_or_next(args, &mut i, "--max-bytes") {
                    Ok(v) => match crate::util::parse_bytes(v) {
                        Ok(n) => opts.max_bytes = Some(n),
                        Err(e) => return usage_err(prog, &e),
                    },
                    Err(e) => return usage_err(prog, &e),
                }
            }
            a if !a.starts_with('-') => match a.parse::<i64>() {
                Ok(d) if d >= 0 => opts.days = Some(d),
                _ => return usage_err(prog, &format!("invalid DAYS argument '{a}'")),
            },
            a => return usage_err(prog, &format!("unrecognized option '{a}'")),
        }
        i += 1;
    }

    let dirs: Vec<TrashDir> = trashdir::resolve_dirs_opts(&opts.trash_dirs, opts.home_only);
    if dirs.is_empty() && !opts.trash_dirs.is_empty() {
        eprintln!(
            "{prog}: no valid --trash-dir pins (need non-symlink files/ and info/ directories)"
        );
        return 2;
    }

    // TTY empty browser when no age/size filter (full interactive review).
    // --json / --max-bytes imply non-interactive (scripts).
    #[cfg(feature = "tui")]
    if !opts.plain
        && !opts.json
        && opts.days.is_none()
        && opts.max_bytes.is_none()
        && stdin_is_tty()
    {
        let entries = list::collect(&dirs);
        let refs: Vec<&list::Entry> = entries.iter().collect();
        return crate::empty_tui::run(prog, refs, opts.dry_run);
    }

    let cutoff = opts.days.map(|d| info::now_epoch() - d * 86_400);

    if let Some(cap) = opts.max_bytes {
        return empty_max_bytes(prog, &dirs, cutoff, cap, &opts);
    }

    let removed = AtomicU64::new(0);
    let bytes = AtomicU64::new(0);
    let errors = AtomicU64::new(0);

    for dir in &dirs {
        // Exclusive lock for this trash root so put cannot interleave a half pair.
        let _lock = match trashdir::TrashLock::acquire(&dir.root) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("{prog}: cannot lock trash '{}': {e}", dir.root.display());
                errors.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        };
        empty_one(prog, dir, cutoff, &opts, &removed, &bytes, &errors);
    }

    let n = removed.load(Ordering::Relaxed);
    let b = bytes.load(Ordering::Relaxed);
    if opts.json {
        let days = match opts.days {
            Some(d) => d.to_string(),
            None => "null".into(),
        };
        println!(
            "{{\"dry_run\":{},\"removed\":{},\"bytes\":{},\"days\":{},\"max_bytes\":{}}}",
            if opts.dry_run { "true" } else { "false" },
            n,
            b,
            days,
            "null"
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
            } else {
                eprintln!("{verb} {n} {noun}");
            }
        }
    }
    if errors.load(Ordering::Relaxed) > 0 {
        1
    } else {
        0
    }
}

/// Older-than-cutoff check; entries with a missing or unparsable
/// DeletionDate count as old (they are broken metadata, purge them).
fn is_old(date: Option<&str>, cutoff: i64) -> bool {
    match date.and_then(info::parse_local_to_epoch) {
        Some(t) => t <= cutoff,
        None => true,
    }
}

fn empty_one(
    prog: &str,
    dir: &TrashDir,
    cutoff: Option<i64>,
    opts: &Opts,
    removed: &AtomicU64,
    bytes: &AtomicU64,
    errors: &AtomicU64,
) {
    let info_dir = dir.info();
    let files_dir = dir.files();

    // Full empty (no age filter): real wipe, or dry-run that inventories the
    // same two roots (every child of files/ and info/), not only .trashinfo rows.
    if cutoff.is_none() {
        if opts.dry_run {
            full_empty_dry_run(dir, opts, removed, bytes);
        } else {
            full_empty_wipe(prog, dir, opts, removed, errors);
        }
        return;
    }

    let mut kept_names: Vec<String> = Vec::new();
    let mut victims: Vec<(PathBuf, Option<PathBuf>, PathBuf)> = Vec::new(); // (target, info, display)

    if let Ok(entries) = fs::read_dir(&info_dir) {
        for entry in entries.flatten() {
            let info_path = entry.path();
            let fname = entry.file_name();
            let fname = fname.to_string_lossy();
            let Some(name) = fname.strip_suffix(".trashinfo") else {
                continue;
            };
            let take = match cutoff {
                None => true,
                Some(cut) => {
                    let parsed = fs::read_to_string(&info_path).map(|c| info::parse(&c)).ok();
                    is_old(
                        parsed.as_ref().and_then(|i| i.deletion_date.as_deref()),
                        cut,
                    )
                }
            };
            if take {
                victims.push((
                    files_dir.join(name),
                    Some(info_path),
                    dir.root.join("files").join(name),
                ));
            } else {
                kept_names.push(name.to_string());
            }
        }
    }

    // Orphans in files/ with no .trashinfo are stale debris. They have no
    // DeletionDate, so any empty (including a days filter) purges them.
    if let Ok(entries) = fs::read_dir(&files_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let has_info = info_dir
                .join(format!("{}.trashinfo", name.to_string_lossy()))
                .exists();
            if !has_info {
                let p = entry.path();
                victims.push((p.clone(), None, p));
            }
        }
    }

    let cache = trashdir::DirectorySizesMap::load(dir);
    victims.par_iter().for_each(|(target, info_path, display)| {
        if opts.dry_run {
            // Prefer FreeDesktop directorysizes for directory payloads; fall back
            // to a disk_usage walk. Always add the .trashinfo size when present.
            let name = target
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let sz = if info_path.is_some() && !name.is_empty() {
                trashdir::entry_reclaim_bytes_cached(dir, &name, Some(&cache))
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
        match trashdir::remove_any_path(target) {
            Ok(()) => {
                if let Some(ip) = info_path {
                    match fs::remove_file(ip) {
                        Ok(()) => {}
                        // A concurrent empty may have won the race.
                        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                        Err(e) => {
                            eprintln!("{prog}: cannot remove '{}': {e}", ip.display());
                            errors.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
                if opts.verbose {
                    println!("removed {}", display.display());
                }
                removed.fetch_add(1, Ordering::Relaxed);
            }
            Err(e) => {
                eprintln!("{prog}: cannot remove '{}': {e}", display.display());
                errors.fetch_add(1, Ordering::Relaxed);
            }
        }
    });

    if !opts.dry_run {
        prune_directorysizes(dir, &kept_names);
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
    let cache = trashdir::DirectorySizesMap::load(dir);
    if let Ok(entries) = fs::read_dir(&files_dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            // Directory payloads: use validated directorysizes when present.
            let sz = trashdir::files_child_reclaim_bytes_cached(dir, &p, Some(&cache));
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
) {
    let info_dir = dir.info();
    let files_dir = dir.files();

    // Verbose still lists intended removals before delete (stable UX).
    if opts.verbose {
        if let Ok(entries) = fs::read_dir(&files_dir) {
            for entry in entries.flatten() {
                println!("removed {}", entry.path().display());
            }
        }
    }

    match crate::fastdelete::wipe_two_parallel(&files_dir, &info_dir) {
        Ok((n_files, _n_info)) => {
            // Top-level `files/` children ≈ trashed items (incl. orphans).
            removed.fetch_add(n_files, Ordering::Relaxed);
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

    // Full empty: nothing kept — drop directorysizes entirely.
    prune_directorysizes(dir, &[]);
    crate::catalog_cache::invalidate(dir);
}

struct SizedItem {
    dir: TrashDir,
    name: String,
    target: PathBuf,
    info: Option<PathBuf>,
    display: PathBuf,
    bytes: u64,
}

/// Indices to drop (largest first) so the remaining sum is `<= cap`.
fn prune_indices(sizes: &[u64], cap: u64) -> Vec<usize> {
    let total: u64 = sizes.iter().copied().sum();
    if total <= cap {
        return Vec::new();
    }
    let mut order: Vec<usize> = (0..sizes.len()).collect();
    order.sort_by(|&a, &b| sizes[b].cmp(&sizes[a]).then(a.cmp(&b)));
    let mut remaining = total;
    let mut drop = Vec::new();
    for i in order {
        if remaining <= cap {
            break;
        }
        remaining = remaining.saturating_sub(sizes[i]);
        drop.push(i);
    }
    drop
}

fn collect_sized_items(dirs: &[TrashDir], cutoff: Option<i64>) -> Vec<SizedItem> {
    let mut items = Vec::new();
    for dir in dirs {
        let cache = trashdir::DirectorySizesMap::load(dir);
        let entries = list::collect(std::slice::from_ref(dir));
        let mut named: std::collections::HashSet<String> = std::collections::HashSet::new();
        for e in entries {
            if let Some(cut) = cutoff {
                if !is_old(e.date.as_deref(), cut) {
                    continue;
                }
            }
            named.insert(e.name.clone());
            let sz = trashdir::entry_reclaim_bytes_cached(dir, &e.name, Some(&cache));
            items.push(SizedItem {
                dir: dir.clone(),
                name: e.name.clone(),
                target: dir.files().join(&e.name),
                info: Some(dir.info().join(format!("{}.trashinfo", e.name))),
                display: dir.root.join("files").join(&e.name),
                bytes: sz,
            });
        }
        // Orphans have no date; include them only on an unfiltered prune.
        if cutoff.is_none() {
            if let Ok(rd) = fs::read_dir(dir.files()) {
                for ent in rd.flatten() {
                    let name = ent.file_name().to_string_lossy().into_owned();
                    if named.contains(&name) {
                        continue;
                    }
                    let has_info = dir.info().join(format!("{name}.trashinfo")).exists();
                    if has_info {
                        continue;
                    }
                    let p = ent.path();
                    let sz = trashdir::files_child_reclaim_bytes_cached(dir, &p, Some(&cache));
                    items.push(SizedItem {
                        dir: dir.clone(),
                        name,
                        target: p.clone(),
                        info: None,
                        display: p,
                        bytes: sz,
                    });
                }
            }
        }
    }
    items
}

fn empty_max_bytes(
    prog: &str,
    dirs: &[TrashDir],
    cutoff: Option<i64>,
    cap: u64,
    opts: &Opts,
) -> i32 {
    let items = collect_sized_items(dirs, cutoff);
    let sizes: Vec<u64> = items.iter().map(|i| i.bytes).collect();
    let drop_at = prune_indices(&sizes, cap);
    let victims: Vec<&SizedItem> = drop_at.iter().map(|&i| &items[i]).collect();

    let removed = AtomicU64::new(0);
    let bytes = AtomicU64::new(0);
    let errors = AtomicU64::new(0);

    if opts.dry_run {
        for v in &victims {
            bytes.fetch_add(v.bytes, Ordering::Relaxed);
            if opts.verbose {
                println!(
                    "would remove {} ({})",
                    v.display.display(),
                    crate::fastdelete::format_bytes(v.bytes)
                );
            }
            removed.fetch_add(1, Ordering::Relaxed);
        }
    } else {
        // Group victim names per trash root so we lock once and prune the cache.
        let mut by_root: std::collections::HashMap<PathBuf, Vec<&SizedItem>> =
            std::collections::HashMap::new();
        for v in &victims {
            by_root.entry(v.dir.root.clone()).or_default().push(*v);
        }
        let all_names_by_root = {
            let mut m: std::collections::HashMap<PathBuf, Vec<String>> =
                std::collections::HashMap::new();
            for it in &items {
                m.entry(it.dir.root.clone())
                    .or_default()
                    .push(it.name.clone());
            }
            m
        };
        for dir in dirs {
            let Some(dir_victims) = by_root.get(&dir.root) else {
                continue;
            };
            let _lock = match trashdir::TrashLock::acquire(&dir.root) {
                Ok(l) => l,
                Err(e) => {
                    eprintln!("{prog}: cannot lock trash '{}': {e}", dir.root.display());
                    errors.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
            };
            dir_victims.par_iter().for_each(|v| {
                match trashdir::remove_any_path(&v.target) {
                    Ok(()) => {
                        if let Some(ip) = &v.info {
                            match fs::remove_file(ip) {
                                Ok(()) => {}
                                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                                Err(e) => {
                                    eprintln!("{prog}: cannot remove '{}': {e}", ip.display());
                                    errors.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                        }
                        if opts.verbose {
                            println!("removed {}", v.display.display());
                        }
                        removed.fetch_add(1, Ordering::Relaxed);
                        bytes.fetch_add(v.bytes, Ordering::Relaxed);
                    }
                    Err(e) => {
                        eprintln!("{prog}: cannot remove '{}': {e}", v.display.display());
                        errors.fetch_add(1, Ordering::Relaxed);
                    }
                }
            });
            let drop_names: std::collections::HashSet<&str> =
                dir_victims.iter().map(|v| v.name.as_str()).collect();
            let kept: Vec<String> = all_names_by_root
                .get(&dir.root)
                .into_iter()
                .flatten()
                .filter(|n| !drop_names.contains(n.as_str()))
                .cloned()
                .collect();
            prune_directorysizes(dir, &kept);
        }
    }

    let n = removed.load(Ordering::Relaxed);
    let b = bytes.load(Ordering::Relaxed);
    if opts.json {
        let days = match opts.days {
            Some(d) => d.to_string(),
            None => "null".into(),
        };
        println!(
            "{{\"dry_run\":{},\"removed\":{},\"bytes\":{},\"days\":{},\"max_bytes\":{}}}",
            if opts.dry_run { "true" } else { "false" },
            n,
            b,
            days,
            cap
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
                eprintln!(
                    "{verb} {n} {noun} ({}, approximately reclaimable)",
                    crate::fastdelete::format_bytes(b)
                );
            } else {
                eprintln!("{verb} {n} {noun}");
            }
        }
    }
    if errors.load(Ordering::Relaxed) > 0 {
        1
    } else {
        0
    }
}

/// Rewrite the spec's `directorysizes` cache keeping only surviving entries;
/// drop the file outright when nothing survives.
fn prune_directorysizes(dir: &TrashDir, kept: &[String]) {
    let path = dir.root.join("directorysizes");
    let Ok(content) = fs::read_to_string(&path) else {
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
                keep.contains(String::from_utf8_lossy(&decoded).as_ref())
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
    fn missing_date_counts_as_old() {
        assert!(is_old(None, 0));
        assert!(is_old(Some("garbage"), 0));
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

    #[test]
    fn prune_indices_drops_largest_first() {
        assert!(prune_indices(&[100, 20, 10], 200).is_empty());
        assert_eq!(prune_indices(&[100, 20, 10], 30), vec![0]);
        assert_eq!(prune_indices(&[100, 20, 10], 0), vec![0, 1, 2]);
        // Ties: lower original index first among equal sizes.
        assert_eq!(prune_indices(&[10, 50, 50], 10), vec![1, 2]);
    }
}
