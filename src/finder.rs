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
    // Finder writes .DS_Store lazily, and on a fresh account not at all
    // for a while; the put log knows where rtrash's own puts came from.
    let logged = crate::finder_shared::read_log();
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
        let original = original.map(|p| firmlink_short(&p)).or_else(|| {
            logged
                .iter()
                .rev()
                .find(|l| l.trash == trash && l.name == name)
                .map(|l| l.original.clone())
        });
        out.push(Item { name, original });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// `p` without the `/System/Volumes/Data` prefix when the shorter path
/// names the same place: macOS firmlinks show the data volume's
/// `/Users`, `/Applications` and the like at the root, and Finder's
/// Put Back folders count from the data volume.
pub fn firmlink_short(p: &Path) -> PathBuf {
    firmlink_in(p, Path::new("/System/Volumes/Data"), Path::new("/"))
}

/// [`firmlink_short`] with the data volume at `data` and the root at
/// `root`, so a test can stand them up in a scratch folder.
fn firmlink_in(p: &Path, data: &Path, root: &Path) -> PathBuf {
    use std::os::unix::fs::MetadataExt;
    let Ok(rest) = p.strip_prefix(data) else {
        return p.to_path_buf();
    };
    // The item itself is in the trash, so check its first folder.
    let Some(first) = rest.components().next() else {
        return p.to_path_buf();
    };
    let same = match (
        fs::metadata(root.join(first)),
        fs::metadata(data.join(first)),
    ) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    };
    if same {
        root.join(rest)
    } else {
        p.to_path_buf()
    }
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
        let hit = items.iter().find(|i| {
            crate::finder_shared::nfc(&i.name) == crate::finder_shared::nfc(w)
                || i.original
                    .as_deref()
                    .is_some_and(|o| crate::finder_shared::same_path(o, &crate::util::arg_path(w)))
        });
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
    // Carry the trash's total: the item's size leaves with it, and a
    // failure, which may leave part of it behind, measures the trash.
    let trash = src.parent().unwrap_or(src).to_path_buf();
    let size = crate::finder_total::measure_item(src);
    let r = move_back_inner(src, dest, force);
    if src.symlink_metadata().is_err() && r.is_ok() {
        crate::finder_total::carry(&trash, size, false);
    } else if src.symlink_metadata().is_err() {
        crate::finder_total::uncarried(&trash);
    }
    r
}

fn move_back_inner(src: &Path, dest: &Path, force: bool) -> Result<(), String> {
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

/// The folders in `dir`, by name; none when it cannot be read.
fn dirs_in(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        // `/Volumes/Macintosh HD` links back to `/`; skip links.
        .filter(|p| fs::symlink_metadata(p).is_ok_and(|m| m.is_dir()))
        .collect();
    out.sort();
    out
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
    // The mount table reader is Linux-only; on macOS every drive Finder
    // shows is a folder of /Volumes.
    let volumes = if cfg!(target_os = "macos") {
        dirs_in(Path::new("/Volumes"))
    } else {
        Vec::new()
    };
    for root in crate::trashdir::mount_points()
        .into_iter()
        .chain(volumes)
        .chain(extra)
    {
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
/// the FreeDesktop listing prints them (`--json` too). `last` keeps only
/// the newest rows, as tail -n would.
/// How `list` prints the Finder Trash rows.
pub struct ListShape<'a> {
    pub json: bool,
    pub format: Option<&'a [crate::list::FmtPart]>,
    /// `--last=N`: only the last N rows.
    pub last: Option<usize>,
    /// `--depth=N`: the tree under each trashed folder, N levels down.
    pub depth: usize,
    /// `--show-trashpath`: where each item sits in the Finder Trash.
    pub show_trash: bool,
}

pub fn default_list(
    prog: &str,
    shape: ListShape<'_>,
    refused: Option<&str>,
    filter: crate::finder_shared::Filter,
    keep: &dyn Fn(&Path, &str) -> bool,
) -> i32 {
    let ListShape {
        json,
        format,
        last,
        depth,
        show_trash,
    } = shape;
    if let Some(what) = refused {
        return finder_only(prog, what);
    }
    let rows = match read_all(prog) {
        Ok(r) => filter.apply(r),
        Err(c) => return c,
    };
    let mut rows: Vec<_> = rows
        .into_iter()
        .filter(|e| keep(&e.shown, &e.name))
        .collect();
    if let Some(n) = last {
        let cut = rows.len().saturating_sub(n);
        rows.drain(..cut);
    }
    if json {
        let out: Vec<String> = rows
            .iter()
            .map(|e| {
                let orig = e.original.as_ref().map_or("null".to_string(), |o| {
                    format!("\"{}\"", json_escape(&o.to_string_lossy()))
                });
                let at = if show_trash {
                    let p = json_escape(&e.path().to_string_lossy());
                    format!(",\"trash_path\":\"{p}\"")
                } else {
                    String::new()
                };
                format!(
                    "{{\"date\":\"{}\",\"name\":\"{}\",\"trash\":\"{}\",\"original\":{orig}{at}}}",
                    e.date,
                    json_escape(&e.name),
                    json_escape(&e.trash.to_string_lossy())
                )
            })
            .collect();
        println!("[{}]", out.join(","));
        return 0;
    }
    if let Some(parts) = format {
        use std::io::Write;
        let wants_size = parts.contains(&crate::list::FmtPart::Field(crate::list::Field::Size));
        let mut out = std::io::BufWriter::new(std::io::stdout().lock());
        for e in &rows {
            let row = crate::list::FmtRow {
                date: &e.date,
                epoch: e.epoch,
                path: &e.shown,
                name: &e.name,
                trash: &e.trash,
                batch: "",
                size: wants_size.then(|| crate::fastdelete::disk_usage(&e.path())),
                owner: None,
            };
            if out
                .write_all(&crate::list::render_format(parts, &row))
                .is_err()
            {
                return 0;
            }
        }
        let _ = out.flush();
        return 0;
    }
    use std::io::Write;
    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    for e in &rows {
        let date = e.date.replacen('T', " ", 1);
        let res = writeln!(out, "{date} {}", crate::util::term_path(&e.shown))
            .and_then(|()| crate::listfmt::trash_line(&mut out, &e.path(), show_trash))
            .and_then(|()| crate::list::write_tree(&mut out, &e.path(), depth));
        if res.is_err() {
            return 0;
        }
    }
    let _ = out.flush();
    0
}

/// What `restore` asks of the Finder trashes.
pub(crate) struct RestoreOpts<'a> {
    /// PATH: a Put Back place or a name in the trash.
    pub(crate) target: Option<&'a Path>,
    /// `--name`, `--glob` or `--regex`, tested on the Put Back place.
    pub(crate) matcher: Option<&'a crate::restore::Matcher>,
    /// `--directory=DIR`: only items whose Put Back place is under DIR.
    pub(crate) directory: Option<&'a Path>,
    /// trash-restore's own scope: with nothing in it, say so as trash-cli does.
    pub(crate) cwd_note: bool,
    /// trash-restore takes PATH as a scope: the item there and every item
    /// trashed from under it.
    pub(crate) path_scope: bool,
    pub(crate) all: bool,
    pub(crate) latest: bool,
    /// `--to=DIR`: restore there, keeping each file name.
    pub(crate) to: Option<&'a Path>,
    pub(crate) conflict: Conflict,
    pub(crate) json: bool,
    pub(crate) plain: bool,
    pub(crate) newest_first: Option<bool>,
    /// `--sort`: the numbered list order (date by default, path, or none).
    pub(crate) sort: Option<crate::restore::Sort>,
    /// `--older-than` and `--newer-than`.
    pub(crate) age: crate::filters::AgeFilter,
    /// `--larger-than` and `--smaller-than`, on each item's disk usage.
    pub(crate) size: crate::filters::SizeFilter,
    /// `--dry-run`: say what each restore would do and move nothing.
    pub(crate) dry_run: bool,
}

/// Nothing matched: say so, and with `--json` print the empty summary
/// the FreeDesktop restore prints, so a script always gets one object.
fn no_match(prog: &str, json: bool) -> i32 {
    eprintln!("{prog}: no trashed files match");
    if json {
        println!("{{\"restored\":[],\"failed\":0,\"skipped\":0}}");
    }
    1
}

/// The numbered pick over `list` (oldest first): the restore browser on a
/// terminal, else rows and an answer on stdin. `Err` carries the exit code.
fn pick<'a>(
    prog: &str,
    list: &[&'a crate::finder_shared::Entry],
    o: &RestoreOpts,
) -> Result<Vec<&'a crate::finder_shared::Entry>, i32> {
    #[cfg(feature = "tui")]
    if !o.plain && !o.json && !o.dry_run && o.to.is_none() && crate::util::stdin_is_tty() {
        let newest = o.newest_first.unwrap_or(true);
        let mut refs = list.to_vec();
        if newest {
            refs.reverse();
        }
        return Err(crate::restore_tui::run(
            prog,
            refs,
            o.conflict == Conflict::Overwrite,
            newest,
        ));
    }
    let _ = o.plain;
    // The numbered list order, as on FreeDesktop: by date, oldest first
    // unless --newest-first; by path; or as read with --sort=none.
    let mut list = list.to_vec();
    match o.sort {
        Some(crate::restore::Sort::Path) => list.sort_by(|a, b| a.shown.cmp(&b.shown)),
        // Largest first; the rows come oldest first, so ties keep that.
        Some(crate::restore::Sort::Size) => {
            list.sort_by_cached_key(|e| std::cmp::Reverse(crate::fastdelete::disk_usage(&e.path())))
        }
        Some(crate::restore::Sort::None) => {}
        Some(crate::restore::Sort::Date) | None => {
            list.sort_by(|a, b| a.epoch.cmp(&b.epoch).then_with(|| a.name.cmp(&b.name)));
            if o.newest_first.unwrap_or(false) {
                list.reverse();
            }
        }
    }
    // trash-cli separates the columns with one space.
    let gap = if o.path_scope { " " } else { "  " };
    for (k, e) in list.iter().enumerate() {
        println!(
            "{k:4}{gap}{}{gap}{}",
            e.date.replacen('T', " ", 1),
            crate::util::term_path(&e.shown)
        );
    }
    // trash-cli's prompt, only for someone typing at a terminal.
    if o.path_scope && crate::util::stdin_is_tty() {
        eprint!("What file to restore [0..{}]: ", list.len() - 1);
    }
    let mut line = String::new();
    let read = std::io::stdin().read_line(&mut line);
    if read.is_ok() && line.trim().is_empty() && o.path_scope {
        println!("No files were restored");
        return Err(0);
    }
    if read.is_err() || line.trim().is_empty() {
        eprintln!("{prog}: provide an index 0..{} on stdin", list.len() - 1);
        return Err(1);
    }
    match crate::restore::parse_selection(&line, list.len()) {
        Ok(idx) => Ok(idx.into_iter().map(|k| list[k]).collect()),
        Err(e) => {
            eprintln!("{prog}: invalid selection: {e}");
            Err(1)
        }
    }
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
    let now = crate::util::now_epoch();
    let rows: Vec<_> = rows
        .into_iter()
        .filter(|e| o.age.keeps_at(Some(e.epoch), now))
        .filter(|e| !o.size.is_set() || o.size.keeps(crate::fastdelete::disk_usage(&e.path())))
        .collect();
    // Without --to, only an item with a Put Back record has a place to go.
    // --directory keeps the items from under DIR, which may be gone.
    let placed: Vec<&crate::finder_shared::Entry> = rows
        .iter()
        .filter(|e| e.original.is_some() || o.to.is_some())
        .filter(|e| {
            o.directory
                .is_none_or(|d| crate::finder_shared::under(&e.shown, d))
        })
        .collect();
    let mut chosen: Vec<&crate::finder_shared::Entry> = if let Some(m) = o.matcher {
        placed
            .iter()
            .copied()
            .filter(|e| m.matches_parts(&e.shown, &crate::finder_shared::nfc(&e.name)))
            .collect()
    } else if let Some(t) = o.target {
        let abs = std::env::current_dir()
            .map(|c| c.join(t))
            .unwrap_or_else(|_| t.to_path_buf());
        let matched: Vec<&crate::finder_shared::Entry> = rows
            .iter()
            .filter(|e| {
                e.original
                    .as_deref()
                    .is_some_and(|o| crate::finder_shared::same_path(o, &abs))
                    || crate::finder_shared::same_path(Path::new(&e.name), t)
                    || (o.path_scope
                        && e.original.is_some()
                        && crate::finder_shared::under(&e.shown, &abs))
            })
            .collect();
        // trash-restore asks even on one match, as trash-cli does.
        if o.path_scope && !o.all && !o.latest && !matched.is_empty() {
            match pick(prog, &matched, o) {
                Ok(v) => v,
                Err(c) => return c,
            }
        } else {
            matched
        }
    } else if o.all {
        if placed.is_empty() && o.cwd_note {
            let cwd = std::env::current_dir().unwrap_or_default();
            println!("No files trashed from current dir ('{}')", cwd.display());
            return 1;
        }
        placed.clone()
    } else {
        if placed.is_empty() && o.cwd_note {
            let cwd = std::env::current_dir().unwrap_or_default();
            println!("No files trashed from current dir ('{}')", cwd.display());
            return 1;
        }
        if placed.is_empty() {
            return no_match(prog, o.json);
        }
        match pick(prog, &placed, o) {
            Ok(v) => v,
            Err(c) => return c,
        }
    };
    if chosen.is_empty() {
        return no_match(prog, o.json);
    }
    // Rows come oldest first, so the newest match is the last.
    if o.latest {
        chosen = vec![*chosen.last().unwrap()];
    } else if chosen.len() > 1
        && !o.all
        && !(o.path_scope && o.target.is_some())
        && (o.matcher.is_some() || o.target.is_some())
    {
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
    // A dry-run line: stdout, or stderr when stdout carries the JSON.
    let say = |line: String| {
        if json {
            eprintln!("{line}");
        } else {
            println!("{line}");
        }
    };
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
                if o.dry_run {
                    say(format!(
                        "would skip '{}': it exists",
                        crate::util::term_path(&dest)
                    ));
                } else {
                    eprintln!("{prog}: skipping '{}': it exists", dest.display());
                }
                skipped += 1;
                continue;
            }
            (true, Conflict::Rename) => (crate::restore::free_name(&dest), false),
            (_, c) => (dest, c == Conflict::Overwrite),
        };
        if o.dry_run {
            if e.path().symlink_metadata().is_err() {
                say(format!(
                    "would fail: '{}' is gone from the trash",
                    crate::util::term_path(&e.shown)
                ));
                failed += 1;
            } else if !force && dest.symlink_metadata().is_ok() {
                say(format!(
                    "would fail: '{}' exists",
                    crate::util::term_path(&dest)
                ));
                failed += 1;
            } else {
                let from = e.original.as_deref().unwrap_or(&e.shown);
                if from == dest.as_path() {
                    say(format!("would restore '{}'", crate::util::term_path(&dest)));
                } else {
                    say(format!(
                        "would restore '{}' as '{}'",
                        crate::util::term_path(from),
                        crate::util::term_path(&dest)
                    ));
                }
                done.push((e, dest));
            }
            continue;
        }
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
        let dry = if o.dry_run { "\"dry_run\":true," } else { "" };
        println!(
            "{{{dry}\"restored\":[{}],\"failed\":{failed},\"skipped\":{skipped}}}",
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

/// With a trash budget (`RTRASH_MAX_SIZE` or `policy.max_size`), bring
/// each Finder trash this put used back under it, oldest (or largest)
/// first. The put's own items, given by trash and name, count but stay.
pub fn prune_after_put(prog: &str, moved: &[(PathBuf, String)], quiet: bool) {
    let Some((max, policy, source)) = crate::budget::budget(prog) else {
        return;
    };
    let Ok(all) = crate::finder_shared::entries() else {
        return;
    };
    let mine: Vec<PathBuf> = moved.iter().map(|(t, n)| t.join(n)).collect();
    let mut trashes: Vec<&PathBuf> = moved.iter().map(|(t, _)| t).collect();
    trashes.sort();
    trashes.dedup();
    for trash in trashes {
        let here: Vec<&crate::finder_shared::Entry> =
            all.iter().filter(|e| &e.trash == trash).collect();
        let rows: Vec<(PathBuf, i64, u64)> = here
            .iter()
            .map(|e| {
                let p = e.path();
                let b = crate::fastdelete::disk_usage(&p);
                (p, e.epoch, b)
            })
            .collect();
        let picked = crate::budget::trim_after_put(&rows, &mine, max, policy);
        let mut n = 0u64;
        for i in picked {
            let (path, _, b) = &rows[i];
            let inodes = 1 + crate::trash_stats::count_inodes(path);
            match crate::trashdir::remove_any_path(path) {
                Ok(()) => {
                    crate::finder_total::carry(trash, (*b, inodes), false);
                    n += 1;
                }
                Err(err) => {
                    crate::finder_total::uncarried(trash);
                    eprintln!("{prog}: cannot remove '{}': {err}", path.display());
                }
            }
        }
        if n > 0 && !quiet {
            eprintln!(
                "{prog}: removed {} from '{}' to keep it under {} ({source})",
                crate::util::plural(n, "item", "items"),
                trash.display(),
                crate::util::format_bytes(max)
            );
        }
    }
}

/// `reap --purge-trash` on the Finder trashes of device `dev`: remove
/// the oldest items until `need` (bytes, inodes) is freed, and return
/// what is still needed. A dry run counts the items instead.
pub fn purge_oldest(prog: &str, dev: u64, need: (u64, u64), dry_run: bool) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    let Ok(rows) = read_all(prog) else {
        return need;
    };
    let mut rows: Vec<_> = rows
        .into_iter()
        .filter(|e| std::fs::metadata(&e.trash).is_ok_and(|m| m.dev() == dev))
        .collect();
    rows.sort_by_key(|e| e.epoch);
    let mut left = need;
    let (mut n, mut freed) = (0u64, 0u64);
    for e in rows {
        if left == (0, 0) {
            break;
        }
        let path = e.path();
        let b = crate::fastdelete::disk_usage(&path);
        let inodes = 1 + crate::trash_stats::count_inodes(&path);
        if !dry_run {
            if let Err(err) = crate::trashdir::remove_any_path(&path) {
                crate::finder_total::uncarried(&e.trash);
                eprintln!("{prog}: cannot remove '{}': {err}", path.display());
                continue;
            }
            crate::finder_total::carry(&e.trash, (b, inodes), false);
        }
        n += 1;
        freed += b;
        left = (left.0.saturating_sub(b), left.1.saturating_sub(inodes));
    }
    if n > 0 {
        eprintln!(
            "{prog}: {} {} ({}) from the Finder Trash",
            if dry_run { "would empty" } else { "emptied" },
            crate::util::plural(n, "item", "items"),
            crate::util::format_bytes(freed),
        );
    }
    left
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
                println!("would remove {}", crate::util::term_path(&e.shown));
            }
            removed += 1;
            continue;
        }
        let inodes = 1 + crate::trash_stats::count_inodes(&path);
        match crate::trashdir::remove_any_path(&path) {
            Ok(()) => {
                crate::finder_total::carry(&e.trash, (b, inodes), false);
                removed += 1;
                if verbose && !json {
                    // The Put Back place, else the path in the trash.
                    println!("removed {}", crate::util::term_path(&e.shown));
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
    // One item at a time, so each lands in the put log with its name in
    // the trash; list and restore find it there before Finder writes
    // its Put Back record.
    let mut logged: Vec<(String, PathBuf, PathBuf)> = Vec::new();
    for p in &paths {
        let moved = fs::symlink_metadata(p)
            .map_err(|e| format!("cannot remove '{}': {e}", p.display()))
            .and_then(|m| put_item(p, &m));
        match moved {
            Ok((name, trash)) => {
                if verbose {
                    println!("trashed '{}'", crate::util::term_path(p));
                }
                logged.push((name, p.clone(), trash));
            }
            Err(e) => {
                eprintln!("{prog}: {e}");
                failed = true;
            }
        }
    }
    if !logged.is_empty() {
        crate::finder_shared::log_batch(crate::info::batch_id(), &logged);
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
    #[test]
    fn volume_folders_skip_links() {
        let t = std::env::temp_dir().join(format!("rtrash-vols-{}", std::process::id()));
        fs::create_dir_all(t.join("Ext")).unwrap();
        fs::write(t.join("file"), b"").unwrap();
        std::os::unix::fs::symlink("/", t.join("Macintosh HD")).unwrap();
        assert_eq!(dirs_in(&t), vec![t.join("Ext")]);
        let _ = fs::remove_dir_all(&t);
    }

    #[test]
    fn firmlinked_data_paths_shorten_to_the_root_form() {
        let t = std::env::temp_dir().join(format!("rtrash-firm-{}", std::process::id()));
        let root = t.join("r");
        let data = root.join("System/Volumes/Data");
        fs::create_dir_all(data.join("Users/u")).unwrap();
        fs::create_dir_all(data.join("private")).unwrap();
        // A link stands in for the firmlink: /Users is the data volume's.
        std::os::unix::fs::symlink(data.join("Users"), root.join("Users")).unwrap();
        let long = data.join("Users/u/a b.txt");
        assert_eq!(
            firmlink_in(&long, &data, &root),
            root.join("Users/u/a b.txt")
        );
        // No such folder at the root: the long form stays.
        let lone = data.join("private/x");
        assert_eq!(firmlink_in(&lone, &data, &root), lone);
        let other = PathBuf::from("/Volumes/X/a");
        assert_eq!(firmlink_in(&other, &data, &root), other);
        let _ = fs::remove_dir_all(&t);
    }

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
