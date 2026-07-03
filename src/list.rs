use std::fs;

use crate::info;
use crate::trashdir::{self, TrashDir};

const HELP: &str = "\
Usage: {prog} [OPTION]...
List trashed items as 'DELETION-DATE ORIGINAL-PATH', oldest first.

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
            out.push(Entry {
                original: dir.resolve_original(&parsed.path),
                date: parsed.deletion_date,
                epoch,
                name: name.to_string(),
                dir: dir.clone(),
            });
        }
    }
    out.sort_by(|a, b| a.epoch.cmp(&b.epoch).then_with(|| a.original.cmp(&b.original)));
    out
}

pub fn run(prog: &str, args: &[String]) -> i32 {
    if let Some(arg) = args.first() {
        match arg.as_str() {
            "--help" => {
                print!("{}", HELP.replace("{prog}", prog));
                return 0;
            }
            "--version" => {
                println!("{prog} (rtrash) {}", env!("CARGO_PKG_VERSION"));
                return 0;
            }
            a => {
                eprintln!("{prog}: unrecognized option '{a}'");
                return 2;
            }
        }
    }
    for entry in collect(&trashdir::all()) {
        let date = entry.date.as_deref().unwrap_or("????-??-??T??:??:??");
        // trash-list prints "YYYY-MM-DD HH:MM:SS path".
        println!("{} {}", date.replacen('T', " ", 1), entry.original.display());
    }
    0
}
