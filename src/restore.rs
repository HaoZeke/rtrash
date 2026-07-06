use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use crate::list;
use crate::trashdir;
use crate::util::stdin_is_tty;

const HELP: &str = "\
Usage: {prog} [OPTION]... [PATH]
Restore a trashed item to its original location. With PATH, restore the item
originally at PATH; without, choose among items trashed from under the
current directory. A single match restores directly; multiple matches are
listed for interactive selection.

  -f, --force     overwrite an existing file at the original location
      --home-only  only the home trash (skip volume trash)
      --trash-dir=PATH  only consider this trash directory (repeatable)
      --help      display this help and exit
      --version   output version information and exit
";

pub fn run(prog: &str, args: &[String]) -> i32 {
    let mut force = false;
    let mut home_only = false;
    let mut target: Option<PathBuf> = None;
    let mut trash_dirs: Vec<PathBuf> = Vec::new();
    for arg in args {
        match arg.as_str() {
            "-f" | "--force" => force = true,
            "--home-only" => home_only = true,
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

    let dirs = trashdir::resolve_dirs_opts(&trash_dirs, home_only);
    if dirs.is_empty() && !trash_dirs.is_empty() {
        eprintln!("{prog}: no valid --trash-dir pins (need files/ and info/ directories)");
        return 2;
    }
    let entries = list::collect(&dirs);
    let matches: Vec<&list::Entry> = match &target {
        Some(t) => {
            let abs = absolutize(t);
            entries.iter().filter(|e| e.original == abs).collect()
        }
        None => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
            entries
                .iter()
                .filter(|e| e.original.starts_with(&cwd))
                .collect()
        }
    };

    if matches.is_empty() {
        eprintln!("{prog}: no trashed files match");
        return 1;
    }

    let chosen: &list::Entry = if matches.len() == 1 {
        matches[0]
    } else {
        for (i, e) in matches.iter().enumerate() {
            let date = e.date.as_deref().unwrap_or("????-??-??T??:??:??");
            println!(
                "{i:4}  {}  {}",
                date.replacen('T', " ", 1),
                e.original.display()
            );
        }
        if !stdin_is_tty() {
            eprintln!(
                "{prog}: {} matches; stdin is not a terminal, cannot select interactively",
                matches.len()
            );
            return 1;
        }
        eprint!("What file to restore [0..{}]: ", matches.len() - 1);
        io::stderr().flush().ok();
        let mut line = String::new();
        if io::stdin().lock().read_line(&mut line).is_err() {
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

    restore_entry(prog, chosen, force)
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

fn restore_entry(prog: &str, entry: &list::Entry, force: bool) -> i32 {
    let src = entry.dir.files().join(&entry.name);
    let dest = &entry.original;

    // Never destroy the live destination until the trash payload is known good.
    if !src.symlink_metadata().is_ok() {
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
        if let Err(e) = trashdir::remove_any_path(dest) {
            eprintln!(
                "{prog}: cannot remove existing '{}': {e}",
                dest.display()
            );
            // Best-effort: put payload back into the trash name.
            let _ = trashdir::relocate(&staged, &src);
            return 1;
        }
    }

    if let Err(e) = fs::rename(&staged, dest) {
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
