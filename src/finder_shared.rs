//! The Finder Trash under the shared commands: the restore browser, undo,
//! status and trash-stats on macOS, where plain commands use `~/.Trash`
//! (see [`crate::finder::is_default`]).
//!
//! Finder keeps no `.trashinfo`, so plain put writes each run's items to
//! an undo log of its own: `$XDG_STATE_HOME/rtrash/finder-undo.tsv`, one
//! `BATCH<TAB>EPOCH<TAB>NAME<TAB>ORIGINAL<TAB>TRASH` line per item, the
//! last three percent-encoded (a line without TRASH means `~/.Trash`). Undo restores the items of a run that still
//! sit in the trash under the names the log gives.

use std::fs;
use std::io::Write;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use crate::finder::{self, Item};
use crate::restore::Conflict;
use crate::util::{url_decode, url_encode};

/// One Finder Trash item as the shared commands see it.
#[derive(Debug, Clone)]
pub struct Entry {
    pub trash: PathBuf,
    pub name: String,
    /// The Put Back place, when Finder recorded one.
    pub original: Option<PathBuf>,
    /// What rows show: the Put Back place, else the path in the trash.
    pub shown: PathBuf,
    /// Trash time as `YYYY-MM-DDTHH:MM:SS`, from the item's change time.
    pub date: String,
    pub epoch: i64,
}

impl Entry {
    pub fn path(&self) -> PathBuf {
        self.trash.join(&self.name)
    }

    /// Move the item to `dest`; an existing `dest` stops it unless `force`.
    pub fn restore_to(&self, dest: &Path, force: bool) -> Result<(), String> {
        if !force && dest.symlink_metadata().is_ok() {
            return Err(format!(
                "refusing to overwrite existing '{}' (use -f to force)",
                dest.display()
            ));
        }
        finder::move_back(&self.path(), dest, force)
    }
}

/// Every item in every Finder trash ([`finder::trashes`]), oldest first.
pub fn entries() -> std::io::Result<Vec<Entry>> {
    let mut all = Vec::new();
    for t in finder::trashes() {
        all.extend(from_items(&t)?);
    }
    all.sort_by(|a, b| a.epoch.cmp(&b.epoch).then_with(|| a.name.cmp(&b.name)));
    Ok(all)
}

/// `s` in Unicode NFC, the form a user types and the FreeDesktop trash
/// keeps; Finder may hand back a name decomposed (NFD).
pub fn nfc(s: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    s.nfc().collect()
}

/// `p` in NFC when it is UTF-8; other bytes stay as they are.
pub fn nfc_path(p: &Path) -> PathBuf {
    match p.to_str() {
        Some(s) => PathBuf::from(nfc(s)),
        None => p.to_path_buf(),
    }
}

/// The entry of `dir` named `name`, or failing that the one whose
/// composed (NFC) form matches; None when neither is there.
pub fn resolve_name(dir: &Path, name: &str) -> Option<String> {
    if dir.join(name).symlink_metadata().is_ok() {
        return Some(name.to_string());
    }
    let want = nfc(name);
    fs::read_dir(dir).ok()?.flatten().find_map(|d| {
        let n = d.file_name().into_string().ok()?;
        (nfc(&n) == want).then_some(n)
    })
}

/// Whether `a` and `b` name the same path once both are in NFC.
pub fn same_path(a: &Path, b: &Path) -> bool {
    if a == b || nfc_path(a) == nfc_path(b) {
        return true;
    }
    // One spelling may run through a symbolic link (/var against
    // /private/var on macOS) or a firmlink; compare the real folders.
    let real = |p: &Path| {
        let dir = std::fs::canonicalize(p.parent()?).ok()?;
        Some(nfc_path(&dir.join(p.file_name()?)))
    };
    matches!((real(a), real(b)), (Some(x), Some(y)) if x == y)
}

fn from_items(trash: &Path) -> std::io::Result<Vec<Entry>> {
    use std::os::unix::fs::MetadataExt;
    let items: Vec<Item> = match finder::items(trash) {
        Ok(i) => i,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out: Vec<Entry> = items
        .into_iter()
        .map(|i| {
            let epoch = fs::symlink_metadata(trash.join(&i.name)).map_or(0, |m| m.ctime());
            // Shown, listed and matched in NFC; `original` keeps the bytes
            // Finder recorded, so a restore lands on exactly that path.
            let shown = nfc_path(&i.original.clone().unwrap_or_else(|| trash.join(&i.name)));
            Entry {
                trash: trash.to_path_buf(),
                date: crate::info::local_string(epoch),
                epoch,
                shown,
                original: i.original,
                name: i.name,
            }
        })
        .collect();
    out.sort_by(|a, b| a.epoch.cmp(&b.epoch).then_with(|| a.name.cmp(&b.name)));
    Ok(out)
}

/// Age and size filters over Finder items: the age counts from the time an
/// item went into the trash (its change time there), the size is its disk
/// usage, as on the FreeDesktop trash.
#[derive(Clone, Copy, Debug, Default)]
pub struct Filter {
    pub age: crate::filters::AgeFilter,
    pub size: crate::filters::SizeFilter,
}

impl Filter {
    pub fn apply(&self, v: Vec<Entry>) -> Vec<Entry> {
        let now = crate::util::now_epoch();
        v.into_iter()
            .filter(|e| self.age.keeps_at(Some(e.epoch), now))
            .filter(|e| {
                !self.size.is_set() || self.size.keeps(crate::fastdelete::disk_usage(&e.path()))
            })
            .collect()
    }
}

#[cfg(feature = "tui")]
impl crate::restore_tui::TuiItem for Entry {
    fn original(&self) -> &Path {
        &self.shown
    }
    fn date(&self) -> Option<&str> {
        Some(&self.date)
    }
    fn restore(&self, force: bool) -> Result<(PathBuf, Vec<String>), String> {
        let dest = self
            .original
            .clone()
            .ok_or_else(|| format!("'{}' has no Put Back record", self.name))?;
        self.restore_to(&dest, force).map(|()| (dest, Vec::new()))
    }
    fn missing(&self) -> bool {
        self.original.is_none()
    }
}

// ---- undo log ----

fn log_path() -> Option<PathBuf> {
    let state = std::env::var_os("XDG_STATE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .map(|h| PathBuf::from(h).join(".local/state"))
        })?;
    Some(state.join("rtrash").join("finder-undo.tsv"))
}

/// One logged item of a put run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Logged {
    pub batch: String,
    pub epoch: i64,
    pub name: String,
    pub original: PathBuf,
    pub trash: PathBuf,
}

fn decode_path(s: &str) -> PathBuf {
    PathBuf::from(std::ffi::OsString::from_vec(url_decode(s)))
}

fn parse_line(line: &str) -> Option<Logged> {
    let mut f = line.split('\t');
    let batch = f.next()?.to_string();
    let epoch = f.next()?.parse().ok()?;
    let name = String::from_utf8_lossy(&url_decode(f.next()?)).into_owned();
    let original = decode_path(f.next()?);
    let trash = match f.next() {
        Some(t) => decode_path(t),
        None => finder::home_trash()?,
    };
    Some(Logged {
        batch,
        epoch,
        name,
        original,
        trash,
    })
}

pub(crate) fn read_log() -> Vec<Logged> {
    log_path()
        .and_then(|p| fs::read_to_string(p).ok())
        .map(|s| s.lines().filter_map(parse_line).collect())
        .unwrap_or_default()
}

/// Add this put run's Finder items to the undo log, dropping lines whose
/// item has left the trash, so the log stays as small as the trash.
pub fn log_batch(batch: &str, items: &[(String, PathBuf, PathBuf)]) {
    let Some(path) = log_path() else { return };
    let mut keep: Vec<Logged> = read_log()
        .into_iter()
        .filter(|l| l.trash.join(&l.name).symlink_metadata().is_ok())
        .collect();
    let now = crate::util::now_epoch();
    for (name, original, trash) in items {
        keep.push(Logged {
            batch: batch.to_string(),
            epoch: now,
            name: name.clone(),
            original: original.clone(),
            trash: trash.clone(),
        });
    }
    let mut body = String::new();
    for l in &keep {
        body.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\n",
            l.batch,
            l.epoch,
            url_encode(l.name.as_bytes()),
            url_encode(l.original.as_os_str().as_bytes()),
            url_encode(l.trash.as_os_str().as_bytes())
        ));
    }
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    let written = fs::File::create(&tmp).and_then(|mut f| f.write_all(body.as_bytes()));
    if written.is_ok() {
        let _ = fs::rename(&tmp, &path);
    }
}

/// Put runs whose items still sit in the trash, newest first.
fn runs() -> Vec<(String, Vec<Logged>)> {
    let mut by: Vec<(String, Vec<Logged>)> = Vec::new();
    for mut l in read_log() {
        // Finder may store the name in another Unicode form than the one
        // put logged; find it by its composed form.
        match resolve_name(&l.trash, &l.name) {
            Some(n) => l.name = n,
            None => continue,
        }
        match by.iter_mut().find(|(b, _)| *b == l.batch) {
            Some((_, v)) => v.push(l),
            None => by.push((l.batch.clone(), vec![l])),
        }
    }
    // Batch ids start with milliseconds, so they order runs.
    let key = |b: &str| {
        b.split('-')
            .next()
            .and_then(|n| n.parse::<i64>().ok())
            .unwrap_or(0)
    };
    by.sort_by_key(|(b, _)| std::cmp::Reverse(key(b)));
    by
}

/// Items in the Finder trashes, and how many of them the latest put run
/// did not leave.
pub fn outside_latest_put() -> (usize, usize) {
    let all = entries().unwrap_or_default();
    let runs = runs();
    let latest = runs.first().map(|(_, v)| v.as_slice()).unwrap_or(&[]);
    let foreign = all
        .iter()
        .filter(|e| {
            !latest
                .iter()
                .any(|l| l.trash == e.trash && l.name == e.name)
        })
        .count();
    (all.len(), foreign)
}

/// `undo` on the Finder Trash.
pub fn undo(
    prog: &str,
    wanted: Option<&str>,
    list_only: bool,
    dry_run: bool,
    conflict: Conflict,
    json: bool,
) -> i32 {
    let runs = runs();
    let esc = crate::util::json_escape;
    if list_only {
        if json {
            let rows: Vec<String> = runs
                .iter()
                .map(|(b, v)| {
                    let o: Vec<String> = v
                        .iter()
                        .map(|l| format!("\"{}\"", esc(&l.original.to_string_lossy())))
                        .collect();
                    format!(
                        "{{\"batch\":\"{}\",\"deletion_date\":\"{}\",\"count\":{},\"originals\":[{}]}}",
                        esc(b),
                        crate::info::local_string(v[0].epoch),
                        v.len(),
                        o.join(",")
                    )
                })
                .collect();
            println!("[{}]", rows.join(","));
        } else {
            for (b, v) in &runs {
                let more = if v.len() > 1 {
                    format!(" (+{} more)", v.len() - 1)
                } else {
                    String::new()
                };
                println!(
                    "{b}  {}  {:>4} item{}  {}{more}",
                    crate::info::local_string(v[0].epoch).replacen('T', " ", 1),
                    v.len(),
                    if v.len() == 1 { " " } else { "s" },
                    v[0].original.display()
                );
            }
        }
        return 0;
    }
    let chosen = match wanted {
        Some(id) => runs.iter().find(|(b, _)| b == id),
        None => runs.first(),
    };
    let Some((batch, items)) = chosen else {
        match wanted {
            Some(id) => eprintln!("{prog}: no trashed items from put run '{id}'"),
            None => eprintln!("{prog}: no put run to undo in the Finder Trash"),
        }
        return 1;
    };
    let (mut restored, mut failed, mut skipped) = (Vec::new(), 0usize, 0usize);
    for l in items {
        let exists = l.original.symlink_metadata().is_ok();
        let (dest, force) = match (exists, conflict) {
            (true, Conflict::Skip) => {
                eprintln!("{prog}: skipping '{}': it exists", l.original.display());
                skipped += 1;
                continue;
            }
            (true, Conflict::Rename) => (crate::restore::free_name(&l.original), false),
            (true, Conflict::Refuse) => {
                eprintln!(
                    "{prog}: refusing to overwrite existing '{}' (use -f to force)",
                    l.original.display()
                );
                failed += 1;
                continue;
            }
            (_, c) => (l.original.clone(), c == Conflict::Overwrite),
        };
        if dry_run {
            println!("would restore '{}'", dest.display());
            restored.push(dest);
            continue;
        }
        match finder::move_back(&l.trash.join(&l.name), &dest, force) {
            Ok(()) => {
                if !json {
                    println!("restored '{}'", crate::util::term_path(&dest));
                }
                restored.push(dest);
            }
            Err(e) => {
                eprintln!("{prog}: {e}");
                failed += 1;
            }
        }
    }
    if json {
        let r: Vec<String> = restored
            .iter()
            .map(|d| format!("{{\"restored_to\":\"{}\"}}", esc(&d.to_string_lossy())))
            .collect();
        println!(
            "{{\"batch\":\"{}\",\"restored\":[{}],\"failed\":{failed},\"skipped\":{skipped}}}",
            esc(batch),
            r.join(",")
        );
    }
    i32::from(failed > 0)
}

// ---- status and trash-stats ----

/// `status` on the Finder trashes, in the shape of the FreeDesktop one.
pub fn status(prog: &str, json: bool, filter: Filter) -> i32 {
    struct Row {
        trash: PathBuf,
        items: u64,
        bytes: u64,
        unplaced: u64,
    }
    let mut rows = Vec::new();
    for trash in finder::trashes() {
        let all = match from_items(&trash) {
            Ok(a) => filter.apply(a),
            Err(e) => {
                eprintln!("{prog}: cannot read '{}': {e}", trash.display());
                return 1;
            }
        };
        rows.push(Row {
            items: all.len() as u64,
            bytes: all
                .iter()
                .map(|e| crate::fastdelete::disk_usage(&e.path()))
                .sum(),
            unplaced: all.iter().filter(|e| e.original.is_none()).count() as u64,
            trash,
        });
    }
    let items: u64 = rows.iter().map(|r| r.items).sum();
    let bytes: u64 = rows.iter().map(|r| r.bytes).sum();
    if json {
        let body: Vec<String> = rows
            .iter()
            .map(|r| {
                format!(
                    "    {{\"path\":\"{}\",\"items\":{},\"bytes\":{},\"orphans\":0,\"dangling\":0,\"finder\":true,\"without_put_back\":{}}}",
                    crate::util::json_escape(&r.trash.to_string_lossy()),
                    r.items,
                    r.bytes,
                    r.unplaced
                )
            })
            .collect();
        println!(
            "{{\n  \"roots\": [\n{}\n  ],\n  \"total_items\": {items},\n  \"total_bytes\": {bytes}\n}}",
            body.join(",\n")
        );
        return 0;
    }
    for r in &rows {
        let note = if r.unplaced > 0 {
            format!(" ({} without Put Back)", r.unplaced)
        } else {
            String::new()
        };
        println!(
            "{}\t{}{note}\t{}",
            r.trash.display(),
            crate::util::plural(r.items, "item", "items"),
            crate::fastdelete::format_bytes(r.bytes)
        );
    }
    println!(
        "Total: {} ({})",
        crate::util::plural(items, "item", "items"),
        crate::fastdelete::format_bytes(bytes)
    );
    for r in &rows {
        if let Ok(s) = crate::space::query(&r.trash) {
            println!(
                "Free on {}: {}",
                r.trash.display(),
                crate::space::free_line(&s)
            );
        }
    }
    0
}

/// The bytes on disk and item count of one Finder trash.
pub fn size_of(trash: &Path) -> (u64, u64) {
    let items = from_items(trash).unwrap_or_default();
    let bytes = items
        .iter()
        .map(|e| crate::fastdelete::disk_usage(&e.path()))
        .sum();
    (bytes, items.len() as u64)
}

/// The trash-stats summaries of the Finder trashes.
pub fn stats(top: usize) -> Vec<crate::trash_stats::Stats> {
    finder::trashes()
        .into_iter()
        .filter(|t| t.is_dir())
        .map(|trash| {
            let items: Vec<crate::trash_stats::Item> = from_items(&trash)
                .unwrap_or_default()
                .into_iter()
                .map(|e| crate::trash_stats::Item {
                    bytes: crate::fastdelete::disk_usage(&e.path()),
                    name: e.name,
                    original: e.shown,
                    date: Some(e.date),
                    epoch: e.epoch,
                })
                .collect();
            let inodes = crate::trash_stats::count_inodes(&trash);
            crate::trash_stats::summarize(trash, items, inodes, 0, top)
        })
        .collect()
}

/// `rm` on the Finder trashes: permanently remove the items whose Put Back
/// place, its file name or the name in the trash matches a pattern.
pub fn rm(
    prog: &str,
    patterns: &[String],
    dry_run: bool,
    verbose: bool,
    json: bool,
    filter: Filter,
) -> i32 {
    let rows = match entries() {
        Ok(r) => filter.apply(r),
        Err(e) => {
            eprintln!("{prog}: cannot read the Finder Trash: {e}");
            return 1;
        }
    };
    let (mut removed, mut bytes, mut status) = (0u64, 0u64, 0);
    let mut matched: Vec<String> = Vec::new();
    for e in &rows {
        let path_s = e.shown.to_string_lossy();
        let base = e
            .shown
            .file_name()
            .map(|b| b.to_string_lossy().into_owned())
            .unwrap_or_default();
        let name = nfc(&e.name);
        let hit = patterns.iter().any(|p| {
            let p = nfc(p);
            crate::util::fnmatch(&p, &path_s)
                || crate::util::fnmatch(&p, &base)
                || crate::util::fnmatch(&p, &name)
        });
        if !hit {
            continue;
        }
        let m = crate::finder_total::measure_item(&e.path());
        let sz = m.0;
        if dry_run {
            if !json {
                println!("{}", crate::util::term_path(&e.shown));
            }
        } else if let Err(err) = crate::trashdir::remove_any_path(&e.path()) {
            crate::finder_total::uncarried(&e.trash);
            eprintln!("{prog}: cannot remove '{}': {err}", e.path().display());
            status = 1;
            continue;
        } else {
            crate::finder_total::carry(&e.trash, m, false);
            if verbose && !json {
                println!("removed '{}'", crate::util::term_path(&e.shown));
            }
        }
        removed += 1;
        bytes = bytes.saturating_add(sz);
        matched.push(path_s.into_owned());
    }
    let noun = if removed == 1 { "item" } else { "items" };
    if json {
        let m: Vec<String> = matched
            .iter()
            .map(|p| format!("\"{}\"", crate::util::json_escape(p)))
            .collect();
        println!(
            "{{\"dry_run\":{dry_run},\"removed\":{removed},\"bytes\":{bytes},\"matches\":[{}]}}",
            m.join(",")
        );
    } else if dry_run {
        eprintln!(
            "Would permanently remove {removed} {noun} ({}, approximately reclaimable)",
            crate::fastdelete::format_bytes(bytes)
        );
    } else if removed > 0 || verbose {
        eprintln!("Permanently removed {removed} {noun}");
    }
    status
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_lines_round_trip_odd_names() {
        let l = Logged {
            batch: "1700000000000-42".into(),
            epoch: 1_700_000_000,
            name: "a\tb %é".into(),
            original: PathBuf::from("/x/a\tb %é"),
            trash: PathBuf::from("/Volumes/X/.Trashes/501"),
        };
        let line = format!(
            "{}\t{}\t{}\t{}\t{}",
            l.batch,
            l.epoch,
            url_encode(l.name.as_bytes()),
            url_encode(l.original.as_os_str().as_bytes()),
            url_encode(l.trash.as_os_str().as_bytes())
        );
        assert_eq!(parse_line(&line), Some(l));
        assert_eq!(parse_line("broken"), None);
    }

    #[cfg(feature = "tui")]
    #[test]
    fn browser_restore_refuses_a_taken_place_unless_forced() {
        use crate::restore_tui::TuiItem;
        let base = std::env::temp_dir().join(format!("rtrash-fs-{}", std::process::id()));
        let trash = base.join(".Trash");
        fs::create_dir_all(&trash).unwrap();
        fs::write(trash.join("f"), b"old").unwrap();
        fs::write(base.join("f"), b"taken").unwrap();
        let e = Entry {
            trash: trash.clone(),
            name: "f".into(),
            original: Some(base.join("f")),
            shown: base.join("f"),
            date: "2026-01-01T00:00:00".into(),
            epoch: 0,
        };
        assert!(e.restore(false).is_err());
        assert_eq!(fs::read(base.join("f")).unwrap(), b"taken");
        assert_eq!(e.restore(true).unwrap().0, base.join("f"));
        assert_eq!(fs::read(base.join("f")).unwrap(), b"old");
        let none = Entry {
            original: None,
            ..e
        };
        assert!(none.missing());
        let _ = fs::remove_dir_all(&base);
    }
}
