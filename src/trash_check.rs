//! `rtrash status --check`: an integrity check of FreeDesktop trashes,
//! with `--fix` to repair what can be repaired without touching a payload.
//!
//! Each `.trashinfo` in `info/` is read once and sorted into one problem
//! or none. Records that cannot serve a restore (unreadable, over the
//! record cap, no `Path=`, or a volume trash's `Path=` that climbs out
//! of its volume or names a path outside it) and records whose payload
//! is gone are what `--fix` removes; a
//! payload a bad record named stays, as an orphan that list and rm show.
//! A missing or unreadable `DeletionDate=`, a payload with no record, and
//! a name in `info/` that is not `NAME.trashinfo` are reported only: each
//! still lists, and the user decides. Stale `directorysizes` lines need no
//! check here, as every status run drops them (`DirSizes::write_back`).
//!
//! Each name is sorted by its file type before its body is read: only a
//! regular file can be a record, so a folder, link or other file named
//! `NAME.trashinfo` is stray, kept, and never reaches `--fix`. The removal
//! itself, [`crate::permanent_guard::remove_record`], refuses anything but
//! a regular file too, so a classifier bug cannot hand it a tree.
//!
//! `--fix` removes records through [`crate::permanent_guard::remove_record`], so
//! `RTRASH_NO_PERMANENT` refuses it and `RTRASH_MAX_PERMANENT` caps it,
//! counted before the first removal. With `RTRASH_ALLOW_ROOTS` set, a
//! record whose payload is gone must name a path under a root, and a bad
//! record, whose origin is unknown, is refused: the run removes nothing.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::trashdir::{self, TrashDir};

/// Why a stray name is not a record.
const NOT_TRASHINFO: &str = "not a .trashinfo";
const NOT_REGULAR: &str = "not a regular file, so not a record";

/// One finding in one trash.
#[derive(Debug, PartialEq, Eq)]
pub enum Problem {
    /// A record whose payload is gone; the original path it names.
    Dangling { record: PathBuf, origin: PathBuf },
    /// A record that cannot serve a restore, and why.
    BadRecord { record: PathBuf, why: String },
    /// A record with no readable `DeletionDate=`.
    BadDate { record: PathBuf },
    /// A payload in `files/` with no record.
    Orphan { payload: PathBuf },
    /// A name in `info/` that is not a record: not `NAME.trashinfo`, or
    /// not a regular file; `what` says which.
    Stray { path: PathBuf, what: &'static str },
}

impl Problem {
    /// The record `--fix` removes, when it removes one.
    fn fix_target(&self) -> Option<&PathBuf> {
        match self {
            Problem::Dangling { record, .. } | Problem::BadRecord { record, .. } => Some(record),
            _ => None,
        }
    }

    /// The report line, the same with and without `--fix`.
    fn line(&self) -> String {
        let t = crate::util::term_path;
        match self {
            Problem::Dangling { record, .. } => {
                format!("dangling: {} (the payload is gone)", t(record))
            }
            Problem::BadRecord { record, why } => format!("bad record: {}: {why}", t(record)),
            Problem::BadDate { record } => format!(
                "bad date: {} (kept; age filters take it as oldest)",
                t(record)
            ),
            Problem::Orphan { payload } => {
                format!("orphan: {} (kept; rm by its name deletes it)", t(payload))
            }
            Problem::Stray { path, what } => format!("stray: {} (kept; {what})", t(path)),
        }
    }
}

/// The original path the regular-file record `record` in `dir` names,
/// with the record as parsed, or why it cannot serve a restore: it cannot
/// be read, has no `Path=`, or its `Path=` climbs out of its volume. The
/// same reasons make a record bad in [`scan`], and an `empty --dangling`
/// under `RTRASH_ALLOW_ROOTS` takes an error as an unknown origin.
pub fn record_origin(
    dir: &TrashDir,
    record: &Path,
) -> Result<(PathBuf, crate::info::TrashInfo), String> {
    let body = crate::info::read_bounded(record, crate::info::RECORD_CAP)
        .map_err(|e| format!("cannot be read ({e})"))?;
    let parsed = crate::info::parse_bytes(&body);
    if parsed.path.as_os_str().is_empty() {
        return Err("no Path line".to_string());
    }
    let origin = dir
        .resolve_original_checked(&parsed.path)
        .map_err(|e| e.to_string())?;
    Ok((origin, parsed))
}

/// The problems in `dir`: records in name order, then orphans.
pub fn scan(dir: &TrashDir) -> Vec<Problem> {
    let mut names: Vec<std::ffi::OsString> = std::fs::read_dir(dir.info())
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name())
        .collect();
    names.sort();
    let mut out = Vec::new();
    for fname in names {
        let record = dir.info().join(&fname);
        let stem = fname
            .as_bytes()
            .strip_suffix(b".trashinfo")
            .filter(|s| !s.is_empty());
        let Some(stem) = stem else {
            out.push(Problem::Stray {
                path: record,
                what: NOT_TRASHINFO,
            });
            continue;
        };
        // By file type first, the link itself not followed: only a
        // regular file is a record, and only a record can be removed.
        match record.symlink_metadata() {
            Ok(m) if m.file_type().is_file() => {}
            Ok(_) => {
                out.push(Problem::Stray {
                    path: record,
                    what: NOT_REGULAR,
                });
                continue;
            }
            Err(e) => {
                let why = format!("cannot be read ({e})");
                out.push(Problem::BadRecord { record, why });
                continue;
            }
        }
        let payload = dir.files().join(OsStr::from_bytes(stem));
        let has_payload = payload.symlink_metadata().is_ok();
        let (origin, parsed) = match record_origin(dir, &record) {
            Ok(o) => o,
            Err(why) => {
                out.push(Problem::BadRecord { record, why });
                continue;
            }
        };
        if !has_payload {
            out.push(Problem::Dangling { record, origin });
            continue;
        }
        let dated = parsed
            .deletion_date
            .as_deref()
            .and_then(crate::info::parse_local_to_epoch)
            .is_some();
        if !dated {
            out.push(Problem::BadDate { record });
        }
    }
    for o in crate::list::orphans(std::slice::from_ref(dir)) {
        out.push(Problem::Orphan {
            payload: o.original,
        });
    }
    out
}

/// `status --check`, and `--fix` with or without `-n`, over `dirs`.
/// Exit 0 when a check finds nothing or `--fix` removed every record it
/// took on; 1 when a check or a preview finds problems, or a removal
/// failed; 2 when a guard refuses the fix.
pub fn run(prog: &str, dirs: &[TrashDir], fix: bool, dry_run: bool) -> i32 {
    let found: Vec<(&TrashDir, Vec<Problem>)> = dirs.iter().map(|d| (d, scan(d))).collect();
    let all = || found.iter().flat_map(|(_, ps)| ps.iter());
    let fixable = all().filter(|p| p.fix_target().is_some()).count() as u64;
    let total = all().count() as u64;
    let acting = fix && !dry_run;
    if acting && fixable > 0 {
        let scoped: Vec<(PathBuf, Option<PathBuf>)> = all()
            .filter_map(|p| match p {
                Problem::Dangling { record, origin } => {
                    Some((record.clone(), Some(origin.clone())))
                }
                Problem::BadRecord { record, .. } => Some((record.clone(), None)),
                _ => None,
            })
            .collect();
        if let Some(code) = crate::allow_roots::refuse_outside_or_unplaced(prog, &scoped) {
            return code;
        }
        if let Some(code) = crate::permanent_guard::check_count(prog, fixable) {
            return code;
        }
    }
    let mut removed = 0u64;
    let mut failed = 0u64;
    for (dir, problems) in &found {
        let _lock = match (acting && problems.iter().any(|p| p.fix_target().is_some()))
            .then(|| trashdir::TrashLock::acquire(&dir.root))
        {
            Some(Err(e)) => {
                eprintln!("{prog}: cannot lock trash '{}': {e}", dir.root.display());
                failed += 1;
                continue;
            }
            l => l,
        };
        for p in problems {
            println!("{}", p.line());
            let Some(record) = p.fix_target() else {
                continue;
            };
            if !fix {
                continue;
            }
            if dry_run {
                println!("would remove {}", crate::util::term_path(record));
                continue;
            }
            match crate::permanent_guard::remove_record(record) {
                Ok(()) => {
                    println!("removed {}", crate::util::term_path(record));
                    crate::trash_total::forget(dir);
                    crate::trash_total::touched(dir);
                    removed += 1;
                }
                Err(e) => {
                    eprintln!("{prog}: cannot remove '{}': {e}", record.display());
                    failed += 1;
                }
            }
        }
    }
    let plural = crate::util::plural;
    if acting {
        eprintln!(
            "{prog}: removed {}; {} left to review",
            plural(removed, "record", "records"),
            plural(total - fixable, "problem", "problems")
        );
    } else {
        eprintln!(
            "{prog}: {} found, {} --fix would remove",
            plural(total, "problem", "problems"),
            plural(fixable, "record", "records")
        );
    }
    if failed > 0 || (!acting && total > 0) {
        1
    } else {
        0
    }
}
