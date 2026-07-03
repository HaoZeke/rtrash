use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use crate::util::{url_decode, url_encode};

pub struct TrashInfo {
    /// Original path, absolute or relative to the trash topdir.
    pub path: PathBuf,
    pub deletion_date: Option<String>,
}

pub fn render(original: &Path, date: &str) -> String {
    format!(
        "[Trash Info]\nPath={}\nDeletionDate={}\n",
        url_encode(original.as_os_str().as_bytes()),
        date
    )
}

/// Parse a `.trashinfo` file body. Per spec, the first occurrence of a key wins.
pub fn parse(content: &str) -> TrashInfo {
    let mut path = PathBuf::new();
    let mut date = None;
    for line in content.lines() {
        if let Some(v) = line.strip_prefix("Path=") {
            if path.as_os_str().is_empty() {
                path = PathBuf::from(OsString::from_vec(url_decode(v.trim_end())));
            }
        } else if let Some(v) = line.strip_prefix("DeletionDate=") {
            if date.is_none() {
                date = Some(v.trim().to_string());
            }
        }
    }
    TrashInfo {
        path,
        deletion_date: date,
    }
}

/// Current local time as `YYYY-MM-DDTHH:MM:SS` (the trashinfo format).
pub fn now_local_string() -> String {
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec
        )
    }
}

pub fn now_epoch() -> i64 {
    unsafe { libc::time(std::ptr::null_mut()) as i64 }
}

fn parse_num(b: &[u8]) -> Option<i32> {
    let mut v: i32 = 0;
    for &c in b {
        if !c.is_ascii_digit() {
            return None;
        }
        v = v.checked_mul(10)?.checked_add((c - b'0') as i32)?;
    }
    Some(v)
}

/// Parse `YYYY-MM-DDTHH:MM:SS` (local time) to a unix timestamp.
pub fn parse_local_to_epoch(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let year = parse_num(&b[0..4])?;
    let mon = parse_num(&b[5..7])?;
    let mday = parse_num(&b[8..10])?;
    let hour = parse_num(&b[11..13])?;
    let min = parse_num(&b[14..16])?;
    let sec = parse_num(&b[17..19])?;
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        tm.tm_year = year - 1900;
        tm.tm_mon = mon - 1;
        tm.tm_mday = mday;
        tm.tm_hour = hour;
        tm.tm_min = min;
        tm.tm_sec = sec;
        tm.tm_isdst = -1;
        let t = libc::mktime(&mut tm);
        if t == -1 {
            None
        } else {
            Some(t as i64)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_and_parse_roundtrip() {
        let body = render(Path::new("/home/u/my file.txt"), "2026-07-03T12:00:00");
        assert!(body.contains("Path=/home/u/my%20file.txt"));
        let info = parse(&body);
        assert_eq!(info.path, Path::new("/home/u/my file.txt"));
        assert_eq!(info.deletion_date.as_deref(), Some("2026-07-03T12:00:00"));
    }

    #[test]
    fn first_key_wins() {
        let info = parse("[Trash Info]\nPath=/a\nPath=/b\nDeletionDate=2020-01-01T00:00:00\n");
        assert_eq!(info.path, Path::new("/a"));
    }

    #[test]
    fn date_roundtrip() {
        let now = now_epoch();
        let s = now_local_string();
        let back = parse_local_to_epoch(&s).unwrap();
        assert!((back - now).abs() <= 2);
    }

    #[test]
    fn bad_date_is_none() {
        assert!(parse_local_to_epoch("not-a-date").is_none());
        assert!(parse_local_to_epoch("2026-07-03 12:00:00").is_none());
    }
}
