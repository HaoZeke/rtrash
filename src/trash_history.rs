//! The history behind `trash-stats --history`: a line `EPOCH EVENT BYTES
//! ITEMS` per command that changed a trash, in a small text file. On Unix
//! the file is `rtrash-history` in each trash root; on Windows it is one
//! file per drive's Recycle Bin under `%LOCALAPPDATA%\\rtrash\\history`.

use std::fs;
use std::path::Path;

/// Past this size a history keeps only its newer half.
pub const CAP: u64 = 64 * 1024;

/// The event a `status --check --fix` that removed records logs, on Unix
/// and on Windows alike.
pub const FIX_EVENT: &str = "fix";

/// The event an `undo` that brought items back logs, for the Finder Trash
/// and the FreeDesktop trashes alike.
pub const UNDO_EVENT: &str = "undo";

/// One line of a history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Point {
    pub epoch: i64,
    pub event: String,
    pub bytes: u64,
    pub items: u64,
}

fn parse(line: &str) -> Option<Point> {
    let mut f = line.split_whitespace();
    Some(Point {
        epoch: f.next()?.parse().ok()?,
        event: f.next()?.to_string(),
        bytes: f.next()?.parse().ok()?,
        items: f.next()?.parse().ok()?,
    })
}

/// The history in `path`, oldest first; lines that do not parse are left
/// out.
pub fn read(path: &Path) -> Vec<Point> {
    fs::read_to_string(path)
        .map(|s| s.lines().filter_map(parse).collect())
        .unwrap_or_default()
}

/// Append a line for `event` to the history in `path`, trimming the file
/// to its newer half once it passes [`CAP`]. Best effort.
pub fn append(path: &Path, event: &str, bytes: u64, items: u64) {
    use std::io::Write;
    if fs::metadata(path).is_ok_and(|m| m.len() > CAP) {
        if let Ok(body) = fs::read_to_string(path) {
            let lines: Vec<&str> = body.lines().collect();
            let keep = lines[lines.len() / 2..].join("\n") + "\n";
            let _ = crate::file_replace::write_own(path, keep.as_bytes());
        }
    }
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let line = format!("{} {event} {bytes} {items}\n", crate::util::now_epoch());
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = f.write_all(line.as_bytes());
    }
}

/// Windows: the history file of the Recycle Bin of volume `root`: a
/// drive (`C:\\`, in `C.txt`) or a folder a volume is mounted on (in
/// `vol-SERIAL.txt`).
#[cfg(windows)]
pub fn bin_path(root: &Path) -> Option<std::path::PathBuf> {
    let s = root.to_str()?;
    let name = crate::win_recycle::history_name(s, crate::win_recycle::volume_key(root));
    let base = std::env::var_os("LOCALAPPDATA")?;
    Some(
        std::path::PathBuf::from(base)
            .join("rtrash")
            .join("history")
            .join(name),
    )
}

/// Windows: every volume with a Recycle Bin to follow: the drives, then
/// the folders volumes are mounted on.
#[cfg(windows)]
pub fn bin_roots() -> Vec<std::path::PathBuf> {
    let mut v = crate::win_recycle::bin_drives();
    v.extend(crate::win_recycle::mounted_volumes());
    v
}

/// Windows: items and bytes in each drive's Recycle Bin, for
/// [`log_bins`].
#[cfg(windows)]
pub type BinSnapshot = Vec<(std::path::PathBuf, Option<(u64, u64)>)>;

#[cfg(windows)]
pub fn bin_snapshot() -> BinSnapshot {
    bin_roots()
        .into_iter()
        .map(|r| {
            let q = crate::win_recycle::query_bin(&r);
            (r, q)
        })
        .collect()
}

/// Windows: after a command, append a line for each drive whose Recycle
/// Bin changed since `before`.
#[cfg(windows)]
pub fn log_bins(event: &str, before: &BinSnapshot) {
    for (root, now) in bin_snapshot() {
        let Some((items, bytes)) = now else {
            continue;
        };
        let was = before
            .iter()
            .find(|(r, _)| *r == root)
            .and_then(|(_, q)| *q);
        if was == Some((items, bytes)) {
            continue;
        }
        if let Some(p) = bin_path(&root) {
            append(&p, event, bytes, items);
        }
    }
}

/// Windows: after `status --fix`, append a [`FIX_EVENT`] line, with the
/// counts `SHQueryRecycleBinW` reports now, for the volume of each
/// Recycle Bin folder in `fixed` (the folders it removed records from).
/// No snapshot is taken first: a check or a dry run passes none, so a
/// plain status costs no query.
#[cfg(windows)]
pub fn log_fixed(fixed: &[std::path::PathBuf]) {
    for root in crate::win_recycle::bin_dir_roots(fixed) {
        let Some((items, bytes)) = crate::win_recycle::query_bin(&root) else {
            continue;
        };
        if let Some(p) = bin_path(&root) {
            append(&p, FIX_EVENT, bytes, items);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_appends_skips_bad_lines_and_trims_past_the_cap() {
        let dir = std::env::temp_dir().join(format!("rtrash-history-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let p = dir.join("sub/h.txt");
        append(&p, "put", 5, 1);
        let mut body = fs::read_to_string(&p).unwrap();
        body.push_str("garbage\n");
        fs::write(&p, body).unwrap();
        append(&p, "empty", 0, 0);
        let h = read(&p);
        assert_eq!(h.len(), 2, "{h:?}");
        assert_eq!((h[0].event.as_str(), h[0].bytes, h[0].items), ("put", 5, 1));
        assert_eq!((h[1].event.as_str(), h[1].bytes), ("empty", 0));
        // Past the cap, the newer half stays.
        let line = "1 put 1 1\n";
        fs::write(&p, line.repeat((CAP as usize / line.len()) + 10)).unwrap();
        append(&p, "reap", 9, 9);
        let after = fs::metadata(&p).unwrap().len();
        assert!(after < CAP * 2 / 3, "{after}");
        assert_eq!(read(&p).last().unwrap().event, "reap");
        let _ = fs::remove_dir_all(&dir);
    }
}
