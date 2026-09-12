use std::fs;
use std::path::PathBuf;

use rayon::prelude::*;

use crate::info;
use crate::trashdir::{self, TrashDir};
use crate::util::fnmatch;

const HELP: &str = "\
Usage: {prog} [OPTION]... [PATTERN]...
List trashed items as 'DELETION-DATE ORIGINAL-PATH', oldest first.
With PATTERN, only items whose original path or basename matches
(trash-rm shaped; default --match=glob). No PATTERN keeps trash-cli
line format for the full catalog.

A files/ name with no .trashinfo is an emergency row: date
????-??-??T??:??:??, label ORPHAN, no invented Path=. JSON sets
\"orphan\": true and original to null.

      --home-only       only the home trash ($XDG_DATA_HOME/Trash)
      --trash-dir PATH  list only this trash directory (repeatable;
                     `--trash-dir=PATH` also accepted);
                     default is the home trash plus every mounted volume
      --cwd-only        only items whose original path is under cwd
      --match=MODE      glob (default), literal, or regex
      --older-than=DAYS only items trashed more than DAYS days ago
                     (same cutoff model as `empty DAYS`)
      --newer-than=DAYS only items trashed within the last DAYS days
      --orphans         only files/ names with no .trashinfo
      --larger-than=N   only items larger than N bytes
                     (N, Nk/K, Nm/M, Ng/G; 1024-based)
      --smaller-than=N  only items smaller than N bytes (same size syntax)
      --json            emit a JSON array (scripts; one object per item).
                     Includes a bytes field when size is computed
      --help            display this help and exit
      --version         output version information and exit
";

pub struct Entry {
    pub date: Option<String>,
    pub epoch: i64,
    pub original: std::path::PathBuf,
    pub name: String,
    pub dir: TrashDir,
    /// True when `files/<name>` has no matching `.trashinfo`.
    pub orphan: bool,
}

impl Entry {
    /// Path column for list/TUI: original Path=, or an emergency orphan label.
    pub fn path_label(&self) -> String {
        if self.orphan {
            format!("ORPHAN {} (no .trashinfo)", self.name)
        } else {
            self.original.display().to_string()
        }
    }
}

pub fn collect(dirs: &[TrashDir]) -> Vec<Entry> {
    let mut out: Vec<Entry> = dirs.par_iter().flat_map(collect_one).collect();
    out.sort_by(|a, b| {
        a.epoch
            .cmp(&b.epoch)
            .then_with(|| a.original.cmp(&b.original))
            .then_with(|| a.name.cmp(&b.name))
    });
    out
}

fn collect_one(dir: &TrashDir) -> Vec<Entry> {
    let mut out = collect_info_entries(dir);
    out.extend(collect_orphan_entries(dir));
    out
}

fn collect_info_entries(dir: &TrashDir) -> Vec<Entry> {
    let Ok(rd) = fs::read_dir(dir.info()) else {
        return Vec::new();
    };
    let infos: Vec<_> = rd.flatten().collect();
    infos
        .par_iter()
        .filter_map(|entry| parse_info_entry(dir, entry))
        .collect()
}

/// `files/` children with no matching `.trashinfo` (spec: present as such).
fn collect_orphan_entries(dir: &TrashDir) -> Vec<Entry> {
    let Ok(rd) = fs::read_dir(dir.files()) else {
        return Vec::new();
    };
    rd.flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            if dir.info().join(format!("{name}.trashinfo")).exists() {
                return None;
            }
            Some(Entry {
                date: None,
                epoch: 0,
                original: PathBuf::new(),
                name,
                dir: dir.clone(),
                orphan: true,
            })
        })
        .collect()
}

fn parse_info_entry(dir: &TrashDir, entry: &fs::DirEntry) -> Option<Entry> {
    let fname = entry.file_name();
    let fname = fname.to_string_lossy();
    let name = fname.strip_suffix(".trashinfo")?;
    let content = fs::read_to_string(entry.path()).ok()?;
    let parsed = info::parse(&content);
    let epoch = parsed
        .deletion_date
        .as_deref()
        .and_then(info::parse_local_to_epoch)
        .unwrap_or(0);
    let original = dir.resolve_original_checked(&parsed.path).ok()?;
    Some(Entry {
        original,
        date: parsed.deletion_date,
        epoch,
        name: name.to_string(),
        dir: dir.clone(),
        orphan: false,
    })
}

/// Age filter aligned with `empty DAYS`: missing/unparsable date counts as old.
pub fn is_older_than(entry: &Entry, days: i64) -> bool {
    let cutoff = info::now_epoch() - days.saturating_mul(86_400);
    match entry.date.as_deref().and_then(info::parse_local_to_epoch) {
        Some(t) => t <= cutoff,
        None => true,
    }
}

/// Inverse window: trashed *after* the same cutoff (missing date excluded).
pub fn is_newer_than(entry: &Entry, days: i64) -> bool {
    let cutoff = info::now_epoch() - days.saturating_mul(86_400);
    match entry.date.as_deref().and_then(info::parse_local_to_epoch) {
        Some(t) => t > cutoff,
        None => false,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MatchMode {
    Glob,
    Literal,
    Regex,
}

fn entry_matches(entry: &Entry, pat: &str, mode: MatchMode) -> bool {
    let path_s = entry.original.to_string_lossy();
    let base = entry
        .original
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_default();
    match mode {
        MatchMode::Glob => {
            fnmatch(pat, path_s.as_ref())
                || fnmatch(pat, base.as_str())
                || fnmatch(pat, &entry.name)
        }
        MatchMode::Literal => {
            path_s.as_ref() == pat || base == pat || entry.name == pat || path_s.contains(pat)
        }
        MatchMode::Regex => match regex_compile(pat) {
            Ok(re) => {
                re.is_match(path_s.as_ref()) || re.is_match(&base) || re.is_match(&entry.name)
            }
            Err(_) => false,
        },
    }
}

fn regex_compile(pat: &str) -> Result<RegexPat, ()> {
    RegexPat::new(pat)
}

/// Minimal regex: `regex` crate when available via a tiny wrapper that
/// compiles `regex::Regex`.
struct RegexPat(regex::Regex);

impl RegexPat {
    fn new(pat: &str) -> Result<Self, ()> {
        regex::Regex::new(pat).map(Self).map_err(|_| ())
    }
    fn is_match(&self, text: &str) -> bool {
        self.0.is_match(text)
    }
}

/// Apply optional older/newer day filters (AND if both set).
pub fn filter_age(
    entries: Vec<Entry>,
    older_than: Option<i64>,
    newer_than: Option<i64>,
) -> Vec<Entry> {
    entries
        .into_iter()
        .filter(|e| {
            if let Some(d) = older_than {
                if !is_older_than(e, d) {
                    return false;
                }
            }
            if let Some(d) = newer_than {
                if !is_newer_than(e, d) {
                    return false;
                }
            }
            true
        })
        .collect()
}

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

fn parse_days_flag(arg: &str, name: &str) -> Result<i64, String> {
    let prefix = format!("{name}=");
    if let Some(rest) = arg.strip_prefix(&prefix) {
        return parse_nonneg_days(rest);
    }
    Err(format!("expected {name}=DAYS"))
}

/// Escape a string for JSON (UTF-8, control chars).
pub fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

pub fn entry_to_json(e: &Entry) -> String {
    entry_to_json_opt_bytes(e, None)
}

fn entry_to_json_opt_bytes(e: &Entry, bytes: Option<u64>) -> String {
    let date = match e.date.as_deref() {
        Some(d) => format!("\"{}\"", json_escape(d)),
        None if e.orphan => "null".into(),
        None => "\"\"".into(),
    };
    let original = if e.orphan {
        "null".into()
    } else {
        format!("\"{}\"", json_escape(&e.original.to_string_lossy()))
    };
    let mut s = format!(
        "{{\"deletion_date\":{date},\"original\":{original},\"name\":\"{}\",\"trash_dir\":\"{}\",\"orphan\":{}",
        json_escape(&e.name),
        json_escape(&e.dir.root.to_string_lossy()),
        if e.orphan { "true" } else { "false" },
    );
    if let Some(b) = bytes {
        s.push_str(&format!(",\"bytes\":{b}"));
    }
    s.push('}');
    s
}

pub fn entries_to_json_array(entries: &[Entry]) -> String {
    entries_to_json_sized(entries, None)
}

fn entries_to_json_sized(entries: &[Entry], sizes: Option<&[u64]>) -> String {
    let mut out = String::from("[\n");
    for (i, e) in entries.iter().enumerate() {
        if i > 0 {
            out.push_str(",\n");
        }
        out.push_str("  ");
        let bytes = sizes.and_then(|s| s.get(i).copied());
        out.push_str(&entry_to_json_opt_bytes(e, bytes));
    }
    if !entries.is_empty() {
        out.push('\n');
    }
    out.push(']');
    out
}

/// Reclaimable size per entry (directorysizes-when-valid, else walk).
fn entry_bytes(
    e: &Entry,
    caches: &mut std::collections::HashMap<PathBuf, trashdir::DirectorySizesMap>,
) -> u64 {
    let cache = caches
        .entry(e.dir.root.clone())
        .or_insert_with(|| trashdir::DirectorySizesMap::load(&e.dir));
    trashdir::entry_reclaim_bytes_cached(&e.dir, &e.name, Some(cache))
}

fn filter_size(
    entries: Vec<Entry>,
    larger_than: Option<u64>,
    smaller_than: Option<u64>,
) -> (Vec<Entry>, Vec<u64>) {
    let mut caches = std::collections::HashMap::new();
    let mut kept = Vec::new();
    let mut sizes = Vec::new();
    for e in entries {
        let sz = entry_bytes(&e, &mut caches);
        if let Some(n) = larger_than {
            if sz <= n {
                continue;
            }
        }
        if let Some(n) = smaller_than {
            if sz >= n {
                continue;
            }
        }
        kept.push(e);
        sizes.push(sz);
    }
    (kept, sizes)
}

pub fn run(prog: &str, args: &[String]) -> i32 {
    let mut trash_dirs: Vec<PathBuf> = Vec::new();
    let mut home_only = false;
    let mut json = false;
    let mut orphans_only = false;
    let mut older_than: Option<i64> = None;
    let mut newer_than: Option<i64> = None;
    let mut larger_than: Option<u64> = None;
    let mut smaller_than: Option<u64> = None;
    let mut cwd_only = false;
    let mut match_mode = MatchMode::Glob;
    let mut patterns: Vec<String> = Vec::new();
    let mut i = 0usize;
    while i < args.len() {
        let arg = &args[i];
        match arg.as_str() {
            "--home-only" => home_only = true,
            "--cwd-only" => cwd_only = true,
            "--orphans" => orphans_only = true,
            "--json" => json = true,
            "--match=glob" => match_mode = MatchMode::Glob,
            "--match=literal" => match_mode = MatchMode::Literal,
            "--match=regex" => match_mode = MatchMode::Regex,
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
                        eprintln!("Try '{prog} --help' for more information.");
                        return 2;
                    }
                }
            }
            a if a.starts_with("--older-than=") => match parse_days_flag(a, "--older-than") {
                Ok(d) => older_than = Some(d),
                Err(e) => {
                    eprintln!("{prog}: {e}");
                    return 2;
                }
            },
            a if a.starts_with("--newer-than=") => match parse_days_flag(a, "--newer-than") {
                Ok(d) => newer_than = Some(d),
                Err(e) => {
                    eprintln!("{prog}: {e}");
                    return 2;
                }
            },
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
            a if a == "--larger-than" || a.starts_with("--larger-than=") => {
                match crate::util::take_eq_or_next(args, &mut i, "--larger-than") {
                    Ok(v) => match crate::util::parse_bytes(v) {
                        Ok(n) => larger_than = Some(n),
                        Err(e) => {
                            eprintln!("{prog}: {e}");
                            return 2;
                        }
                    },
                    Err(e) => {
                        eprintln!("{prog}: {e}");
                        return 2;
                    }
                }
            }
            a if a == "--smaller-than" || a.starts_with("--smaller-than=") => {
                match crate::util::take_eq_or_next(args, &mut i, "--smaller-than") {
                    Ok(v) => match crate::util::parse_bytes(v) {
                        Ok(n) => smaller_than = Some(n),
                        Err(e) => {
                            eprintln!("{prog}: {e}");
                            return 2;
                        }
                    },
                    Err(e) => {
                        eprintln!("{prog}: {e}");
                        return 2;
                    }
                }
            }
            a if a.starts_with("--match=") => {
                eprintln!("{prog}: --match must be glob, literal, or regex");
                return 2;
            }
            a if a.starts_with('-') && a.len() > 1 => {
                eprintln!("{prog}: unrecognized option '{a}'");
                eprintln!("Try '{prog} --help' for more information.");
                return 2;
            }
            a => patterns.push(a.to_string()),
        }
        i += 1;
    }
    let dirs = trashdir::resolve_dirs_opts(&trash_dirs, home_only);
    if dirs.is_empty() && !trash_dirs.is_empty() {
        eprintln!("{prog}: no valid --trash-dir pins");
        return 2;
    }
    let mut entries = filter_age(collect(&dirs), older_than, newer_than);
    if orphans_only {
        entries.retain(|e| e.orphan);
    }
    if cwd_only {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        entries.retain(|e| !e.orphan && e.original.starts_with(&cwd));
    }
    if !patterns.is_empty() {
        if match_mode == MatchMode::Regex {
            for pat in &patterns {
                if regex_compile(pat).is_err() {
                    eprintln!("{prog}: invalid regex '{pat}'");
                    return 2;
                }
            }
        }
        entries.retain(|e| patterns.iter().any(|pat| entry_matches(e, pat, match_mode)));
    }
    let size_filter = larger_than.is_some() || smaller_than.is_some();
    let sizes = if size_filter {
        let (kept, sz) = filter_size(entries, larger_than, smaller_than);
        entries = kept;
        Some(sz)
    } else {
        None
    };
    if json {
        println!("{}", entries_to_json_sized(&entries, sizes.as_deref()));
    } else {
        for entry in &entries {
            let date = entry.date.as_deref().unwrap_or("????-??-??T??:??:??");
            println!("{} {}", date.replacen('T', " ", 1), entry.path_label());
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trashdir::TrashDir;
    use std::path::PathBuf;

    fn entry_with(date: Option<&str>, epoch: i64) -> Entry {
        Entry {
            date: date.map(str::to_string),
            epoch,
            original: PathBuf::from("/tmp/x"),
            name: "x".into(),
            dir: TrashDir {
                root: PathBuf::from("/tmp/Trash"),
                topdir: None,
            },
            orphan: false,
        }
    }

    #[test]
    fn json_escape_quotes_and_controls() {
        assert_eq!(json_escape("a\"b\\c"), "a\\\"b\\\\c");
        assert!(json_escape("ok").contains("ok"));
    }

    #[test]
    fn age_older_includes_missing_date() {
        let e = entry_with(None, 0);
        assert!(is_older_than(&e, 0));
        assert!(!is_newer_than(&e, 365));
    }

    #[test]
    fn age_newer_recent() {
        let now = info::now_epoch();
        let e = entry_with(Some(&info::now_local_string()), now);
        assert!(is_newer_than(&e, 1));
        assert!(!is_older_than(&e, 1));
    }

    #[test]
    fn filter_age_and() {
        let now = info::now_epoch();
        let recent = entry_with(Some(&info::now_local_string()), now);
        let old = entry_with(Some("2001-01-01T00:00:00"), 978_307_200);
        let out = filter_age(vec![recent, old], Some(30), None);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].epoch, 978_307_200);
    }

    #[test]
    fn entries_json_array_shape() {
        let e = entry_with(Some("2020-01-02T03:04:05"), 1);
        let s = entries_to_json_array(std::slice::from_ref(&e));
        assert!(s.starts_with('['));
        assert!(s.contains("\"deletion_date\":\"2020-01-02T03:04:05\""));
        assert!(s.contains("\"original\":\"/tmp/x\""));
        assert!(s.contains("\"orphan\":false"));
        assert!(
            !s.contains("\"bytes\":"),
            "unfiltered JSON has no bytes: {s}"
        );
        assert!(s.ends_with(']'));
        let with = entries_to_json_sized(std::slice::from_ref(&e), Some(&[4096]));
        assert!(with.contains("\"bytes\":4096"), "{with}");
    }

    #[test]
    fn orphan_json_nulls_original() {
        let mut e = entry_with(None, 0);
        e.orphan = true;
        e.original = PathBuf::new();
        e.name = "debris".into();
        let s = entry_to_json(&e);
        assert!(s.contains("\"orphan\":true"), "{s}");
        assert!(s.contains("\"original\":null"), "{s}");
        assert!(s.contains("\"deletion_date\":null"), "{s}");
        assert!(!s.contains("\"original\":\"\""), "{s}");
        assert_eq!(e.path_label(), "ORPHAN debris (no .trashinfo)");
    }
}
