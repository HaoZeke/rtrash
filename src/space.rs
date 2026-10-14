//! Free space and free inodes of a file system, and the check a copy
//! across devices makes before it starts.
//!
//! A put or restore that can't rename copies the tree instead. A copy that
//! fills the target leaves every program on it failing, so the copy first
//! measures the tree and refuses when it would leave less than
//! [`MIN_FREE_PERCENT`] of the bytes or inodes free, or fewer bytes than the
//! floor (`RTRASH_MIN_FREE` or `policy.min_free`).

use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

/// Share of a file system a copy must leave free, in percent.
pub const MIN_FREE_PERCENT: u64 = 5;

/// Share below which a status warns, in percent.
pub const WARN_FREE_PERCENT: u64 = 10;

/// Set by `--no-space-check`: copy whatever the target has left.
static SKIP_CHECK: AtomicBool = AtomicBool::new(false);

pub fn skip_check(on: bool) {
    SKIP_CHECK.store(on, Ordering::Relaxed);
}

/// Size and free room of one file system.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Space {
    pub total: u64,
    /// Bytes this user may still write.
    pub avail: u64,
    /// Inodes in all, when the file system has a fixed number.
    pub inodes: Option<u64>,
    /// Inodes this user may still use.
    pub inodes_avail: Option<u64>,
}

impl Space {
    /// Percent of the bytes still free, rounded down.
    pub fn avail_percent(&self) -> u64 {
        percent(self.avail, self.total)
    }

    /// Percent of the inodes still free, when the file system counts them.
    pub fn inodes_percent(&self) -> Option<u64> {
        match (self.inodes_avail, self.inodes) {
            (Some(a), Some(t)) if t > 0 => Some(percent(a, t)),
            _ => None,
        }
    }

    /// The lows under `pct`, as text such as `3% of bytes free`.
    pub fn lows(&self, pct: u64) -> Vec<String> {
        let mut out = Vec::new();
        if self.total > 0 && self.avail_percent() < pct {
            out.push(format!("{}% of bytes free", self.avail_percent()));
        }
        if let Some(p) = self.inodes_percent().filter(|&p| p < pct) {
            out.push(format!("{p}% of inodes free"));
        }
        out
    }
}

fn percent(part: u64, whole: u64) -> u64 {
    if whole == 0 {
        return 100;
    }
    (u128::from(part) * 100 / u128::from(whole)) as u64
}

/// The file system holding `path`.
#[cfg(unix)]
pub fn query(path: &Path) -> io::Result<Space> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is NUL-terminated and `s` is a plain C struct.
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let unit = if s.f_frsize > 0 {
        s.f_frsize as u64
    } else {
        s.f_bsize as u64
    };
    // btrfs and some network file systems report no inode limit as 0.
    let files = s.f_files as u64;
    Ok(Space {
        total: (s.f_blocks as u64).saturating_mul(unit),
        avail: (s.f_bavail as u64).saturating_mul(unit),
        inodes: (files > 0).then_some(files),
        inodes_avail: (files > 0).then_some(s.f_favail as u64),
    })
}

/// The volume holding `path`. Windows gives no inode count.
#[cfg(windows)]
pub fn query(path: &Path) -> io::Result<Space> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let w: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
    // SAFETY: `w` is NUL-terminated; the outputs are plain integers.
    if unsafe { GetDiskFreeSpaceExW(w.as_ptr(), &mut avail, &mut total, &mut free) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Space {
        total,
        avail,
        inodes: None,
        inodes_avail: None,
    })
}

/// Bytes on disk and inodes a copy of the tree at `src` takes. Symlinks are
/// not followed, and a file with several links inside the tree counts once,
/// as the copy keeps them as links.
#[cfg(unix)]
pub fn tree_need(src: &Path, meta: &std::fs::Metadata) -> io::Result<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let size = |m: &std::fs::Metadata| m.blocks().saturating_mul(512).max(m.len());
    let mut seen = std::collections::HashSet::new();
    let (mut bytes, mut inodes) = (size(meta), 1u64);
    if !meta.is_dir() {
        return Ok((bytes, inodes));
    }
    let mut stack = vec![src.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir)? {
            let e = e?;
            let m = e.metadata()?;
            if !m.is_dir() && m.nlink() > 1 && !seen.insert((m.dev(), m.ino())) {
                continue;
            }
            bytes = bytes.saturating_add(size(&m));
            inodes += 1;
            if m.is_dir() {
                stack.push(e.path());
            }
        }
    }
    Ok((bytes, inodes))
}

/// Whether copying `need_bytes` and `need_inodes` onto `space` leaves
/// enough free; the error names what runs short.
pub fn check_fits(
    space: &Space,
    need_bytes: u64,
    need_inodes: u64,
    floor: u64,
) -> Result<(), String> {
    let mut short = Vec::new();
    let keep = (u128::from(space.total) * u128::from(MIN_FREE_PERCENT) / 100) as u64;
    let keep = keep.max(floor);
    let left = space.avail.checked_sub(need_bytes);
    if left.is_none_or(|l| l < keep) {
        short.push(format!(
            "{} of {} free, the copy needs {} and must leave {}",
            fmt_bytes(space.avail),
            fmt_bytes(space.total),
            fmt_bytes(need_bytes),
            fmt_bytes(keep)
        ));
    }
    if let (Some(total), Some(avail)) = (space.inodes, space.inodes_avail) {
        let keep = total * MIN_FREE_PERCENT / 100;
        let left = avail.checked_sub(need_inodes);
        if left.is_none_or(|l| l < keep) {
            short.push(format!(
                "{avail} of {total} inodes free, the copy needs {need_inodes} and must leave {keep}"
            ));
        }
    }
    if short.is_empty() {
        Ok(())
    } else {
        Err(short.join("; "))
    }
}

/// `40.0 GiB of 126 GiB (31%), 7500000 of 8200000 inodes (91%)`.
pub fn free_line(s: &Space) -> String {
    let mut out = format!(
        "{} of {} ({}%)",
        fmt_bytes(s.avail),
        fmt_bytes(s.total),
        s.avail_percent()
    );
    match (s.inodes_avail, s.inodes, s.inodes_percent()) {
        (Some(a), Some(t), Some(p)) => out.push_str(&format!(", {a} of {t} inodes ({p}%)")),
        _ => out.push_str(", no inode limit"),
    }
    out
}

/// Refuse to copy `need_bytes` onto the volume holding `dest_dir` when
/// that would leave it short, unless `--no-space-check` was given. A
/// volume that can't be measured lets the copy go ahead. Windows has no
/// inode count, so only bytes are checked.
pub fn guard_bytes(need_bytes: u64, dest_dir: &Path) -> io::Result<()> {
    if SKIP_CHECK.load(Ordering::Relaxed) {
        return Ok(());
    }
    let floor = floor().map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let Ok(space) = query(dest_dir) else {
        return Ok(());
    };
    check_fits(&space, need_bytes, 0, floor).map_err(|why| short_error(dest_dir, why))
}

fn short_error(dest_dir: &Path, why: String) -> io::Error {
    io::Error::new(
        io::ErrorKind::StorageFull,
        format!(
            "not enough room to copy onto {}: {why} (--no-space-check copies anyway)",
            dest_dir.display()
        ),
    )
}

fn fmt_bytes(n: u64) -> String {
    crate::util::format_bytes(n)
}

/// The byte floor from `RTRASH_MIN_FREE`, else `policy.min_free`, else 0.
pub fn floor() -> Result<u64, String> {
    if let Some(v) = std::env::var_os("RTRASH_MIN_FREE") {
        let v = v.to_string_lossy();
        return crate::filters::parse_size(&v)
            .ok_or_else(|| format!("invalid RTRASH_MIN_FREE '{v}' (a size such as 10G)"));
    }
    let cfg = crate::config::get();
    match cfg.str("policy", "min_free") {
        None => Ok(0),
        Some(v) => crate::filters::parse_size(v)
            .ok_or_else(|| cfg.bad_value("policy", "min_free", "a size such as 10G")),
    }
}

/// Refuse a copy of `src` into the directory `dest_dir` that would leave
/// its file system short, unless `--no-space-check` was given. A file
/// system that can't be measured lets the copy go ahead.
#[cfg(unix)]
pub fn guard_copy(src: &Path, meta: &std::fs::Metadata, dest_dir: &Path) -> io::Result<()> {
    if SKIP_CHECK.load(Ordering::Relaxed) {
        return Ok(());
    }
    let floor = floor().map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    guard_copy_with(src, meta, dest_dir, floor)
}

#[cfg(unix)]
fn guard_copy_with(
    src: &Path,
    meta: &std::fs::Metadata,
    dest_dir: &Path,
    floor: u64,
) -> io::Result<()> {
    let Ok(space) = query(dest_dir) else {
        return Ok(());
    };
    let (bytes, inodes) = tree_need(src, meta)?;
    check_fits(&space, bytes, inodes, floor).map_err(|why| short_error(dest_dir, why))
}

#[cfg(test)]
mod tests {
    use super::*;

    const G: u64 = 1 << 30;

    fn disk(total: u64, avail: u64, inodes: Option<(u64, u64)>) -> Space {
        Space {
            total,
            avail,
            inodes: inodes.map(|i| i.0),
            inodes_avail: inodes.map(|i| i.1),
        }
    }

    #[test]
    fn a_copy_must_leave_five_percent_of_bytes() {
        let s = disk(100 * G, 20 * G, None);
        assert!(check_fits(&s, 10 * G, 1, 0).is_ok());
        assert!(check_fits(&s, 15 * G, 1, 0).is_ok());
        let e = check_fits(&s, 16 * G, 1, 0).unwrap_err();
        assert!(e.contains("must leave 5.00 GiB"), "{e}");
        assert!(check_fits(&s, 30 * G, 1, 0).is_err());
    }

    #[test]
    fn the_floor_raises_what_a_copy_must_leave() {
        let s = disk(100 * G, 20 * G, None);
        assert!(check_fits(&s, 10 * G, 1, 0).is_ok());
        let e = check_fits(&s, 10 * G, 1, 12 * G).unwrap_err();
        assert!(e.contains("must leave 12.00 GiB"), "{e}");
    }

    #[test]
    fn a_copy_must_leave_five_percent_of_inodes() {
        let s = disk(100 * G, 90 * G, Some((1000, 100)));
        assert!(check_fits(&s, G, 50, 0).is_ok());
        let e = check_fits(&s, G, 60, 0).unwrap_err();
        assert!(e.contains("100 of 1000 inodes free"), "{e}");
        // No inode limit (btrfs): only bytes count.
        let s = disk(100 * G, 90 * G, None);
        assert!(check_fits(&s, G, u64::MAX, 0).is_ok());
    }

    #[test]
    fn lows_name_each_short_resource() {
        let s = disk(100 * G, 9 * G, Some((1000, 50)));
        assert_eq!(s.lows(10), ["9% of bytes free", "5% of inodes free"]);
        assert!(disk(100 * G, 50 * G, Some((1000, 500))).lows(10).is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn windows_volumes_report_bytes_and_refuse_an_overfill() {
        let tmp = std::env::temp_dir();
        let s = query(&tmp).unwrap();
        assert!(s.total > 0 && s.avail <= s.total, "{s:?}");
        assert_eq!(s.inodes, None);
        assert!(free_line(&s).ends_with("no inode limit"));
        let e = guard_bytes(s.avail, &tmp).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::StorageFull);
        assert!(e.to_string().contains("must leave"), "{e}");
        guard_bytes(0, &tmp).ok();
    }

    #[cfg(unix)]
    #[test]
    fn tree_need_counts_each_linked_file_once() {
        let root = std::env::temp_dir().join(format!("rtrash-space-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("d")).unwrap();
        std::fs::write(root.join("d/a"), b"x").unwrap();
        std::fs::hard_link(root.join("d/a"), root.join("d/b")).unwrap();
        std::fs::write(root.join("c"), b"y").unwrap();
        let meta = std::fs::symlink_metadata(&root).unwrap();
        let (bytes, inodes) = tree_need(&root, &meta).unwrap();
        assert_eq!(inodes, 4, "root, d, a and c");
        assert!(bytes >= 2);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn guard_refuses_a_copy_under_the_floor() {
        let dir = std::env::temp_dir();
        let src = dir.join(format!("rtrash-space-guard-{}", std::process::id()));
        std::fs::write(&src, b"z").unwrap();
        let meta = std::fs::symlink_metadata(&src).unwrap();
        let e = guard_copy_with(&src, &meta, &dir, u64::MAX).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::StorageFull);
        assert!(e.to_string().contains("--no-space-check"), "{e}");
        guard_copy_with(&src, &meta, &dir, 0).unwrap();
        let _ = std::fs::remove_file(&src);
    }

    #[cfg(unix)]
    #[test]
    fn query_reads_the_temp_file_system() {
        let s = query(&std::env::temp_dir()).unwrap();
        assert!(s.total > 0 && s.avail <= s.total, "{s:?}");
    }
}
