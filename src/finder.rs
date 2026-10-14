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

/// Largest `.DS_Store` read; the ones Finder writes stay far below it.
const DS_STORE_CAP: u64 = 64 << 20;

/// Every item in `trash`, sorted by name, with its Put Back location.
/// A missing or damaged `.DS_Store` leaves the locations unknown, as does
/// one that is no regular file or is larger than [`DS_STORE_CAP`]: a trash
/// on a foreign drive may hold a named pipe or a link to `/dev/zero` there.
pub fn items(trash: &Path) -> std::io::Result<Vec<Item>> {
    let records = crate::info::read_bounded(&trash.join(".DS_Store"), DS_STORE_CAP)
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
        Some("put") => logged("put", || put(&format!("{prog} put"), &args[1..])),
        Some("list") => list(&format!("{prog} list"), &args[1..]),
        Some("restore") => logged("restore", || {
            restore(&format!("{prog} restore"), &args[1..])
        }),
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

/// Run `f`, then log the trashes it changed under `event`; the explicit
/// `finder` actions log whatever the default trash is.
fn logged(event: &str, f: impl FnOnce() -> i32) -> i32 {
    let snap = crate::finder_total::snapshot_always();
    let rc = f();
    crate::finder_total::log(event, &snap);
    rc
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
pub(crate) fn move_back(src: &Path, dest: &Path, force: bool) -> Result<(), String> {
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
            let _ = crate::trashdir::relocate_back(&staged, src);
            return Err(format!("cannot remove existing '{}': {e}", dest.display()));
        }
    }
    fs::rename(&staged, dest).map_err(|e| format!("cannot place '{}': {e}", dest.display()))
}

/// Ask Finder to trash `paths`, so it records Put Back for each.
/// `RTRASH_OSASCRIPT` names another program to run in place of osascript
/// (tests use a stand-in that moves the files).
fn finder_delete(paths: &[PathBuf]) -> std::io::Result<std::process::ExitStatus> {
    // Paths travel as argv, never inside the script text, so no name
    // can inject AppleScript.
    const SCRIPT: &str = "on run argv
set targets to {}
repeat with p in argv
set end of targets to (POSIX file (contents of p))
end repeat
tell application \"Finder\" to delete targets
end run";
    let prog = std::env::var_os("RTRASH_OSASCRIPT").unwrap_or_else(|| "osascript".into());
    std::process::Command::new(prog)
        .arg("-e")
        .arg(SCRIPT)
        .args(paths)
        .stdout(std::process::Stdio::null())
        .status()
}

/// Whether plain put, list, restore and empty use the Finder Trash.
/// `RTRASH_MACOS_TRASH`, else `macos.trash` in the config, picks finder or
/// freedesktop; macOS defaults to finder and other systems to freedesktop.
/// A `--trash-dir` pin (`pinned`) always means the FreeDesktop layout.
pub fn is_default(pinned: bool) -> bool {
    if pinned {
        return false;
    }
    let env = std::env::var("RTRASH_MACOS_TRASH").ok();
    let choice = env
        .as_deref()
        .or_else(|| crate::config::get().str("macos", "trash"));
    match choice {
        Some("freedesktop") => false,
        Some("finder") => true,
        _ => cfg!(target_os = "macos"),
    }
}

/// Every Finder trash of this user that exists: `~/.Trash` first, then
/// `.Trashes/UID` at the root of each mounted volume. `RTRASH_FINDER_VOLUMES`
/// adds volume roots, separated by ':', for drives the mount table misses.
pub fn trashes() -> Vec<PathBuf> {
    let uid = unsafe { libc::getuid() };
    let mut out: Vec<PathBuf> = home_trash().into_iter().collect();
    let extra = std::env::var_os("RTRASH_FINDER_VOLUMES")
        .map(|v| std::env::split_paths(&v).collect::<Vec<_>>())
        .unwrap_or_default();
    for root in crate::trashdir::mount_points().into_iter().chain(extra) {
        let t = root.join(".Trashes").join(uid.to_string());
        if t.is_dir() && !out.contains(&t) {
            out.push(t);
        }
    }
    out
}

/// The Finder trash an item at `abs` goes to: `~/.Trash` on the home
/// volume, else `.Trashes/UID` at the root of the item's volume (the
/// highest ancestor on the same device).
pub fn trash_for(abs: &Path, dev: u64) -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    let home = home_trash()?;
    let home_dev = home
        .parent()
        .and_then(|h| fs::metadata(h).ok())
        .map(|m| m.dev());
    if home_dev == Some(dev) {
        return Some(home);
    }
    let mut root = abs.parent()?.to_path_buf();
    while let Some(up) = root.parent() {
        match fs::metadata(up) {
            Ok(m) if m.dev() == dev => root = up.to_path_buf(),
            _ => break,
        }
    }
    let uid = unsafe { libc::getuid() };
    Some(root.join(".Trashes").join(uid.to_string()))
}

/// Trash `abs` into its Finder trash for plain put; returns the name it
/// got there and the trash. Finder moves it and records Put Back; a
/// symbolic link, which Finder would follow, moves in as itself without a
/// record. An item on an external drive goes to that drive's trash, as
/// Finder does it.
pub fn put_item(abs: &Path, meta: &fs::Metadata) -> Result<(String, PathBuf), String> {
    use std::os::unix::fs::MetadataExt;
    let trash = trash_for(abs, meta.dev()).ok_or("HOME unset")?;
    let names = |t: &Path| -> std::collections::HashSet<std::ffi::OsString> {
        fs::read_dir(t)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.file_name())
            .collect()
    };
    if meta.file_type().is_symlink() {
        fs::create_dir_all(&trash)
            .map_err(|e| format!("cannot create '{}': {e}", trash.display()))?;
        let name = abs.file_name().ok_or("no file name")?;
        let mut dest = trash.join(name);
        if dest.symlink_metadata().is_ok() {
            dest = crate::restore::free_name(&dest);
        }
        fs::rename(abs, &dest).map_err(|e| e.to_string())?;
        let got = dest.file_name().unwrap_or_default();
        return Ok((got.to_string_lossy().into_owned(), trash));
    }
    let before = names(&trash);
    match finder_delete(&[abs.to_path_buf()]) {
        Ok(s) if s.success() => {}
        Ok(s) => return Err(format!("Finder refused the move (osascript {s})")),
        Err(e) => return Err(format!("cannot run osascript: {e}")),
    }
    if abs.symlink_metadata().is_ok() {
        return Err("Finder left it in place".into());
    }
    let mut new: Vec<_> = names(&trash).difference(&before).cloned().collect();
    new.sort();
    let name = new
        .into_iter()
        .find(|n| n != ".DS_Store")
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    Ok((name, trash))
}

fn finder_only(prog: &str, what: &str) -> i32 {
    eprintln!(
        "{prog}: {what} does not apply to the Finder Trash; pin --trash-dir or set RTRASH_MACOS_TRASH=freedesktop"
    );
    2
}

fn read_all(prog: &str) -> Result<Vec<crate::finder_shared::Entry>, i32> {
    crate::finder_shared::entries().map_err(|e| {
        eprintln!("{prog}: cannot read the Finder Trash: {e}");
        1
    })
}

/// `list` on the Finder trashes: `DATE ORIGINAL` rows, oldest first, as
/// the FreeDesktop listing prints them (`--json` too).
pub fn default_list(
    prog: &str,
    json: bool,
    refused: Option<&str>,
    filter: crate::finder_shared::Filter,
) -> i32 {
    if let Some(what) = refused {
        return finder_only(prog, what);
    }
    let rows = match read_all(prog) {
        Ok(r) => filter.apply(r),
        Err(c) => return c,
    };
    if json {
        let out: Vec<String> = rows
            .iter()
            .map(|e| {
                let orig = e.original.as_ref().map_or("null".to_string(), |o| {
                    format!("\"{}\"", json_escape(&o.to_string_lossy()))
                });
                format!(
                    "{{\"date\":\"{}\",\"name\":\"{}\",\"trash\":\"{}\",\"original\":{orig}}}",
                    e.date,
                    json_escape(&e.name),
                    json_escape(&e.trash.to_string_lossy())
                )
            })
            .collect();
        println!("[{}]", out.join(","));
        return 0;
    }
    for e in &rows {
        println!(
            "{} {}",
            e.date.replacen('T', " ", 1),
            crate::util::term_path(&e.shown)
        );
    }
    0
}

/// What `restore` asks of the Finder trashes.
pub(crate) struct RestoreOpts<'a> {
    /// PATH: a Put Back place or a name in the trash.
    pub(crate) target: Option<&'a Path>,
    /// `--name`, `--glob` or `--regex`, tested on the Put Back place.
    pub(crate) matcher: Option<&'a crate::restore::Matcher>,
    pub(crate) all: bool,
    pub(crate) latest: bool,
    /// `--to=DIR`: restore there, keeping each file name.
    pub(crate) to: Option<&'a Path>,
    pub(crate) conflict: Conflict,
    pub(crate) json: bool,
    pub(crate) plain: bool,
    pub(crate) newest_first: Option<bool>,
}

/// `restore` on the Finder trashes. PATH names the item by its Put Back
/// place or its name in the trash, and `--name`, `--glob` and `--regex`
/// match as on the FreeDesktop trash; several matches need `--all` or
/// `--latest`. Without either, a numbered table offers every item with a
/// Put Back record and stdin picks, or on a terminal the restore browser
/// opens unless `plain`.
pub(crate) fn default_restore(prog: &str, o: &RestoreOpts) -> i32 {
    let rows = match read_all(prog) {
        Ok(r) => r,
        Err(c) => return c,
    };
    // Without --to, only an item with a Put Back record has a place to go.
    let placed: Vec<&crate::finder_shared::Entry> = rows
        .iter()
        .filter(|e| e.original.is_some() || o.to.is_some())
        .collect();
    let mut chosen: Vec<&crate::finder_shared::Entry> = if let Some(m) = o.matcher {
        placed
            .iter()
            .copied()
            .filter(|e| m.matches_parts(&e.shown, &e.name))
            .collect()
    } else if let Some(t) = o.target {
        let abs = std::env::current_dir()
            .map(|c| c.join(t))
            .unwrap_or_else(|_| t.to_path_buf());
        rows.iter()
            .filter(|e| e.original.as_deref() == Some(abs.as_path()) || Path::new(&e.name) == t)
            .collect()
    } else if o.all {
        placed.clone()
    } else {
        if placed.is_empty() {
            eprintln!("{prog}: no trashed files match");
            return 1;
        }
        #[cfg(feature = "tui")]
        if !o.plain && !o.json && o.to.is_none() && crate::util::stdin_is_tty() {
            let newest = o.newest_first.unwrap_or(true);
            let mut refs = placed.clone();
            if newest {
                refs.reverse();
            }
            return crate::restore_tui::run(prog, refs, o.conflict == Conflict::Overwrite, newest);
        }
        let _ = (o.plain, o.newest_first);
        for (k, e) in placed.iter().enumerate() {
            println!(
                "{k:4}  {}  {}",
                e.date.replacen('T', " ", 1),
                crate::util::term_path(&e.shown)
            );
        }
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_err() || line.trim().is_empty() {
            eprintln!("{prog}: provide an index 0..{} on stdin", placed.len() - 1);
            return 1;
        }
        match crate::restore::parse_selection(&line, placed.len()) {
            Ok(idx) => idx.into_iter().map(|k| placed[k]).collect(),
            Err(e) => {
                eprintln!("{prog}: invalid selection: {e}");
                return 1;
            }
        }
    };
    if chosen.is_empty() {
        eprintln!("{prog}: no trashed files match");
        return 1;
    }
    // Rows come oldest first, so the newest match is the last.
    if o.latest {
        chosen = vec![*chosen.last().unwrap()];
    } else if chosen.len() > 1 && !o.all && (o.matcher.is_some() || o.target.is_some()) {
        if o.target.is_some() {
            // Several items with one Put Back place: the newest, as before.
            chosen = vec![*chosen.last().unwrap()];
        } else {
            eprintln!(
                "{prog}: {} items match; pass --latest or --all",
                chosen.len()
            );
            return 1;
        }
    }
    let conflict = o.conflict;
    let json = o.json;
    let (mut done, mut failed, mut skipped) = (Vec::new(), 0usize, 0usize);
    for e in chosen {
        let to_dir = o.to.map(|d| {
            let d = if d.is_absolute() {
                d.to_path_buf()
            } else {
                std::env::current_dir().unwrap_or_default().join(d)
            };
            d.join(e.shown.file_name().unwrap_or_default())
        });
        let Some(dest) = to_dir.or_else(|| e.original.clone()) else {
            eprintln!("{prog}: '{}' has no Put Back record", e.name);
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
            (_, c) => (dest, c == Conflict::Overwrite),
        };
        match e.restore_to(&dest, force) {
            Ok(()) => {
                if !json {
                    println!("restored '{}'", crate::util::term_path(&dest));
                }
                done.push((e, dest));
            }
            Err(err) => {
                eprintln!("{prog}: {err}");
                failed += 1;
            }
        }
    }
    if json {
        let rows: Vec<String> = done
            .iter()
            .map(|(e, d)| {
                format!(
                    "{{\"name\":\"{}\",\"restored_to\":\"{}\"}}",
                    json_escape(&e.name),
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

/// `empty` on the Finder trashes: remove every item, or with `cutoff`
/// only those trashed before it. `.DS_Store` stays; Finder drops stale
/// records.
/// What `empty` takes on the Finder Trash: an age cut, a size budget
/// and which items the budget removes first.
pub struct EmptyOpts {
    pub cutoff: Option<i64>,
    pub max_size: Option<u64>,
    pub prune: crate::retention::Policy,
    pub dry_run: bool,
    pub verbose: bool,
    pub json: bool,
}

/// The items `empty` removes: those older than the cutoff (all of them
/// without one, unless a budget is set), then, under a budget, the
/// oldest or largest of the rest until the others fit in it.
fn empty_pick(
    rows: Vec<(crate::finder_shared::Entry, u64)>,
    cutoff: Option<i64>,
    max: Option<u64>,
    prune: crate::retention::Policy,
) -> Vec<(crate::finder_shared::Entry, u64)> {
    let age_pass = cutoff.is_some() || max.is_none();
    let (mut gone, mut rest): (Vec<_>, Vec<_>) = rows
        .into_iter()
        .partition(|(e, _)| age_pass && cutoff.is_none_or(|c| e.epoch < c));
    if let Some(max) = max {
        match prune {
            crate::retention::Policy::Oldest => rest.sort_by_key(|(e, _)| e.epoch),
            crate::retention::Policy::Largest => rest.sort_by_key(|(_, b)| std::cmp::Reverse(*b)),
        }
        let mut left: u64 = rest.iter().map(|(_, b)| b).sum();
        for r in rest {
            if left <= max {
                break;
            }
            left -= r.1;
            gone.push(r);
        }
    }
    gone
}

pub fn default_empty(prog: &str, o: &EmptyOpts) -> i32 {
    let (dry_run, verbose, json) = (o.dry_run, o.verbose, o.json);
    let rows = match read_all(prog) {
        Ok(r) => r,
        Err(c) => return c,
    };
    let rows: Vec<_> = rows
        .into_iter()
        .map(|e| {
            let b = crate::fastdelete::disk_usage(&e.path());
            (e, b)
        })
        .collect();
    let mut removed = 0u64;
    let mut bytes = 0u64;
    let mut failed = 0u64;
    for (e, b) in empty_pick(rows, o.cutoff, o.max_size, o.prune) {
        bytes += b;
        let path = e.path();
        if dry_run {
            if !json {
                println!("would remove {}", crate::util::term_path(&path));
            }
            removed += 1;
            continue;
        }
        match crate::trashdir::remove_any_path(&path) {
            Ok(()) => {
                crate::finder_total::carry(&e.trash, b, false);
                removed += 1;
                if verbose && !json {
                    println!("removed {}", crate::util::term_path(&path));
                }
            }
            Err(err) => {
                // Part of the item may be gone, so measure afterwards.
                crate::finder_total::uncarried(&e.trash);
                eprintln!("{prog}: cannot remove '{}': {err}", path.display());
                failed += 1;
            }
        }
    }
    if json {
        println!(
            "{{\"dry_run\":{dry_run},\"removed\":{removed},\"bytes\":{bytes},\"failed\":{failed}}}"
        );
    } else if !verbose && !dry_run {
        eprintln!("Removed {}", crate::util::plural(removed, "item", "items"));
    }
    i32::from(failed > 0)
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
        let status = finder_delete(&paths);
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

    #[test]
    fn trash_for_picks_home_or_the_volume_root() {
        use std::os::unix::fs::MetadataExt;
        let home = std::env::temp_dir().join(format!("rtrash-tf-{}", std::process::id()));
        fs::create_dir_all(&home).unwrap();
        // HOME is process-wide; read it back instead of changing it.
        let Some(h) = home_trash() else { return };
        let hdev = fs::metadata(h.parent().unwrap()).map(|m| m.dev());
        if let Ok(dev) = hdev {
            assert_eq!(trash_for(&h.parent().unwrap().join("x"), dev), Some(h));
        }
        // A device no ancestor has stops at the parent directory.
        let f = home.join("sub/f");
        let got = trash_for(&f, u64::MAX).unwrap();
        assert!(got.starts_with(home.join("sub/.Trashes")), "{got:?}");
        let _ = fs::remove_dir_all(&home);
    }
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
