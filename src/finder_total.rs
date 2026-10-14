//! Running totals and history for the Finder trashes, behind
//! `trash-stats --total` and `--history` on macOS.
//!
//! Finder shows every file in a trash, so nothing goes inside one. Each
//! trash gets `KEY.total` and `KEY.history` under
//! `$XDG_STATE_HOME/rtrash/finder-totals`, KEY being the trash path with
//! `%` and `/` percent-encoded. A total (`v2 BYTES ITEMS INODES STAMP`; `v1` files lack INODES) carries
//! the modification time and entry count of the trash directory, which
//! change whenever Finder or rtrash adds or removes an item, so a total that another tool
//! made out of date reads as stale.
//!
//! A command that may change a trash takes a [`Snapshot`] of the trash
//! times before it and calls [`log`] after it: each trash whose time moved
//! gets a new total and a history line. Put, rm, empty, restore and undo
//! report each item they add or remove with [`carry`]; when the total was
//! current before the command, the new one is the old plus those changes,
//! with no walk; the inodes of each item (itself and everything below it)
//! carry the same way. A failed removal or a trash without a current total
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

/// A stored total: bytes, items, and inodes (files and directories in
/// the trash, as `trash-stats` counts them) when known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Total {
    pub bytes: u64,
    pub items: u64,
    pub inodes: Option<u64>,
}

fn parse(body: &str) -> Option<(Total, u128)> {
    let mut f = body.split_whitespace();
    let v = f.next()?;
    let bytes = f.next()?.parse().ok()?;
    let items = f.next()?.parse().ok()?;
    let inodes = match v {
        "v1" => None,
        "v2" => Some(f.next()?.parse().ok()?),
        _ => return None,
    };
    let s = f.next()?.parse().ok()?;
    Some((
        Total {
            bytes,
            items,
            inodes,
        },
        s,
    ))
}

/// The stored total of `trash`, with whether it is current. Reads one
/// small file and never walks the trash.
pub fn stored_total(trash: &Path) -> Option<(Total, bool)> {
    let (t, s) = parse(&fs::read_to_string(file(trash, "total")?).ok()?)?;
    Some((t, stamp(trash) == Some(s)))
}

/// [`stored_total`] as bytes, items and whether current.
pub fn stored(trash: &Path) -> Option<(u64, u64, bool)> {
    stored_total(trash).map(|(t, c)| (t.bytes, t.items, c))
}

/// Walk `trash` once (its items and inodes, as `trash-stats` counts
/// them) and store the total. Best effort.
pub fn measure(trash: &Path) -> Total {
    let before = stamp(trash);
    let (bytes, items) = crate::finder_shared::size_of(trash);
    let t = Total {
        bytes,
        items,
        inodes: Some(crate::trash_stats::count_inodes(trash)),
    };
    // A change during the walk leaves the total stale.
    if before.is_some() && stamp(trash) == before {
        store(trash, &t);
    }
    t
}

fn store(trash: &Path, t: &Total) {
    if let (Some(s), Some(p)) = (stamp(trash), file(trash, "total")) {
        if let Some(d) = p.parent() {
            let _ = fs::create_dir_all(d);
        }
        let line = match t.inodes {
            Some(i) => format!("v2 {} {} {i} {s}\n", t.bytes, t.items),
            None => format!("v1 {} {} {s}\n", t.bytes, t.items),
        };
        let _ = crate::trashdir::atomic_write(&p, line.as_bytes());
    }
}

/// The history of `trash`, oldest first.
pub fn history(trash: &Path) -> Vec<crate::trash_history::Point> {
    file(trash, "history")
        .map(|p| crate::trash_history::read(&p))
        .unwrap_or_default()
}

/// What one command changed in one trash: bytes, items and inodes added
/// (negative: removed), and whether every change was carried.
#[derive(Debug, Clone)]
struct Change {
    trash: PathBuf,
    bytes: i128,
    items: i64,
    inodes: i128,
    whole: bool,
}

static CARRIED: std::sync::Mutex<Vec<Change>> = std::sync::Mutex::new(Vec::new());

fn note(trash: &Path, bytes: i128, items: i64, inodes: i128, whole: bool) {
    let mut c = CARRIED.lock().unwrap_or_else(|e| e.into_inner());
    match c.iter_mut().find(|x| x.trash == trash) {
        Some(x) => {
            x.bytes += bytes;
            x.items += items;
            x.inodes += inodes;
            x.whole &= whole;
        }
        None => c.push(Change {
            trash: trash.to_path_buf(),
            bytes,
            items,
            inodes,
            whole,
        }),
    }
}

/// The size on disk and inodes of an item about to move, for [`carry`].
pub fn measure_item(path: &Path) -> (u64, u64) {
    (
        crate::fastdelete::disk_usage(path),
        1 + crate::trash_stats::count_inodes(path),
    )
}

/// Report one item this command put into `trash` (`added`) or removed
/// from it, with its size and inodes from [`measure_item`].
pub fn carry(trash: &Path, (bytes, inodes): (u64, u64), added: bool) {
    let sign = if added { 1 } else { -1 };
    note(
        trash,
        sign * i128::from(bytes),
        sign as i64,
        sign * i128::from(inodes),
        true,
    );
}

/// Report a change to `trash` that carries no sizes, so [`log`]
/// measures it.
pub fn uncarried(trash: &Path) {
    note(trash, 0, 0, 0, false);
}

/// The times of the Finder trashes before a command, each with its total
/// when that was current; None where the Finder Trash is not the
/// default, so the command logs nothing.
pub struct Snapshot(Option<Vec<SnapRow>>);

/// A trash, its stamp and its total when current.
type SnapRow = (PathBuf, Option<u128>, Option<Total>);

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
                let cur = stored_total(&t).filter(|x| x.1).map(|x| x.0);
                (t, s, cur)
            })
            .collect(),
    ))
}

fn add(v: u64, d: i128) -> u64 {
    (i128::from(v) + d).clamp(0, i128::from(u64::MAX)) as u64
}

/// The total after `c`, from the total before it.
fn carried_total(old: Total, c: &Change) -> Total {
    Total {
        bytes: add(old.bytes, c.bytes),
        items: add(old.items, i128::from(c.items)),
        inodes: old.inodes.map(|i| add(i, c.inodes)),
    }
}

/// After a command: give each trash whose time moved since `before` a
/// new total (carried when it can be, else measured) and a history line
/// under `event`.
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
        let c = carried.iter().find(|x| x.trash == t && x.whole);
        let total = match (old, c) {
            (Some(old), Some(c)) => {
                let n = carried_total(old, c);
                store(&t, &n);
                n
            }
            _ => measure(&t),
        };
        if let Some(p) = file(&t, "history") {
            crate::trash_history::append(&p, event, total.bytes, total.items);
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
    }

    #[test]
    fn totals_parse_both_versions_and_carry_changes() {
        let t = |b, n, i| Total {
            bytes: b,
            items: n,
            inodes: i,
        };
        assert_eq!(parse("v1 10 2 99"), Some((t(10, 2, None), 99)));
        assert_eq!(parse("v2 10 2 7 99"), Some((t(10, 2, Some(7)), 99)));
        assert_eq!(parse("v0 10 2 99"), None);
        let c = Change {
            trash: PathBuf::from("/t"),
            bytes: -4,
            items: -1,
            inodes: -3,
            whole: true,
        };
        assert_eq!(carried_total(t(10, 2, Some(7)), &c), t(6, 1, Some(4)));
        assert_eq!(carried_total(t(1, 0, None), &c), t(0, 0, None));
    }
}
