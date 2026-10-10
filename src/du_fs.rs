//! Directory access for `rtrash du`: read a directory's entries from its
//! file descriptor and stat each entry relative to it, without building a
//! path per entry.
//!
//! On Linux the entries come from raw `getdents64` into a reused
//! per-thread buffer, which skips the `fstat` that `opendir` adds and the
//! small buffer of musl's `readdir`. Other Unix systems use `fdopendir` and
//! `readdir`. Both stat with `fstatat(dirfd, name, AT_SYMLINK_NOFOLLOW)`.

use std::ffi::{CStr, CString, OsStr};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// The fields of `stat` that du uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stat {
    pub mode: u32,
    pub size: u64,
    pub blocks: u64,
    pub nlink: u64,
    pub ino: u64,
    pub dev: u64,
    pub mtime: i64,
}

// The casts are no-ops on Linux, but `stat` field and mode types differ
// across Unix systems (`mode_t` is `u16` on macOS, `st_size` is signed).
#[allow(clippy::unnecessary_cast)]
impl Stat {
    fn from_raw(s: &libc::stat) -> Self {
        Stat {
            mode: s.st_mode as u32,
            size: s.st_size as u64,
            blocks: s.st_blocks as u64,
            nlink: s.st_nlink as u64,
            ino: s.st_ino as u64,
            dev: s.st_dev as u64,
            mtime: s.st_mtime as i64,
        }
    }

    pub fn is_dir(&self) -> bool {
        self.mode & libc::S_IFMT as u32 == libc::S_IFDIR as u32
    }

    pub fn is_file(&self) -> bool {
        self.mode & libc::S_IFMT as u32 == libc::S_IFREG as u32
    }

    /// Apparent size and allocated bytes (`st_blocks * 512`).
    pub fn sizes(&self) -> (u64, u64) {
        (self.size, self.blocks.saturating_mul(512))
    }
}

/// What the directory entry says about its type before any stat.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Dir,
    /// Anything that is known not to be a directory.
    Other,
    Unknown,
}

/// The entries of one directory: names packed into one buffer, each
/// followed by a NUL so it can go straight to `fstatat`.
#[derive(Debug, Default)]
pub struct Entries {
    names: Vec<u8>,
    /// (start in `names`, length without the NUL, kind)
    index: Vec<(usize, usize, Kind)>,
}

impl Entries {
    pub fn len(&self) -> usize {
        self.index.len()
    }

    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    fn push(&mut self, name: &[u8], kind: Kind) {
        if name == b"." || name == b".." {
            return;
        }
        let start = self.names.len();
        self.names.extend_from_slice(name);
        self.names.push(0);
        self.index.push((start, name.len(), kind));
    }

    /// Name and kind of entry `i`.
    pub fn get(&self, i: usize) -> (&CStr, Kind) {
        let (start, len, kind) = self.index[i];
        // SAFETY: push stored the name followed by exactly one NUL, and a
        // directory entry name never contains a NUL byte.
        let c = unsafe { CStr::from_bytes_with_nul_unchecked(&self.names[start..=start + len]) };
        (c, kind)
    }
}

/// An open directory.
#[derive(Debug)]
pub struct Dir {
    fd: OwnedFd,
}

fn cvt(r: libc::c_int) -> io::Result<libc::c_int> {
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

/// Retry a call that failed with EINTR.
fn retry<T>(mut f: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    loop {
        match f() {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            r => return r,
        }
    }
}

const OPEN_DIR: libc::c_int =
    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK;

impl Dir {
    /// Open the directory at `path` without following a final symlink.
    pub fn open(path: &Path) -> io::Result<Dir> {
        let c = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        Self::open_at(libc::AT_FDCWD, &c)
    }

    /// Open the subdirectory `name` of this directory.
    pub fn open_child(&self, name: &CStr) -> io::Result<Dir> {
        Self::open_at(self.fd.as_raw_fd(), name)
    }

    fn open_at(dirfd: RawFd, name: &CStr) -> io::Result<Dir> {
        // SAFETY: `name` is NUL-terminated; a returned fd is owned by us.
        let fd = retry(|| cvt(unsafe { libc::openat(dirfd, name.as_ptr(), OPEN_DIR) }))?;
        Ok(Dir {
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
        })
    }

    /// Stat of the directory itself.
    pub fn stat(&self) -> io::Result<Stat> {
        let mut s = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: fstat fills `s` on success.
        cvt(unsafe { libc::fstat(self.fd.as_raw_fd(), s.as_mut_ptr()) })?;
        Ok(Stat::from_raw(unsafe { &s.assume_init() }))
    }

    /// Stat of entry `name`, not following symlinks.
    pub fn stat_at(&self, name: &CStr) -> io::Result<Stat> {
        stat_at(self.fd.as_raw_fd(), name)
    }

    /// Read every entry except `.` and `..`. Returns the entries read and
    /// whether reading stopped on an error.
    pub fn entries(&self) -> (Entries, bool) {
        #[cfg(target_os = "linux")]
        {
            read_getdents(self.fd.as_raw_fd())
        }
        #[cfg(not(target_os = "linux"))]
        {
            read_readdir(self.fd.as_raw_fd())
        }
    }
}

fn stat_at(dirfd: RawFd, name: &CStr) -> io::Result<Stat> {
    let mut s = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `name` is NUL-terminated and fstatat fills `s` on success.
    retry(|| {
        cvt(unsafe {
            libc::fstatat(
                dirfd,
                name.as_ptr(),
                s.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        })
    })?;
    Ok(Stat::from_raw(unsafe { &s.assume_init() }))
}

/// Stat `path` without following a final symlink.
pub fn lstat(path: &Path) -> io::Result<Stat> {
    let c = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    stat_at(libc::AT_FDCWD, &c)
}

/// Bytes of the per-thread `getdents64` buffer.
#[cfg(target_os = "linux")]
const GETDENTS_BUF: usize = 64 * 1024;

#[cfg(target_os = "linux")]
fn read_getdents(fd: RawFd) -> (Entries, bool) {
    use std::cell::RefCell;
    thread_local! {
        static BUF: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    }
    BUF.with(|buf| {
        let mut buf = buf.borrow_mut();
        if buf.len() < GETDENTS_BUF {
            buf.resize(GETDENTS_BUF, 0);
        }
        let mut out = Entries::default();
        loop {
            let n = retry(|| {
                // SAFETY: the buffer is writable for its full length.
                let r = unsafe {
                    libc::syscall(
                        libc::SYS_getdents64,
                        fd,
                        buf.as_mut_ptr() as *mut libc::c_void,
                        buf.len(),
                    )
                };
                if r < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(r as usize)
                }
            });
            let n = match n {
                Ok(0) => return (out, false),
                Ok(n) => n,
                Err(_) => return (out, true),
            };
            parse_dirents(&buf[..n], &mut out);
        }
    })
}

/// Parse `linux_dirent64` records: ino (8), off (8), reclen (2), type (1),
/// then the NUL-terminated name.
#[cfg(target_os = "linux")]
fn parse_dirents(buf: &[u8], out: &mut Entries) {
    let mut off = 0usize;
    while off + 19 < buf.len() {
        let reclen = u16::from_ne_bytes([buf[off + 16], buf[off + 17]]) as usize;
        if reclen == 0 || off + reclen > buf.len() {
            break;
        }
        let d_type = buf[off + 18];
        let rec = &buf[off + 19..off + reclen];
        let len = rec.iter().position(|b| *b == 0).unwrap_or(rec.len());
        out.push(&rec[..len], kind_of(d_type));
        off += reclen;
    }
}

#[cfg(target_os = "linux")]
fn kind_of(d_type: u8) -> Kind {
    match d_type {
        libc::DT_DIR => Kind::Dir,
        libc::DT_UNKNOWN => Kind::Unknown,
        _ => Kind::Other,
    }
}

/// Portable reader: `readdir` on a duplicate of the descriptor, so the
/// original stays usable for `fstatat` and `openat`. The type is left
/// unknown, so every entry gets a stat. A read error ends the listing
/// like the end of the directory does.
#[cfg_attr(target_os = "linux", allow(dead_code))]
fn read_readdir(fd: RawFd) -> (Entries, bool) {
    let mut out = Entries::default();
    // SAFETY: the duplicate descriptor belongs to the DIR stream from
    // fdopendir on, and closedir releases it. Each dirent is read before
    // the next readdir call.
    unsafe {
        let dup = libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0);
        if dup < 0 {
            return (out, true);
        }
        let dir = libc::fdopendir(dup);
        if dir.is_null() {
            libc::close(dup);
            return (out, true);
        }
        libc::rewinddir(dir);
        loop {
            let ent = libc::readdir(dir);
            if ent.is_null() {
                break;
            }
            let name = CStr::from_ptr((*ent).d_name.as_ptr());
            out.push(name.to_bytes(), Kind::Unknown);
        }
        libc::closedir(dir);
        (out, false)
    }
}

/// Lift the soft limit on open files to the hard limit: the walk keeps a
/// descriptor open for each directory on the path being read.
pub fn raise_nofile_limit() {
    // SAFETY: getrlimit/setrlimit only read and write the struct.
    unsafe {
        let mut r = std::mem::MaybeUninit::<libc::rlimit>::uninit();
        if libc::getrlimit(libc::RLIMIT_NOFILE, r.as_mut_ptr()) != 0 {
            return;
        }
        let mut r = r.assume_init();
        if r.rlim_cur < r.rlim_max {
            r.rlim_cur = r.rlim_max;
            libc::setrlimit(libc::RLIMIT_NOFILE, &r);
        }
    }
}

/// Name bytes of a `CStr` entry as an `OsStr`.
pub fn os(name: &CStr) -> &OsStr {
    OsStr::from_bytes(name.to_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn names(e: &Entries) -> Vec<(String, Kind)> {
        let mut v: Vec<_> = (0..e.len())
            .map(|i| {
                let (n, k) = e.get(i);
                (n.to_string_lossy().into_owned(), k)
            })
            .collect();
        v.sort();
        v
    }

    #[test]
    fn both_readers_list_the_same_names_and_stats_match_std() {
        let root = std::env::temp_dir().join(format!("rtrash-du-fs-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("a.txt"), b"hello").unwrap();
        std::os::unix::fs::symlink("a.txt", root.join("link")).unwrap();
        for i in 0..3000 {
            // Enough names to need more than one buffer fill.
            fs::write(root.join(format!("f-{i:05}-{}", "x".repeat(40))), b"").unwrap();
        }
        let d = Dir::open(&root).unwrap();
        let (e, failed) = d.entries();
        assert!(!failed);
        assert_eq!(e.len(), 3003);
        let (r, failed) = read_readdir(d.fd.as_raw_fd());
        assert!(!failed);
        let a: Vec<String> = names(&e).into_iter().map(|x| x.0).collect();
        let b: Vec<String> = names(&r).into_iter().map(|x| x.0).collect();
        assert_eq!(a, b);
        #[cfg(target_os = "linux")]
        {
            let n = names(&e);
            assert!(n.contains(&("sub".into(), Kind::Dir)));
            assert!(n.contains(&("link".into(), Kind::Other)));
        }

        use std::os::unix::fs::MetadataExt;
        let m = fs::symlink_metadata(root.join("link")).unwrap();
        let s = d.stat_at(c"link").unwrap();
        assert_eq!((s.size, s.ino, s.nlink), (m.len(), m.ino(), m.nlink()));
        assert!(!s.is_dir() && !s.is_file());
        let sub = d.open_child(c"sub").unwrap();
        assert!(sub.stat().unwrap().is_dir());
        assert!(d.open_child(c"link").is_err(), "symlinks are not followed");
        assert_eq!(lstat(&root.join("a.txt")).unwrap().size, 5);
        fs::remove_dir_all(&root).unwrap();
    }
}
