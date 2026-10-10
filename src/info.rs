use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use crate::util::{url_decode, url_encode};

pub struct TrashInfo {
    /// Original path, absolute or relative to the trash topdir.
    pub path: PathBuf,
    pub deletion_date: Option<String>,
    /// The put run that trashed the item ([`BATCH_KEY`]), when rtrash wrote it.
    pub batch: Option<String>,
}

/// Extra `.trashinfo` key naming the put run, so `rtrash undo` can restore
/// one run as a unit. The spec lets readers ignore keys they do not know,
/// and other tools do.
pub const BATCH_KEY: &str = "X-Rtrash-Batch";

/// This process's put batch id: `MILLIS-PID` (milliseconds since the Unix
/// epoch), the same for every item one run trashes. The leading number
/// orders runs, even two in the same second.
pub fn batch_id() -> &'static str {
    static ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    ID.get_or_init(|| {
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        format!("{ms}-{}", std::process::id())
    })
}

pub fn render(original: &Path, date: &str) -> String {
    render_batch(original, date, None)
}

/// [`render`] plus the batch key when `batch` names one.
pub fn render_batch(original: &Path, date: &str, batch: Option<&str>) -> String {
    let mut body = format!(
        "[Trash Info]\nPath={}\nDeletionDate={}\n",
        url_encode(original.as_os_str().as_bytes()),
        date
    );
    if let Some(b) = batch {
        body.push_str(&format!("{BATCH_KEY}={b}\n"));
    }
    body
}

/// Parse a `.trashinfo` file body. Per spec, the first occurrence of a key wins.
pub fn parse(content: &str) -> TrashInfo {
    let mut path = PathBuf::new();
    let mut date = None;
    let mut batch = None;
    let batch_prefix = format!("{BATCH_KEY}=");
    for line in content.lines() {
        if let Some(v) = line.strip_prefix("Path=") {
            if path.as_os_str().is_empty() {
                path = PathBuf::from(OsString::from_vec(url_decode(v.trim_end())));
            }
        } else if let Some(v) = line.strip_prefix("DeletionDate=") {
            if date.is_none() {
                date = Some(v.trim().to_string());
            }
        } else if let Some(v) = line.strip_prefix(batch_prefix.as_str()) {
            let v = v.trim();
            if batch.is_none() && !v.is_empty() {
                batch = Some(v.to_string());
            }
        }
    }
    TrashInfo {
        path,
        deletion_date: date,
        batch,
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

pub use crate::util::now_epoch;

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
    if !(0..60).contains(&min) || !(0..61).contains(&sec) {
        return mktime_local(year, mon, mday, hour, min, sec);
    }
    // glibc stats /etc/localtime on every mktime when TZ is unset, so a
    // trash of thousands of items paid one stat each. Offsets change on
    // hour boundaries, so one mktime per hour serves every entry in it.
    thread_local! {
        static HOURS: std::cell::RefCell<HourCache> = std::cell::RefCell::new(HourCache::new());
    }
    let key = (year, mon, mday, hour);
    let base = HOURS.with(|h| {
        let mut h = h.borrow_mut();
        if h.len() > 4096 {
            h.clear();
        }
        *h.entry(key)
            .or_insert_with(|| mktime_local(year, mon, mday, hour, 0, 0))
    })?;
    Some(base + i64::from(min) * 60 + i64::from(sec))
}

/// Epoch of each local `(year, month, day, hour)` seen, from mktime.
type HourCache = std::collections::HashMap<(i32, i32, i32, i32), Option<i64>>;

fn mktime_local(year: i32, mon: i32, mday: i32, hour: i32, min: i32, sec: i32) -> Option<i64> {
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
    fn batch_key_roundtrips_and_plain_render_omits_it() {
        let body = render_batch(Path::new("/a"), "2026-07-03T12:00:00", Some("17-42"));
        assert!(body.ends_with("X-Rtrash-Batch=17-42\n"), "{body}");
        assert_eq!(parse(&body).batch.as_deref(), Some("17-42"));
        assert_eq!(parse(&render(Path::new("/a"), "x")).batch, None);
        assert_eq!(batch_id(), batch_id());
    }

    #[test]
    fn hour_cache_matches_mktime_for_every_minute_and_second() {
        for s in [
            "2026-03-08T01:59:59",
            "2026-03-08T03:00:00",
            "2026-11-01T01:30:00",
            "2024-02-29T23:59:60",
            "2026-07-03T12:34:56",
        ] {
            let b = s.as_bytes();
            let n = |r: std::ops::Range<usize>| parse_num(&b[r]).unwrap();
            let want = mktime_local(n(0..4), n(5..7), n(8..10), n(11..13), n(14..16), n(17..19));
            assert_eq!(parse_local_to_epoch(s), want, "{s}");
        }
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
