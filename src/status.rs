//! Trash size/count summary (item count and reclaimable size per trash root).

use std::path::PathBuf;

use crate::fastdelete;
use crate::list;
use crate::space;
use crate::trashdir::{self, TrashDir};
use crate::util::plural;

const HELP: &str = "\
Usage: {prog} [OPTION]...
Summarize discovered trash: item count and approximate reclaimable size
(per root and total). Directory payloads use FreeDesktop directorysizes when
valid; otherwise the same disk_usage walk as empty --dry-run.
Then give the free bytes and free inodes of each trash's file system, with
a warning on stderr when either is under 10%.
Records whose payload is gone count as items and are noted, with a hint
on stderr to remove them with `rtrash empty --dangling`.

      --home-only       only the home trash ($XDG_DATA_HOME/Trash)
      --trash-dir=PATH  only this trash directory (repeatable)
      --older-than=DAYS only count items trashed more than DAYS days ago
      --newer-than=DAYS only count items trashed within the last DAYS days
      --larger-than=SIZE  only count items taking more than SIZE on disk
      --smaller-than=SIZE only count items taking less than SIZE on disk
                     (SIZE as in `empty --max-size`: 500M, 1.5G, 64k)
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
    let mut size = list::SizeFilter::default();
    let mut i = 0usize;
    while i < args.len() {
        match size.take(args, &mut i) {
            Some(Ok(())) => {
                i += 1;
                continue;
            }
            Some(Err(e)) => {
                eprintln!("{prog}: {e}");
                return 2;
            }
            None => {}
        }
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
                trash_dirs.push(crate::util::arg_path(&a["--trash-dir=".len()..]));
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
    if let Err(e) = size.check() {
        eprintln!("{prog}: {e}");
        return 2;
    }
    let dirs = trashdir::resolve_dirs_opts(&trash_dirs, home_only);
    if dirs.is_empty() && !trash_dirs.is_empty() {
        eprintln!("{prog}: no valid --trash-dir pins");
        return 2;
    }
    if json {
        print_status_json(&dirs, older_than, newer_than, size);
    } else {
        print_status(&dirs, older_than, newer_than, size);
    }
    0
}

pub(crate) struct RootSummary {
    pub path: String,
    pub items: u64,
    pub bytes: u64,
    /// Of `items`, payloads with no `.trashinfo`.
    pub orphans: u64,
    /// Of `items`, records whose payload is gone.
    pub dangling: u64,
}

pub(crate) fn summarize_dirs(
    dirs: &[TrashDir],
    older_than: Option<i64>,
    newer_than: Option<i64>,
    size: list::SizeFilter,
) -> (Vec<RootSummary>, u64, u64) {
    let mut roots = Vec::new();
    let mut total_items = 0u64;
    let mut total_bytes = 0u64;
    for dir in dirs {
        let mut entries = list::filter_age(
            list::collect(std::slice::from_ref(dir)),
            older_than,
            newer_than,
        );
        // One read of the cache per root; walked directories are written
        // back at the end, so the next status is a lookup.
        let sizes = trashdir::DirSizes::load(dir);
        let mut each: Vec<u64> = {
            use rayon::prelude::*;
            entries
                .par_iter()
                .map(|e| sizes.entry_bytes(&e.name))
                .collect()
        };
        sizes.write_back();
        if size.is_set() {
            let mut kept = each.iter();
            entries.retain(|_| kept.next().is_some_and(|&n| size.keeps(n)));
            each.retain(|&n| size.keeps(n));
        }
        let dangling = entries.iter().filter(|e| e.missing()).count() as u64;
        let mut bytes: u64 = each.iter().sum();
        // Orphans (payloads with no .trashinfo) count as items, but only on
        // an unfiltered status: they have no date for an age filter.
        let mut orphans = 0u64;
        if older_than.is_none() && newer_than.is_none() {
            for o in list::orphans(std::slice::from_ref(dir)) {
                let n = fastdelete::disk_usage(&o.original);
                if size.keeps(n) {
                    orphans += 1;
                    bytes = bytes.saturating_add(n);
                }
            }
        }
        let n = entries.len() as u64 + orphans;
        total_items = total_items.saturating_add(n);
        total_bytes = total_bytes.saturating_add(bytes);
        roots.push(RootSummary {
            path: dir.root.display().to_string(),
            items: n,
            bytes,
            orphans,
            dangling,
        });
    }
    (roots, total_items, total_bytes)
}

pub fn print_status(
    dirs: &[TrashDir],
    older_than: Option<i64>,
    newer_than: Option<i64>,
    size: list::SizeFilter,
) {
    if dirs.is_empty() {
        eprintln!("No trash directories found");
        eprintln!("Total: 0 items (0 B)");
        return;
    }
    let (roots, total_items, total_bytes) = summarize_dirs(dirs, older_than, newer_than, size);
    for r in &roots {
        let mut notes = Vec::new();
        if r.orphans > 0 {
            notes.push(format!("{} without .trashinfo", r.orphans));
        }
        if r.dangling > 0 {
            notes.push(format!("{} without a payload", r.dangling));
        }
        let orphans = if notes.is_empty() {
            String::new()
        } else {
            format!(" ({})", notes.join(", "))
        };
        println!(
            "{}\t{}{orphans}\t{}",
            r.path,
            plural(r.items, "item", "items"),
            fastdelete::format_bytes(r.bytes)
        );
    }
    println!(
        "Total: {} ({})",
        plural(total_items, "item", "items"),
        fastdelete::format_bytes(total_bytes)
    );
    let dangling: u64 = roots.iter().map(|r| r.dangling).sum();
    if dangling > 0 {
        eprintln!(
            "rtrash: {} cannot be restored, as the payload is gone; remove {} with `rtrash empty --dangling`",
            plural(dangling, "record", "records"),
            if dangling == 1 { "it" } else { "them" }
        );
    }
    for d in dirs {
        let Ok(s) = space::query(&d.root) else {
            continue;
        };
        println!("Free on {}: {}", d.root.display(), free_line(&s));
        let lows = s.lows(space::WARN_FREE_PERCENT);
        if !lows.is_empty() {
            eprintln!(
                "rtrash: warning: {} is low: {}; free space with `rtrash empty`",
                d.root.display(),
                lows.join(", ")
            );
        }
    }
}

pub(crate) fn free_line(s: &space::Space) -> String {
    space::free_line(s)
}

/// The free space fields of one root's JSON object.
fn free_json(root: &std::path::Path) -> String {
    let Ok(s) = space::query(root) else {
        return String::new();
    };
    let opt = |v: Option<u64>| v.map_or("null".to_string(), |n| n.to_string());
    format!(
        ",\"fs_bytes\":{},\"fs_free_bytes\":{},\"fs_inodes\":{},\"fs_free_inodes\":{},\"fs_low\":{}",
        s.total,
        s.avail,
        opt(s.inodes),
        opt(s.inodes_avail),
        !s.lows(space::WARN_FREE_PERCENT).is_empty()
    )
}

fn print_status_json(
    dirs: &[TrashDir],
    older_than: Option<i64>,
    newer_than: Option<i64>,
    size: list::SizeFilter,
) {
    let (roots, total_items, total_bytes) = summarize_dirs(dirs, older_than, newer_than, size);
    let mut out = String::from("{\n  \"roots\": [\n");
    for (i, r) in roots.iter().enumerate() {
        if i > 0 {
            out.push_str(",\n");
        }
        out.push_str(&format!(
            "    {{\"path\":\"{}\",\"items\":{},\"bytes\":{},\"orphans\":{},\"dangling\":{}{}}}",
            list::json_escape(&r.path),
            r.items,
            r.bytes,
            r.orphans,
            r.dangling,
            free_json(&dirs[i].root)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_line_gives_bytes_and_inodes() {
        let s = space::Space {
            total: 100 << 30,
            avail: 8 << 30,
            inodes: Some(1000),
            inodes_avail: Some(910),
        };
        assert_eq!(
            free_line(&s),
            "8.00 GiB of 100.00 GiB (8%), 910 of 1000 inodes (91%)"
        );
        assert_eq!(s.lows(space::WARN_FREE_PERCENT), ["8% of bytes free"]);
        let s = space::Space {
            inodes: None,
            inodes_avail: None,
            ..s
        };
        assert!(free_line(&s).ends_with(", no inode limit"));
    }
}
