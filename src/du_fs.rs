//! Directory access for `rtrash du`: read a directory's entries from its
//! file descriptor and stat each entry relative to it, without building a
//! path per entry.
//!
//! On Linux the entries come from raw `getdents64` into a reused
//! per-thread buffer, which skips the `fstat` that `opendir` adds and the
//! small buffer of musl's `readdir`. On macOS `getattrlistbulk` returns
//! the names and the stat fields of many entries per call, and the walk
//! falls back to `readdir` when a file system lacks it or the calls turn
//! out to wait on I/O. Other Unix systems use `fdopendir` and `readdir`.
//! Entries without a stat from the listing get one from
//! `fstatat(dirfd, name, AT_SYMLINK_NOFOLLOW)`.

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
    /// Stat of each entry when the listing returned one (macOS bulk
    /// reads); empty otherwise.
    stats: Vec<Option<Stat>>,
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

    /// Add an entry along with the stat its listing returned.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    fn push_with_stat(&mut self, name: &[u8], kind: Kind, stat: Option<Stat>) {
        let before = self.index.len();
        self.push(name, kind);
        if self.index.len() > before {
            self.stats.resize(before, None);
            self.stats.push(stat);
        }
    }

    /// Stat of entry `i` if the listing returned one.
    pub fn stat(&self, i: usize) -> Option<Stat> {
        self.stats.get(i).copied().flatten()
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
        #[cfg(target_os = "macos")]
        {
            bulk::read(self.fd.as_raw_fd()).unwrap_or_else(|| read_readdir(self.fd.as_raw_fd()))
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
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

/// `getattrlistbulk(2)` on macOS. The parser is plain byte handling, so it
/// also builds on other systems for its tests.
#[cfg(any(target_os = "macos", test))]
mod bulk {
    use super::{Entries, Kind, Stat};

    // sys/attr.h
    pub const ATTR_CMN_NAME: u32 = 0x0000_0001;
    pub const ATTR_CMN_DEVID: u32 = 0x0000_0002;
    pub const ATTR_CMN_OBJTYPE: u32 = 0x0000_0008;
    pub const ATTR_CMN_MODTIME: u32 = 0x0000_0400;
    pub const ATTR_CMN_FILEID: u32 = 0x0200_0000;
    pub const ATTR_CMN_ERROR: u32 = 0x2000_0000;
    pub const ATTR_CMN_RETURNED_ATTRS: u32 = 0x8000_0000;
    pub const ATTR_FILE_LINKCOUNT: u32 = 0x0000_0001;
    pub const ATTR_FILE_ALLOCSIZE: u32 = 0x0000_0004;
    pub const ATTR_FILE_DATALENGTH: u32 = 0x0000_0200;

    pub const COMMON: u32 = ATTR_CMN_RETURNED_ATTRS
        | ATTR_CMN_NAME
        | ATTR_CMN_DEVID
        | ATTR_CMN_OBJTYPE
        | ATTR_CMN_MODTIME
        | ATTR_CMN_FILEID
        | ATTR_CMN_ERROR;
    pub const FILE: u32 = ATTR_FILE_LINKCOUNT | ATTR_FILE_ALLOCSIZE | ATTR_FILE_DATALENGTH;

    // sys/vnode.h `enum vtype`, and the matching `st_mode` file types.
    const VREG: u32 = 1;
    const VDIR: u32 = 2;
    const VBLK: u32 = 3;
    const VCHR: u32 = 4;
    const VLNK: u32 = 5;
    const VSOCK: u32 = 6;
    const VFIFO: u32 = 7;

    fn mode_of(objtype: u32) -> u32 {
        match objtype {
            VREG => 0o100000,
            VDIR => 0o040000,
            VBLK => 0o060000,
            VCHR => 0o020000,
            VLNK => 0o120000,
            VSOCK => 0o140000,
            VFIFO => 0o010000,
            _ => 0,
        }
    }

    // With FSOPT_PACK_INVAL_ATTRS every requested attribute has its slot,
    // in bit order within each group, after the entry length and the
    // returned set. Slots are 4-byte aligned, so 8-byte fields are read
    // unaligned.
    const RETURNED: usize = 4;
    const NAME: usize = RETURNED + 20;
    const DEVID: usize = NAME + 8;
    const OBJTYPE: usize = DEVID + 4;
    const MODTIME: usize = OBJTYPE + 4;
    const FILEID: usize = MODTIME + 16;
    const ERROR: usize = FILEID + 8;
    const LINKCOUNT: usize = ERROR + 4;
    const ALLOCSIZE: usize = LINKCOUNT + 4;
    const DATALENGTH: usize = ALLOCSIZE + 8;
    const FIXED: usize = DATALENGTH + 8;

    fn u32_at(b: &[u8], at: usize) -> u32 {
        u32::from_ne_bytes(b[at..at + 4].try_into().unwrap())
    }

    fn u64_at(b: &[u8], at: usize) -> u64 {
        u64::from_ne_bytes(b[at..at + 8].try_into().unwrap())
    }

    /// Parse `count` records from one `getattrlistbulk` fill. Returns
    /// false if the buffer does not hold what the call promised.
    pub fn parse(buf: &[u8], count: usize, out: &mut Entries) -> bool {
        let mut off = 0usize;
        for _ in 0..count {
            if off + FIXED > buf.len() {
                return false;
            }
            let len = u32_at(buf, off) as usize;
            if len < FIXED || off + len > buf.len() {
                return false;
            }
            let rec = &buf[off..off + len];
            off += len;
            let common = u32_at(rec, RETURNED);
            let file = u32_at(rec, RETURNED + 12);
            // The name offset counts from its own attrreference slot, and
            // its length includes the NUL.
            let name_at = NAME as i64 + i64::from(u32_at(rec, NAME) as i32);
            let name_len = u32_at(rec, NAME + 4) as usize;
            if common & ATTR_CMN_NAME == 0 || name_at < 0 || name_at as usize + name_len > rec.len()
            {
                return false;
            }
            let raw = &rec[name_at as usize..name_at as usize + name_len];
            let name = raw.split(|b| *b == 0).next().unwrap_or(raw);
            let objtype = u32_at(rec, OBJTYPE);
            let failed = common & ATTR_CMN_ERROR != 0 && u32_at(rec, ERROR) != 0;
            let typed = common & ATTR_CMN_OBJTYPE != 0;
            let kind = match objtype {
                _ if failed || !typed => Kind::Unknown,
                VDIR => Kind::Dir,
                _ => Kind::Other,
            };
            let complete = common & (ATTR_CMN_DEVID | ATTR_CMN_MODTIME | ATTR_CMN_FILEID)
                == ATTR_CMN_DEVID | ATTR_CMN_MODTIME | ATTR_CMN_FILEID
                && file & FILE == FILE;
            // Directories get their stat from their own descriptor, so
            // only other entries keep the one from the listing.
            let stat = (kind == Kind::Other && complete).then(|| Stat {
                mode: mode_of(objtype),
                size: u64_at(rec, DATALENGTH),
                // stat(2) reports the total allocation in 512-byte blocks.
                blocks: u64_at(rec, ALLOCSIZE).div_ceil(512),
                nlink: u64::from(u32_at(rec, LINKCOUNT)),
                ino: u64_at(rec, FILEID),
                // dev_t is an i32 there; widen it the way `st_dev as u64`
                // does so it compares equal to lstat's.
                dev: u32_at(rec, DEVID) as i32 as u64,
                mtime: u64_at(rec, MODTIME) as i64,
            });
            out.push_with_stat(name, kind, stat);
        }
        true
    }

    #[cfg(target_os = "macos")]
    pub use imp::read;

    #[cfg(target_os = "macos")]
    mod imp {
        use super::*;
        use std::cell::RefCell;
        use std::os::fd::RawFd;
        use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
        use std::sync::OnceLock;
        use std::time::Instant;

        const ATTR_BIT_MAP_COUNT: u16 = 5;
        const FSOPT_PACK_INVAL_ATTRS: u64 = 0x0000_0008;
        const BUF: usize = 64 * 1024;
        /// Entries timed before deciding whether the bulk reads wait on
        /// I/O.
        const SAMPLE: u64 = 4096;
        /// Above this many nanoseconds per entry the reads are waiting on
        /// the disk. A warm cache takes about one microsecond, and
        /// `readdir` with parallel stats keeps more requests in flight.
        const SLOW_NS_PER_ENTRY: u64 = 20_000;

        static OFF: AtomicBool = AtomicBool::new(false);
        static TIMED_ENTRIES: AtomicU64 = AtomicU64::new(0);
        static TIMED_NS: AtomicU64 = AtomicU64::new(0);

        /// `RTRASH_DU_BULK=0` turns the bulk reads off and `1` keeps them
        /// on regardless of timing; anything else lets the timing decide.
        fn setting() -> Option<bool> {
            static S: OnceLock<Option<bool>> = OnceLock::new();
            *S.get_or_init(|| match std::env::var("RTRASH_DU_BULK").as_deref() {
                Ok("0") => Some(false),
                Ok("1") => Some(true),
                _ => None,
            })
        }

        fn record(entries: u64, ns: u64) {
            if setting().is_some() {
                return;
            }
            let n = TIMED_ENTRIES.fetch_add(entries, Ordering::Relaxed) + entries;
            let t = TIMED_NS.fetch_add(ns, Ordering::Relaxed) + ns;
            if n >= SAMPLE && t / n > SLOW_NS_PER_ENTRY {
                OFF.store(true, Ordering::Relaxed);
            }
        }

        /// Read a directory with `getattrlistbulk`. `None` means the
        /// caller should use `readdir`: the bulk reads are off, the file
        /// system does not support them, or they failed before returning
        /// anything.
        pub fn read(fd: RawFd) -> Option<(Entries, bool)> {
            if setting() == Some(false) || (setting().is_none() && OFF.load(Ordering::Relaxed)) {
                return None;
            }
            thread_local! {
                static BUFFER: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
            }
            BUFFER.with(|b| {
                let mut buf = b.borrow_mut();
                if buf.len() < BUF {
                    buf.resize(BUF, 0);
                }
                let mut list = libc::attrlist {
                    bitmapcount: ATTR_BIT_MAP_COUNT,
                    reserved: 0,
                    commonattr: COMMON,
                    volattr: 0,
                    dirattr: 0,
                    fileattr: FILE,
                    forkattr: 0,
                };
                let mut out = Entries::default();
                loop {
                    let start = Instant::now();
                    // SAFETY: `list` and the buffer outlive the call, and
                    // the buffer is writable for its full length.
                    let n = unsafe {
                        libc::getattrlistbulk(
                            fd,
                            &mut list as *mut libc::attrlist as *mut libc::c_void,
                            buf.as_mut_ptr() as *mut libc::c_void,
                            buf.len(),
                            FSOPT_PACK_INVAL_ATTRS,
                        )
                    };
                    if n < 0 {
                        let e = std::io::Error::last_os_error();
                        if e.kind() == std::io::ErrorKind::Interrupted {
                            continue;
                        }
                        // Nothing consumed yet: let readdir try instead.
                        return if out.is_empty() {
                            None
                        } else {
                            Some((out, true))
                        };
                    }
                    if n == 0 {
                        return Some((out, false));
                    }
                    record(n as u64, start.elapsed().as_nanos() as u64);
                    if !parse(&buf, n as usize, &mut out) {
                        return Some((out, true));
                    }
                }
            })
        }
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

    /// One packed `getattrlistbulk` record as macOS lays it out.
    fn bulk_record(name: &str, objtype: u32, error: u32, file_valid: bool) -> Vec<u8> {
        use bulk::*;
        let mut r = Vec::new();
        let common = COMMON & !ATTR_CMN_RETURNED_ATTRS;
        let push32 = |r: &mut Vec<u8>, v: u32| r.extend_from_slice(&v.to_ne_bytes());
        let push64 = |r: &mut Vec<u8>, v: u64| r.extend_from_slice(&v.to_ne_bytes());
        push32(&mut r, 0); // length, patched below
        push32(&mut r, common);
        push32(&mut r, 0);
        push32(&mut r, 0);
        push32(&mut r, if file_valid { FILE } else { 0 });
        push32(&mut r, 0);
        let name_slot = r.len();
        push32(&mut r, 0); // offset, patched below
        push32(&mut r, name.len() as u32 + 1);
        push32(&mut r, (-3i32) as u32); // devid
        push32(&mut r, objtype);
        push64(&mut r, 1_700_000_000); // mtime seconds
        push64(&mut r, 5); // nanoseconds
        push64(&mut r, 4242); // fileid
        push32(&mut r, error);
        push32(&mut r, 2); // link count
        push64(&mut r, 4097); // allocation
        push64(&mut r, 1234); // data length
        let at = r.len();
        r[name_slot..name_slot + 4].copy_from_slice(&((at - name_slot) as u32).to_ne_bytes());
        r.extend_from_slice(name.as_bytes());
        r.push(0);
        while r.len() % 4 != 0 {
            r.push(0);
        }
        let len = r.len() as u32;
        r[..4].copy_from_slice(&len.to_ne_bytes());
        r
    }

    #[test]
    fn bulk_records_parse_into_entries_and_stats() {
        let mut buf = Vec::new();
        buf.extend(bulk_record("file.bin", 1, 0, true));
        buf.extend(bulk_record("sub", 2, 0, false));
        buf.extend(bulk_record("gone", 1, libc::EACCES as u32, true));
        buf.extend(bulk_record("link", 5, 0, true));
        let mut e = Entries::default();
        assert!(bulk::parse(&buf, 4, &mut e));
        assert_eq!(e.len(), 4);
        let (n, k) = e.get(0);
        assert_eq!((n.to_bytes(), k), (&b"file.bin"[..], Kind::Other));
        let s = e.stat(0).unwrap();
        assert!(s.is_file());
        assert_eq!((s.size, s.blocks, s.nlink, s.ino), (1234, 9, 2, 4242));
        assert_eq!((s.dev, s.mtime), (u64::MAX - 2, 1_700_000_000));
        assert_eq!(e.get(1).1, Kind::Dir);
        assert_eq!(e.stat(1), None, "directories stat their own descriptor");
        assert_eq!(e.get(2).1, Kind::Unknown);
        assert_eq!(e.stat(2), None, "a failed entry gets a plain stat later");
        let l = e.stat(3).unwrap();
        assert!(!l.is_file() && !l.is_dir());
        // A record cut short is refused rather than read past its end.
        let mut short = Entries::default();
        assert!(!bulk::parse(&buf[..buf.len() - 8], 4, &mut short));
    }
}
