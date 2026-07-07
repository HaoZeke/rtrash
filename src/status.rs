//! Trash size/count summary (item count and reclaimable size per trash root).

use std::path::PathBuf;

use crate::fastdelete;
use crate::list;
use crate::trashdir::{self, TrashDir};

const HELP: &str = "\
Usage: {prog} [OPTION]...
Summarize discovered trash: item count and approximate reclaimable size
(per root and total). Directory payloads use FreeDesktop directorysizes when
valid; otherwise the same disk_usage walk as empty --dry-run.

      --home-only       only the home trash ($XDG_DATA_HOME/Trash)
      --trash-dir=PATH  only this trash directory (repeatable)
      --help            display this help and exit
      --version         output version information and exit
";

pub fn run(prog: &str, args: &[String]) -> i32 {
    let mut trash_dirs: Vec<PathBuf> = Vec::new();
    let mut home_only = false;
    for arg in args {
        match arg.as_str() {
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
            a => {
                eprintln!("{prog}: unrecognized option '{a}'");
                eprintln!("Try '{prog} --help' for more information.");
                return 2;
            }
        }
    }
    let dirs = trashdir::resolve_dirs_opts(&trash_dirs, home_only);
    if dirs.is_empty() && !trash_dirs.is_empty() {
        eprintln!("{prog}: no valid --trash-dir pins");
        return 2;
    }
    print_status(&dirs);
    0
}

pub fn print_status(dirs: &[TrashDir]) {
    let mut total_items = 0u64;
    let mut total_bytes = 0u64;
    if dirs.is_empty() {
        eprintln!("No trash directories found");
        eprintln!("Total: 0 items (0 B)");
        return;
    }
    for dir in dirs {
        let entries = list::collect(std::slice::from_ref(dir));
        let mut bytes = 0u64;
        let mut counted: std::collections::HashSet<String> = std::collections::HashSet::new();
        for e in &entries {
            bytes = bytes.saturating_add(trashdir::entry_reclaim_bytes(dir, &e.name));
            counted.insert(e.name.clone());
        }
        // Orphans in files/ without info (full empty would purge them).
        if let Ok(rd) = std::fs::read_dir(dir.files()) {
            for ent in rd.flatten() {
                let name = ent.file_name().to_string_lossy().into_owned();
                if counted.contains(&name) {
                    continue;
                }
                let has_info = dir.info().join(format!("{name}.trashinfo")).exists();
                if !has_info {
                    // Orphans never use directorysizes (no matching .trashinfo mtime).
                    bytes = bytes.saturating_add(fastdelete::disk_usage(&ent.path()));
                }
            }
        }
        let n = entries.len() as u64;
        total_items = total_items.saturating_add(n);
        total_bytes = total_bytes.saturating_add(bytes);
        println!(
            "{}\t{} items\t{}",
            dir.root.display(),
            n,
            fastdelete::format_bytes(bytes)
        );
    }
    println!(
        "Total: {} items ({})",
        total_items,
        fastdelete::format_bytes(total_bytes)
    );
}
