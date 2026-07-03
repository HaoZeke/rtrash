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
      --help      display this help and exit
      --version   output version information and exit
";

pub fn run(prog: &str, args: &[String]) -> i32 {
    let mut force = false;
    let mut target: Option<PathBuf> = None;
    for arg in args {
        match arg.as_str() {
            "-f" | "--force" => force = true,
            "--help" => {
                print!("{}", HELP.replace("{prog}", prog));
                return 0;
            }
            "--version" => {
                println!("{prog} (rtrash) {}", env!("CARGO_PKG_VERSION"));
                return 0;
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

    let entries = list::collect(&trashdir::all());
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

    if dest.symlink_metadata().is_ok() && !force {
        eprintln!(
            "{prog}: refusing to overwrite existing '{}' (use -f to force)",
            dest.display()
        );
        return 1;
    }
    if let Some(parent) = dest.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            eprintln!("{prog}: cannot create '{}': {e}", parent.display());
            return 1;
        }
    }
    if let Err(e) = fs::rename(&src, dest) {
        eprintln!(
            "{prog}: cannot restore '{}' to '{}': {e}",
            src.display(),
            dest.display()
        );
        return 1;
    }
    let info_path = entry.dir.info().join(format!("{}.trashinfo", entry.name));
    if let Err(e) = fs::remove_file(&info_path) {
        eprintln!(
            "{prog}: warning: cannot remove '{}': {e}",
            info_path.display()
        );
    }
    println!("restored '{}'", dest.display());
    0
}
