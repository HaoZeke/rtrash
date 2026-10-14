//! The history behind `trash-stats --history`: a line `EPOCH EVENT BYTES
//! ITEMS` per command that changed a trash, in a small text file:
//! `rtrash-history` in each trash root.

use std::fs;
use std::path::Path;

/// Past this size a history keeps only its newer half.
pub const CAP: u64 = 64 * 1024;

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
            let tmp = path.with_extension("tmp");
            if fs::write(&tmp, keep).is_ok() {
                let _ = fs::rename(&tmp, path);
            }
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
