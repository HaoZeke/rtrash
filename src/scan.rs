//! Dirent-only live-tree scan for reclaimable artifact directories.
//!
//! Walks directory names, never regular-file contents. Records cargo `target/`,
//! `node_modules`, `.venv`, and any directory that contains `CACHEDIR.TAG`.
//! Prunes `.git` and never descends into `Zotero`.

use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

const HELP: &str = "\
Usage: {prog} scan [OPTION]... [DIR]...
List reclaimable artifact directories under DIR (default: cwd).
Walks directory names only: does not open regular files.

Records:
  - a directory that contains CACHEDIR.TAG
  - cargo target/ (next to Cargo.toml, or with CACHEDIR.TAG)
  - node_modules
  - .venv

Prunes .git. Never descends into Zotero (or --exclude names).
Stays on one filesystem unless --no-xdev.

Stdout is one absolute path per line, for `rtrash put --files-from`.

      --xdev            do not cross mount points (default)
      --no-xdev         follow other filesystems
      --exclude NAME    skip a directory basename (repeatable;
                     default: Zotero, pCloudDrive, .Trash, Trash, .git)
      --json            JSON array of {\"path\":...}
      -0, --zero        NUL-separate paths (for put --files-from -0)
      --help            display this help and exit
      --version         output version information and exit
";

const DEFAULT_EXCLUDE: &[&str] = &["Zotero", "pCloudDrive", ".Trash", "Trash", ".git"];

struct Opts {
    roots: Vec<PathBuf>,
    xdev: bool,
    extra_exclude: Vec<String>,
    json: bool,
    zero: bool,
}

fn usage_err(prog: &str, msg: &str) -> i32 {
    eprintln!("{prog}: {msg}");
    eprintln!("Try '{prog} scan --help' for more information.");
    2
}

fn is_excluded(name: &str, extra: &[String]) -> bool {
    DEFAULT_EXCLUDE.iter().any(|d| *d == name) || extra.iter().any(|d| d == name)
}

fn is_artifact(dir: &Path) -> bool {
    let name = dir.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if name == "node_modules" || name == ".venv" {
        return true;
    }
    let tag = dir.join("CACHEDIR.TAG");
    if tag.is_file() {
        return true;
    }
    if name == "target" {
        if let Some(parent) = dir.parent() {
            if parent.join("Cargo.toml").is_file() {
                return true;
            }
        }
    }
    false
}

fn walk(
    dir: &Path,
    root_dev: u64,
    xdev: bool,
    extra_exclude: &[String],
    out: &mut Vec<PathBuf>,
) -> io::Result<()> {
    let meta = match fs::symlink_metadata(dir) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Ok(());
    }
    if xdev && meta.dev() != root_dev {
        return Ok(());
    }
    let name = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if is_excluded(&name, extra_exclude) {
        return Ok(());
    }
    if is_artifact(dir) {
        out.push(dir.to_path_buf());
        return Ok(());
    }
    let rd = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => return Ok(()),
        Err(e) => return Err(e),
    };
    for ent in rd.flatten() {
        let ft = match ent.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if ft.is_dir() && !ft.is_symlink() {
            walk(&ent.path(), root_dev, xdev, extra_exclude, out)?;
        }
    }
    Ok(())
}

pub fn run(prog: &str, args: &[String]) -> i32 {
    let mut opts = Opts {
        roots: Vec::new(),
        xdev: true,
        extra_exclude: Vec::new(),
        json: false,
        zero: false,
    };
    let mut i = 0usize;
    while i < args.len() {
        let arg = &args[i];
        match arg.as_str() {
            "--help" | "-h" => {
                print!("{}", HELP.replace("{prog}", prog));
                return 0;
            }
            "--version" | "-V" => {
                println!("{prog} (rtrash) {}", env!("CARGO_PKG_VERSION"));
                return 0;
            }
            "--xdev" => opts.xdev = true,
            "--no-xdev" => opts.xdev = false,
            "--json" => opts.json = true,
            "-0" | "--zero" => opts.zero = true,
            "--exclude" => {
                i += 1;
                match args.get(i) {
                    Some(n) => opts.extra_exclude.push(n.clone()),
                    None => return usage_err(prog, "--exclude requires NAME"),
                }
            }
            a if a.starts_with("--exclude=") => {
                opts.extra_exclude.push(a["--exclude=".len()..].to_string());
            }
            a if a.starts_with('-') => {
                return usage_err(prog, &format!("unrecognized option '{a}'"));
            }
            a => opts.roots.push(PathBuf::from(a)),
        }
        i += 1;
    }
    if opts.roots.is_empty() {
        match std::env::current_dir() {
            Ok(p) => opts.roots.push(p),
            Err(e) => {
                eprintln!("{prog}: {e}");
                return 1;
            }
        }
    }

    let mut found = Vec::new();
    for root in &opts.roots {
        let abs = match root.canonicalize() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("{prog}: {}: {e}", root.display());
                return 1;
            }
        };
        let meta = match fs::symlink_metadata(&abs) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("{prog}: {}: {e}", abs.display());
                return 1;
            }
        };
        if let Err(e) = walk(&abs, meta.dev(), opts.xdev, &opts.extra_exclude, &mut found) {
            eprintln!("{prog}: {}: {e}", abs.display());
            return 1;
        }
    }
    found.sort();
    found.dedup();

    if opts.json {
        print!("[");
        for (i, p) in found.iter().enumerate() {
            if i > 0 {
                print!(",");
            }
            let s = p
                .display()
                .to_string()
                .replace('\\', "\\\\")
                .replace('"', "\\\"");
            print!("{{\"path\":\"{s}\"}}");
        }
        println!("]");
        return 0;
    }
    let sep = if opts.zero { "\0" } else { "\n" };
    for p in &found {
        print!("{}{sep}", p.display());
    }
    if !opts.zero && !found.is_empty() && !found.last().unwrap().as_os_str().is_empty() {
        // already printed newline per entry
    }
    0
}
