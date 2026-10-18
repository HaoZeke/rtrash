//! A running total of each trash directory's size, so the checks after a
//! put (`policy.warn_size`, `policy.max_size`) cost a file read instead
//! of a pass over the trash.
//!
//! The total lives in `rtrash-total` in the trash root, stamped with the
//! modification time of `info/`. Any tool that adds or removes an item
//! changes that time, so a stale total is never trusted; the next reader
//! measures the trash once and stores a fresh one. Put, restore, empty
//! and the prune behind `empty --max-size` and `reap` carry a total that
//! was current before them, under the trash lock. A change that cannot
//! carry the total drops it with [`forget`], since the stamp alone cannot
//! tell a change in the same tick of a coarse clock from none.
//!
//! Another tool can make such a change too. So a stamp less than two
//! seconds old (the racy-git window) is trusted only when the number of
//! records in `info/` still equals the stored item count. That costs one
//! directory read and only right after a change; puts spaced further
//! apart still answer from the file alone, and a burst of puts reads
//! names instead of measuring the whole trash.

use std::fs;
use std::path::PathBuf;

use crate::trashdir::TrashDir;

const FILE: &str = "rtrash-total";

fn path(dir: &TrashDir) -> PathBuf {
    dir.root.join(FILE)
}

/// Modification time of `info/` in nanoseconds, rounded down to the
/// test clock's tick (see [`tick`]).
fn stamp(dir: &TrashDir) -> Option<u128> {
    let m = fs::metadata(dir.info()).ok()?.modified().ok()?;
    let ns = m.duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos();
    Some(match tick() {
        Some(t) => ns / t * t,
        None => ns,
    })
}

/// Test seam, in debug builds only: `RTRASH_TEST_STAMP_TICK=NS` makes
/// `info/` times as coarse as NS nanoseconds, so a test can make every
/// change fall in the one tick that a coarse file system clock gives.
fn tick() -> Option<u128> {
    if !cfg!(debug_assertions) {
        return None;
    }
    std::env::var("RTRASH_TEST_STAMP_TICK")
        .ok()?
        .parse()
        .ok()
        .filter(|&t| t > 0)
}

/// Parse `v1 BYTES ITEMS STAMP`.
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

fn now_ns() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos())
}

/// `.trashinfo` records in `info/`, by name only.
fn records(dir: &TrashDir) -> Option<u64> {
    let rd = fs::read_dir(dir.info()).ok()?;
    Some(
        rd.flatten()
            .filter(|e| e.file_name().as_encoded_bytes().ends_with(b".trashinfo"))
            .count() as u64,
    )
}

/// Whether a total stored with stamp `s` and `items` records is current
/// at time `now` (nanoseconds): the stamp must match, and a stamp from
/// the last two seconds (or the future) must also match the record count.
fn is_current(dir: &TrashDir, s: u128, items: u64, now: u128) -> bool {
    if stamp(dir) != Some(s) {
        return false;
    }
    now.saturating_sub(s) >= 2_000_000_000 || records(dir) == Some(items)
}

/// The stored bytes and item count, when they match the trash as it is.
pub fn cached(dir: &TrashDir) -> Option<(u64, u64)> {
    let (bytes, items, s) = parse(&fs::read_to_string(path(dir)).ok()?)?;
    is_current(dir, s, items, now_ns()).then_some((bytes, items))
}

/// The stored bytes and item count with whether they are current: a
/// stale total is one another tool changed the trash after. Reads one
/// small file and never walks the trash.
pub fn stored(dir: &TrashDir) -> Option<(u64, u64, bool)> {
    let (bytes, items, s) = parse(&fs::read_to_string(path(dir)).ok()?)?;
    Some((bytes, items, is_current(dir, s, items, now_ns())))
}

/// Store `bytes` and `items` as current. Best effort: a trash root this
/// user cannot write simply goes without a total.
pub fn store(dir: &TrashDir, bytes: u64, items: u64) {
    if let Some(s) = stamp(dir) {
        let _ = crate::file_replace::write_own(
            &path(dir),
            format!("v1 {bytes} {items} {s}\n").as_bytes(),
        );
    }
}

/// Drop the stored total of `dir` after a change that does not carry
/// it, so the next reader measures again. The stamp alone cannot catch
/// a change in the same tick of a coarse clock as the store.
pub fn forget(dir: &TrashDir) {
    let _ = fs::remove_file(path(dir));
}

/// The trash's size in bytes (payloads and `.trashinfo` files, as
/// `status` counts them): the stored total when current, else measured
/// and stored.
pub fn bytes(prog: &str, dir: &TrashDir) -> u64 {
    if let Some((b, _)) = cached(dir) {
        return b;
    }
    let items = crate::list::collect(std::slice::from_ref(dir)).len() as u64;
    let b = crate::retention::prune(
        prog,
        std::slice::from_ref(dir),
        &crate::retention::Prune {
            max: u64::MAX,
            policy: crate::retention::Policy::Oldest,
            dry_run: true,
            verbose: false,
            protect_batch: None,
            gone_before: None,
            free_inodes: 0,
        },
    )
    .left;
    store(dir, b, items);
    b
}

/// The history file beside the total (see [`crate::trash_history`]).
const HISTORY: &str = "rtrash-history";

/// The history of `dir`, oldest first.
pub fn history(dir: &TrashDir) -> Vec<crate::trash_history::Point> {
    crate::trash_history::read(&dir.root.join(HISTORY))
}

/// Trashes this process changed, for [`log_touched`].
static TOUCHED: std::sync::Mutex<Vec<TrashDir>> = std::sync::Mutex::new(Vec::new());

/// Note that this command added items to `dir` or removed some.
pub fn touched(dir: &TrashDir) {
    let mut t = TOUCHED.lock().unwrap_or_else(|e| e.into_inner());
    if !t.iter().any(|d| d.root == dir.root) {
        t.push(dir.clone());
    }
}

/// After a command: append the total of each trash it changed (measured
/// once if stale) to that trash's history under `event`. Only the
/// trashes [`touched`] names are read, so this costs nothing for a
/// command that changed none.
pub fn log_touched(event: &str) {
    let dirs = std::mem::take(&mut *TOUCHED.lock().unwrap_or_else(|e| e.into_inner()));
    for dir in dirs {
        let (bytes, items) = match cached(&dir) {
            Some(t) => t,
            None => {
                let b = bytes("rtrash", &dir);
                (b, cached(&dir).map_or(0, |(_, i)| i))
            }
        };
        crate::trash_history::append(&dir.root.join(HISTORY), event, bytes, items);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn totals_parse_and_go_stale_when_info_changes() {
        assert_eq!(parse("v1 10 2 99\n"), Some((10, 2, 99)));
        assert_eq!(parse("v2 10 2 99"), None);
        assert_eq!(parse("v1 x 2 99"), None);
        let root = std::env::temp_dir().join(format!("rtrash-total-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("info")).unwrap();
        fs::create_dir_all(root.join("files")).unwrap();
        let dir = TrashDir::from_root(&root).unwrap();
        assert_eq!(cached(&dir), None);
        store(&dir, 123, 0);
        assert_eq!(cached(&dir), Some((123, 0)));
        // A new record either moves the info/ time or, in the same tick,
        // changes the record count; stale both ways, however slow the run.
        fs::write(root.join("info/x.trashinfo"), "[Trash Info]\n").unwrap();
        assert_eq!(cached(&dir), None, "a new item left the total current");
        // The racy rule, with the clock passed in so no step depends on
        // how long the test takes.
        let s = stamp(&dir).unwrap();
        assert!(is_current(&dir, s, 1, s), "fresh stamp, count matches");
        assert!(!is_current(&dir, s, 2, s), "fresh stamp, count differs");
        assert!(!is_current(&dir, s, 2, s + 1_999_999_999), "still fresh");
        assert!(
            is_current(&dir, s, 2, s + 2_000_000_000),
            "old stamp, file alone"
        );
        assert!(!is_current(&dir, s + 1, 1, s), "stamp differs");
        let _ = fs::remove_dir_all(&root);
    }
}
