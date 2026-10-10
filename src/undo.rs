//! `rtrash undo`: restore every item one put run trashed.
//!
//! Each put run writes the same batch id into its `.trashinfo` files (the
//! extra key [`crate::info::BATCH_KEY`]). Items trashed by other tools carry
//! no id, so undo never touches them.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::list;
use crate::restore;
use crate::trashdir;

const HELP: &str = "\
Usage: {prog} [OPTION]... [BATCH]
Restore every item one put run trashed, to its original location.

Each rtrash put run tags its items with one batch id. Without BATCH, undo
the most recent run in scope; with BATCH, undo that run. Items trashed by
other tools carry no batch id and never match.

  -l, --list      list put runs, newest first: id, date, item count, paths
  -n, --dry-run   show what would be restored and change nothing
  -f, --force     overwrite existing files at the original locations
      --json      print {batch, restored, failed} as JSON on stdout
                    (with --list, an array of runs)
      --home-only  only the home trash (skip volume trash)
      --trash-dir=PATH  only consider this trash directory (repeatable)
      --help      display this help and exit
      --version   output version information and exit

Exit status: 0 when every item came back, 1 when none matched or any
item failed, 2 on a usage error.
";

/// One put run found in the trash.
pub struct Batch<'a> {
    pub id: String,
    /// Newest deletion time among its items.
    pub epoch: i64,
    pub entries: Vec<&'a list::Entry>,
}

/// Group entries by batch id, newest run first. Entries without an id are
/// left out.
pub fn batches(entries: &[list::Entry]) -> Vec<Batch<'_>> {
    let mut by_id: BTreeMap<&str, Vec<&list::Entry>> = BTreeMap::new();
    for e in entries {
        if let Some(b) = e.batch.as_deref() {
            by_id.entry(b).or_default().push(e);
        }
    }
    let mut out: Vec<Batch> = by_id
        .into_iter()
        .map(|(id, mut entries)| {
            entries.sort_by(|a, b| a.original.cmp(&b.original));
            Batch {
                id: id.to_string(),
                epoch: entries.iter().map(|e| e.epoch).max().unwrap_or(0),
                entries,
            }
        })
        .collect();
    // Ids start with the run's start in milliseconds, which orders runs
    // within one second; deletion time covers ids from other writers.
    out.sort_by(|a, b| {
        let key = |x: &Batch| (id_millis(&x.id), x.epoch);
        key(b).cmp(&key(a)).then_with(|| b.id.cmp(&a.id))
    });
    out
}

fn id_millis(id: &str) -> i64 {
    id.split('-')
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

pub fn run(prog: &str, args: &[String]) -> i32 {
    let mut list_only = false;
    let mut dry_run = false;
    let mut force = false;
    let mut json = false;
    let mut home_only = false;
    let mut trash_dirs: Vec<PathBuf> = Vec::new();
    let mut wanted: Option<String> = None;
    for arg in args {
        match arg.as_str() {
            "-l" | "--list" => list_only = true,
            "-n" | "--dry-run" => dry_run = true,
            "-f" | "--force" => force = true,
            "--json" => json = true,
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
                trash_dirs.push(crate::util::arg_path(&a["--trash-dir=".len()..]));
            }
            a if a.starts_with('-') && a.len() > 1 => {
                eprintln!("{prog}: unrecognized option '{a}'");
                eprintln!("Try '{prog} --help' for more information.");
                return 2;
            }
            a => {
                if wanted.is_some() {
                    eprintln!("{prog}: only one BATCH may be given");
                    return 2;
                }
                wanted = Some(a.to_string());
            }
        }
    }

    let dirs = trashdir::resolve_dirs_opts(&trash_dirs, home_only);
    if dirs.is_empty() && !trash_dirs.is_empty() {
        eprintln!("{prog}: no valid --trash-dir pins (need files/ and info/ directories)");
        return 2;
    }
    let entries = list::collect(&dirs);
    let runs = batches(&entries);

    if list_only {
        print_runs(&runs, json);
        return 0;
    }

    let chosen = match &wanted {
        Some(id) => runs.iter().find(|b| &b.id == id),
        None => runs.first(),
    };
    let Some(batch) = chosen else {
        match &wanted {
            Some(id) => eprintln!("{prog}: no trashed items from put run '{id}'"),
            None => {
                eprintln!("{prog}: no put run to undo (items from other tools carry no batch id)")
            }
        }
        if json {
            println!("{}", batch_json(wanted.as_deref().unwrap_or(""), &[], 0));
        }
        return 1;
    };

    if dry_run {
        for e in &batch.entries {
            let line = format!("would restore '{}'", e.original.display());
            if json {
                eprintln!("{line}");
            } else {
                println!("{line}");
            }
        }
        if json {
            println!("{}", batch_json(&batch.id, &batch.entries, 0));
        }
        return 0;
    }

    let mut restored: Vec<&list::Entry> = Vec::new();
    let mut failed = 0usize;
    for e in &batch.entries {
        match restore::restore_one(e, force) {
            Ok(done) => {
                for w in &done.warnings {
                    eprintln!("{prog}: warning: {w}");
                }
                if !json {
                    println!("restored '{}'", done.dest.display());
                }
                restored.push(e);
            }
            Err(msg) => {
                eprintln!("{prog}: {msg}");
                failed += 1;
            }
        }
    }
    if json {
        println!("{}", batch_json(&batch.id, &restored, failed));
    } else if failed > 0 {
        eprintln!(
            "{prog}: run {}: {} restored, {failed} failed",
            batch.id,
            restored.len()
        );
    }
    i32::from(failed > 0)
}

fn print_runs(runs: &[Batch], json: bool) {
    if json {
        let rows: Vec<String> = runs
            .iter()
            .map(|b| {
                let date = b
                    .entries
                    .iter()
                    .max_by_key(|e| e.epoch)
                    .and_then(|e| e.date.as_deref())
                    .unwrap_or("");
                let items: Vec<String> = b
                    .entries
                    .iter()
                    .map(|e| format!("\"{}\"", list::json_escape(&e.original.to_string_lossy())))
                    .collect();
                format!(
                    "  {{\"batch\":\"{}\",\"deletion_date\":\"{}\",\"count\":{},\"originals\":[{}]}}",
                    list::json_escape(&b.id),
                    list::json_escape(date),
                    b.entries.len(),
                    items.join(",")
                )
            })
            .collect();
        if rows.is_empty() {
            println!("[]");
        } else {
            println!("[\n{}\n]", rows.join(",\n"));
        }
        return;
    }
    for b in runs {
        let date = b
            .entries
            .iter()
            .max_by_key(|e| e.epoch)
            .and_then(|e| e.date.as_deref())
            .unwrap_or("????-??-??T??:??:??");
        let first = b.entries.first().map(|e| e.original.as_path());
        let more = match b.entries.len() {
            0 | 1 => String::new(),
            n => format!(" (+{} more)", n - 1),
        };
        println!(
            "{}  {}  {:>4} item{}  {}{more}",
            b.id,
            date.replacen('T', " ", 1),
            b.entries.len(),
            if b.entries.len() == 1 { " " } else { "s" },
            first
                .map(Path::display)
                .map(|d| d.to_string())
                .unwrap_or_default(),
        );
    }
}

/// `{"batch": ID, "restored": [...], "failed": N}`.
fn batch_json(id: &str, restored: &[&list::Entry], failed: usize) -> String {
    let items: Vec<(&Path, &str, &Path)> = restored
        .iter()
        .map(|e| (e.original.as_path(), e.name.as_str(), e.dir.root.as_path()))
        .collect();
    let rest = crate::util::items_json("restored", &items, failed);
    format!(
        "{{\"batch\":\"{}\",{}",
        list::json_escape(id),
        rest.strip_prefix('{').unwrap_or(&rest)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trashdir::TrashDir;

    fn entry(path: &str, epoch: i64, batch: Option<&str>) -> list::Entry {
        list::Entry {
            date: None,
            epoch,
            original: PathBuf::from(path),
            name: path.rsplit('/').next().unwrap().to_string(),
            dir: TrashDir {
                root: PathBuf::from("/tmp/trash"),
                topdir: None,
            },
            batch: batch.map(str::to_string),
        }
    }

    #[test]
    fn runs_group_by_id_newest_first_and_skip_untagged() {
        let es = vec![
            entry("/a/old", 100, Some("100000-7")),
            entry("/a/z", 200, Some("200950-3")),
            entry("/a/y", 200, Some("200950-3")),
            entry("/a/same-second", 200, Some("200100-8")),
            entry("/a/gio", 300, None),
        ];
        let runs = batches(&es);
        let ids: Vec<&str> = runs.iter().map(|b| b.id.as_str()).collect();
        assert_eq!(ids, ["200950-3", "200100-8", "100000-7"]);
        let paths: Vec<&Path> = runs[0]
            .entries
            .iter()
            .map(|e| e.original.as_path())
            .collect();
        assert_eq!(paths, [Path::new("/a/y"), Path::new("/a/z")]);
    }
}
