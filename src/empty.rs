use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use rayon::prelude::*;

use crate::info;
use crate::trashdir::{self, TrashDir};
use crate::util::url_decode;

const HELP: &str = "\
Usage: {prog} [OPTION]... [DAYS]
Purge trashed items. With DAYS, only items trashed more than DAYS days ago.

  -n, --dry-run    report what would be removed without removing anything
  -v, --verbose    print each removed item
  -f, --force      accepted for trash-cli compatibility (emptying never prompts)
      --trash-dir=PATH  empty only this trash directory (repeatable);
                     default is the home trash plus every mounted volume
      --help       display this help and exit
      --version    output version information and exit
";

struct Opts {
    days: Option<i64>,
    dry_run: bool,
    verbose: bool,
    trash_dirs: Vec<PathBuf>,
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
        trash_dirs: Vec::new(),
    };
    for arg in args {
        match arg.as_str() {
            "-n" | "--dry-run" => opts.dry_run = true,
            "-v" | "--verbose" => opts.verbose = true,
            "-f" | "--force" => {}
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
                    .push(PathBuf::from(&a["--trash-dir=".len()..]));
            }
            a if !a.starts_with('-') => match a.parse::<i64>() {
                Ok(d) if d >= 0 => opts.days = Some(d),
                _ => return usage_err(prog, &format!("invalid DAYS argument '{a}'")),
            },
            a => return usage_err(prog, &format!("unrecognized option '{a}'")),
        }
    }

    let dirs: Vec<TrashDir> = if opts.trash_dirs.is_empty() {
        trashdir::all()
    } else {
        opts.trash_dirs
            .iter()
            .map(|p| TrashDir {
                root: p.clone(),
                topdir: None,
            })
            .collect()
    };

    let cutoff = opts.days.map(|d| info::now_epoch() - d * 86_400);
    let removed = AtomicU64::new(0);
    let errors = AtomicU64::new(0);

    for dir in &dirs {
        empty_one(prog, dir, cutoff, &opts, &removed, &errors);
    }

    let n = removed.load(Ordering::Relaxed);
    let verb = if opts.dry_run { "Would remove" } else { "Removed" };
    let noun = if n == 1 { "item" } else { "items" };
    if opts.verbose || opts.dry_run || n > 0 {
        eprintln!("{verb} {n} {noun}");
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
    errors: &AtomicU64,
) {
    let info_dir = dir.info();
    let files_dir = dir.files();

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
                    let parsed = fs::read_to_string(&info_path)
                        .map(|c| info::parse(&c))
                        .ok();
                    is_old(parsed.as_ref().and_then(|i| i.deletion_date.as_deref()), cut)
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

    // Orphans in files/ with no .trashinfo are stale debris; purge them on a
    // full empty only (a days filter has no date to compare against).
    if cutoff.is_none() {
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
    }

    victims.par_iter().for_each(|(target, info_path, display)| {
        if opts.dry_run {
            if opts.verbose {
                println!("would remove {}", display.display());
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
    let _ = fs::write(&path, filtered);
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
        assert!(is_old(Some("2001-01-01T00:00:00"), info::now_epoch() - 3600));
    }
}
