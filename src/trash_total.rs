//! A running total of each trash directory's size, so the checks after a
//! put (`policy.warn_size`, `policy.max_size`) cost a file read instead
//! of a pass over the trash.
//!
//! The total lives in `rtrash-total` in the trash root, stamped with the
//! modification time of `info/`. Any tool that adds or removes an item
//! changes that time, so a stale total is never trusted; the next reader
//! measures the trash once and stores a fresh one. A put by rtrash adds
//! its item to a total that was current before it, under the trash lock.

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
        let _ = fs::remove_dir_all(&root);
    }
}
