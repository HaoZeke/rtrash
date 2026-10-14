//! `rtrash trash-stats`: one summary per trash directory: size, items,
//! inodes, orphans, the oldest and newest entries and the largest ones,
//! as text or JSON for scripts and dashboards.

use std::fs;
use std::path::{Path, PathBuf};

use crate::list;
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
}

/// Files and directories at or below `path`, links not followed.
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

/// Gather the summary of `dir`, keeping the `top` largest items.
pub fn gather(dir: &TrashDir, top: usize) -> Stats {
    let sizes = trashdir::DirSizes::load(dir);
    let entries = list::collect(std::slice::from_ref(dir));
    let orphans = list::orphans(std::slice::from_ref(dir));
    let orphan_count = orphans.len() as u64;
    let mut items: Vec<Item> = entries
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
    let bytes = items.iter().map(|i| i.bytes).sum();
    let dated = |i: &&Item| i.date.is_some();
    let oldest = items.iter().filter(dated).min_by_key(|i| i.epoch).cloned();
    let newest = items.iter().filter(dated).max_by_key(|i| i.epoch).cloned();
    let count = items.len() as u64;
    items.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.name.cmp(&b.name)));
    items.truncate(top);
    Stats {
        root: dir.root.clone(),
        bytes,
        items: count,
        inodes: count_inodes(&dir.files()) + count_inodes(&dir.info()),
        orphans: orphan_count,
        oldest,
        newest,
        top: items,
    }
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
pub fn to_json(all: &[Stats]) -> String {
    let opt = |i: &Option<Item>| i.as_ref().map_or("null".to_string(), item_json);
    let trashes: Vec<String> = all
        .iter()
        .map(|s| {
            let top: Vec<String> = s.top.iter().map(item_json).collect();
            format!(
                r#"{{"path":{},"bytes":{},"items":{},"inodes":{},"orphans":{},"oldest":{},"newest":{},"top":[{}]}}"#,
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

fn print_text(all: &[Stats]) {
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
    let mut top = 5usize;
    for a in args {
        match a.as_str() {
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
    let dirs = trashdir::resolve_dirs_opts(&pins, home_only);
    let all: Vec<Stats> = dirs
        .iter()
        .filter(|d| d.info().is_dir() || d.files().is_dir())
        .map(|d| gather(d, top))
        .collect();
    if json {
        println!("{}", to_json(&all));
    } else {
        print_text(&all);
    }
    0
}
