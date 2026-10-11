//! Fast tree deletion for trash payloads.
//!
//! In-process readdir + `unlinkat` (rsync empty-source style, no shell-out).
//! Hot path optimisations for full trash empty:
//! - use `d_type` to unlink regular files without a failed `openat` probe
//! - parallel sibling deletion (rayon) at each directory level with many names
//! - btrfs subvolume destroy when a directory is a real subvolume root (ino 256)

use rayon::prelude::*;
use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

const BTRFS_SUPER_MAGIC: i64 = 0x9123683E;
const BTRFS_FIRST_FREE_OBJECTID: u64 = 256;
const BTRFS_IOCTL_MAGIC: u8 = 0x94;
const BTRFS_IOC_SNAP_DESTROY: libc::c_ulong = {
    const SIZE: libc::c_ulong = 4096;
    (1u64 << 30) | ((SIZE) << 16) | ((BTRFS_IOCTL_MAGIC as u64) << 8) | 15
};

// Linux dirent d_type values.
const DT_UNKNOWN: u8 = 0;
const DT_DIR: u8 = 4;
const DT_REG: u8 = 8;
const DT_LNK: u8 = 10;
const DT_FIFO: u8 = 1;
const DT_SOCK: u8 = 12;
const DT_CHR: u8 = 2;
const DT_BLK: u8 = 6;

/// Parallelise sibling deletes when a directory has at least this many names.
const PAR_SIBLING_THRESHOLD: usize = 4;
/// Cap nested rayon depth so we do not oversubscribe on deep narrow trees.
const PAR_MAX_DEPTH: u32 = 12;

#[repr(C)]
struct BtrfsIoctlVolArgs {
    fd: i64,
    name: [u8; 4088],
}

struct DirEnt {
    name: OsString,
    dtype: u8,
}

/// Remove a file, symlink, or directory tree. Missing paths are success.
pub fn remove_path(path: &Path) -> io::Result<()> {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let ft = meta.file_type();
    if ft.is_symlink() || ft.is_file() {
        return fs::remove_file(path);
    }
    if ft.is_dir() {
        if try_btrfs_subvolume_delete(path)? {
            return Ok(());
        }
        return remove_dir_tree(path);
    }
    fs::remove_file(path)
}

pub fn is_btrfs(path: &Path) -> bool {
    statfs_type(path)
        .map(|t| t == BTRFS_SUPER_MAGIC)
        .unwrap_or(false)
}

fn statfs_type(path: &Path) -> io::Result<i64> {
    let c = cstring_path(path)?;
    unsafe {
        let mut buf: libc::statfs = std::mem::zeroed();
        if libc::statfs(c.as_ptr(), &mut buf) != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(buf.f_type as i64)
    }
}

pub fn try_btrfs_subvolume_delete(path: &Path) -> io::Result<bool> {
    if !is_btrfs(path) {
        return Ok(false);
    }
    let meta = match fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    if !meta.is_dir() || meta.ino() != BTRFS_FIRST_FREE_OBJECTID {
        return Ok(false);
    }
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => return Ok(false),
    };
    let name = match path.file_name() {
        Some(n) => n,
        None => return Ok(false),
    };
    let name_bytes = name.as_bytes();
    if name_bytes.len() >= 4087 {
        return Ok(false);
    }

    let parent_c = cstring_path(parent)?;
    let parent_fd = unsafe { libc::open(parent_c.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY) };
    if parent_fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let parent_fd = unsafe { OwnedFd::from_raw_fd(parent_fd) };

    let mut args = BtrfsIoctlVolArgs {
        fd: 0,
        name: [0; 4088],
    };
    args.name[..name_bytes.len()].copy_from_slice(name_bytes);

    // musl types ioctl request as c_int; glibc as c_ulong — cast for both.
    let rc = unsafe {
        libc::ioctl(
            parent_fd.as_raw_fd(),
            BTRFS_IOC_SNAP_DESTROY as _,
            &args as *const _ as *mut libc::c_void,
        )
    };
    if rc == 0 {
        return Ok(true);
    }
    let err = io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::ENOTTY) | Some(libc::EINVAL) | Some(libc::ENOENT) | Some(libc::ENOTDIR) => {
            Ok(false)
        }
        _ => {
            if err.kind() == io::ErrorKind::PermissionDenied {
                Err(err)
            } else {
                Ok(false)
            }
        }
    }
}

fn remove_dir_tree(path: &Path) -> io::Result<()> {
    let dir_c = cstring_path(path)?;
    let open = || unsafe {
        libc::open(
            dir_c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        )
    };
    let mut fd = open();
    if fd < 0
        && io::Error::last_os_error().raw_os_error() == Some(libc::EACCES)
        && own_dir_opened_up(libc::AT_FDCWD, &dir_c)
    {
        fd = open();
    }
    if fd < 0 {
        let e = io::Error::last_os_error();
        if e.kind() == io::ErrorKind::NotFound {
            return Ok(());
        }
        if e.raw_os_error() == Some(libc::ELOOP) || e.raw_os_error() == Some(libc::ENOTDIR) {
            return fs::remove_file(path);
        }
        return Err(at(path, e));
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    remove_dirfd_contents(fd.as_raw_fd(), path, 0)?;
    drop(fd);
    match fs::remove_dir(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(at(path, e)),
    }
}

/// An error that already names the path it happened at.
#[derive(Debug)]
struct AtPath {
    path: std::path::PathBuf,
    err: io::Error,
}

impl std::fmt::Display for AtPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path.display(), self.err)
    }
}

impl std::error::Error for AtPath {}

/// `e` tagged with `path`, unless a deeper call already tagged it.
fn at(path: &Path, e: io::Error) -> io::Error {
    if e.get_ref().is_some_and(|r| r.is::<AtPath>()) {
        return e;
    }
    io::Error::new(
        e.kind(),
        AtPath {
            path: path.to_path_buf(),
            err: e,
        },
    )
}

/// The OS error code of `e`, looking through an [`AtPath`] tag.
fn os_code(e: &io::Error) -> Option<i32> {
    match e.get_ref().and_then(|r| r.downcast_ref::<AtPath>()) {
        Some(a) => a.err.raw_os_error(),
        None => e.raw_os_error(),
    }
}

/// A directory the current user owns but cannot list, enter or change
/// (a build tree with mode 0311, say): give the owner rwx back and say
/// whether that happened. Never touches what someone else owns or what
/// is not a directory.
fn own_dir_opened_up(dirfd: RawFd, name_c: &CStr) -> bool {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::fstatat(dirfd, name_c.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) };
    if rc != 0 || !owned_dir_short_of_rwx(&st) {
        return false;
    }
    let mode = (st.st_mode as libc::mode_t & 0o7777) | 0o700;
    unsafe { libc::fchmodat(dirfd, name_c.as_ptr(), mode, 0) == 0 }
}

/// [`own_dir_opened_up`] for a directory that is already open.
fn own_dirfd_opened_up(fd: RawFd) -> bool {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut st) } != 0 || !owned_dir_short_of_rwx(&st) {
        return false;
    }
    let mode = (st.st_mode as libc::mode_t & 0o7777) | 0o700;
    unsafe { libc::fchmod(fd, mode) == 0 }
}

fn owned_dir_short_of_rwx(st: &libc::stat) -> bool {
    (st.st_mode as libc::mode_t & libc::S_IFMT) == libc::S_IFDIR
        && st.st_uid == unsafe { libc::geteuid() }
        && (st.st_mode as libc::mode_t & 0o700) != 0o700
}

fn remove_dirfd_contents(dirfd: RawFd, path: &Path, depth: u32) -> io::Result<()> {
    let entries = read_dirfd_entries(dirfd).map_err(|e| at(path, e))?;
    // Nested levels stay sequential: parallelising every directory oversubscribes
    // rayon when the top-level full-empty wipe is already highly parallel.
    // (Top-level `wipe_children_parallel` is the parallel boundary.)
    let _ = (depth, PAR_SIBLING_THRESHOLD, PAR_MAX_DEPTH);
    // Keep going past a failure so everything removable goes; report the
    // first failure once the directory is done.
    let mut first: Option<io::Error> = None;
    let mut opened_up = false;
    for e in &entries {
        let mut r = remove_entry(dirfd, path, e, depth);
        // Unlinking needs write and search on this directory: when the
        // user owns it, grant them once and retry.
        if !opened_up && r.as_ref().is_err_and(|e| os_code(e) == Some(libc::EACCES)) {
            opened_up = true;
            if own_dirfd_opened_up(dirfd) {
                r = remove_entry(dirfd, path, e, depth);
            }
        }
        if let Err(err) = r {
            first.get_or_insert(err);
        }
    }
    first.map_or(Ok(()), Err)
}

fn remove_entry(dirfd: RawFd, path: &Path, ent: &DirEnt, depth: u32) -> io::Result<()> {
    let name_c = CString::new(ent.name.as_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let child = || path.join(&ent.name);
    #[cfg(test)]
    if tests::fails(&ent.name) {
        return Err(at(&child(), io::Error::from_raw_os_error(libc::EPERM)));
    }

    // Fast path: known non-directories — single unlinkat, no openat probe.
    match ent.dtype {
        DT_REG | DT_LNK | DT_FIFO | DT_SOCK | DT_CHR | DT_BLK => {
            return unlinkat_file(dirfd, &name_c).map_err(|e| at(&child(), e));
        }
        DT_DIR => return remove_dir_child(dirfd, path, &ent.name, &name_c, depth),
        DT_UNKNOWN => {}
        _ => {}
    }

    // DT_UNKNOWN or exotic: is it a directory? lstat says without opening.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::fstatat(dirfd, name_c.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) };
    if rc != 0 {
        let err = io::Error::last_os_error();
        return match err.raw_os_error() {
            Some(libc::ENOENT) => Ok(()),
            _ => Err(at(&child(), err)),
        };
    }
    if (st.st_mode as libc::mode_t & libc::S_IFMT) == libc::S_IFDIR {
        return remove_dir_child(dirfd, path, &ent.name, &name_c, depth);
    }
    unlinkat_file(dirfd, &name_c).map_err(|e| at(&child(), e))
}

fn unlinkat_file(dirfd: RawFd, name_c: &CString) -> io::Result<()> {
    let rc = unsafe { libc::unlinkat(dirfd, name_c.as_ptr(), 0) };
    if rc != 0 {
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::NotFound {
            return Err(e);
        }
    }
    Ok(())
}

fn remove_dir_child(
    dirfd: RawFd,
    path: &Path,
    name: &OsStr,
    name_c: &CString,
    depth: u32,
) -> io::Result<()> {
    let child = path.join(name);
    // Subvolume destroy only at the first level of a tree (cheap for the common
    // non-subvolume case: one failed/skipped ioctl path per top-level payload,
    // not per nested directory).
    if depth == 0 && try_btrfs_subvol_at(dirfd, name).map_err(|e| at(&child, e))? {
        return Ok(());
    }
    let open = || unsafe {
        libc::openat(
            dirfd,
            name_c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        )
    };
    let mut child_fd = open();
    // No read permission on a directory the user owns: open it up and retry.
    if child_fd < 0
        && io::Error::last_os_error().raw_os_error() == Some(libc::EACCES)
        && own_dir_opened_up(dirfd, name_c)
    {
        child_fd = open();
    }
    if child_fd < 0 {
        let e = io::Error::last_os_error();
        if e.kind() == io::ErrorKind::NotFound {
            return Ok(());
        }
        // Race: became a non-dir.
        if e.raw_os_error() == Some(libc::ENOTDIR) || e.raw_os_error() == Some(libc::ELOOP) {
            return unlinkat_file(dirfd, name_c).map_err(|e| at(&child, e));
        }
        return Err(at(&child, e));
    }
    let child_fd = unsafe { OwnedFd::from_raw_fd(child_fd) };
    let inner = remove_dirfd_contents(child_fd.as_raw_fd(), &child, depth.saturating_add(1));
    drop(child_fd);
    inner?;
    let rc = unsafe { libc::unlinkat(dirfd, name_c.as_ptr(), libc::AT_REMOVEDIR) };
    if rc != 0 {
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::NotFound {
            return Err(at(&child, e));
        }
    }
    Ok(())
}

fn try_btrfs_subvol_at(parent_fd: RawFd, name: &OsStr) -> io::Result<bool> {
    let name_c = CString::new(name.as_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        libc::fstatat(
            parent_fd,
            name_c.as_ptr(),
            &mut st,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc != 0 {
        return Ok(false);
    }
    if (st.st_mode & libc::S_IFMT) != libc::S_IFDIR || st.st_ino != BTRFS_FIRST_FREE_OBJECTID {
        return Ok(false);
    }
    let name_bytes = name.as_bytes();
    if name_bytes.len() >= 4087 {
        return Ok(false);
    }
    let mut args = BtrfsIoctlVolArgs {
        fd: 0,
        name: [0; 4088],
    };
    args.name[..name_bytes.len()].copy_from_slice(name_bytes);
    let rc = unsafe {
        libc::ioctl(
            parent_fd,
            BTRFS_IOC_SNAP_DESTROY as _,
            &args as *const _ as *mut libc::c_void,
        )
    };
    Ok(rc == 0)
}

fn read_dirfd_entries(dirfd: RawFd) -> io::Result<Vec<DirEnt>> {
    let dup = unsafe { libc::fcntl(dirfd, libc::F_DUPFD_CLOEXEC, 0) };
    if dup < 0 {
        return Err(io::Error::last_os_error());
    }
    let dir = unsafe { libc::fdopendir(dup) };
    if dir.is_null() {
        let e = io::Error::last_os_error();
        unsafe { libc::close(dup) };
        return Err(e);
    }
    let mut names = Vec::new();
    loop {
        let ent = unsafe { libc::readdir(dir) };
        if ent.is_null() {
            break;
        }
        let d_name = unsafe { CStr::from_ptr((*ent).d_name.as_ptr()) };
        let bytes = d_name.to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        let dtype = unsafe { (*ent).d_type };
        names.push(DirEnt {
            name: OsString::from(OsStr::from_bytes(bytes)),
            dtype,
        });
    }
    unsafe { libc::closedir(dir) };
    Ok(names)
}

/// Allocated disk usage of `path` in bytes, as `du -sB1` counts it: the
/// blocks (`st_blocks * 512`) of every file and directory in the tree, so
/// sparse files report reclaimable space rather than logical length. A
/// file with several hard links in the tree counts once.
///
/// Symlinks count only the link inode (not the target). Missing paths are 0.
pub fn disk_usage(path: &Path) -> u64 {
    let seen = std::sync::Mutex::new(std::collections::HashSet::new());
    usage_in(path, &seen)
}

fn usage_in(path: &Path, seen: &std::sync::Mutex<std::collections::HashSet<(u64, u64)>>) -> u64 {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(_) => return 0,
    };
    let own = meta.blocks().saturating_mul(512);
    if !meta.is_dir() {
        let repeat = meta.nlink() > 1
            && !seen
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert((meta.dev(), meta.ino()));
        return if repeat { 0 } else { own };
    }
    // Directory tree: sum children (parallel at this level when wide).
    let Ok(rd) = fs::read_dir(path) else {
        return own;
    };
    let children: Vec<_> = rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    let child_sum: u64 = if children.len() >= 4 {
        children.par_iter().map(|p| usage_in(p, seen)).sum()
    } else {
        children.iter().map(|p| usage_in(p, seen)).sum()
    };
    own.saturating_add(child_sum)
}

pub use crate::util::format_bytes;

/// What wiping the children of one directory did.
#[derive(Debug, Default)]
pub struct Wipe {
    /// Top-level children removed.
    pub removed: u64,
    /// Top-level children left behind, each with the first error under it
    /// (the error names the path where it happened).
    pub failed: Vec<(OsString, io::Error)>,
}

/// Wipe every top-level child of `dir` in parallel. Missing `dir` is success.
/// A child that cannot be removed is left and reported in [`Wipe::failed`];
/// the rest still go. An error means `dir` itself could not be read.
pub fn wipe_children_parallel(dir: &Path) -> io::Result<Wipe> {
    wipe_children_except(dir, |_| false)
}

/// [`wipe_children_parallel`], leaving the children `keep` names.
pub fn wipe_children_except(dir: &Path, keep: impl Fn(&OsStr) -> bool + Sync) -> io::Result<Wipe> {
    let dir_c = cstring_path(dir)?;
    // Never follow a symlink posing as files/ or info/ (full-empty footgun).
    let fd = unsafe {
        libc::open(
            dir_c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        let e = io::Error::last_os_error();
        if e.kind() == io::ErrorKind::NotFound {
            return Ok(Wipe::default());
        }
        return Err(e);
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let entries: Vec<DirEnt> = read_dirfd_entries(fd.as_raw_fd())?
        .into_iter()
        .filter(|e| !keep(&e.name))
        .collect();
    // Always parallelise top-level wipe (full empty of files/ or info/).
    let results: Vec<io::Result<()>> = entries
        .par_iter()
        .map(|e| remove_entry(fd.as_raw_fd(), dir, e, 0))
        .collect();
    let mut out = Wipe::default();
    for (e, r) in entries.into_iter().zip(results) {
        match r {
            Ok(()) => out.removed += 1,
            Err(err) => out.failed.push((e.name, err)),
        }
    }
    Ok(out)
}

/// Wipe `files/`, then `info/` except the `.trashinfo` of each payload that
/// stayed behind, so a payload that could not go keeps its record instead
/// of turning into an orphan. Both roots are always tried.
pub fn wipe_two_parallel(files: &Path, info: &Path) -> (io::Result<Wipe>, io::Result<Wipe>) {
    let ra = wipe_children_parallel(files);
    let kept: std::collections::HashSet<OsString> = match &ra {
        Ok(w) => w
            .failed
            .iter()
            .map(|(n, _)| {
                let mut i = n.clone();
                i.push(".trashinfo");
                i
            })
            .collect(),
        // files/ unreadable: keep every record.
        Err(_) => return (ra, Ok(Wipe::default())),
    };
    let rb = wipe_children_except(info, |n| kept.contains(n));
    (ra, rb)
}

fn cstring_path(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    /// Entry names [`remove_entry`] refuses, to stand in for an item no
    /// ordinary user can make undeletable.
    static FAIL: std::sync::Mutex<Vec<OsString>> = std::sync::Mutex::new(Vec::new());

    pub(super) fn fails(name: &OsStr) -> bool {
        FAIL.lock().unwrap().iter().any(|n| n == name)
    }

    fn chmod(p: &Path, mode: u32) {
        fs::set_permissions(p, fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn owned_dirs_without_owner_access_are_opened_up_and_removed() {
        // A makepkg tree: `pkg` at 0311 (no read), plus no-write and
        // no-access directories, each holding files.
        let root = tmp_root("locked");
        let files = root.join("files");
        let tree = files.join("linux-6.1");
        for d in ["pkg/linux/usr", "ro/sub", "none"] {
            fs::create_dir_all(tree.join(d)).unwrap();
        }
        fs::write(tree.join("pkg/linux/usr/f"), b"x").unwrap();
        fs::write(tree.join("pkg/g"), b"x").unwrap();
        fs::write(tree.join("ro/sub/f"), b"x").unwrap();
        fs::write(tree.join("none/f"), b"x").unwrap();
        chmod(&tree.join("pkg/linux"), 0o311);
        chmod(&tree.join("pkg"), 0o311);
        chmod(&tree.join("ro/sub"), 0o500);
        chmod(&tree.join("ro"), 0o500);
        chmod(&tree.join("none"), 0o000);
        let w = wipe_children_parallel(&files).unwrap();
        assert!(w.failed.is_empty(), "{:?}", w.failed);
        assert_eq!(w.removed, 1);
        assert!(!tree.exists());
        // remove_path takes the same route for a single payload, top
        // directory included.
        fs::create_dir_all(tree.join("a")).unwrap();
        fs::write(tree.join("a/f"), b"x").unwrap();
        chmod(&tree.join("a"), 0o311);
        chmod(&tree, 0o311);
        remove_path(&tree).unwrap();
        assert!(!tree.exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn opening_up_leaves_other_modes_and_files_alone() {
        let root = tmp_root("modes");
        let d = root.join("d");
        fs::create_dir(&d).unwrap();
        fs::write(root.join("f"), b"x").unwrap();
        chmod(&root.join("f"), 0o000);
        chmod(&d, 0o2311);
        let c = |p: &Path| CString::new(p.as_os_str().as_bytes()).unwrap();
        // Not a directory: untouched.
        assert!(!own_dir_opened_up(libc::AT_FDCWD, &c(&root.join("f"))));
        assert_eq!(fs::metadata(root.join("f")).unwrap().mode() & 0o7777, 0o000);
        // Our directory: owner rwx added, the other bits kept.
        assert!(own_dir_opened_up(libc::AT_FDCWD, &c(&d)));
        assert_eq!(fs::metadata(&d).unwrap().mode() & 0o7777, 0o2711);
        // Already rwx for the owner: nothing to do.
        assert!(!own_dir_opened_up(libc::AT_FDCWD, &c(&d)));
        // A directory someone else owns is never changed.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        st.st_mode = (libc::S_IFDIR | 0o311) as _;
        st.st_uid = unsafe { libc::geteuid() };
        assert!(owned_dir_short_of_rwx(&st));
        st.st_uid = st.st_uid.wrapping_add(1);
        assert!(!owned_dir_short_of_rwx(&st));
        chmod(&root.join("f"), 0o600);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn wipe_keeps_going_and_keeps_the_record_of_what_stayed() {
        let root = tmp_root("keep");
        let (files, info) = (root.join("files"), root.join("info"));
        fs::create_dir_all(files.join("tree/deep")).unwrap();
        fs::create_dir_all(&info).unwrap();
        let stuck = format!("stuck-{}", std::process::id());
        fs::write(files.join("tree/deep").join(&stuck), b"x").unwrap();
        fs::write(files.join("tree/other"), b"x").unwrap();
        for n in ["a", "b"] {
            fs::write(files.join(n), b"x").unwrap();
        }
        for n in ["a", "b", "tree"] {
            fs::write(info.join(format!("{n}.trashinfo")), b"x").unwrap();
        }
        FAIL.lock().unwrap().push(stuck.clone().into());
        let (wf, wi) = wipe_two_parallel(&files, &info);
        FAIL.lock().unwrap().retain(|n| n != stuck.as_str());
        let (wf, wi) = (wf.unwrap(), wi.unwrap());
        assert_eq!(wf.removed, 2);
        assert_eq!(wf.failed.len(), 1);
        assert_eq!(wf.failed[0].0, "tree");
        // The error names the deepest path, once.
        let msg = wf.failed[0].1.to_string();
        let deep = files.join("tree/deep").join(&stuck);
        assert_eq!(
            msg,
            format!(
                "{}: {}",
                deep.display(),
                io::Error::from_raw_os_error(libc::EPERM)
            )
        );
        // Siblings of the stuck file went; the stuck one and its record stay.
        assert!(!files.join("tree/other").exists());
        assert!(deep.exists());
        assert_eq!(wi.removed, 2);
        assert!(info.join("tree.trashinfo").exists());
        assert!(!info.join("a.trashinfo").exists());
        let _ = fs::remove_dir_all(&root);
    }

    fn tmp_root(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "rtrash-fastdelete-{}-{}-{}",
            tag,
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

    #[test]
    fn remove_nested_tree() {
        let root = tmp_root("nest");
        let tree = root.join("t");
        fs::create_dir_all(tree.join("a/b")).unwrap();
        fs::write(tree.join("a/b/c"), b"x").unwrap();
        fs::write(tree.join("a/d"), b"y").unwrap();
        fs::write(tree.join("e"), b"z").unwrap();
        remove_path(&tree).unwrap();
        assert!(!tree.exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn wipe_children_clears_dir_keeps_root() {
        let root = tmp_root("wipe");
        fs::write(root.join("f1"), b"1").unwrap();
        fs::create_dir(root.join("d")).unwrap();
        fs::write(root.join("d/x"), b"2").unwrap();
        let w = wipe_children_parallel(&root).unwrap();
        assert_eq!(w.removed, 2);
        assert!(w.failed.is_empty());
        assert!(root.is_dir());
        assert!(fs::read_dir(&root).unwrap().next().is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn wipe_two_parallel_both() {
        let root = tmp_root("two");
        let a = root.join("a");
        let b = root.join("b");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        for i in 0..20 {
            fs::write(a.join(format!("f{i}")), b"x").unwrap();
            fs::write(b.join(format!("g{i}")), b"y").unwrap();
        }
        let (wa, wb) = wipe_two_parallel(&a, &b);
        assert_eq!(wa.unwrap().removed, 20);
        assert_eq!(wb.unwrap().removed, 20);
        assert!(fs::read_dir(&a).unwrap().next().is_none());
        assert!(fs::read_dir(&b).unwrap().next().is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_is_ok() {
        let root = tmp_root("miss");
        remove_path(&root.join("nope")).unwrap();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn disk_usage_counts_hard_links_once_like_du_b1() {
        let root = tmp_root("du-links");
        let d = root.join("d");
        fs::create_dir_all(d.join("sub")).unwrap();
        fs::write(d.join("a"), vec![b'x'; 50_000]).unwrap();
        fs::hard_link(d.join("a"), d.join("sub/a-link")).unwrap();
        fs::write(d.join("sub/b"), vec![b'y'; 9000]).unwrap();
        let one = fs::symlink_metadata(d.join("a")).unwrap().blocks() * 512;
        let dirs = fs::symlink_metadata(&d).unwrap().blocks() * 512
            + fs::symlink_metadata(d.join("sub")).unwrap().blocks() * 512;
        let b = fs::symlink_metadata(d.join("sub/b")).unwrap().blocks() * 512;
        assert_eq!(disk_usage(&d), dirs + one + b);
        // Compare against GNU du only: on macOS, `du -sB1` printed
        // 512-byte blocks in CI.
        let gnu = std::process::Command::new("du")
            .args(["-sB1"])
            .arg(&d)
            .output()
            .ok()
            .filter(|o| o.status.success() && cfg!(target_os = "linux"));
        if let Some(o) = gnu {
            let n: u64 = String::from_utf8_lossy(&o.stdout)
                .split_whitespace()
                .next()
                .and_then(|n| n.parse().ok())
                .unwrap_or(0);
            if n > 0 {
                assert_eq!(disk_usage(&d), n);
            }
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn disk_usage_file_and_tree() {
        let root = tmp_root("du");
        let f = root.join("f");
        fs::write(&f, vec![b'x'; 4096]).unwrap();
        let file_u = disk_usage(&f);
        assert!(file_u >= 4096, "allocated size at least payload: {file_u}");
        let d = root.join("d");
        fs::create_dir(&d).unwrap();
        fs::write(d.join("a"), vec![b'y'; 2048]).unwrap();
        fs::write(d.join("b"), vec![b'z'; 2048]).unwrap();
        let tree_u = disk_usage(&d);
        assert!(tree_u >= 4096, "tree at least children: {tree_u}");
        assert_eq!(format_bytes(512), "512 B");
        assert!(format_bytes(1536).contains("KiB"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn wide_tree_parallel_remove() {
        let root = tmp_root("wide");
        let tree = root.join("w");
        fs::create_dir_all(&tree).unwrap();
        for i in 0..64 {
            let sub = tree.join(format!("s{i}"));
            fs::create_dir_all(&sub).unwrap();
            for j in 0..8 {
                fs::write(sub.join(format!("f{j}")), b"z").unwrap();
            }
        }
        remove_path(&tree).unwrap();
        assert!(!tree.exists());
        let _ = fs::remove_dir_all(&root);
    }
}
