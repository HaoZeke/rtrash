//! A running total of each trash directory's size, so the checks after a
//! put (`policy.warn_size`, `policy.max_size`) cost a file read instead
//! of a pass over the trash.
//!
//! The total lives in `rtrash-total` in the trash root, stamped with the
//! modification time of `info/`. Any tool that adds or removes an item
//! changes that time, so a stale total is never trusted; the next reader
//! measures the trash once and stores a fresh one. Put, restore, empty
//! and the prune behind `empty --max-size` and `reap` carry a total that
//! was current before them, under the trash lock.

use std::fs;
use std::path::PathBuf;

use crate::trashdir::TrashDir;

const FILE: &str = "rtrash-total";

fn path(dir: &TrashDir) -> PathBuf {
    dir.root.join(FILE)
}

/// Modification time of `info/` in nanoseconds.
fn stamp(dir: &TrashDir) -> Option<u128> {
    let m = fs::metadata(dir.info()).ok()?.modified().ok()?;
    Some(m.duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos())
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

/// The stored bytes and item count, when they match the trash as it is.
pub fn cached(dir: &TrashDir) -> Option<(u64, u64)> {
    let (bytes, items, s) = parse(&fs::read_to_string(path(dir)).ok()?)?;
    (stamp(dir)? == s).then_some((bytes, items))
}

/// Store `bytes` and `items` as current. Best effort: a trash root this
/// user cannot write simply goes without a total.
pub fn store(dir: &TrashDir, bytes: u64, items: u64) {
    if let Some(s) = stamp(dir) {
        let _ = crate::trashdir::atomic_write(
            &path(dir),
            format!("v1 {bytes} {items} {s}\n").as_bytes(),
        );
    }
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

/// The history file beside the total: a line `EPOCH EVENT BYTES ITEMS`
/// per command that changed the trash.
const HISTORY: &str = "rtrash-history";
/// Past this size the history keeps only its newer half.
const HISTORY_CAP: u64 = 64 * 1024;

/// One line of a trash's history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Point {
    pub epoch: i64,
    pub event: String,
    pub bytes: u64,
    pub items: u64,
}

fn parse_point(line: &str) -> Option<Point> {
    let mut f = line.split_whitespace();
    Some(Point {
        epoch: f.next()?.parse().ok()?,
        event: f.next()?.to_string(),
        bytes: f.next()?.parse().ok()?,
        items: f.next()?.parse().ok()?,
    })
}

/// The history of `dir`, oldest first; lines that do not parse are left
/// out.
pub fn history(dir: &TrashDir) -> Vec<Point> {
    fs::read_to_string(dir.root.join(HISTORY))
        .map(|s| s.lines().filter_map(parse_point).collect())
        .unwrap_or_default()
}

/// Append a line for `event` to the history of `dir`, trimming the file
/// to its newer half once it passes [`HISTORY_CAP`]. Best effort.
fn append(dir: &TrashDir, event: &str, bytes: u64, items: u64) {
    use std::io::Write;
    let p = dir.root.join(HISTORY);
    if fs::metadata(&p).is_ok_and(|m| m.len() > HISTORY_CAP) {
        if let Ok(body) = fs::read_to_string(&p) {
            let lines: Vec<&str> = body.lines().collect();
            let keep = lines[lines.len() / 2..].join("\n") + "\n";
            let _ = crate::trashdir::atomic_write(&p, keep.as_bytes());
        }
    }
    let line = format!("{} {event} {bytes} {items}\n", crate::util::now_epoch());
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&p) {
        let _ = f.write_all(line.as_bytes());
    }
}

/// The modification time of each trash's `info/`, before a command.
pub type Snapshot = Vec<(PathBuf, Option<std::time::SystemTime>)>;

fn info_mtime(dir: &TrashDir) -> Option<std::time::SystemTime> {
    fs::metadata(dir.info()).and_then(|m| m.modified()).ok()
}

/// Note when each trash last changed, for [`log_changed`].
pub fn snapshot() -> Snapshot {
    crate::trashdir::all()
        .into_iter()
        .map(|d| {
            let t = info_mtime(&d);
            (d.root, t)
        })
        .collect()
}

/// After a command: for each trash whose `info/` changed since `before`
/// (or that is new), append its total (measured if stale) to its history
/// under `event`.
pub fn log_changed(event: &str, before: &Snapshot) {
    for dir in crate::trashdir::all() {
        let now = info_mtime(&dir);
        let was = before.iter().find(|(r, _)| *r == dir.root).map(|(_, t)| *t);
        if now.is_none() || was == Some(now) {
            continue;
        }
        let (bytes, items) = match cached(&dir) {
            Some(t) => t,
            None => {
                let b = bytes("rtrash", &dir);
                (b, cached(&dir).map_or(0, |(_, i)| i))
            }
        };
        append(&dir, event, bytes, items);
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
        store(&dir, 123, 4);
        assert_eq!(cached(&dir), Some((123, 4)));
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(root.join("info/x.trashinfo"), "[Trash Info]\n").unwrap();
        assert_eq!(cached(&dir), None, "a new item left the total current");
        append(&dir, "put", 5, 1);
        fs::write(root.join("rtrash-history"), {
            let mut s = fs::read_to_string(root.join("rtrash-history")).unwrap();
            s.push_str("garbage\n");
            s
        })
        .unwrap();
        append(&dir, "empty", 0, 0);
        let h = history(&dir);
        assert_eq!(h.len(), 2, "{h:?}");
        assert_eq!((h[0].event.as_str(), h[0].bytes, h[0].items), ("put", 5, 1));
        assert_eq!((h[1].event.as_str(), h[1].bytes), ("empty", 0));
        let _ = fs::remove_dir_all(&root);
    }
}
