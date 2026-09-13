use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use crate::list;
use crate::trashdir;
use crate::util::stdin_is_tty;

const HELP: &str = "\
Usage: {prog} [OPTION]... [PATH]...
Restore trashed items to their original location (or --restore-to DIR).

With PATH (repeatable), restore items originally at those paths (exact
original-path match). Several PATH operands restore each unique match.
Without PATH on a TTY, open the interactive restore browser (ratatui): filter,
navigate, restore one or more items. Without a TTY (or with a piped index),
print a numbered table and read a selection from stdin. Numbered pick accepts
trash-cli indexes: `0-2, 3`.

By default every trashed item in scope is listed; pass --cwd-only to limit to
items whose original path is under the current directory. A single match
restores immediately. --if-unique makes any other count an error (exit 1)
instead of printing a numbered table or reading stdin.

  -f, --force, --overwrite  overwrite an existing file at the destination
      --all       restore every item in the current scope
      --group     restore every item sharing the DeletionDate second of the
                     newest match (or of PATH, if one PATH is given)
      --restore-to DIR  relocate payloads into DIR (keep basename);
                     `--restore-to=DIR` also accepted
      --cwd-only  without PATH, only items trashed from under the current directory
      --plain     force numbered line selection even on a TTY (no TUI)
      --if-unique  restore only when exactly one item matches; otherwise
                     exit 1 without prompting
      --home-only  only the home trash (skip volume trash)
      --trash-dir PATH  only consider this trash directory (repeatable;
                     `--trash-dir=PATH` also accepted)
      --help      display this help and exit
      --version   output version information and exit
";

pub fn run(prog: &str, args: &[String]) -> i32 {
    let mut force = false;
    let mut home_only = false;
    let mut cwd_only = false;
    let mut plain = false;
    let mut if_unique = false;
    let mut restore_all = false;
    let mut restore_group = false;
    let mut restore_to: Option<PathBuf> = None;
    let mut targets: Vec<PathBuf> = Vec::new();
    let mut trash_dirs: Vec<PathBuf> = Vec::new();
    let mut i = 0usize;
    while i < args.len() {
        let arg = &args[i];
        match arg.as_str() {
            "-f" | "--force" | "--overwrite" => force = true,
            "--home-only" => home_only = true,
            "--cwd-only" => cwd_only = true,
            "--plain" => plain = true,
            "--if-unique" => if_unique = true,
            "--all" => restore_all = true,
            "--group" => restore_group = true,
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
                    Ok(p) => trash_dirs.push(p),
                    Err(e) => {
                        eprintln!("{prog}: {e}");
                        return 2;
                    }
                }
            }
            a if is_restore_to_flag(a) => match take_restore_to_value(args, &mut i) {
                Ok(p) => restore_to = Some(p),
                Err(e) => {
                    eprintln!("{prog}: {e}");
                    return 2;
                }
            },
            a if a.starts_with('-') && a.len() > 1 => {
                eprintln!("{prog}: unrecognized option '{a}'");
                return 2;
            }
            a => targets.push(PathBuf::from(a)),
        }
        i += 1;
    }

    let restore_to = restore_to.map(|p| absolutize(&p));

    let dirs = trashdir::resolve_dirs_opts(&trash_dirs, home_only);
    if dirs.is_empty() && !trash_dirs.is_empty() {
        eprintln!("{prog}: no valid --trash-dir pins (need files/ and info/ directories)");
        return 2;
    }
    let entries = list::collect(&dirs);
    // Orphans have no Path=; never offer them as restore targets.
    let restorable: Vec<&list::Entry> = entries.iter().filter(|e| !e.orphan).collect();
    let scoped: Vec<&list::Entry> = if cwd_only {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        restorable
            .iter()
            .copied()
            .filter(|e| e.original.starts_with(&cwd))
            .collect()
    } else {
        restorable
    };

    let mut matches: Vec<&list::Entry> = if restore_all {
        scoped.clone()
    } else if !targets.is_empty() {
        let abs: Vec<PathBuf> = targets.iter().map(|t| absolutize(t)).collect();
        for (raw, want) in targets.iter().zip(abs.iter()) {
            if !scoped.iter().any(|e| e.original == *want) {
                eprintln!("{prog}: no trashed files match '{}'", raw.display());
                return 1;
            }
        }
        scoped
            .iter()
            .copied()
            .filter(|e| abs.iter().any(|t| e.original == *t))
            .collect()
    } else {
        scoped.clone()
    };

    if restore_group && !restore_all {
        let Some(newest) = matches.iter().max_by(|a, b| {
            a.epoch
                .cmp(&b.epoch)
                .then_with(|| a.original.cmp(&b.original))
        }) else {
            eprintln!("{prog}: no trashed files match");
            return 1;
        };
        let epoch = newest.epoch;
        matches = scoped
            .iter()
            .copied()
            .filter(|e| e.epoch == epoch)
            .collect();
    }

    if matches.is_empty() {
        eprintln!("{prog}: no trashed files match");
        return 1;
    }

    if if_unique && matches.len() != 1 {
        eprintln!(
            "{prog}: {} matches; --if-unique requires exactly one",
            matches.len()
        );
        return 1;
    }

    let batch = restore_all || restore_group || targets.len() > 1;

    // TTY + multi-item: prefer ratatui browser when built with `tui` (unless --plain).
    #[cfg(feature = "tui")]
    if !plain && !batch && targets.is_empty() && matches.len() > 1 && stdin_is_tty() {
        return crate::restore_tui::run(prog, matches, force, restore_to);
    }
    #[cfg(not(feature = "tui"))]
    let _ = plain;

    if matches.len() == 1 || batch {
        return restore_entries(prog, &matches, force, restore_to.as_deref());
    }

    for (i, e) in matches.iter().enumerate() {
        let date = e.date.as_deref().unwrap_or("????-??-??T??:??:??");
        println!(
            "{i:4}  {}  {}",
            date.replacen('T', " ", 1),
            e.original.display()
        );
    }
    // Piped index for scripts / tests: `printf '0\n' | rtrash restore`.
    // trash-cli ranges: `printf '0-1\n' | rtrash restore --plain`.
    if stdin_is_tty() {
        eprint!("What file to restore [0..{}]: ", matches.len() - 1);
        io::stderr().flush().ok();
    }
    let mut line = String::new();
    if io::stdin().lock().read_line(&mut line).is_err() {
        eprintln!("{prog}: failed to read selection from stdin");
        return 1;
    }
    if line.trim().is_empty() {
        eprintln!(
            "{prog}: {} matches; provide an index 0..{} on stdin",
            matches.len(),
            matches.len() - 1
        );
        return 1;
    }
    let idxs = match parse_restore_indexes(line.trim(), matches.len()) {
        Ok(v) => v,
        Err(_) => {
            eprintln!("{prog}: invalid selection");
            return 1;
        }
    };
    let chosen: Vec<&list::Entry> = idxs.into_iter().map(|i| matches[i]).collect();
    restore_entries(prog, &chosen, force, restore_to.as_deref())
}

fn is_restore_to_flag(arg: &str) -> bool {
    arg == "--restore-to" || arg.starts_with("--restore-to=")
}

fn take_restore_to_value(args: &[String], i: &mut usize) -> Result<PathBuf, &'static str> {
    let arg = args
        .get(*i)
        .map(String::as_str)
        .ok_or("--restore-to requires DIR")?;
    if let Some(rest) = arg.strip_prefix("--restore-to=") {
        if rest.is_empty() {
            return Err("--restore-to requires DIR");
        }
        return Ok(PathBuf::from(rest));
    }
    if arg == "--restore-to" {
        *i += 1;
        return args
            .get(*i)
            .filter(|s| !s.is_empty())
            .map(|s| PathBuf::from(s))
            .ok_or("--restore-to requires DIR");
    }
    Err("--restore-to requires DIR")
}

/// trash-cli numbered pick: `0`, `0-2`, `0-2, 3`.
fn parse_restore_indexes(input: &str, n: usize) -> Result<Vec<usize>, ()> {
    if n == 0 {
        return Err(());
    }
    let mut out = Vec::new();
    for raw in input.split(',') {
        let part = raw.trim();
        if part.is_empty() {
            return Err(());
        }
        if let Some((a, b)) = part.split_once('-') {
            if a.trim().is_empty() || b.trim().is_empty() {
                return Err(());
            }
            let start: isize = a.trim().parse().map_err(|_| ())?;
            let stop: isize = b.trim().parse().map_err(|_| ())?;
            if start < 0 || stop < 0 {
                return Err(());
            }
            if start > stop {
                return Err(());
            }
            for i in start as usize..=stop as usize {
                if i >= n {
                    return Err(());
                }
                out.push(i);
            }
        } else {
            let i: usize = part.parse().map_err(|_| ())?;
            if i >= n {
                return Err(());
            }
            out.push(i);
        }
    }
    let mut seen = vec![false; n];
    out.retain(|&i| {
        if seen[i] {
            false
        } else {
            seen[i] = true;
            true
        }
    });
    if out.is_empty() {
        Err(())
    } else {
        Ok(out)
    }
}

/// Restore each chosen catalog entry. Shared by the CLI, TUI, and HUD.
pub fn restore_entries(
    prog: &str,
    chosen: &[&list::Entry],
    force: bool,
    restore_to: Option<&Path>,
) -> i32 {
    let mut rc = 0;
    let mut roots = Vec::new();
    for e in chosen {
        let code = restore_entry(prog, e, force, restore_to);
        if code != 0 {
            rc = code;
        }
        if !roots.iter().any(|d: &crate::trashdir::TrashDir| d.root == e.dir.root) {
            roots.push(e.dir.clone());
        }
    }
    for d in roots {
        crate::catalog_cache::invalidate(&d);
    }
    rc
}

fn absolutize(p: &Path) -> PathBuf {
    if p.is_absolute() {
        lexical_clean(p)
    } else {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        lexical_clean(&cwd.join(p))
    }
}

/// Lexically drop `.` and resolve `..` so user-typed paths match recorded ones.
fn lexical_clean(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
}

/// Destination path for a restore: original location, or `DIR/basename`.
pub(crate) fn dest_path(entry: &list::Entry, restore_to: Option<&Path>) -> PathBuf {
    match restore_to {
        Some(dir) => {
            let base = entry
                .original
                .file_name()
                .unwrap_or_else(|| std::ffi::OsStr::new(entry.name.as_str()));
            dir.join(base)
        }
        None => entry.original.clone(),
    }
}

/// Restore one entry to its original path (or `--restore-to`). Shared by CLI,
/// piped pick, TUI, and HUD.
pub fn restore_entry(
    prog: &str,
    entry: &list::Entry,
    force: bool,
    restore_to: Option<&Path>,
) -> i32 {
    if entry.orphan {
        eprintln!(
            "{prog}: cannot restore orphan '{}' (no .trashinfo; no original Path=)",
            entry.name
        );
        return 1;
    }
    let _lock = match trashdir::TrashLock::acquire(&entry.dir.root) {
        Ok(l) => l,
        Err(e) => {
            eprintln!(
                "{prog}: cannot lock trash '{}': {e}",
                entry.dir.root.display()
            );
            return 1;
        }
    };
    let src = entry.dir.files().join(&entry.name);
    let dest = dest_path(entry, restore_to);

    // Never destroy the live destination until the trash payload is known good.
    if src.symlink_metadata().is_err() {
        eprintln!(
            "{prog}: trash payload missing for '{}' (stale .trashinfo?)",
            entry.name
        );
        return 1;
    }

    let dest_exists = dest.symlink_metadata().is_ok();
    if dest_exists && !force {
        eprintln!(
            "{prog}: refusing to overwrite existing '{}' (use -f to force)",
            dest.display()
        );
        return 1;
    }

    let parent = match dest.parent() {
        Some(p) => p,
        None => {
            eprintln!("{prog}: cannot restore to '{}': no parent", dest.display());
            return 1;
        }
    };
    if let Err(e) = fs::create_dir_all(parent) {
        eprintln!("{prog}: cannot create '{}': {e}", parent.display());
        return 1;
    }

    // Stage into the destination directory first, then swap over any blocker.
    // On failure the live dest (if any) is left intact.
    let staged = parent.join(format!(
        ".rtrash-restore-{}-{}",
        std::process::id(),
        entry.name
    ));
    let _ = trashdir::remove_any_path(&staged);
    if let Err(e) = trashdir::relocate(&src, &staged) {
        eprintln!(
            "{prog}: cannot restore '{}' to staging area: {e}",
            src.display()
        );
        let _ = trashdir::remove_any_path(&staged);
        return 1;
    }

    if dest_exists {
        // Payload is durable under `staged`; only now remove the blocker.
        if let Err(e) = trashdir::remove_any_path(&dest) {
            eprintln!("{prog}: cannot remove existing '{}': {e}", dest.display());
            // Best-effort: put payload back into the trash name.
            let _ = trashdir::relocate(&staged, &src);
            return 1;
        }
    }

    if let Err(e) = fs::rename(&staged, &dest) {
        // Same-FS rename of staged→dest; EXDEV should not happen (same parent).
        eprintln!(
            "{prog}: cannot place restored file at '{}': {e}",
            dest.display()
        );
        // Leave staged for recovery; try not to drop payload.
        return 1;
    }

    let info_path = entry.dir.info().join(format!("{}.trashinfo", entry.name));
    if let Err(e) = fs::remove_file(&info_path) {
        eprintln!(
            "{prog}: warning: cannot remove '{}': {e}",
            info_path.display()
        );
    }
    trashdir::directorysizes_remove(&entry.dir, &entry.name);
    println!("restored '{}'", dest.display());
    0
}

#[cfg(test)]
mod tests {
    use super::parse_restore_indexes;

    #[test]
    fn parse_indexes_single() {
        assert_eq!(parse_restore_indexes("0", 4), Ok(vec![0]));
        assert_eq!(parse_restore_indexes(" 3 ", 4), Ok(vec![3]));
    }

    #[test]
    fn parse_indexes_range_and_comma() {
        assert_eq!(parse_restore_indexes("0-2, 3", 4), Ok(vec![0, 1, 2, 3]));
        assert_eq!(parse_restore_indexes("0-1", 3), Ok(vec![0, 1]));
        assert_eq!(parse_restore_indexes("1,3", 4), Ok(vec![1, 3]));
    }

    #[test]
    fn parse_indexes_rejects_bad() {
        assert!(parse_restore_indexes("", 3).is_err());
        assert!(parse_restore_indexes("0-", 3).is_err());
        assert!(parse_restore_indexes("-1", 3).is_err());
        assert!(parse_restore_indexes("2-0", 3).is_err());
        assert!(parse_restore_indexes("9", 3).is_err());
        assert!(parse_restore_indexes("0-9", 3).is_err());
        assert!(parse_restore_indexes("x", 3).is_err());
    }
}
