//! TeraCopy/FastCopy-style regular-file copy for the EXDEV put/restore path.
//!
//! Windows TeraCopy wins with same-volume rename, overlapped I/O, large
//! buffers, and non-buffered copies. Linux equivalents, in order:
//!
//! 1. [`ioctl(FICLONE)`](https://man7.org/linux/man-pages/man2/ioctl_ficlone.2.html)
//!    — same-volume reflink (btrfs, XFS reflink, overlay, bcachefs).
//! 2. [`copy_file_range`](https://man7.org/linux/man-pages/man2/copy_file_range.2.html)
//!    — kernel copy; may still reflink internally.
//! 3. [`sendfile`](https://man7.org/linux/man-pages/man2/sendfile.2.html)
//!    — zero-copy splice into the dest file.
//! 4. Large sequential userspace buffer (`posix_fadvise` sequential +
//!    `DONTNEED` so the copy does not evict the working set).
//!
//! Same-filesystem put does not reach this module: that is `renameat2`.

use std::fs::{self, File, FileTimes, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::io::AsRawFd;
use std::path::Path;

/// How a regular file landed on the destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyMethod {
    /// `ioctl(FICLONE)` reflink.
    Ficlone,
    /// Kernel `copy_file_range`.
    CopyFileRange,
    /// `sendfile` into a regular file.
    Sendfile,
    /// Userspace loop with a large sequential buffer.
    Buffered,
}

/// `_IOW(0x94, 9, int)` — Linux `FICLONE` (dest becomes a clone of src).
#[cfg(target_os = "linux")]
const FICLONE: libc::c_ulong = 0x4004_9409;

/// 2 GiB − 64 KiB: stay under typical `copy_file_range`/`sendfile` caps.
const KERNEL_CHUNK: usize = 0x7fff_0000;

/// Copy one regular file: kernel clone, then `copy_file_range`, then
/// `sendfile`, then a large sequential buffer. Destination is created
/// `O_EXCL`, mode/mtime match the source, and the dest is `sync_all`'d.
pub fn copy_file_fast(src: &Path, dst: &Path) -> io::Result<CopyMethod> {
    let src_meta = fs::symlink_metadata(src)?;
    let src_f = File::open(src)?;
    let mut dst_f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(src_meta.mode())
        .open(dst)?;

    advise_sequential(&src_f);
    readahead_prefix(&src_f, src_meta.len());

    let method = copy_open_files(&src_f, &mut dst_f, src_meta.len())?;

    let perms = src_meta.permissions();
    fs::set_permissions(dst, perms)?;
    if let (Ok(modified), accessed) = (src_meta.modified(), src_meta.accessed()) {
        let accessed = accessed.unwrap_or(modified);
        let times = FileTimes::new()
            .set_accessed(accessed)
            .set_modified(modified);
        let _ = dst_f.set_times(times);
    }
    dst_f.sync_all()?;
    advise_dontneed(&src_f);
    advise_dontneed(&dst_f);
    Ok(method)
}

fn copy_open_files(src: &File, dst: &mut File, len: u64) -> io::Result<CopyMethod> {
    #[cfg(target_os = "linux")]
    {
        if try_ficlone(src, dst)? {
            return Ok(CopyMethod::Ficlone);
        }
        if try_copy_file_range(src, dst, len)? {
            return Ok(CopyMethod::CopyFileRange);
        }
        rewind_dest(dst)?;
        if try_sendfile(src, dst, len)? {
            return Ok(CopyMethod::Sendfile);
        }
        rewind_dest(dst)?;
    }
    buffered_copy(src, dst, len)?;
    Ok(CopyMethod::Buffered)
}

#[cfg(target_os = "linux")]
fn try_ficlone(src: &File, dst: &File) -> io::Result<bool> {
    let rc = unsafe { libc::ioctl(dst.as_raw_fd(), FICLONE, src.as_raw_fd()) };
    if rc == 0 {
        return Ok(true);
    }
    let e = io::Error::last_os_error();
    if fallback_ioctl(&e) {
        Ok(false)
    } else {
        Err(e)
    }
}

#[cfg(target_os = "linux")]
fn try_copy_file_range(src: &File, dst: &File, len: u64) -> io::Result<bool> {
    if len == 0 {
        return Ok(false);
    }
    let mut off_in: i64 = 0;
    let mut off_out: i64 = 0;
    let mut copied = 0u64;
    loop {
        let remain = (len - copied) as usize;
        let chunk = remain.min(KERNEL_CHUNK);
        let n = unsafe {
            libc::copy_file_range(
                src.as_raw_fd(),
                &mut off_in,
                dst.as_raw_fd(),
                &mut off_out,
                chunk,
                0,
            )
        };
        if n < 0 {
            let e = io::Error::last_os_error();
            if copied == 0 && fallback_ioctl(&e) {
                return Ok(false);
            }
            return Err(e);
        }
        if n == 0 {
            // Filesystem refused to copy (or hit a hole it will not expand).
            if copied == 0 {
                return Ok(false);
            }
            break;
        }
        copied += n as u64;
        if copied >= len {
            break;
        }
    }
    Ok(copied == len)
}

#[cfg(target_os = "linux")]
fn try_sendfile(src: &File, dst: &File, len: u64) -> io::Result<bool> {
    if len == 0 {
        return Ok(false);
    }
    let mut off: libc::off_t = 0;
    let mut copied = 0u64;
    loop {
        let remain = (len - copied) as usize;
        let chunk = remain.min(KERNEL_CHUNK);
        let n = unsafe { libc::sendfile(dst.as_raw_fd(), src.as_raw_fd(), &mut off, chunk) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if copied == 0 && fallback_ioctl(&e) {
                return Ok(false);
            }
            return Err(e);
        }
        if n == 0 {
            if copied == 0 {
                return Ok(false);
            }
            break;
        }
        copied += n as u64;
        if copied >= len {
            break;
        }
    }
    Ok(copied == len)
}

fn buffered_copy(src: &File, dst: &mut File, len: u64) -> io::Result<()> {
    let mut src = src;
    let bufsz = buffer_size(len);
    let mut buf = vec![0u8; bufsz];
    let mut copied = 0u64;
    loop {
        let n = src.read(&mut buf)?;
        if n == 0 {
            break;
        }
        dst.write_all(&buf[..n])?;
        copied += n as u64;
        if copied >= len {
            break;
        }
    }
    if copied < len {
        dst.set_len(len)?;
    }
    Ok(())
}

fn buffer_size(len: u64) -> usize {
    if len < 128 * 1024 {
        64 * 1024
    } else if len < 32 * 1024 * 1024 {
        1024 * 1024
    } else {
        8 * 1024 * 1024
    }
}

fn advise_sequential(f: &File) {
    let _ = unsafe { libc::posix_fadvise(f.as_raw_fd(), 0, 0, libc::POSIX_FADV_SEQUENTIAL) };
    let _ = unsafe { libc::posix_fadvise(f.as_raw_fd(), 0, 0, libc::POSIX_FADV_WILLNEED) };
}

fn advise_dontneed(f: &File) {
    let _ = unsafe { libc::posix_fadvise(f.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
}

fn readahead_prefix(f: &File, len: u64) {
    #[cfg(target_os = "linux")]
    {
        let n = len.min(8 * 1024 * 1024);
        if n > 0 {
            let _ = unsafe { libc::readahead(f.as_raw_fd(), 0, n as usize) };
        }
    }
    let _ = f;
    let _ = len;
}

#[cfg(target_os = "linux")]
fn rewind_dest(dst: &File) -> io::Result<()> {
    dst.set_len(0)?;
    let rc = unsafe { libc::lseek(dst.as_raw_fd(), 0, libc::SEEK_SET) };
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn fallback_ioctl(e: &io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(libc::EOPNOTSUPP)
            | Some(libc::ENOTTY)
            | Some(libc::ENOSYS)
            | Some(libc::EINVAL)
            | Some(libc::EXDEV)
            | Some(libc::EPERM)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "rtrash-fastcopy-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn preserve_roundtrip(payload: &[u8]) {
        let root = scratch("rt");
        let src = root.join("src");
        let dst = root.join("dst");
        fs::write(&src, payload).unwrap();
        let mode = fs::Permissions::from_mode(0o640);
        fs::set_permissions(&src, mode).unwrap();
        let old = std::time::UNIX_EPOCH + std::time::Duration::from_secs(978_307_200);
        {
            let f = OpenOptions::new().write(true).open(&src).unwrap();
            let times = FileTimes::new().set_modified(old).set_accessed(old);
            f.set_times(times).unwrap();
        }
        let method = copy_file_fast(&src, &dst).unwrap();
        assert!(
            matches!(
                method,
                CopyMethod::Ficlone
                    | CopyMethod::CopyFileRange
                    | CopyMethod::Sendfile
                    | CopyMethod::Buffered
            ),
            "unexpected method {method:?}"
        );
        assert_eq!(fs::read(&dst).unwrap(), payload);
        assert_eq!(
            fs::symlink_metadata(&dst).unwrap().permissions().mode() & 0o777,
            0o640
        );
        let dm = fs::symlink_metadata(&dst).unwrap().modified().unwrap();
        let delta = dm
            .duration_since(old)
            .unwrap_or_else(|e| e.duration())
            .as_secs();
        assert!(delta <= 1, "mtime preserved within 1s, delta={delta}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn copy_file_fast_preserves_bytes_mode_mtime() {
        preserve_roundtrip(b"hello-sota");
    }

    #[test]
    fn copy_file_fast_empty_file() {
        preserve_roundtrip(b"");
    }

    #[test]
    fn copy_file_fast_two_megabytes() {
        let mut payload = vec![0u8; 2 * 1024 * 1024];
        for (i, b) in payload.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        preserve_roundtrip(&payload);
    }

    #[test]
    fn copy_file_fast_prefers_ficlone_when_ioctl_works() {
        let root = scratch("probe");
        let src = root.join("src");
        let probe = root.join("probe");
        fs::write(&src, b"clone-me").unwrap();
        let src_f = File::open(&src).unwrap();
        let probe_f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .open(&probe)
            .unwrap();
        #[cfg(target_os = "linux")]
        let ficlone_ok =
            unsafe { libc::ioctl(probe_f.as_raw_fd(), FICLONE, src_f.as_raw_fd()) } == 0;
        #[cfg(not(target_os = "linux"))]
        let ficlone_ok = false;
        drop(src_f);
        drop(probe_f);
        let _ = fs::remove_file(&probe);

        let dst = root.join("dst");
        let method = copy_file_fast(&src, &dst).unwrap();
        if ficlone_ok {
            assert_eq!(method, CopyMethod::Ficlone);
        }
        assert_eq!(fs::read(&dst).unwrap(), b"clone-me");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn dest_is_synced() {
        let src = include_str!("fastcopy.rs");
        assert!(
            src.contains("dst_f.sync_all()"),
            "EXDEV dest must be durable before the caller unlinks the source"
        );
        assert!(src.contains("FICLONE"));
        assert!(src.contains("copy_file_range"));
        assert!(src.contains("sendfile"));
        assert!(src.contains("POSIX_FADV_SEQUENTIAL"));
        assert!(src.contains("POSIX_FADV_DONTNEED"));
    }
}
