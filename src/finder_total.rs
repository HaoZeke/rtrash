//! Running totals and history for the Finder trashes, behind
//! `trash-stats --total` and `--history` on macOS.
//!
//! Finder shows every file in a trash, so nothing goes inside one. Each
//! trash gets `KEY.total` and `KEY.history` under
//! `$XDG_STATE_HOME/rtrash/finder-totals`, KEY being the trash path with
//! `%` and `/` percent-encoded. A total (`v1 BYTES ITEMS STAMP`) carries
//! the modification time and entry count of the trash directory, which
//! change whenever Finder or rtrash adds or removes an item, so a total that another tool
//! made out of date reads as stale.
//!
//! A command that may change a trash takes a [`Snapshot`] of the trash
//! times before it and calls [`log`] after it: each trash whose time moved
//! gets a new total and a history line. Put, rm and empty report each
//! item they add or remove with [`carry`]; when the total was current
//! before the command, the new one is the old plus those changes, with no
//! walk. Any other change (restore, undo, a trash without a current total)
//! measures the trash once.

use std::fs;
use std::path::{Path, PathBuf};

fn state_dir() -> Option<PathBuf> {
    let state = std::env::var_os("XDG_STATE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .map(|h| PathBuf::from(h).join(".local/state"))
        })?;
    Some(state.join("rtrash").join("finder-totals"))
}

fn key(trash: &Path) -> String {
    trash
        .to_string_lossy()
        .replace('%', "%25")
        .replace('/', "%2F")
}

fn file(trash: &Path, ext: &str) -> Option<PathBuf> {
    Some(state_dir()?.join(format!("{}.{ext}", key(trash))))
}

/// The modification time of the trash directory in nanoseconds, mixed
/// with its entry count: a coarse file system clock can give two changes
/// in a row the same time, but not the same count as well unless one put
/// and one removal fall in the same tick.
fn stamp(trash: &Path) -> Option<u128> {
    let m = fs::metadata(trash).ok()?.modified().ok()?;
    let ns = m.duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos();
    let count = fs::read_dir(trash).ok()?.count() as u128;
    Some(ns.wrapping_mul(1_000_003).wrapping_add(count))
}

fn parse(body: &str) -> Option<(u64, u64, u128)> {
    let mut f = body.split_whitespace();
    if f.next()? != "v1" {
        return None;
    }
    Some((
        f.next()?.parse().ok()?,
        f.next()?.parse().ok()?,
        f.next()?.parse().ok()?,
    ))
}

/// The stored bytes and items of `trash`, with whether they are current.
/// Reads one small file and never walks the trash.
pub fn stored(trash: &Path) -> Option<(u64, u64, bool)> {
    let (b, n, s) = parse(&fs::read_to_string(file(trash, "total")?).ok()?)?;
    Some((b, n, stamp(trash) == Some(s)))
}

/// Walk `trash` once (its items, as `trash-stats` counts them) and store
/// the total. Best effort.
pub fn measure(trash: &Path) -> (u64, u64) {
    let before = stamp(trash);
    let (b, n) = crate::finder_shared::size_of(trash);
    if let (Some(s), Some(p)) = (before, file(trash, "total")) {
        // A change during the walk leaves the total stale.
        if stamp(trash) == Some(s) {
            if let Some(d) = p.parent() {
                let _ = fs::create_dir_all(d);
            }
            let _ = crate::trashdir::atomic_write(&p, format!("v1 {b} {n} {s}\n").as_bytes());
        }
    }
    (b, n)
}

fn store(trash: &Path, b: u64, n: u64) {
    if let (Some(s), Some(p)) = (stamp(trash), file(trash, "total")) {
        if let Some(d) = p.parent() {
            let _ = fs::create_dir_all(d);
        }
        let _ = crate::trashdir::atomic_write(&p, format!("v1 {b} {n} {s}\n").as_bytes());
    }
}

/// The history of `trash`, oldest first.
pub fn history(trash: &Path) -> Vec<crate::trash_history::Point> {
    file(trash, "history")
        .map(|p| crate::trash_history::read(&p))
        .unwrap_or_default()
}

/// Per trash: bytes and items this command added (negative: removed),
/// and whether every change it made was carried.
type Carried = Vec<(PathBuf, i128, i64, bool)>;
static CARRIED: std::sync::Mutex<Carried> = std::sync::Mutex::new(Vec::new());

fn note(trash: &Path, bytes: i128, items: i64, whole: bool) {
    let mut c = CARRIED.lock().unwrap_or_else(|e| e.into_inner());
    match c.iter_mut().find(|x| x.0 == trash) {
        Some(x) => {
            x.1 += bytes;
            x.2 += items;
            x.3 &= whole;
        }
        None => c.push((trash.to_path_buf(), bytes, items, whole)),
    }
}

/// Report one item this command put into `trash` (`added`) or removed
/// from it, with its size on disk.
pub fn carry(trash: &Path, bytes: u64, added: bool) {
    let b = i128::from(bytes);
    if added {
        note(trash, b, 1, true);
    } else {
        note(trash, -b, -1, true);
    }
}

/// Report a change to `trash` that carries no sizes, so [`log`]
/// measures it.
pub fn uncarried(trash: &Path) {
    note(trash, 0, 0, false);
}

/// The times of the Finder trashes before a command, each with its total
/// when that was current; None where the Finder Trash is not the
/// default, so the command logs nothing.
pub struct Snapshot(Option<Vec<SnapRow>>);

/// A trash, its stamp and its total when current.
type SnapRow = (PathBuf, Option<u128>, Option<(u64, u64)>);

pub fn snapshot() -> Snapshot {
    if !crate::finder::is_default(false) {
        return Snapshot(None);
    }
    snapshot_always()
}

/// [`snapshot`] whatever the default trash is, for `rtrash finder`.
pub fn snapshot_always() -> Snapshot {
    Snapshot(Some(
        crate::finder::trashes()
            .into_iter()
            .map(|t| {
                let s = stamp(&t);
                let cur = stored(&t).filter(|x| x.2).map(|x| (x.0, x.1));
                (t, s, cur)
            })
            .collect(),
    ))
}

/// After a command: measure each trash whose time moved since `before`,
/// store its total and append it to its history under `event`.
pub fn log(event: &str, before: &Snapshot) {
    let Some(before) = &before.0 else {
        return;
    };
    let carried = std::mem::take(&mut *CARRIED.lock().unwrap_or_else(|e| e.into_inner()));
    for t in crate::finder::trashes() {
        let now = stamp(&t);
        let snap = before.iter().find(|x| x.0 == t);
        let was = snap.and_then(|x| x.1);
        if now.is_none() || now == was {
            continue;
        }
        let old = snap.and_then(|x| x.2);
        let c = carried.iter().find(|x| x.0 == t);
        let (b, n) = match (old, c) {
            (Some((ob, on)), Some(&(_, db, dn, true))) => {
                let b = (i128::from(ob) + db).max(0) as u64;
                let n = (on as i64 + dn).max(0) as u64;
                store(&t, b, n);
                (b, n)
            }
            _ => measure(&t),
        };
        if let Some(p) = file(&t, "history") {
            crate::trash_history::append(&p, event, b, n);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_encode_slashes_and_percents() {
        assert_eq!(key(Path::new("/Users/a/.Trash")), "%2FUsers%2Fa%2F.Trash");
        assert_eq!(
            key(Path::new("/V/50%/.Trashes/501")),
            "%2FV%2F50%25%2F.Trashes%2F501"
        );
        assert_eq!(parse("v1 10 2 99"), Some((10, 2, 99)));
        assert_eq!(parse("v0 10 2 99"), None);
    }
}
