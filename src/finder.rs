//! The macOS Finder Trash with Put Back.
//!
//! `rtrash finder put` asks Finder to trash files, so Finder records where
//! each came from and its own Put Back works. `list` and `restore` read
//! those records from the trash's `.DS_Store` (see [`crate::ds_store`]) and
//! move items back without Finder. Reading needs no macOS, so list and
//! restore run on any Unix against `--trash-dir`; put needs Finder.
//!
//! Untested on a real Mac so far: the parser passes on files written by
//! this crate and by the Python `ds_store` package, and put compiles for
//! macOS, but neither has run against Finder yet.

use std::fs;
use std::path::{Path, PathBuf};

use crate::ds_store;
use crate::restore::Conflict;
use crate::util::json_escape;

const HELP: &str = "\
Usage: {prog} put [-v] [-f] FILE...
  or:  {prog} list [--json] [--trash-dir=DIR]
  or:  {prog} restore [OPTION]... NAME|ORIGINAL...
Use the macOS Finder Trash, with Put Back.

put asks Finder to move each FILE to the Trash (macOS only), so Finder
records the original location and its own Put Back works. Symbolic links
get refused; use plain put for those.
list prints each item in the Finder Trash as 'NAME -> ORIGINAL-PATH',
reading the Put Back records from the trash's .DS_Store ('?' when none).
restore moves items back to their Put Back location, matched by the name
inside the trash or by the original path. It leaves .DS_Store alone;
Finder drops stale records itself.

      --trash-dir=DIR  use this Finder trash (default ~/.Trash); the volume
                     root for Put Back paths sits above a '.Trashes' parent
      --all        restore every item that has a Put Back record
  -f, --force      put: ignore missing files; restore: overwrite an
                     existing destination (same as --conflict=overwrite)
      --conflict=POLICY  refuse (default), overwrite, rename or skip
  -v, --verbose    put: name each trashed file
      --json       list: a JSON array; restore: {restored, failed, skipped}
      --help       display this help and exit
";

/// One item in a Finder trash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// Name inside the trash.
    pub name: String,
    /// Where Put Back would return it, when Finder recorded that.
    pub original: Option<PathBuf>,
}

/// The default Finder trash, `~/.Trash`.
pub fn home_trash() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(|h| PathBuf::from(h).join(".Trash"))
}

/// The volume root Put Back paths count from: the parent of a
/// `.Trashes` folder for a volume trash (`/Volumes/X/.Trashes/501`),
/// else `/`.
pub fn volume_root(trash: &Path) -> PathBuf {
    match trash.parent() {
        Some(p) if p.file_name().is_some_and(|n| n == ".Trashes") => {
            p.parent().unwrap_or(Path::new("/")).to_path_buf()
        }
        _ => PathBuf::from("/"),
    }
}

/// Every item in `trash`, sorted by name, with its Put Back location.
/// A missing or damaged `.DS_Store` leaves the locations unknown.
pub fn items(trash: &Path) -> std::io::Result<Vec<Item>> {
    let records = fs::read(trash.join(".DS_Store"))
        .ok()
        .and_then(|b| ds_store::parse(&b).ok())
        .unwrap_or_default();
    let pb = ds_store::put_back(&records);
    let root = volume_root(trash);
    let mut out = Vec::new();
    for ent in fs::read_dir(trash)? {
        let ent = ent?;
        let name = ent.file_name().to_string_lossy().into_owned();
        if name == ".DS_Store" {
            continue;
        }
        let original = pb.get(&name).and_then(|p| {
            let folder = p.folder.as_deref()?;
            let base = p.name.clone().unwrap_or_else(|| name.clone());
            Some(join_put_back(&root, folder, &base))
        });
        out.push(Item { name, original });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// `root/folder/name`, refusing to climb out of `root` through `..`.
fn join_put_back(root: &Path, folder: &str, name: &str) -> PathBuf {
    let mut p = root.to_path_buf();
    for part in folder.split('/').chain(std::iter::once(name)) {
        match part {
            "" | "." => {}
            ".." => {
                if p != root {
                    p.pop();
                }
            }
            s => p.push(s),
        }
    }
    p
}

pub fn run(prog: &str, args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("put") => put(&format!("{prog} put"), &args[1..]),
        Some("list") => list(&format!("{prog} list"), &args[1..]),
        Some("restore") => restore(&format!("{prog} restore"), &args[1..]),
        Some("--help") | Some("-h") => {
            print!("{}", HELP.replace("{prog}", prog));
            0
        }
        Some(other) => {
            eprintln!("{prog}: unknown action '{other}' (put, list or restore)");
            2
        }
        None => {
            eprintln!("{prog}: missing action (put, list or restore)");
            eprintln!("Try '{prog} --help' for more information.");
            2
        }
    }
}

fn trash_or_default(prog: &str, pinned: Option<PathBuf>) -> Result<PathBuf, i32> {
    match pinned.or_else(home_trash) {
        Some(t) if t.is_dir() => Ok(t),
        Some(t) => {
            eprintln!("{prog}: no Finder trash at '{}'", t.display());
            Err(1)
        }
        None => {
            eprintln!("{prog}: HOME unset; pass --trash-dir");
            Err(2)
        }
    }
}

fn list(prog: &str, args: &[String]) -> i32 {
    let mut json = false;
    let mut pinned = None;
    for a in args {
        match a.as_str() {
            "--json" => json = true,
            a if a.starts_with("--trash-dir=") => {
                pinned = Some(crate::util::arg_path(&a["--trash-dir=".len()..]))
            }
            a => {
                eprintln!("{prog}: unrecognized argument '{a}'");
                return 2;
            }
        }
    }
    let trash = match trash_or_default(prog, pinned) {
        Ok(t) => t,
        Err(c) => return c,
    };
    let items = match items(&trash) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("{prog}: cannot read '{}': {e}", trash.display());
            return 1;
        }
    };
    if json {
        let rows: Vec<String> = items
            .iter()
            .map(|i| {
                let orig = i.original.as_ref().map_or("null".to_string(), |o| {
                    format!("\"{}\"", json_escape(&o.to_string_lossy()))
                });
                format!(
                    "{{\"name\":\"{}\",\"original\":{orig}}}",
                    json_escape(&i.name)
                )
            })
            .collect();
        println!("[{}]", rows.join(","));
    } else {
        for i in &items {
            let orig = i
                .original
                .as_ref()
                .map_or("?".to_string(), |o| o.display().to_string());
            println!(
                "{} -> {}",
                crate::util::term_text(&i.name),
                crate::util::term_text(&orig)
            );
        }
    }
    0
}

fn restore(prog: &str, args: &[String]) -> i32 {
    let mut json = false;
    let mut all = false;
    let mut conflict = Conflict::Refuse;
    let mut pinned = None;
    let mut wanted: Vec<String> = Vec::new();
    for a in args {
        match a.as_str() {
            "--json" => json = true,
            "--all" => all = true,
            "-f" | "--force" => conflict = Conflict::Overwrite,
            a if a.starts_with("--conflict=") => match Conflict::parse(&a["--conflict=".len()..]) {
                Some(c) => conflict = c,
                None => {
                    eprintln!("{prog}: invalid --conflict '{}'", &a["--conflict=".len()..]);
                    return 2;
                }
            },
            a if a.starts_with("--trash-dir=") => {
                pinned = Some(crate::util::arg_path(&a["--trash-dir=".len()..]))
            }
            a if a.starts_with('-') && a.len() > 1 => {
                eprintln!("{prog}: unrecognized option '{a}'");
                return 2;
            }
            a => wanted.push(a.to_string()),
        }
    }
    if wanted.is_empty() && !all {
        eprintln!("{prog}: name items to restore, or pass --all");
        return 2;
    }
    let trash = match trash_or_default(prog, pinned) {
        Ok(t) => t,
        Err(c) => return c,
    };
    let items = match items(&trash) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("{prog}: cannot read '{}': {e}", trash.display());
            return 1;
        }
    };
    let mut chosen: Vec<&Item> = Vec::new();
    let mut failed = 0usize;
    if all {
        chosen.extend(items.iter().filter(|i| i.original.is_some()));
    }
    for w in &wanted {
        let hit = items
            .iter()
            .find(|i| i.name == *w || i.original.as_deref().is_some_and(|o| o == Path::new(w)));
        match hit {
            Some(i) if !chosen.contains(&i) => chosen.push(i),
            Some(_) => {}
            None => {
                eprintln!("{prog}: no item named '{w}' in '{}'", trash.display());
                failed += 1;
            }
        }
    }
    let mut done: Vec<(&Item, PathBuf)> = Vec::new();
    let mut skipped = 0usize;
    for item in chosen {
        let Some(dest) = item.original.clone() else {
            eprintln!("{prog}: '{}' has no Put Back record", item.name);
            failed += 1;
            continue;
        };
        let exists = dest.symlink_metadata().is_ok();
        let (dest, force) = match (exists, conflict) {
            (true, Conflict::Skip) => {
                eprintln!("{prog}: skipping '{}': it exists", dest.display());
                skipped += 1;
                continue;
            }
            (true, Conflict::Rename) => (crate::restore::free_name(&dest), false),
            (true, Conflict::Refuse) => {
                eprintln!(
                    "{prog}: refusing to overwrite existing '{}' (use -f to force)",
                    dest.display()
                );
                failed += 1;
                continue;
            }
            (_, c) => (dest, c == Conflict::Overwrite),
        };
        match move_back(&trash.join(&item.name), &dest, force) {
            Ok(()) => {
                if !json {
                    println!("restored '{}'", crate::util::term_path(&dest));
                }
                done.push((item, dest));
            }
            Err(e) => {
                eprintln!("{prog}: {e}");
                failed += 1;
            }
        }
    }
    if json {
        let rows: Vec<String> = done
            .iter()
            .map(|(i, d)| {
                format!(
                    "{{\"name\":\"{}\",\"restored_to\":\"{}\"}}",
                    json_escape(&i.name),
                    json_escape(&d.to_string_lossy())
                )
            })
            .collect();
        println!(
            "{{\"restored\":[{}],\"failed\":{failed},\"skipped\":{skipped}}}",
            rows.join(",")
        );
    }
    i32::from(failed > 0)
}

/// Move `src` to `dest`, creating the parent. With `force`, an existing
/// `dest` goes only after the item sits staged beside it.
fn move_back(src: &Path, dest: &Path, force: bool) -> Result<(), String> {
    let parent = dest
        .parent()
        .ok_or_else(|| format!("cannot restore to '{}': no parent", dest.display()))?;
    fs::create_dir_all(parent).map_err(|e| format!("cannot create '{}': {e}", parent.display()))?;
    if !force {
        return crate::trashdir::relocate(src, dest)
            .map_err(|e| format!("cannot restore to '{}': {e}", dest.display()));
    }
    let name = dest.file_name().unwrap_or_default().to_string_lossy();
    let staged = parent.join(format!(".rtrash-restore-{}-{name}", std::process::id()));
    crate::trashdir::relocate(src, &staged)
        .map_err(|e| format!("cannot stage '{}': {e}", src.display()))?;
    if dest.symlink_metadata().is_ok() {
        if let Err(e) = crate::trashdir::remove_any_path(dest) {
            let _ = crate::trashdir::relocate(&staged, src);
            return Err(format!("cannot remove existing '{}': {e}", dest.display()));
        }
    }
    fs::rename(&staged, dest).map_err(|e| format!("cannot place '{}': {e}", dest.display()))
}

#[cfg(target_os = "macos")]
fn put(prog: &str, args: &[String]) -> i32 {
    let mut force = false;
    let mut verbose = false;
    let mut files: Vec<PathBuf> = Vec::new();
    let mut opts_done = false;
    for a in args {
        match a.as_str() {
            "--" if !opts_done => opts_done = true,
            "-f" | "--force" if !opts_done => force = true,
            "-v" | "--verbose" if !opts_done => verbose = true,
            a if !opts_done && a.starts_with('-') && a.len() > 1 => {
                eprintln!("{prog}: unrecognized option '{a}'");
                return 2;
            }
            a => files.push(crate::util::arg_path(a)),
        }
    }
    let mut failed = false;
    let mut paths: Vec<PathBuf> = Vec::new();
    for f in files {
        match fs::symlink_metadata(&f) {
            Ok(m) if m.file_type().is_symlink() => {
                eprintln!(
                    "{prog}: refusing symbolic link '{}'; use rtrash put",
                    f.display()
                );
                failed = true;
            }
            Ok(_) => match std::env::current_dir().map(|cwd| cwd.join(&f)) {
                Ok(abs) => paths.push(abs),
                Err(e) => {
                    eprintln!("{prog}: cannot resolve '{}': {e}", f.display());
                    failed = true;
                }
            },
            Err(_) if force => {}
            Err(e) => {
                eprintln!("{prog}: cannot remove '{}': {e}", f.display());
                failed = true;
            }
        }
    }
    if !paths.is_empty() {
        // Paths travel as argv, never inside the script text, so no name
        // can inject AppleScript.
        const SCRIPT: &str = "on run argv
set targets to {}
repeat with p in argv
set end of targets to (POSIX file (contents of p))
end repeat
tell application \"Finder\" to delete targets
end run";
        let status = std::process::Command::new("osascript")
            .arg("-e")
            .arg(SCRIPT)
            .args(&paths)
            .stdout(std::process::Stdio::null())
            .status();
        match status {
            Ok(s) if s.success() => {
                if verbose {
                    for p in &paths {
                        println!("trashed '{}'", crate::util::term_path(p));
                    }
                }
            }
            Ok(s) => {
                eprintln!("{prog}: Finder refused the move (osascript {s})");
                failed = true;
            }
            Err(e) => {
                eprintln!("{prog}: cannot run osascript: {e}");
                failed = true;
            }
        }
    }
    i32::from(failed)
}

#[cfg(not(target_os = "macos"))]
fn put(prog: &str, _args: &[String]) -> i32 {
    eprintln!("{prog}: needs Finder, so macOS only; use rtrash put");
    1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ds_store::{Record, Value};

    fn ustr(name: &str, code: &[u8; 4], s: &str) -> Record {
        Record {
            name: name.into(),
            code: *code,
            value: Value::Ustr(s.into()),
        }
    }

    #[test]
    fn items_join_put_back_records_to_the_volume_root() {
        let base = std::env::temp_dir().join(format!("rtrash-finder-{}", std::process::id()));
        let trash = base.join("Vol/.Trashes/501");
        fs::create_dir_all(&trash).unwrap();
        fs::write(trash.join("a 10.11.12"), b"a").unwrap();
        fs::write(trash.join("b"), b"b").unwrap();
        fs::write(trash.join("evil"), b"e").unwrap();
        fs::write(trash.join("bare"), b"x").unwrap();
        let recs = [
            ustr("a 10.11.12", b"ptbL", "Users/ann/Desktop/"),
            ustr("a 10.11.12", b"ptbN", "a"),
            ustr("b", b"ptbL", "data/"),
            ustr("evil", b"ptbL", "../../../etc/"),
        ];
        fs::write(trash.join(".DS_Store"), ds_store::write(&recs)).unwrap();
        let root = base.join("Vol");
        assert_eq!(volume_root(&trash), root);
        assert_eq!(volume_root(Path::new("/Users/ann/.Trash")), Path::new("/"));
        let got = items(&trash).unwrap();
        let find = |n: &str| got.iter().find(|i| i.name == n).unwrap().original.clone();
        assert_eq!(find("a 10.11.12"), Some(root.join("Users/ann/Desktop/a")));
        assert_eq!(find("b"), Some(root.join("data/b")));
        assert_eq!(find("evil"), Some(root.join("etc/evil")), "no climbing out");
        assert_eq!(find("bare"), None);
        assert!(got.iter().all(|i| i.name != ".DS_Store"));
        let _ = fs::remove_dir_all(&base);
    }
}
