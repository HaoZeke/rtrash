//! `rtrash trash-stats`: one summary per trash directory: size, items,
//! inodes, orphans, the oldest and newest entries and the largest ones,
//! as text or JSON for scripts and dashboards. On Windows, one summary
//! per drive's Recycle Bin.

#[cfg(unix)]
use std::fs;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use crate::list;
#[cfg(unix)]
use crate::trashdir::{self, TrashDir};
use crate::util::{format_bytes, json_escape, str_from_os_ref};

const HELP: &str = "\
Usage: {prog} [OPTION]...
For each trash directory: its size on disk, item count, inodes (files and
directories under files/ and info/), orphans (payloads without a
.trashinfo), the oldest and newest items, and the largest items.

      --top=N           list the N largest items (default 5; 0 for none)
      --home-only       only the home trash ($XDG_DATA_HOME/Trash)
      --trash-dir=PATH  only this trash directory (repeatable)
      --history         also show each trash's history: its size and items
                        after each put, empty and reap that changed it
      --json            print a JSON object ({trashes, total})
      --help            display this help and exit
      --version         output version information and exit
";

/// One trashed item as the summary shows it.
#[derive(Debug, Clone)]
pub struct Item {
    pub name: String,
    pub original: PathBuf,
    pub date: Option<String>,
    pub epoch: i64,
    pub bytes: u64,
}

/// The summary of one trash directory.
#[derive(Debug, Clone)]
pub struct Stats {
    pub root: PathBuf,
    pub bytes: u64,
    pub items: u64,
    pub inodes: u64,
    pub orphans: u64,
    pub oldest: Option<Item>,
    pub newest: Option<Item>,
    pub top: Vec<Item>,
    /// (epoch, event, bytes, items) after each command that changed it,
    /// oldest first; filled only for `--history`.
    pub history: Vec<(i64, String, u64, u64)>,
}

/// Files and directories at or below `path`, links not followed.
#[cfg(unix)]
fn count_inodes(path: &Path) -> u64 {
    let mut n = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            n += 1;
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                stack.push(e.path());
            }
        }
    }
    n
}

/// The summary of `items` in the trash at `root`, keeping the `top`
/// largest.
pub fn summarize(
    root: PathBuf,
    mut items: Vec<Item>,
    inodes: u64,
    orphans: u64,
    top: usize,
) -> Stats {
    let bytes = items.iter().map(|i| i.bytes).sum();
    let dated = |i: &&Item| i.date.is_some();
    let oldest = items.iter().filter(dated).min_by_key(|i| i.epoch).cloned();
    let newest = items.iter().filter(dated).max_by_key(|i| i.epoch).cloned();
    let count = items.len() as u64;
    items.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.name.cmp(&b.name)));
    items.truncate(top);
    Stats {
        root,
        bytes,
        items: count,
        inodes,
        orphans,
        oldest,
        newest,
        top: items,
        history: Vec::new(),
    }
}

/// Gather the summary of `dir`, keeping the `top` largest items.
#[cfg(unix)]
pub fn gather(dir: &TrashDir, top: usize) -> Stats {
    let sizes = trashdir::DirSizes::load(dir);
    let entries = list::collect(std::slice::from_ref(dir));
    let orphans = list::orphans(std::slice::from_ref(dir));
    let orphan_count = orphans.len() as u64;
    let items: Vec<Item> = entries
        .into_iter()
        .chain(orphans)
        .map(|e| Item {
            bytes: sizes.entry_bytes(&e.name),
            name: e.name,
            original: e.original,
            date: e.date,
            epoch: e.epoch,
        })
        .collect();
    sizes.write_back();
    let inodes = count_inodes(&dir.files()) + count_inodes(&dir.info());
    summarize(dir.root.clone(), items, inodes, orphan_count, top)
}

/// The summaries of each drive's Recycle Bin: a `$R` payload and its `$I`
/// record count as the item's inodes; the bin keeps no orphans apart.
#[cfg(windows)]
fn gather_bins(top: usize) -> Vec<Stats> {
    crate::win_recycle::bin_items()
        .into_iter()
        .map(|(root, raw)| {
            let inodes = raw.iter().map(|r| r.files + 1).sum();
            let items = raw
                .into_iter()
                .map(|r| Item {
                    name: r.name,
                    original: r.original,
                    date: Some(crate::platform::epoch_to_iso_utc(r.epoch)),
                    epoch: r.epoch,
                    bytes: r.bytes,
                })
                .collect();
            summarize(root, items, inodes, 0, top)
        })
        .collect()
}

fn path_json(p: &Path) -> String {
    format!("\"{}\"", json_escape(&str_from_os_ref(p.as_os_str())))
}

fn item_json(i: &Item) -> String {
    format!(
        r#"{{"name":"{}","original":{},"date":{},"epoch":{},"bytes":{}}}"#,
        json_escape(&i.name),
        path_json(&i.original),
        i.date
            .as_ref()
            .map_or("null".to_string(), |d| format!("\"{}\"", json_escape(d))),
        i.epoch,
        i.bytes
    )
}

/// The JSON object for a set of summaries.
pub fn to_json(all: &[Stats], with_history: bool) -> String {
    let opt = |i: &Option<Item>| i.as_ref().map_or("null".to_string(), item_json);
    let trashes: Vec<String> = all
        .iter()
        .map(|s| {
            let top: Vec<String> = s.top.iter().map(item_json).collect();
            let history = if with_history {
                let points: Vec<String> = s
                    .history
                    .iter()
                    .map(|(t, e, b, n)| {
                        format!(
                            r#"{{"epoch":{t},"event":"{}","bytes":{b},"items":{n}}}"#,
                            json_escape(e)
                        )
                    })
                    .collect();
                format!(r#","history":[{}]"#, points.join(","))
            } else {
                String::new()
            };
            format!(
                r#"{{"path":{},"bytes":{},"items":{},"inodes":{},"orphans":{},"oldest":{},"newest":{},"top":[{}]{history}}}"#,
                path_json(&s.root),
                s.bytes,
                s.items,
                s.inodes,
                s.orphans,
                opt(&s.oldest),
                opt(&s.newest),
                top.join(",")
            )
        })
        .collect();
    let sum = |f: fn(&Stats) -> u64| all.iter().map(f).sum::<u64>();
    format!(
        r#"{{"trashes":[{}],"total":{{"bytes":{},"items":{},"inodes":{},"orphans":{}}}}}"#,
        trashes.join(","),
        sum(|s| s.bytes),
        sum(|s| s.items),
        sum(|s| s.inodes),
        sum(|s| s.orphans)
    )
}

fn print_text(all: &[Stats], with_history: bool) {
    let line = |label: &str, i: &Item| {
        println!(
            "  {label}: {} ({}, {}, from {})",
            crate::util::term_text(&i.name),
            i.date.as_deref().unwrap_or("no date"),
            format_bytes(i.bytes),
            crate::util::term_path(&i.original)
        );
    };
    for s in all {
        println!(
            "{}: {}, {}, {} inodes, {} orphans",
            crate::util::term_path(&s.root),
            crate::util::plural(s.items, "item", "items"),
            format_bytes(s.bytes),
            s.inodes,
            s.orphans
        );
        if let Some(i) = &s.oldest {
            line("oldest", i);
        }
        if let Some(i) = &s.newest {
            line("newest", i);
        }
        for (n, i) in s.top.iter().enumerate() {
            line(&format!("#{}", n + 1), i);
        }
        if with_history {
            if s.history.is_empty() {
                println!("  history: none yet");
            }
            for (t, e, b, n) in &s.history {
                println!(
                    "  {}  {:<5}  {}, {}",
                    crate::platform::epoch_to_iso_utc(*t),
                    crate::util::term_text(e),
                    format_bytes(*b),
                    crate::util::plural(*n, "item", "items")
                );
            }
        }
    }
    let bytes: u64 = all.iter().map(|s| s.bytes).sum();
    let items: u64 = all.iter().map(|s| s.items).sum();
    println!(
        "Total: {} ({})",
        crate::util::plural(items, "item", "items"),
        format_bytes(bytes)
    );
}

pub fn run(prog: &str, args: &[String]) -> i32 {
    let mut pins: Vec<PathBuf> = Vec::new();
    let mut home_only = false;
    let mut json = false;
    let mut with_history = false;
    let mut top = 5usize;
    for a in args {
        match a.as_str() {
            "--home-only" => home_only = true,
            "--json" => json = true,
            "--history" => with_history = true,
            "--help" => {
                print!("{}", HELP.replace("{prog}", prog));
                return 0;
            }
            "--version" => {
                println!("{prog} (rtrash) {}", env!("CARGO_PKG_VERSION"));
                return 0;
            }
            a if a.starts_with("--trash-dir=") => {
                pins.push(crate::util::arg_path(&a["--trash-dir=".len()..]));
            }
            a if a.starts_with("--top=") => match a["--top=".len()..].parse() {
                Ok(n) => top = n,
                Err(_) => {
                    eprintln!("{prog}: invalid --top '{}' (a count)", &a["--top=".len()..]);
                    return 2;
                }
            },
            other => {
                eprintln!("{prog}: unrecognized option '{other}'");
                eprintln!("Try '{prog} --help' for more information.");
                return 2;
            }
        }
    }
    #[cfg(unix)]
    let all: Vec<Stats> = trashdir::resolve_dirs_opts(&pins, home_only)
        .iter()
        .filter(|d| d.info().is_dir() || d.files().is_dir())
        .map(|d| {
            let mut s = gather(d, top);
            if with_history {
                s.history = crate::trash_total::history(d)
                    .into_iter()
                    .map(|p| (p.epoch, p.event, p.bytes, p.items))
                    .collect();
            }
            s
        })
        .collect();
    // The Recycle Bin has one place per drive; there is nothing to narrow.
    #[cfg(windows)]
    let all: Vec<Stats> = {
        let _ = (home_only, &pins);
        let mut all = gather_bins(top);
        if with_history {
            // A drive whose bin is empty has no items to list, but its
            // history (often ending in that empty) still counts.
            for drive in crate::win_recycle::bin_drives() {
                let bin = drive.join("$Recycle.Bin");
                let has = crate::trash_history::bin_path(&drive).is_some_and(|p| p.exists());
                if has && !all.iter().any(|s| s.root == bin) {
                    all.push(summarize(bin, Vec::new(), 0, 0, top));
                }
            }
            all.sort_by(|a, b| a.root.cmp(&b.root));
            for s in &mut all {
                let drive = s.root.parent().unwrap_or(&s.root).to_path_buf();
                s.history = crate::trash_history::bin_path(&drive)
                    .map(|p| crate::trash_history::read(&p))
                    .unwrap_or_default()
                    .into_iter()
                    .map(|p| (p.epoch, p.event, p.bytes, p.items))
                    .collect();
            }
        }
        all
    };
    if json {
        println!("{}", to_json(&all, with_history));
    } else {
        print_text(&all, with_history);
    }
    0
}
