use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use crate::util::{url_decode_bytes, url_encode};

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

/// Largest `.trashinfo` read: a path of `PATH_MAX` bytes, each escaped to
/// three, fits many times over.
pub const RECORD_CAP: u64 = 1 << 20;

/// Read a regular file of at most `cap` bytes. A FIFO, device or directory
/// where a record or the `directorysizes` cache should be (a symlink to one
/// counts) is refused before it is opened, and the open itself never waits,
/// so a planted FIFO cannot hang a listing and `/dev/zero` cannot fill the
/// memory.
pub fn read_bounded(path: &Path, cap: u64) -> std::io::Result<Vec<u8>> {
    use std::io::{Error, ErrorKind, Read};
    use std::os::unix::fs::OpenOptionsExt;
    let not_regular = || Error::new(ErrorKind::InvalidData, "not a regular file");
    if !std::fs::metadata(path)?.is_file() {
        return Err(not_regular());
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(path)?;
    // The path may have changed between the two looks; trust the open file.
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(not_regular());
    }
    let too_big = || Error::new(ErrorKind::InvalidData, format!("larger than {cap} bytes"));
    if meta.len() > cap {
        return Err(too_big());
    }
    let mut body = Vec::with_capacity(meta.len() as usize);
    file.take(cap + 1).read_to_end(&mut body)?;
    if body.len() as u64 > cap {
        return Err(too_big());
    }
    Ok(body)
}

/// Parse a `.trashinfo` file body. Per spec, the first occurrence of a key wins.
pub fn parse(content: &str) -> TrashInfo {
    parse_bytes(content.as_bytes())
}

/// [`parse`] for raw file bytes. Another tool may write a `Path=` with
/// bytes that are not UTF-8 instead of escaping them; those bytes are kept,
/// so the item still lists and restores to the right place.
pub fn parse_bytes(content: &[u8]) -> TrashInfo {
    let mut path = PathBuf::new();
    let mut date = None;
    let mut batch = None;
    let batch_key = format!("{BATCH_KEY}=");
    for line in content.split(|&b| b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if let Some(v) = line.strip_prefix(b"Path=") {
            if path.as_os_str().is_empty() {
                path = PathBuf::from(OsString::from_vec(url_decode_bytes(trim_end(v))));
            }
        } else if let Some(v) = line.strip_prefix(b"DeletionDate=") {
            if date.is_none() {
                date = Some(String::from_utf8_lossy(trim_end(trim_start(v))).into_owned());
            }
        } else if let Some(v) = line.strip_prefix(batch_key.as_bytes()) {
            let v = trim_end(trim_start(v));
            if batch.is_none() && !v.is_empty() {
                batch = Some(String::from_utf8_lossy(v).into_owned());
            }
        }
    }
    TrashInfo {
        path,
        deletion_date: date,
        batch,
    }
}

fn trim_end(mut b: &[u8]) -> &[u8] {
    while let [rest @ .., last] = b {
        if !last.is_ascii_whitespace() {
            break;
        }
        b = rest;
    }
    b
}

fn trim_start(mut b: &[u8]) -> &[u8] {
    while let [first, rest @ ..] = b {
        if !first.is_ascii_whitespace() {
            break;
        }
        b = rest;
    }
    b
}

/// Current local time as `YYYY-MM-DDTHH:MM:SS` (the trashinfo format).
pub fn now_local_string() -> String {
    local_string(unsafe { libc::time(std::ptr::null_mut()) } as i64)
}

/// `epoch` as local time in the trashinfo format.
pub fn local_string(epoch: i64) -> String {
    unsafe {
        let t = epoch as libc::time_t;
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
    fn bounded_reads_take_small_regular_files_only() {
        let dir = std::env::temp_dir().join(format!("rtrash-bounded-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("r.trashinfo");
        std::fs::write(&file, b"[Trash Info]\nPath=/a\n").unwrap();
        assert_eq!(read_bounded(&file, 64).unwrap(), b"[Trash Info]\nPath=/a\n");
        // A symlink to a regular file still reads; the cap holds.
        let link = dir.join("l.trashinfo");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        assert!(read_bounded(&link, 64).is_ok());
        assert!(read_bounded(&file, 8).is_err());
        // A FIFO with no writer: refused at once instead of waiting.
        let fifo = dir.join("f.trashinfo");
        let c = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        assert!(read_bounded(&fifo, RECORD_CAP).is_err());
        // An endless device, through a symlink, and a directory.
        let zero = dir.join("z.trashinfo");
        std::os::unix::fs::symlink("/dev/zero", &zero).unwrap();
        assert!(read_bounded(&zero, RECORD_CAP).is_err());
        assert!(read_bounded(&dir, RECORD_CAP).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

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
