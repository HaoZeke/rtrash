use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use crate::list;
use crate::trashdir;
use crate::util::stdin_is_tty;

const HELP: &str = "\
Usage: {prog} [OPTION]... [PATH]
Restore a trashed item to its original location.

With PATH, restore the item originally at PATH (exact original path match).
With --name=NAME, restore the item whose file name, or whose name inside
the trash, is NAME. When several items match, --latest restores the most
recently trashed one without asking.
Without PATH on a TTY, open the interactive restore browser (ratatui): filter,
navigate, restore one or more items. Without a TTY (or with a piped index),
print a numbered table and read a selection from stdin.

By default every trashed item in scope is listed; pass --cwd-only to limit to
items whose original path is under the current directory. A single match
restores immediately.

The browser lists the most recently trashed item first, with the cursor on
it. The numbered table lists oldest first, so the newest item sits next to
the index prompt. --newest-first and --oldest-first set the order for both.

  -f, --force     overwrite an existing file at the original location
      --cwd-only  without PATH, only items trashed from under the current directory
      --plain     force numbered line selection even on a TTY (no TUI)
      --newest-first  list the most recently trashed item first
      --oldest-first  list the oldest trashed item first
      --name=NAME  match items by file name or trash name instead of PATH
      --latest    of several matches, restore the most recently trashed
      --json      print {restored, failed} as JSON on stdout; implies
                    --plain, and the numbered table goes to stderr
      --home-only  only the home trash (skip volume trash)
      --trash-dir=PATH  only consider this trash directory (repeatable)
      --help      display this help and exit
      --version   output version information and exit
";

pub fn run(prog: &str, args: &[String]) -> i32 {
    let mut force = false;
    let mut home_only = false;
    let mut cwd_only = false;
    let mut plain = false;
    let mut newest_first: Option<bool> = None;
    let mut target: Option<PathBuf> = None;
    let mut name: Option<String> = None;
    let mut latest = false;
    let mut json = false;
    let mut trash_dirs: Vec<PathBuf> = Vec::new();
    for arg in args {
        match arg.as_str() {
            "-f" | "--force" => force = true,
            "--latest" => latest = true,
            "--json" => {
                json = true;
                plain = true;
            }
            a if a.starts_with("--name=") => {
                let n = &a["--name=".len()..];
                if n.is_empty() {
                    eprintln!("{prog}: --name needs a non-empty NAME");
                    return 2;
                }
                name = Some(n.to_string());
            }
            "--home-only" => home_only = true,
            "--cwd-only" => cwd_only = true,
            "--plain" => plain = true,
            "--newest-first" => newest_first = Some(true),
            "--oldest-first" => newest_first = Some(false),
            "--help" => {
                print!("{}", HELP.replace("{prog}", prog));
                return 0;
            }
            "--version" => {
                println!("{prog} (rtrash) {}", env!("CARGO_PKG_VERSION"));
                return 0;
            }
            a if a.starts_with("--trash-dir=") => {
                trash_dirs.push(PathBuf::from(&a["--trash-dir=".len()..]));
            }
            a if a.starts_with('-') && a.len() > 1 => {
                eprintln!("{prog}: unrecognized option '{a}'");
                return 2;
            }
            a => {
                if target.is_some() {
                    eprintln!("{prog}: only one PATH may be given");
                    return 2;
                }
                target = Some(PathBuf::from(a));
            }
        }
    }

    if target.is_some() && name.is_some() {
        eprintln!("{prog}: give PATH or --name, not both");
        return 2;
    }

    let dirs = trashdir::resolve_dirs_opts(&trash_dirs, home_only);
    if dirs.is_empty() && !trash_dirs.is_empty() {
        eprintln!("{prog}: no valid --trash-dir pins (need files/ and info/ directories)");
        return 2;
    }
    let entries = list::collect(&dirs);
    let mut matches: Vec<&list::Entry> = match (&target, &name) {
        (Some(t), _) => {
            let abs = absolutize(t);
            entries.iter().filter(|e| e.original == abs).collect()
        }
        (None, Some(n)) => entries.iter().filter(|e| name_matches(e, n)).collect(),
        (None, None) if cwd_only => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
            entries
                .iter()
                .filter(|e| e.original.starts_with(&cwd))
                .collect()
        }
        // trash-cli parity: bare `restore` offers every item in scope.
        (None, None) => entries.iter().collect(),
    };

    if matches.is_empty() {
        eprintln!("{prog}: no trashed files match");
        if json {
            println!("{}", restored_json(&[], 0));
        }
        return 1;
    }
    if latest {
        order_entries(&mut matches, true);
        matches.truncate(1);
    }

    // TTY + multi-item: prefer ratatui browser when built with `tui` (unless --plain).
    #[cfg(feature = "tui")]
    if !plain && target.is_none() && name.is_none() && matches.len() > 1 && stdin_is_tty() {
        let newest = newest_first.unwrap_or(true);
        order_entries(&mut matches, newest);
        return crate::restore_tui::run(prog, matches, force, newest);
    }
    #[cfg(not(feature = "tui"))]
    let _ = plain;
    order_entries(&mut matches, newest_first.unwrap_or(false));

    let chosen: &list::Entry = if matches.len() == 1 {
        matches[0]
    } else {
        for (i, e) in matches.iter().enumerate() {
            let date = e.date.as_deref().unwrap_or("????-??-??T??:??:??");
            let row = format!(
                "{i:4}  {}  {}",
                date.replacen('T', " ", 1),
                e.original.display()
            );
            // Keep stdout for the JSON summary.
            if json {
                eprintln!("{row}");
            } else {
                println!("{row}");
            }
        }
        // Piped index for scripts / tests: `printf '0\n' | rtrash restore`.
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
        match line.trim().parse::<usize>() {
            Ok(i) if i < matches.len() => matches[i],
            _ => {
                eprintln!("{prog}: invalid selection");
                return 1;
            }
        }
    };

    let code = restore_entry_quiet(prog, chosen, force, json);
    if json {
        let ok = code == 0;
        let items = if ok { vec![chosen] } else { Vec::new() };
        println!("{}", restored_json(&items, usize::from(!ok)));
    }
    code
}

/// Whether `e` goes by `name`: its original file name or its name under
/// `files/` in the trash.
fn name_matches(e: &list::Entry, name: &str) -> bool {
    e.name == name
        || e.original
            .file_name()
            .is_some_and(|f| f == std::ffi::OsStr::new(name))
}

/// `{"restored": [...], "failed": N}` for `restore --json`.
fn restored_json(restored: &[&list::Entry], failed: usize) -> String {
    let items: Vec<(&Path, &str, &Path)> = restored
        .iter()
        .map(|e| (e.original.as_path(), e.name.as_str(), e.dir.root.as_path()))
        .collect();
    crate::util::items_json("restored", &items, failed)
}

/// Order entries by deletion time. `list::collect` yields oldest first, with
/// ties broken by original path; newest first reverses that whole order, so
/// equal timestamps keep a stable, path-sorted position in both directions.
pub fn order_entries(entries: &mut [&list::Entry], newest_first: bool) {
    entries.sort_by(|a, b| {
        a.epoch
            .cmp(&b.epoch)
            .then_with(|| a.original.cmp(&b.original))
    });
    if newest_first {
        entries.reverse();
    }
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

/// Restore one entry to its original path, for the TUI.
#[cfg(feature = "tui")]
pub(crate) fn restore_entry(prog: &str, entry: &list::Entry, force: bool) -> i32 {
    restore_entry_quiet(prog, entry, force, false)
}

/// Restore one entry to its original path, printing the outcome. Shared by
/// the CLI and the piped pick; `quiet` drops the `restored '...'` line.
fn restore_entry_quiet(prog: &str, entry: &list::Entry, force: bool, quiet: bool) -> i32 {
    match restore_one(entry, force) {
        Ok(done) => {
            for w in &done.warnings {
                eprintln!("{prog}: warning: {w}");
            }
            if !quiet {
                println!("restored '{}'", done.dest.display());
            }
            0
        }
        Err(e) => {
            eprintln!("{prog}: {e}");
            1
        }
    }
}

/// A finished restore: where the item went, and problems that did not stop it.
#[derive(Debug)]
pub(crate) struct Restored {
    pub(crate) dest: PathBuf,
    pub(crate) warnings: Vec<String>,
}

/// Restore one entry to its original path without printing anything, so the
/// TUI can call it while it owns the terminal. Errors come back as text
/// without the program name.
pub(crate) fn restore_one(entry: &list::Entry, force: bool) -> Result<Restored, String> {
    let mut warnings = Vec::new();
    let src = entry.dir.files().join(&entry.name);
    let dest = &entry.original;

    // Never destroy the live destination until the trash payload is known good.
    if src.symlink_metadata().is_err() {
        return Err(format!(
            "trash payload missing for '{}' (stale .trashinfo?)",
            entry.name
        ));
    }

    let dest_exists = dest.symlink_metadata().is_ok();
    if dest_exists && !force {
        return Err(format!(
            "refusing to overwrite existing '{}' (use -f to force)",
            dest.display()
        ));
    }

    let parent = match dest.parent() {
        Some(p) => p,
        None => {
            return Err(format!("cannot restore to '{}': no parent", dest.display()));
        }
    };
    if let Err(e) = fs::create_dir_all(parent) {
        return Err(format!("cannot create '{}': {e}", parent.display()));
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
        let err = format!("cannot restore '{}' to staging area: {e}", src.display());
        let _ = trashdir::remove_any_path(&staged);
        return Err(err);
    }

    if dest_exists {
        // Payload is durable under `staged`; only now remove the blocker.
        if let Err(e) = trashdir::remove_any_path(dest) {
            let err = format!("cannot remove existing '{}': {e}", dest.display());
            // Best-effort: put payload back into the trash name.
            let _ = trashdir::relocate(&staged, &src);
            return Err(err);
        }
    }

    if let Err(e) = fs::rename(&staged, dest) {
        // Same-FS rename of staged→dest; EXDEV should not happen (same parent).
        let err = format!("cannot place restored file at '{}': {e}", dest.display());
        // Leave staged for recovery; try not to drop payload.
        return Err(err);
    }

    let info_path = entry.dir.info().join(format!("{}.trashinfo", entry.name));
    if let Err(e) = fs::remove_file(&info_path) {
        warnings.push(format!("cannot remove '{}': {e}", info_path.display()));
    }
    trashdir::directorysizes_remove(&entry.dir, &entry.name);
    Ok(Restored {
        dest: dest.clone(),
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trashdir::TrashDir;

    fn entry(path: &str, epoch: i64) -> list::Entry {
        list::Entry {
            date: None,
            epoch,
            original: PathBuf::from(path),
            name: "n".into(),
            dir: TrashDir {
                root: PathBuf::from("/tmp/trash"),
                topdir: None,
            },
        }
    }

    fn paths(entries: &[&list::Entry]) -> Vec<String> {
        entries
            .iter()
            .map(|e| e.original.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn order_newest_first_puts_latest_on_top() {
        let old = entry("/a/old", 10);
        let mid = entry("/a/mid", 20);
        let new = entry("/a/new", 30);
        let mut v = vec![&mid, &old, &new];
        order_entries(&mut v, true);
        assert_eq!(paths(&v), ["/a/new", "/a/mid", "/a/old"]);
        order_entries(&mut v, false);
        assert_eq!(paths(&v), ["/a/old", "/a/mid", "/a/new"]);
    }

    #[test]
    fn name_matches_file_name_or_trash_name() {
        let mut e = entry("/home/u/report.pdf", 1);
        e.name = "report.2.pdf".into();
        assert!(name_matches(&e, "report.pdf"));
        assert!(name_matches(&e, "report.2.pdf"));
        assert!(!name_matches(&e, "report"));
        assert!(!name_matches(&e, "u/report.pdf"));
    }

    #[test]
    fn restored_json_lists_items() {
        let e = entry("/a/\"q\".txt", 1);
        assert_eq!(
            restored_json(&[&e], 0),
            r#"{"restored":[{"original":"/a/\"q\".txt","name":"n","trash_dir":"/tmp/trash"}],"failed":0}"#
        );
        assert_eq!(restored_json(&[], 1), r#"{"restored":[],"failed":1}"#);
    }

    #[test]
    fn order_ties_reverse_as_a_block() {
        let x = entry("/t/x", 5);
        let y = entry("/t/y", 5);
        let mut v = vec![&y, &x];
        order_entries(&mut v, false);
        assert_eq!(paths(&v), ["/t/x", "/t/y"]);
        order_entries(&mut v, true);
        assert_eq!(paths(&v), ["/t/y", "/t/x"]);
    }
}
