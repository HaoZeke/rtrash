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
//! Orphans (payloads without a record) are left out, as in the total.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use crate::trashdir::TrashDir;

const FILE: &str = "rtrash-dirinodes";

/// Cache lines: name to (inodes in the payload tree, record mtime in ns).
fn load(dir: &TrashDir) -> HashMap<Vec<u8>, (u64, u128)> {
    let Ok(body) = fs::read_to_string(dir.root.join(FILE)) else {
        return HashMap::new();
    };
    let mut lines = body.lines();
    if lines.next() != Some("v1") {
        return HashMap::new();
    }
    lines
        .filter_map(|l| {
            let mut f = l.splitn(3, ' ');
            let n = f.next()?.parse().ok()?;
            let t = f.next()?.parse().ok()?;
            Some((crate::util::url_decode(f.next()?), (n, t)))
        })
        .collect()
}

fn save(dir: &TrashDir, map: &HashMap<Vec<u8>, (u64, u128)>) {
    let mut lines: Vec<String> = map
        .iter()
        .map(|(name, (n, t))| format!("{n} {t} {}", crate::util::url_encode(name)))
        .collect();
    lines.sort();
    let body = format!("v1\n{}\n", lines.join("\n"));
    let _ = crate::trashdir::atomic_write(&dir.root.join(FILE), body.as_bytes());
}

fn mtime_ns(p: &Path) -> Option<u128> {
    let m = fs::metadata(p).ok()?.modified().ok()?;
    Some(m.duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos())
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
    save(dir, &map);
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
    let cache = load(dir);
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
    if next != cache {
        save(dir, &next);
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
        let _ = fs::remove_dir_all(&root);
    }
}
