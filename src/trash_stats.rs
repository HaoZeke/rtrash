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
                        after each put, restore, undo, rm, empty,
                        reap, status --fix and purge that changed it, with
                        the change from the line before and a trend bar
      --since=AGE       with --history, only the lines from the last AGE
                        (90m, 12h, 7d, 2w; a bare number is days)
      --total           only the size and item count of each trash, from
                        the running total that put, restore, empty and
                        reap keep; nothing is walked, so it answers at
                        once however many files the trash holds. A total
                        that another tool made out of date is marked
                        stale; a trash without one shows no size
      --measure         with --total, measure each trash whose total is
                        missing or stale (one walk) and store it, so
                        later --total runs answer at once; also count
                        the inodes of directory items not yet counted
      --space           with --total, also give each trash's file system:
                        free bytes and inodes and whether either is under
                        10% (one statfs per trash, no walk)
      --exit-low        with --total: --space, and exit 3 when any trash's
                        file system is under 10% free bytes or inodes
      --json            print a JSON object ({trashes, total}); history
                        lines carry delta_bytes and delta_items
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
pub(crate) fn count_inodes(path: &Path) -> u64 {
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

/// The change in bytes and items from the line before, as JSON numbers
/// (negative for a drop), or `null` for the first line.
fn deltas(prev: Option<(u64, u64)>, now: (u64, u64)) -> (String, String) {
    let d = |a: u64, b: u64| (i128::from(b) - i128::from(a)).to_string();
    match prev {
        Some((pb, pn)) => (d(pb, now.0), d(pn, now.1)),
        None => ("null".into(), "null".into()),
    }
}

/// The JSON object for a set of summaries.
pub fn to_json(all: &[Stats], with_history: bool) -> String {
    let opt = |i: &Option<Item>| i.as_ref().map_or("null".to_string(), item_json);
    let trashes: Vec<String> = all
        .iter()
        .map(|s| {
            let top: Vec<String> = s.top.iter().map(item_json).collect();
            let history = if with_history {
                let mut prev: Option<(u64, u64)> = None;
                let points: Vec<String> = s
                    .history
                    .iter()
                    .map(|(t, e, b, n)| {
                        let (db, dn) = deltas(prev, (*b, *n));
                        prev = Some((*b, *n));
                        format!(
                            r#"{{"epoch":{t},"event":"{}","bytes":{b},"items":{n},"delta_bytes":{db},"delta_items":{dn}}}"#,
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

/// How a history line's size moved from the line before it: ` (+1.2
/// KiB)` or ` (-300 B)`, or nothing for the first line or no change.
fn change(prev: Option<u64>, now: u64) -> String {
    match prev {
        Some(p) if now > p => format!(" (+{})", format_bytes(now - p)),
        Some(p) if now < p => format!(" (-{})", format_bytes(p - now)),
        _ => String::new(),
    }
}

/// A bar per size, from `▁` for the smallest to `█` for the largest, so a
/// trash's growth reads at a glance. The last 60 sizes at most.
fn sparkline(sizes: &[u64]) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let sizes = &sizes[sizes.len().saturating_sub(60)..];
    let lo = sizes.iter().copied().min().unwrap_or(0);
    let hi = sizes.iter().copied().max().unwrap_or(0);
    sizes
        .iter()
        .map(|&v| {
            if hi == lo {
                return BARS[0];
            }
            let i = (u128::from(v - lo) * 7 + u128::from(hi - lo) / 2) / u128::from(hi - lo);
            BARS[i as usize]
        })
        .collect()
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
            let mut prev = None;
            for (t, e, b, n) in &s.history {
                println!(
                    "  {}  {:<5}  {}, {}{}",
                    crate::platform::epoch_to_iso_utc(*t),
                    crate::util::term_text(e),
                    format_bytes(*b),
                    crate::util::plural(*n, "item", "items"),
                    change(prev, *b)
                );
                prev = Some(*b);
            }
            if s.history.len() > 1 {
                let sizes: Vec<u64> = s.history.iter().map(|h| h.2).collect();
                println!("  trend: {}", sparkline(&sizes));
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
    let mut since: Option<i64> = None;
    let mut top = 5usize;
    let mut total_only = false;
    let mut measure = false;
    let mut space = false;
    let mut exit_low = false;
    for a in args {
        match a.as_str() {
            "--home-only" => home_only = true,
            "--total" => total_only = true,
            "--measure" => measure = true,
            "--space" => space = true,
            "--exit-low" => {
                space = true;
                exit_low = true;
            }
            "--json" => json = true,
            "--history" => with_history = true,
            a if a.starts_with("--since=") => {
                match crate::reap::parse_age(&a["--since=".len()..]) {
                    Some(s) => since = Some(s),
                    None => {
                        eprintln!(
                            "{prog}: invalid --since '{}' (an age such as 12h or 7d)",
                            &a["--since=".len()..]
                        );
                        return 2;
                    }
                }
            }
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
    // macOS sums up the Finder Trash unless a pin or the setting says
    // otherwise.
    #[cfg(unix)]
    if crate::finder::is_default(!pins.is_empty()) {
        return finder_run(
            prog,
            top,
            total_only,
            measure,
            space,
            exit_low,
            with_history,
            since,
            json,
        );
    }
    if (measure || space) && !total_only {
        eprintln!("{prog}: --measure and --space go with --total");
        return 2;
    }
    if total_only {
        if with_history || since.is_some() {
            eprintln!("{prog}: --total does not combine with --history");
            return 2;
        }
        let rows = totals(prog, &pins, home_only, measure);
        let fs: Vec<Option<crate::space::Space>> = if space {
            rows.iter()
                .map(|(r, _)| crate::space::query(r).ok())
                .collect()
        } else {
            Vec::new()
        };
        let inodes = inode_counts(&pins, home_only, measure, &rows);
        if json {
            println!(
                "{}",
                totals_json_full(&rows, space.then_some(&fs[..]), Some(&inodes))
            );
        } else {
            print_totals(&rows, space.then_some(&fs[..]), &inodes);
        }
        return low_exit(exit_low, &fs);
    }
    if since.is_some() && !with_history {
        eprintln!("{prog}: --since goes with --history");
        eprintln!("Try '{prog} --help' for more information.");
        return 2;
    }
    #[cfg(unix)]
    let mut all: Vec<Stats> = trashdir::resolve_dirs_opts(&pins, home_only)
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
    let mut all: Vec<Stats> = {
        let _ = (home_only, &pins);
        let mut all = gather_bins(top);
        if with_history {
            // A drive whose bin is empty has no items to list, but its
            // history (often ending in that empty) still counts.
            for drive in crate::trash_history::bin_roots() {
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
    if let Some(age) = since {
        let cut = crate::util::now_epoch() - age;
        for s in &mut all {
            s.history.retain(|(t, ..)| *t >= cut);
        }
    }
    if json {
        println!("{}", to_json(&all, with_history));
    } else {
        print_text(&all, with_history);
    }
    0
}

/// `--exit-low`: 3 when any of `fs` is under the warning level, else 0.
fn low_exit(exit_low: bool, fs: &[Option<crate::space::Space>]) -> i32 {
    let low = fs
        .iter()
        .flatten()
        .any(|s| !s.lows(crate::space::WARN_FREE_PERCENT).is_empty());
    if exit_low && low {
        3
    } else {
        0
    }
}

/// `trash-stats` on the Finder trashes, with `--total` and `--history`
/// read from [`crate::finder_total`].
#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
fn finder_run(
    prog: &str,
    top: usize,
    total_only: bool,
    measure: bool,
    space: bool,
    exit_low: bool,
    with_history: bool,
    since: Option<i64>,
    json: bool,
) -> i32 {
    use crate::finder_total as ft;
    if (measure || space) && !total_only {
        eprintln!("{prog}: --measure and --space go with --total");
        return 2;
    }
    if total_only && (with_history || since.is_some()) {
        eprintln!("{prog}: --total does not combine with --history");
        return 2;
    }
    if since.is_some() && !with_history {
        eprintln!("{prog}: --since goes with --history");
        eprintln!("Try '{prog} --help' for more information.");
        return 2;
    }
    let trashes: Vec<PathBuf> = crate::finder::trashes()
        .into_iter()
        .filter(|t| t.is_dir())
        .collect();
    if total_only {
        let rows: Vec<TotalRow> = trashes
            .iter()
            .map(|t| {
                let mut r = ft::stored(t);
                if measure && !r.is_some_and(|r| r.2) {
                    ft::measure(t);
                    r = ft::stored(t);
                }
                (t.clone(), r)
            })
            .collect();
        let fs: Vec<Option<crate::space::Space>> = if space {
            trashes
                .iter()
                .map(|t| crate::space::query(t).ok())
                .collect()
        } else {
            Vec::new()
        };
        // The stored total carries the inodes; --measure counts a trash
        // whose total is missing or stale.
        let inodes: Vec<Option<u64>> = trashes
            .iter()
            .map(|t| ft::stored_total(t).and_then(|(x, _)| x.inodes))
            .collect();
        if json {
            println!(
                "{}",
                totals_json_full(&rows, space.then_some(&fs[..]), Some(&inodes))
            );
        } else {
            print_totals(&rows, space.then_some(&fs[..]), &inodes);
        }
        return low_exit(exit_low, &fs);
    }
    let mut all = crate::finder_shared::stats(top);
    if with_history {
        let cut = since.map(|a| crate::util::now_epoch() - a);
        for s in &mut all {
            s.history = ft::history(&s.root)
                .into_iter()
                .filter(|p| cut.is_none_or(|c| p.epoch >= c))
                .map(|p| (p.epoch, p.event, p.bytes, p.items))
                .collect();
        }
    }
    if json {
        println!("{}", to_json(&all, with_history));
    } else {
        print_text(&all, with_history);
    }
    0
}

/// One trash under `--total`: its root and, when a total is stored, the
/// bytes, items and whether the total is current.
pub type TotalRow = (PathBuf, Option<(u64, u64, bool)>);

/// The stored totals of the trashes, without walking any of them.
/// With `measure`, a trash whose total is missing or stale is walked
/// once and its total stored.
#[cfg(unix)]
fn totals(prog: &str, pins: &[PathBuf], home_only: bool, measure: bool) -> Vec<TotalRow> {
    trashdir::resolve_dirs_opts(pins, home_only)
        .iter()
        .filter(|d| d.info().is_dir() || d.files().is_dir())
        .map(|d| {
            let mut t = crate::trash_total::stored(d);
            if measure && !t.is_some_and(|t| t.2) {
                crate::trash_total::bytes(prog, d);
                t = crate::trash_total::stored(d);
            }
            (d.root.clone(), t)
        })
        .collect()
}

/// Windows: the shell keeps each Recycle Bin's size, so ask it.
#[cfg(windows)]
fn totals(_prog: &str, _pins: &[PathBuf], _home_only: bool, _measure: bool) -> Vec<TotalRow> {
    crate::trash_history::bin_roots()
        .into_iter()
        .filter_map(|r| {
            let (n, b) = crate::win_recycle::query_bin(&r)?;
            Some((r, Some((b, n, true))))
        })
        .collect()
}

/// `{trashes:[{path,bytes,items,stale}], total:{bytes,items,stale}}`;
/// a trash without a stored total has null bytes and items, and the
/// total is stale when any trash is stale or has none.
pub fn totals_json(rows: &[TotalRow]) -> String {
    totals_json_space(rows, None)
}

/// The `fs` object of one trash under `--space`: free bytes and inodes
/// with their percentages, and `low` when either is under 10%. Null when
/// the file system cannot be read; inode fields are null where it counts
/// none (btrfs, Windows).
fn fs_json(s: Option<&crate::space::Space>) -> String {
    let Some(s) = s else {
        return "null".into();
    };
    let opt = |v: Option<u64>| v.map_or("null".into(), |v| v.to_string());
    format!(
        r#"{{"total":{},"avail":{},"avail_pct":{},"inodes":{},"inodes_avail":{},"inodes_pct":{},"low":{}}}"#,
        s.total,
        s.avail,
        s.avail_percent(),
        opt(s.inodes),
        opt(s.inodes_avail),
        opt(s.inodes_percent()),
        !s.lows(crate::space::WARN_FREE_PERCENT).is_empty()
    )
}

/// [`totals_json`], with an `fs` object in each trash when `fs` is given
/// (one entry per row).
pub fn totals_json_space(rows: &[TotalRow], fs: Option<&[Option<crate::space::Space>]>) -> String {
    totals_json_full(rows, fs, None)
}

/// The inodes each trash holds (see [`crate::trash_inodes`]), in row
/// order: `None` where a directory item has no cached count and
/// `measure` is off, and always on Windows.
#[cfg(unix)]
fn inode_counts(
    pins: &[PathBuf],
    home_only: bool,
    measure: bool,
    rows: &[TotalRow],
) -> Vec<Option<u64>> {
    let dirs = trashdir::resolve_dirs_opts(pins, home_only);
    rows.iter()
        .map(|(root, _)| {
            dirs.iter()
                .find(|d| &d.root == root)
                .and_then(|d| crate::trash_inodes::count(d, measure))
        })
        .collect()
}

#[cfg(windows)]
fn inode_counts(
    _pins: &[PathBuf],
    _home_only: bool,
    _measure: bool,
    rows: &[TotalRow],
) -> Vec<Option<u64>> {
    vec![None; rows.len()]
}

/// [`totals_json_space`], with `inodes` in each trash and in the total
/// when `inodes` is given; the total's is null when any trash's is.
pub fn totals_json_full(
    rows: &[TotalRow],
    fs: Option<&[Option<crate::space::Space>]>,
    inodes: Option<&[Option<u64>]>,
) -> String {
    let opt = |v: Option<u64>| v.map_or("null".into(), |v| v.to_string());
    let with_fs = |i: usize, obj: String| {
        let mut obj = obj;
        if let Some(ino) = inodes {
            obj = format!(
                "{},\"inodes\":{}}}",
                &obj[..obj.len() - 1],
                opt(ino.get(i).copied().flatten())
            );
        }
        match fs {
            Some(fs) => format!(
                "{},\"fs\":{}}}",
                &obj[..obj.len() - 1],
                fs_json(fs.get(i).and_then(Option::as_ref))
            ),
            None => obj,
        }
    };
    let trashes: Vec<String> = rows
        .iter()
        .enumerate()
        .map(|(i, (root, t))| {
            (
                i,
                match t {
                    Some((b, n, fresh)) => format!(
                        r#"{{"path":{},"bytes":{b},"items":{n},"stale":{}}}"#,
                        path_json(root),
                        !fresh
                    ),
                    None => format!(
                        r#"{{"path":{},"bytes":null,"items":null,"stale":true}}"#,
                        path_json(root)
                    ),
                },
            )
        })
        .map(|(i, obj)| with_fs(i, obj))
        .collect();
    let bytes: u64 = rows.iter().filter_map(|(_, t)| t.map(|t| t.0)).sum();
    let items: u64 = rows.iter().filter_map(|(_, t)| t.map(|t| t.1)).sum();
    let stale = rows.iter().any(|(_, t)| !t.is_some_and(|t| t.2));
    let total_inodes = inodes.map(|ino| {
        let all: Option<u64> = ino.iter().copied().sum();
        format!(",\"inodes\":{}", opt(all))
    });
    format!(
        r#"{{"trashes":[{}],"total":{{"bytes":{bytes},"items":{items},"stale":{stale}{}}}}}"#,
        trashes.join(","),
        total_inodes.unwrap_or_default()
    )
}

fn print_totals(
    rows: &[TotalRow],
    fs: Option<&[Option<crate::space::Space>]>,
    inodes: &[Option<u64>],
) {
    for (i, (root, t)) in rows.iter().enumerate() {
        let ino = inodes.get(i).copied().flatten().map_or(String::new(), |n| {
            format!(", {}", crate::util::plural(n, "inode", "inodes"))
        });
        let free = match fs.map(|f| f.get(i).and_then(Option::as_ref)) {
            None => String::new(),
            Some(None) => "\tfree: unknown".into(),
            Some(Some(s)) => {
                let lows = s.lows(crate::space::WARN_FREE_PERCENT);
                format!(
                    "\tfree {} ({}%{}){}",
                    format_bytes(s.avail),
                    s.avail_percent(),
                    s.inodes_percent()
                        .map_or(String::new(), |p| format!(", {p}% of inodes")),
                    if lows.is_empty() { "" } else { " LOW" }
                )
            }
        };
        match t {
            Some((b, n, fresh)) => println!(
                "{}\t{}\t{}{ino}{}{free}",
                crate::util::term_path(&root),
                format_bytes(*b),
                crate::util::plural(*n, "item", "items"),
                if *fresh { "" } else { "\t(stale)" }
            ),
            None => println!("{}\tno stored total{free}", crate::util::term_path(&root)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn totals_json_marks_stale_and_missing_totals() {
        let rows: Vec<TotalRow> = vec![
            (PathBuf::from("/a"), Some((10, 2, true))),
            (PathBuf::from("/b"), Some((5, 1, false))),
            (PathBuf::from("/c"), None),
        ];
        let j = totals_json(&rows);
        assert!(j.contains(r#""bytes":10,"items":2,"stale":false"#), "{j}");
        assert!(j.contains(r#""bytes":5,"items":1,"stale":true"#), "{j}");
        assert!(
            j.contains(r#""bytes":null,"items":null,"stale":true"#),
            "{j}"
        );
        assert!(
            j.ends_with(r#""total":{"bytes":15,"items":3,"stale":true}}"#),
            "{j}"
        );
        let s = crate::space::Space {
            total: 100,
            avail: 5,
            inodes: Some(1000),
            inodes_avail: Some(500),
        };
        let spaced = totals_json_space(&rows[..2], Some(&[Some(s), None]));
        assert!(
            spaced.contains(r#""stale":false,"fs":{"total":100,"avail":5,"avail_pct":5,"inodes":1000,"inodes_avail":500,"inodes_pct":50,"low":true}}"#),
            "{spaced}"
        );
        assert!(spaced.contains(r#""stale":true,"fs":null}"#), "{spaced}");
        let counted = totals_json_full(&rows[..2], None, Some(&[Some(4), None]));
        assert!(
            counted.contains(r#""stale":false,"inodes":4}"#),
            "{counted}"
        );
        assert!(
            counted.ends_with(r#""stale":true,"inodes":null}}"#),
            "{counted}"
        );
        let both = totals_json_full(&rows[..1], None, Some(&[Some(4)]));
        assert!(both.ends_with(r#""stale":false,"inodes":4}}"#), "{both}");
        let fresh = totals_json(&rows[..1]);
        assert!(fresh.ends_with(r#""stale":false}}"#), "{fresh}");
    }

    #[test]
    fn change_shows_the_growth_or_drop_from_the_line_before() {
        assert_eq!(change(None, 5), "");
        assert_eq!(change(Some(5), 5), "");
        assert_eq!(change(Some(5), 8), format!(" (+{})", format_bytes(3)));
        assert_eq!(change(Some(8), 0), format!(" (-{})", format_bytes(8)));
    }

    #[test]
    fn deltas_are_signed_and_null_for_the_first_line() {
        let s = |a: &str, b: &str| (a.to_string(), b.to_string());
        assert_eq!(deltas(None, (5, 1)), s("null", "null"));
        assert_eq!(deltas(Some((5, 1)), (8, 2)), s("3", "1"));
        assert_eq!(deltas(Some((8, 2)), (0, 0)), s("-8", "-2"));
        assert_eq!(
            deltas(Some((u64::MAX, 0)), (0, 0)).0,
            format!("-{}", u64::MAX)
        );
    }

    #[test]
    fn sparkline_scales_sizes_between_the_smallest_and_largest() {
        assert_eq!(sparkline(&[0, 7, 14, 0]), "▁▅█▁");
        assert_eq!(sparkline(&[5, 5]), "▁▁");
        assert_eq!(sparkline(&[u64::MAX, 0]), "█▁");
        assert_eq!(sparkline(&vec![1; 100]).chars().count(), 60);
    }
}
