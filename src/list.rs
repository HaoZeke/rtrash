use std::fs;
use std::path::PathBuf;

use crate::info;
use crate::trashdir::{self, TrashDir};

const HELP: &str = "\
Usage: {prog} [OPTION]...
List trashed items as 'DELETION-DATE ORIGINAL-PATH', oldest first.

      --home-only       only the home trash ($XDG_DATA_HOME/Trash)
      --trash-dir=PATH  list only this trash directory (repeatable);
                     default is the home trash plus every mounted volume
      --older-than=DAYS only items trashed more than DAYS days ago
                     (same cutoff model as `empty DAYS`)
      --newer-than=DAYS only items trashed within the last DAYS days
      --json            emit a JSON array (scripts; one object per item)
      --help            display this help and exit
      --version         output version information and exit
";

pub struct Entry {
    pub date: Option<String>,
    pub epoch: i64,
    pub original: std::path::PathBuf,
    pub name: String,
    pub dir: TrashDir,
}

pub fn collect(dirs: &[TrashDir]) -> Vec<Entry> {
    let mut out = Vec::new();
    for dir in dirs {
        let Ok(entries) = fs::read_dir(dir.info()) else {
            continue;
        };
        for entry in entries.flatten() {
            let fname = entry.file_name();
            let fname = fname.to_string_lossy();
            let Some(name) = fname.strip_suffix(".trashinfo") else {
                continue;
            };
            let Ok(content) = fs::read_to_string(entry.path()) else {
                continue;
            };
            let parsed = info::parse(&content);
            let epoch = parsed
                .deletion_date
                .as_deref()
                .and_then(info::parse_local_to_epoch)
                .unwrap_or(0);
            // Skip hostile/corrupt relative Path= that escape the volume topdir.
            let original = match dir.resolve_original_checked(&parsed.path) {
                Ok(p) => p,
                Err(_) => continue,
            };
            out.push(Entry {
                original,
                date: parsed.deletion_date,
                epoch,
                name: name.to_string(),
                dir: dir.clone(),
            });
        }
    }
    out.sort_by(|a, b| {
        a.epoch
            .cmp(&b.epoch)
            .then_with(|| a.original.cmp(&b.original))
    });
    out
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
    let date = e.date.as_deref().unwrap_or("");
    format!(
        "{{\"deletion_date\":\"{}\",\"original\":\"{}\",\"name\":\"{}\",\"trash_dir\":\"{}\"}}",
        json_escape(date),
        json_escape(&e.original.to_string_lossy()),
        json_escape(&e.name),
        json_escape(&e.dir.root.to_string_lossy()),
    )
}

pub fn entries_to_json_array(entries: &[Entry]) -> String {
    let mut out = String::from("[\n");
    for (i, e) in entries.iter().enumerate() {
        if i > 0 {
            out.push_str(",\n");
        }
        out.push_str("  ");
        out.push_str(&entry_to_json(e));
    }
    if !entries.is_empty() {
        out.push('\n');
    }
    out.push(']');
    out
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
    let entries = filter_age(collect(&dirs), older_than, newer_than);
    if json {
        println!("{}", entries_to_json_array(&entries));
    } else {
        for entry in &entries {
            let date = entry.date.as_deref().unwrap_or("????-??-??T??:??:??");
            println!(
                "{} {}",
                date.replacen('T', " ", 1),
                entry.original.display()
            );
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
        assert!(s.ends_with(']'));
    }
}
