//! How many inodes a trash holds, without walking every item.
//!
//! Each item takes one inode for its `.trashinfo` file and one for a file
//! payload; a directory payload takes one per entry in its tree. Those
//! tree counts are kept in `rtrash-dirinodes` in the trash root, one line
//! per directory item with the modification time of its record, as
//! `directorysizes` does for sizes. A count then reads `files/` once and
//! stats each record, and only a directory item missing from the cache
//! is walked (with `measure`), once.
//!
//! The answer itself is kept on the file's first line, stamped with the
//! modification time of `info/` as the running total is, so a trash no
//! tool changed since is answered from that line alone.
//! A stamp from the last two seconds is not trusted: directory times can
//! tick coarsely, so a record added in the same tick leaves the stamp
//! unchanged (the racy-git problem).
//!
//! Orphans (payloads without a record) are left out, as in the total.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use crate::trashdir::TrashDir;

const FILE: &str = "rtrash-dirinodes";

/// Cache lines: name to (inodes in the payload tree, record mtime in ns).
type Trees = HashMap<Vec<u8>, (u64, u128)>;

/// The cached answer `(inodes, info stamp)` from the first line, if any,
/// and the tree counts.
fn load_all(dir: &TrashDir) -> (Option<(u64, u128)>, Trees) {
    let Ok(body) = fs::read_to_string(dir.root.join(FILE)) else {
        return (None, HashMap::new());
    };
    let mut lines = body.lines();
    let mut head = lines.next().unwrap_or("").split(' ');
    if head.next() != Some("v1") {
        return (None, HashMap::new());
    }
    let answer = (|| Some((head.next()?.parse().ok()?, head.next()?.parse().ok()?)))();
    let trees = lines
        .filter_map(|l| {
            let mut f = l.splitn(3, ' ');
            let n = f.next()?.parse().ok()?;
            let t = f.next()?.parse().ok()?;
            Some((crate::util::url_decode(f.next()?), (n, t)))
        })
        .collect();
    (answer, trees)
}

fn load(dir: &TrashDir) -> Trees {
    load_all(dir).1
}

/// Write the tree counts, with the answer for the trash as of `info/`
/// stamp when given.
fn save(dir: &TrashDir, map: &Trees, answer: Option<(u64, u128)>) {
    let mut lines: Vec<String> = map
        .iter()
        .map(|(name, (n, t))| format!("{n} {t} {}", crate::util::url_encode(name)))
        .collect();
    lines.sort();
    let head = answer.map_or("v1".to_string(), |(n, s)| format!("v1 {n} {s}"));
    let body = format!("{head}\n{}\n", lines.join("\n"));
    let _ = crate::file_replace::write_own(&dir.root.join(FILE), body.as_bytes());
}

fn mtime_ns(p: &Path) -> Option<u128> {
    let m = fs::metadata(p).ok()?.modified().ok()?;
    Some(m.duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos())
}

/// Whether a change to `info/` could hide behind the stamp `s`: it is
/// less than two seconds old, or in the future.
fn racy(s: u128) -> bool {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    now.saturating_sub(s) < 2_000_000_000
}

/// Cache the tree count of the directory item `name`, just trashed.
/// The caller holds the trash lock. Best effort.
pub fn record(dir: &TrashDir, name: &str, inodes: u64) {
    let Some(t) = mtime_ns(&crate::util::join_info(&dir.info(), name)) else {
        return;
    };
    let mut map = load(dir);
    map.insert(
        crate::util::os_from_str(name).as_encoded_bytes().to_vec(),
        (inodes, t),
    );
    // The put changed the trash, so the stored answer goes.
    save(dir, &map, None);
}

/// Inodes in the tree at `p`, itself included; links are not followed.
/// Reads directories only and stats nothing.
pub fn tree_inodes(p: &Path) -> u64 {
    let mut n = 1u64;
    let mut stack = vec![p.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).into_iter().flatten().flatten() {
            n += 1;
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                stack.push(e.path());
            }
        }
    }
    n
}

/// The inodes `dir` holds: records plus payloads. `None` when a directory
/// item has no cached count and `measure` is off. With `measure`, such
/// items are walked once and cached.
pub fn count(dir: &TrashDir, measure: bool) -> Option<u64> {
    let stamp = mtime_ns(&dir.info());
    let (answer, cache) = load_all(dir);
    if let (Some((n, s)), Some(now)) = (answer, stamp) {
        if s == now && !racy(s) {
            return Some(n);
        }
    }
    let mut next: HashMap<Vec<u8>, (u64, u128)> = HashMap::new();
    let mut total = 0u64;
    let info = dir.info();
    let rd = match fs::read_dir(dir.files()) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Some(0),
        Err(_) => return None,
    };
    for e in rd.flatten() {
        let name = e.file_name();
        let rec = crate::util::join_info(&info, &crate::util::str_from_os_ref(&name));
        let Some(t) = mtime_ns(&rec) else {
            continue;
        };
        let is_dir = e.file_type().is_ok_and(|ft| ft.is_dir());
        if !is_dir {
            total += 2;
            continue;
        }
        let key = name.as_encoded_bytes().to_vec();
        let n = match cache.get(&key) {
            Some(&(n, ct)) if ct == t => n,
            _ if measure => tree_inodes(&e.path()),
            _ => return None,
        };
        next.insert(key, (n, t));
        total += 1 + n;
    }
    // Keep the answer only when info/ did not change during the count.
    let answer = stamp
        .filter(|&s| mtime_ns(&dir.info()) == Some(s))
        .map(|s| (total, s));
    if next != cache || answer.is_some() {
        save(dir, &next, answer);
    }
    Some(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_records_files_and_directory_trees_and_caches_the_trees() {
        let root = std::env::temp_dir().join(format!("rtrash-inodes-{}", std::process::id()));
        let dir = TrashDir {
            root: root.clone(),
            topdir: None,
        };
        fs::create_dir_all(dir.files().join("d/sub")).unwrap();
        fs::create_dir_all(dir.info()).unwrap();
        fs::write(dir.files().join("d/sub/x"), b"x").unwrap();
        fs::write(dir.files().join("f"), b"f").unwrap();
        fs::write(dir.files().join("orphan"), b"o").unwrap();
        for n in ["d", "f"] {
            fs::write(dir.info().join(format!("{n}.trashinfo")), b"[Trash Info]\n").unwrap();
        }
        // d: record, d, sub, x = 4; f: record and file = 2.
        assert_eq!(count(&dir, false), None);
        assert_eq!(count(&dir, true), Some(6));
        assert_eq!(count(&dir, false), Some(6));
        // The answer line is written, but a fresh info/ stamp is not
        // trusted: a record added in the same clock tick leaves it as is.
        let body = fs::read_to_string(root.join(FILE)).unwrap();
        assert!(body.starts_with("v1 6 "), "{body}");
        fs::write(dir.files().join("g"), b"g").unwrap();
        fs::write(dir.info().join("g.trashinfo"), b"[Trash Info]\n").unwrap();
        assert_eq!(count(&dir, false), Some(8));
        // A stale answer whose stamp matches a fresh info/ time, as a
        // coarse clock leaves it: still counted, not served. The time is
        // set to now so the check does not depend on how long the test ran.
        // (Setting a directory's time needs a directory handle: Unix only.)
        #[cfg(unix)]
        {
            let body = fs::read_to_string(root.join(FILE)).unwrap();
            let rest = body.split_once('\n').unwrap().1.to_string();
            let info = fs::File::open(dir.info()).unwrap();
            info.set_modified(std::time::SystemTime::now()).unwrap();
            let fresh = mtime_ns(&dir.info()).unwrap();
            fs::write(root.join(FILE), format!("v1 6 {fresh}\n{rest}")).unwrap();
            assert_eq!(count(&dir, false), Some(8));
            // An old stamp that matches is served from the line alone.
            let old = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
            info.set_modified(old).unwrap();
            let old_ns = mtime_ns(&dir.info()).unwrap();
            fs::write(root.join(FILE), format!("v1 99 {old_ns}\n{rest}")).unwrap();
            assert_eq!(count(&dir, false), Some(99));
        }
        let _ = fs::remove_dir_all(&root);
    }
}
