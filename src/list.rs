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
      --help       display this help and exit
      --version    output version information and exit
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
    for entry in collect(&trashdir::resolve_dirs_opts(&trash_dirs, home_only)) {
        let date = entry.date.as_deref().unwrap_or("????-??-??T??:??:??");
        // trash-list prints "YYYY-MM-DD HH:MM:SS path".
        println!(
            "{} {}",
            date.replacen('T', " ", 1),
            entry.original.display()
        );
    }
    0
}
