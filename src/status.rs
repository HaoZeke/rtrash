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
      --older-than=DAYS only count items trashed more than DAYS days ago
      --newer-than=DAYS only count items trashed within the last DAYS days
      --json            emit a JSON object (scripts)
      --help            display this help and exit
      --version         output version information and exit
";

fn parse_nonneg_days(s: &str) -> Result<i64, String> {
    s.parse::<i64>()
        .map_err(|_| format!("invalid DAYS '{s}'"))
        .and_then(|d| {
            if d < 0 {
                Err(format!("DAYS must be >= 0 (got {d})"))
            } else {
                Ok(d)
            }
        })
}

pub fn run(prog: &str, args: &[String]) -> i32 {
    let mut trash_dirs: Vec<PathBuf> = Vec::new();
    let mut home_only = false;
    let mut json = false;
    let mut older_than: Option<i64> = None;
    let mut newer_than: Option<i64> = None;
    let mut i = 0usize;
    while i < args.len() {
        let arg = &args[i];
        match arg.as_str() {
            "--home-only" => home_only = true,
            "--json" => json = true,
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
            a if a.starts_with("--older-than=") => {
                match parse_nonneg_days(&a["--older-than=".len()..]) {
                    Ok(d) => older_than = Some(d),
                    Err(e) => {
                        eprintln!("{prog}: {e}");
                        return 2;
                    }
                }
            }
            a if a.starts_with("--newer-than=") => {
                match parse_nonneg_days(&a["--newer-than=".len()..]) {
                    Ok(d) => newer_than = Some(d),
                    Err(e) => {
                        eprintln!("{prog}: {e}");
                        return 2;
                    }
                }
            }
            "--older-than" | "--newer-than" => {
                let name = arg.as_str();
                i += 1;
                let Some(val) = args.get(i) else {
                    eprintln!("{prog}: {name} requires DAYS");
                    return 2;
                };
                match parse_nonneg_days(val) {
                    Ok(d) if name == "--older-than" => older_than = Some(d),
                    Ok(d) => newer_than = Some(d),
                    Err(e) => {
                        eprintln!("{prog}: {e}");
                        return 2;
                    }
                }
            }
            a => {
                eprintln!("{prog}: unrecognized option '{a}'");
                eprintln!("Try '{prog} --help' for more information.");
                return 2;
            }
        }
        i += 1;
    }
    let dirs = trashdir::resolve_dirs_opts(&trash_dirs, home_only);
    if dirs.is_empty() && !trash_dirs.is_empty() {
        eprintln!("{prog}: no valid --trash-dir pins");
        return 2;
    }
    if json {
        print_status_json(&dirs, older_than, newer_than);
    } else {
        print_status(&dirs, older_than, newer_than);
    }
    0
}

struct RootSummary {
    path: String,
    items: u64,
    bytes: u64,
}

fn summarize_dirs(
    dirs: &[TrashDir],
    older_than: Option<i64>,
    newer_than: Option<i64>,
) -> (Vec<RootSummary>, u64, u64) {
    let mut roots = Vec::new();
    let mut total_items = 0u64;
    let mut total_bytes = 0u64;
    for dir in dirs {
        let entries = list::filter_age(
            list::collect(std::slice::from_ref(dir)),
            older_than,
            newer_than,
        );
        let mut bytes = 0u64;
        let mut counted: std::collections::HashSet<String> = std::collections::HashSet::new();
        for e in &entries {
            bytes = bytes.saturating_add(trashdir::entry_reclaim_bytes(dir, &e.name));
            counted.insert(e.name.clone());
        }
        // Orphans only on unfiltered full status (age filter has no date for orphans).
        if older_than.is_none() && newer_than.is_none() {
            if let Ok(rd) = std::fs::read_dir(dir.files()) {
                for ent in rd.flatten() {
                    let name = ent.file_name().to_string_lossy().into_owned();
                    if counted.contains(&name) {
                        continue;
                    }
                    let has_info = dir.info().join(format!("{name}.trashinfo")).exists();
                    if !has_info {
                        bytes = bytes.saturating_add(fastdelete::disk_usage(&ent.path()));
                    }
                }
            }
        }
        let n = entries.len() as u64;
        total_items = total_items.saturating_add(n);
        total_bytes = total_bytes.saturating_add(bytes);
        roots.push(RootSummary {
            path: dir.root.display().to_string(),
            items: n,
            bytes,
        });
    }
    (roots, total_items, total_bytes)
}

pub fn print_status(dirs: &[TrashDir], older_than: Option<i64>, newer_than: Option<i64>) {
    if dirs.is_empty() {
        eprintln!("No trash directories found");
        eprintln!("Total: 0 items (0 B)");
        return;
    }
    let (roots, total_items, total_bytes) = summarize_dirs(dirs, older_than, newer_than);
    for r in &roots {
        println!(
            "{}\t{} items\t{}",
            r.path,
            r.items,
            fastdelete::format_bytes(r.bytes)
        );
    }
    println!(
        "Total: {} items ({})",
        total_items,
        fastdelete::format_bytes(total_bytes)
    );
}

fn print_status_json(dirs: &[TrashDir], older_than: Option<i64>, newer_than: Option<i64>) {
    let (roots, total_items, total_bytes) = summarize_dirs(dirs, older_than, newer_than);
    let mut out = String::from("{\n  \"roots\": [\n");
    for (i, r) in roots.iter().enumerate() {
        if i > 0 {
            out.push_str(",\n");
        }
        out.push_str(&format!(
            "    {{\"path\":\"{}\",\"items\":{},\"bytes\":{}}}",
            list::json_escape(&r.path),
            r.items,
            r.bytes
        ));
    }
    if !roots.is_empty() {
        out.push('\n');
    }
    out.push_str("  ],\n");
    out.push_str(&format!(
        "  \"total_items\": {},\n  \"total_bytes\": {}\n}}\n",
        total_items, total_bytes
    ));
    print!("{out}");
}
