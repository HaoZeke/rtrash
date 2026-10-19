//! Unix directory access: `openat`, `fstatat`, and `getdents64` on Linux,
//! `getattrlistbulk` on macOS, `readdir` elsewhere.

use super::{Entries, Kind, Stat};
use std::ffi::{CStr, CString, OsStr};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

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

/// statfs magic of btrfs.
#[cfg(target_os = "linux")]
const BTRFS_MAGIC: u32 = 0x9123_683e;
/// `_IOR(0x94, 25, u64)` from linux/btrfs.h: the flags of a subvolume.
#[cfg(target_os = "linux")]
const BTRFS_IOC_SUBVOL_GETFLAGS: u32 = 0x8008_9419;
/// The read-only bit of those flags.
#[cfg(target_os = "linux")]
const BTRFS_SUBVOL_RDONLY: u64 = 1 << 1;

/// statfs magic numbers of the kernel pseudo file systems ncdu's
/// --exclude-kernfs skips, in this order: proc, sysfs, cgroup, cgroup2,
/// debugfs, tracefs, securityfs, pstore, bpf, configfs, efivarfs and
/// selinuxfs.
#[cfg(target_os = "linux")]
const KERNFS_MAGIC: [u32; 12] = [
    0x9fa0,
    0x6265_6572,
    0x0027_e0eb,
    0x6367_7270,
    0x6462_6720,
    0x7472_6163,
    0x7363_6673,
    0x6165_676c,
    0xcafe_4a11,
    0x6265_6570,
    0xde5e_81e4,
    0xf97c_ff8c,
];

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

    /// Open the directory at `path`, following symlinks (`du -L`).
    pub fn open_follow(path: &Path) -> io::Result<Dir> {
        let c = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        Self::open_with(libc::AT_FDCWD, &c, OPEN_DIR & !libc::O_NOFOLLOW)
    }

    /// Open the subdirectory `name`, or the directory a symlink `name`
    /// points to (`du -L`).
    pub fn open_child_follow(&self, name: &CStr) -> io::Result<Dir> {
        Self::open_with(self.fd.as_raw_fd(), name, OPEN_DIR & !libc::O_NOFOLLOW)
    }

    /// Stat of entry `name`, following a symlink to what it points to.
    pub fn stat_follow(&self, name: &CStr) -> io::Result<Stat> {
        stat_at_flags(self.fd.as_raw_fd(), name, 0)
    }

    fn open_at(dirfd: RawFd, name: &CStr) -> io::Result<Dir> {
        Self::open_with(dirfd, name, OPEN_DIR)
    }

    fn open_with(dirfd: RawFd, name: &CStr, flags: libc::c_int) -> io::Result<Dir> {
        // SAFETY: `name` is NUL-terminated; a returned fd is owned by us.
        let fd = retry(|| cvt(unsafe { libc::openat(dirfd, name.as_ptr(), flags) }))?;
        Ok(Dir {
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
        })
    }

    /// Open the regular file `name` of this directory for reading, without
    /// following a symlink.
    pub fn open_file(&self, name: &CStr) -> io::Result<std::fs::File> {
        const FLAGS: libc::c_int =
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK | libc::O_NOCTTY;
        // SAFETY: `name` is NUL-terminated; a returned fd is owned by us.
        let fd = retry(|| cvt(unsafe { libc::openat(self.fd.as_raw_fd(), name.as_ptr(), FLAGS) }))?;
        Ok(std::fs::File::from(unsafe { OwnedFd::from_raw_fd(fd) }))
    }

    /// True when the directory is on a kernel pseudo file system (proc,
    /// sysfs, cgroup, debugfs and the like), whose sizes are not disk use.
    /// Only Linux reports these; elsewhere it is always false.
    pub fn is_kernfs(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            let mut s = std::mem::MaybeUninit::<libc::statfs>::uninit();
            // SAFETY: fstatfs fills `s` on success; it is read only then.
            if unsafe { libc::fstatfs(self.fd.as_raw_fd(), s.as_mut_ptr()) } != 0 {
                return false;
            }
            let kind = unsafe { s.assume_init() }.f_type as u32;
            KERNFS_MAGIC.contains(&kind)
        }
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
    }

    /// True when the directory is the top of a read-only btrfs
    /// subvolume, as the snapshots of snapper and Timeshift are. Linux
    /// only; elsewhere it is always false.
    pub fn is_readonly_snapshot(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            let mut s = std::mem::MaybeUninit::<libc::statfs>::uninit();
            // SAFETY: fstatfs fills `s` on success; it is read only then.
            if unsafe { libc::fstatfs(self.fd.as_raw_fd(), s.as_mut_ptr()) } != 0 {
                return false;
            }
            if unsafe { s.assume_init() }.f_type as u32 != BTRFS_MAGIC {
                return false;
            }
            let mut flags = 0u64;
            // SAFETY: the ioctl writes one u64 into `flags`; it fails for
            // a directory that is not the top of a subvolume.
            let r = unsafe {
                libc::ioctl(
                    self.fd.as_raw_fd(),
                    BTRFS_IOC_SUBVOL_GETFLAGS as _,
                    &mut flags as *mut u64,
                )
            };
            r == 0 && flags & BTRFS_SUBVOL_RDONLY != 0
        }
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
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
        self.entries_upto(usize::MAX)
    }

    /// [`Self::entries`], stopping once `max` or more are in: the rest of
    /// the buffer that crossed the mark comes along. macOS reads them
    /// all, since its bulk reader times whole listings.
    pub fn entries_upto(&self, max: usize) -> (Entries, bool) {
        #[cfg(target_os = "linux")]
        {
            read_getdents(self.fd.as_raw_fd(), max)
        }
        #[cfg(target_os = "macos")]
        {
            let _ = max;
            super::bulk::read(self.fd.as_raw_fd())
                .unwrap_or_else(|| read_readdir(self.fd.as_raw_fd(), usize::MAX))
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            read_readdir(self.fd.as_raw_fd(), max)
        }
    }
}

fn stat_at(dirfd: RawFd, name: &CStr) -> io::Result<Stat> {
    stat_at_flags(dirfd, name, libc::AT_SYMLINK_NOFOLLOW)
}

fn stat_at_flags(dirfd: RawFd, name: &CStr, flags: libc::c_int) -> io::Result<Stat> {
    let mut s = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `name` is NUL-terminated and fstatat fills `s` on success.
    retry(|| cvt(unsafe { libc::fstatat(dirfd, name.as_ptr(), s.as_mut_ptr(), flags) }))?;
    Ok(Stat::from_raw(unsafe { &s.assume_init() }))
}

/// Stat `path`, following symlinks (`du -L`).
pub fn stat_follow(path: &Path) -> io::Result<Stat> {
    let c = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    stat_at_flags(libc::AT_FDCWD, &c, 0)
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
fn read_getdents(fd: RawFd, max: usize) -> (Entries, bool) {
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
            if out.len() >= max {
                return (out, false);
            }
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
fn read_readdir(fd: RawFd, max: usize) -> (Entries, bool) {
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
        while out.len() < max {
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
/// descriptor open for each directory on the path being read, up to a
/// share of this limit.
pub fn raise_nofile_limit() {
    // SAFETY: getrlimit/setrlimit only read and write the struct.
    unsafe {
        let mut r = std::mem::MaybeUninit::<libc::rlimit>::uninit();
        if libc::getrlimit(libc::RLIMIT_NOFILE, r.as_mut_ptr()) != 0 {
            return;
        }
        let mut r = r.assume_init();
        if r.rlim_cur < r.rlim_max {
            let old = r.rlim_cur;
            r.rlim_cur = r.rlim_max;
            if libc::setrlimit(libc::RLIMIT_NOFILE, &r) != 0 {
                // macOS refuses a soft limit past OPEN_MAX (10240), even
                // with no hard limit; ask for that much instead.
                let open_max: libc::rlim_t = 10240;
                if open_max > old {
                    r.rlim_cur = open_max.min(r.rlim_max);
                    libc::setrlimit(libc::RLIMIT_NOFILE, &r);
                }
            }
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
    use std::collections::BTreeSet;
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

    /// Every stat the listing returned, compared field by field with
    /// `fstatat`. Returns one line per difference.
    fn listing_stat_differences(d: &Dir, e: &Entries) -> Vec<String> {
        let mut out = Vec::new();
        for i in 0..e.len() {
            let (name, kind) = e.get(i);
            let want = match d.stat_at(name) {
                Ok(s) => s,
                Err(err) => {
                    out.push(format!("{name:?}: fstatat failed: {err}"));
                    continue;
                }
            };
            if (kind == Kind::Dir) != want.is_dir() && kind != Kind::Unknown {
                out.push(format!("{name:?}: kind {kind:?}, mode {:o}", want.mode));
            }
            let Some(got) = e.stat(i) else { continue };
            let fields = [
                ("mode", u64::from(got.mode), u64::from(want.mode)),
                ("size", got.size, want.size),
                ("blocks", got.blocks, want.blocks),
                ("nlink", got.nlink, want.nlink),
                ("ino", got.ino, want.ino),
                ("dev", got.dev, want.dev),
                ("mtime", got.mtime as u64, want.mtime as u64),
            ];
            for (field, g, w) in fields {
                if g != w {
                    out.push(format!("{name:?}: {field} listing={g} fstatat={w}"));
                }
            }
        }
        out
    }

    #[test]
    fn both_readers_list_the_same_names_and_stats_match_std() {
        let root = std::env::temp_dir().join(format!("rtrash-du-fs-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("a.txt"), b"hello").unwrap();
        fs::write(root.join("big.bin"), vec![7u8; 100_000]).unwrap();
        fs::hard_link(root.join("a.txt"), root.join("a-link.txt")).unwrap();
        std::os::unix::fs::symlink("a.txt", root.join("link")).unwrap();
        for i in 0..3000 {
            // Enough names to need more than one buffer fill.
            fs::write(root.join(format!("f-{i:05}-{}", "x".repeat(40))), b"").unwrap();
        }
        let d = Dir::open(&root).unwrap();
        // On macOS read with getattrlistbulk itself, not whichever reader
        // the timing heuristic picks.
        #[cfg(target_os = "macos")]
        let (e, failed) = super::super::bulk::read(d.fd.as_raw_fd())
            .expect("getattrlistbulk read nothing from an APFS temp directory");
        #[cfg(not(target_os = "macos"))]
        let (e, failed) = d.entries();
        let (r, r_failed) = read_readdir(d.fd.as_raw_fd(), usize::MAX);
        let a: Vec<String> = names(&e).into_iter().map(|x| x.0).collect();
        let b: Vec<String> = names(&r).into_iter().map(|x| x.0).collect();
        let (sa, sb): (BTreeSet<&String>, BTreeSet<&String>) =
            (a.iter().collect(), b.iter().collect());
        let only_listing: Vec<_> = sa.difference(&sb).take(10).collect();
        let only_readdir: Vec<_> = sb.difference(&sa).take(10).collect();
        let mut problems = listing_stat_differences(&d, &e);
        // How many entries differ in each field, so a capped list still
        // shows whether a few entries or all of them are off.
        let mut per_field = std::collections::BTreeMap::<String, usize>::new();
        for p in &problems {
            let field = p.rsplit("\": ").next().unwrap_or(p);
            let field = field.split(' ').next().unwrap_or(field).to_string();
            *per_field.entry(field).or_default() += 1;
        }
        let total = problems.len();
        problems.truncate(20);
        assert!(
            !failed && !r_failed && a == b && total == 0,
            "listing failed={failed} with {} entries, readdir failed={r_failed} with {}; \
             only in listing {only_listing:?}; only in readdir {only_readdir:?}; \
             {total} stat differences by field {per_field:?}, first ones {problems:#?}",
            a.len(),
            b.len(),
        );
        assert_eq!(e.len(), 3005);
        #[cfg(target_os = "macos")]
        {
            let with_stats = (0..e.len()).filter(|&i| e.stat(i).is_some()).count();
            assert_eq!(with_stats, 3004, "every entry but sub/ carries a stat");
        }
        #[cfg(target_os = "linux")]
        {
            let n = names(&e);
            assert!(n.contains(&("sub".into(), Kind::Dir)));
            assert!(n.contains(&("link".into(), Kind::Other)));
        }

        // A bounded read stops after the buffer that reaches the mark.
        // Fresh descriptors, since a read moves the offset.
        let (p, failed) = Dir::open(&root).unwrap().entries_upto(10);
        assert!(!failed && p.len() >= 10, "{}", p.len());
        #[cfg(not(target_os = "macos"))]
        assert!(p.len() < e.len(), "{} of {}", p.len(), e.len());
        let (r5, _) = read_readdir(Dir::open(&root).unwrap().fd.as_raw_fd(), 5);
        assert_eq!(r5.len(), 5);

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
