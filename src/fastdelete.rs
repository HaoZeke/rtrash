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

/// Owned directory fds one walk keeps open. Deeper frames close their
/// shallowest ancestors and reopen them through `..` on the way back up.
const FD_BUDGET: usize = 16;

/// One directory on the walk's explicit stack.
struct Frame {
    /// Open fd, or `None` for the root (borrowed) and for a frame closed
    /// to stay within [`FD_BUDGET`].
    fd: Option<OwnedFd>,
    /// Name inside the parent frame (empty for the root).
    name: OsString,
    /// Device and inode, recorded when the fd is closed, so a reopen
    /// through `..` can prove it found the same directory.
    id: (u64, u64),
    /// Entries still to remove, last first.
    entries: Vec<DirEnt>,
    depth: u32,
    opened_up: bool,
    first: Option<io::Error>,
}

/// `root` joined with the names of `stack[1..=upto]`. Built only for
/// error messages, so path memory stays linear in depth.
fn frame_path(root: &Path, stack: &[Frame], upto: usize) -> std::path::PathBuf {
    let mut p = root.to_path_buf();
    for f in &stack[1..=upto] {
        p.push(&f.name);
    }
    p
}

/// Remove everything inside `dirfd` without recursion: the walk keeps
/// its own stack, holds at most [`FD_BUDGET`] fds of its own, and
/// handles trees of any depth.
fn remove_dirfd_contents(dirfd: RawFd, path: &Path, depth: u32) -> io::Result<()> {
    // Nested levels stay sequential: parallelising every directory oversubscribes
    // rayon when the top-level full-empty wipe is already highly parallel.
    // (Top-level `wipe_children_parallel` is the parallel boundary.)
    let _ = (PAR_SIBLING_THRESHOLD, PAR_MAX_DEPTH);
    let mut entries = read_dirfd_entries(dirfd).map_err(|e| at(path, e))?;
    entries.reverse();
    remove_listed(dirfd, path, depth, entries)
}

/// [`remove_dirfd_contents`] for a directory already listed, with
/// `entries` in reverse order.
fn remove_listed(dirfd: RawFd, path: &Path, depth: u32, entries: Vec<DirEnt>) -> io::Result<()> {
    let mut stack = vec![Frame {
        fd: None,
        name: OsString::new(),
        id: (0, 0),
        entries,
        depth,
        opened_up: false,
        first: None,
    }];
    // Frames 1..lo have closed fds; frames lo.. are open.
    let mut lo = 1;
    let fd_at = |stack: &[Frame], i: usize| -> RawFd {
        if i == 0 {
            dirfd
        } else {
            stack[i].fd.as_ref().map_or(-1, |f| f.as_raw_fd())
        }
    };
    loop {
        let top = stack.len() - 1;
        let fd = fd_at(&stack, top);
        if let Some(ent) = stack[top].entries.pop() {
            let d = stack[top].depth;
            let opened = stack[top].opened_up;
            let (r, opened_now) = {
                let child = || frame_path(path, &stack, top).join(&ent.name);
                let mut r = remove_or_open(fd, &ent, d, &child);
                // Unlinking needs write and search on this directory: when
                // the user owns it, grant them once and retry.
                let retry = !opened && r.as_ref().is_err_and(|e| os_code(e) == Some(libc::EACCES));
                if retry && own_dirfd_opened_up(fd) {
                    r = remove_or_open(fd, &ent, d, &child);
                }
                (r, retry)
            };
            if opened_now {
                stack[top].opened_up = true;
            }
            match r {
                Ok(None) => {}
                Ok(Some((cfd, entries))) => {
                    stack.push(Frame {
                        fd: Some(cfd),
                        name: ent.name,
                        id: (0, 0),
                        entries,
                        depth: d.saturating_add(1),
                        opened_up: false,
                        first: None,
                    });
                    if stack.len() - lo > FD_BUDGET {
                        let f = &mut stack[lo];
                        if let Some(old) = f.fd.take() {
                            f.id = fd_id(old.as_raw_fd()).map_err(|e| at(path, e))?;
                        }
                        lo += 1;
                    }
                }
                Err(e) => {
                    stack[top].first.get_or_insert(e);
                }
            }
            continue;
        }
        // This directory is empty now (or kept something): leave it.
        if top == 0 {
            return stack.pop().and_then(|f| f.first).map_or(Ok(()), Err);
        }
        let done = stack.pop().expect("non-root frame");
        let p = top - 1;
        if p >= 1 && stack[p].fd.is_none() {
            let cfd = done.fd.as_ref().map_or(-1, |f| f.as_raw_fd());
            let back =
                reopen_parent(cfd, stack[p].id).map_err(|e| at(&frame_path(path, &stack, p), e))?;
            stack[p].fd = Some(back);
            lo = p;
        }
        lo = lo.min(stack.len());
        drop(done.fd);
        if let Some(e) = done.first {
            stack[p].first.get_or_insert(e);
            continue;
        }
        let pfd = fd_at(&stack, p);
        let name_c = CString::new(done.name.as_bytes())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        let mut r = rmdir_at(pfd, &name_c);
        if !stack[p].opened_up
            && r.as_ref()
                .is_err_and(|e| e.raw_os_error() == Some(libc::EACCES))
        {
            stack[p].opened_up = true;
            if own_dirfd_opened_up(pfd) {
                r = rmdir_at(pfd, &name_c);
            }
        }
        if let Err(e) = r {
            let e = at(&frame_path(path, &stack, p).join(&done.name), e);
            stack[p].first.get_or_insert(e);
        }
    }
}

/// Device and inode of an open fd.
fn fd_id(fd: RawFd) -> io::Result<(u64, u64)> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((st.st_dev as u64, st.st_ino as u64))
}

/// Open the parent of `child_fd` through `..` and check it is the
/// directory recorded as `id`, so a tree moved during the walk is never
/// mistaken for its old parent.
fn reopen_parent(child_fd: RawFd, id: (u64, u64)) -> io::Result<OwnedFd> {
    let fd = unsafe {
        libc::openat(
            child_fd,
            c"..".as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    if fd_id(fd.as_raw_fd())? != id {
        return Err(io::Error::other(
            "the directory moved while it was being removed",
        ));
    }
    Ok(fd)
}

fn rmdir_at(dirfd: RawFd, name_c: &CStr) -> io::Result<()> {
    let rc = unsafe { libc::unlinkat(dirfd, name_c.as_ptr(), libc::AT_REMOVEDIR) };
    if rc != 0 {
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::NotFound {
            return Err(e);
        }
    }
    Ok(())
}

/// Remove `ent` from `dirfd` when it is not a directory. A directory is
/// opened and listed instead, for the caller to empty and then remove.
fn remove_or_open(
    dirfd: RawFd,
    ent: &DirEnt,
    depth: u32,
    child: &dyn Fn() -> std::path::PathBuf,
) -> io::Result<Option<(OwnedFd, Vec<DirEnt>)>> {
    let name_c = CString::new(ent.name.as_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    #[cfg(test)]
    if tests::fails(&ent.name) {
        return Err(at(&child(), io::Error::from_raw_os_error(libc::EPERM)));
    }
    if !is_dir_entry(dirfd, ent, &name_c).map_err(|e| at(&child(), e))? {
        return unlinkat_file(dirfd, &name_c)
            .map(|_| None)
            .map_err(|e| at(&child(), e));
    }
    let Some(fd) = open_child_dir(dirfd, &ent.name, &name_c, depth, child)? else {
        return Ok(None);
    };
    let mut entries = read_dirfd_entries(fd.as_raw_fd()).map_err(|e| at(&child(), e))?;
    entries.reverse();
    Ok(Some((fd, entries)))
}

/// Whether `ent` is a directory, from `d_type` or, when that is unknown,
/// from an lstat. A name already gone counts as a non-directory, whose
/// unlink then succeeds.
fn is_dir_entry(dirfd: RawFd, ent: &DirEnt, name_c: &CStr) -> io::Result<bool> {
    match ent.dtype {
        DT_REG | DT_LNK | DT_FIFO | DT_SOCK | DT_CHR | DT_BLK => return Ok(false),
        DT_DIR => return Ok(true),
        DT_UNKNOWN => {}
        _ => {}
    }
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::fstatat(dirfd, name_c.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) };
    if rc != 0 {
        let err = io::Error::last_os_error();
        return match err.raw_os_error() {
            Some(libc::ENOENT) => Ok(false),
            _ => Err(err),
        };
    }
    Ok((st.st_mode as libc::mode_t & libc::S_IFMT) == libc::S_IFDIR)
}

/// Open the directory `name` under `dirfd`. `None` means nothing is
/// left to do: it was a btrfs subvolume and is gone, it vanished, or it
/// turned into a non-directory and was unlinked.
fn open_child_dir(
    dirfd: RawFd,
    name: &OsStr,
    name_c: &CStr,
    depth: u32,
    child: &dyn Fn() -> std::path::PathBuf,
) -> io::Result<Option<OwnedFd>> {
    // Subvolume destroy only at the first level of a tree (cheap for the common
    // non-subvolume case: one failed/skipped ioctl path per top-level payload,
    // not per nested directory).
    if depth == 0 && try_btrfs_subvol_at(dirfd, name).map_err(|e| at(&child(), e))? {
        return Ok(None);
    }
    let open = || unsafe {
        libc::openat(
            dirfd,
            name_c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
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
            return Ok(None);
        }
        // Race: became a non-dir.
        if e.raw_os_error() == Some(libc::ENOTDIR) || e.raw_os_error() == Some(libc::ELOOP) {
            return unlinkat_file(dirfd, &CString::from(name_c))
                .map(|_| None)
                .map_err(|e| at(&child(), e));
        }
        return Err(at(&child(), e));
    }
    Ok(Some(unsafe { OwnedFd::from_raw_fd(child_fd) }))
}

/// Remove one top-level entry of `dirfd` (a file, or a whole tree).
fn remove_entry(dirfd: RawFd, path: &Path, ent: &DirEnt, depth: u32) -> io::Result<()> {
    let child = || path.join(&ent.name);
    let Some((fd, entries)) = remove_or_open(dirfd, ent, depth, &child)? else {
        return Ok(());
    };
    let name_c = CString::new(ent.name.as_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    remove_listed(fd.as_raw_fd(), &child(), depth.saturating_add(1), entries)?;
    drop(fd);
    rmdir_at(dirfd, &name_c).map_err(|e| at(&child(), e))
}

fn unlinkat_file(dirfd: RawFd, name_c: &CStr) -> io::Result<()> {
    let rc = unsafe { libc::unlinkat(dirfd, name_c.as_ptr(), 0) };
    if rc != 0 {
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::NotFound {
            return Err(e);
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
    usage_in(path, &seen, 0)
}

/// One directory of [`deep_usage`]'s walk.
struct UsageFrame {
    fd: Option<OwnedFd>,
    /// Device and inode, recorded when `fd` is closed.
    id: (u64, u64),
    entries: Vec<DirEnt>,
}

/// Bytes below the directory at `path` (its own blocks not included),
/// walked by descriptor with an explicit stack: any depth, at most
/// [`FD_BUDGET`] open directories, each reopened through `..` only after
/// checking it is the same one. What cannot be read counts as nothing.
fn deep_usage(path: &Path, seen: &Seen) -> u64 {
    let Ok(c) = cstring_path(path) else {
        return 0;
    };
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    let fd = unsafe { libc::open(c.as_ptr(), flags) };
    if fd < 0 {
        return 0;
    }
    let root = unsafe { OwnedFd::from_raw_fd(fd) };
    let Ok(entries) = read_dirfd_entries(root.as_raw_fd()) else {
        return 0;
    };
    let mut stack = vec![UsageFrame {
        fd: Some(root),
        id: (0, 0),
        entries,
    }];
    // Frames below `lo` have their descriptors closed; the root never.
    let mut lo = 1;
    let mut total = 0u64;
    while let Some(top) = stack.len().checked_sub(1) {
        let Some(ent) = stack[top].entries.pop() else {
            let done = stack.pop().expect("open frame");
            let Some(p) = top.checked_sub(1) else {
                break;
            };
            if stack[p].fd.is_none() {
                let cfd = done.fd.as_ref().map_or(-1, |f| f.as_raw_fd());
                match reopen_parent(cfd, stack[p].id) {
                    Ok(fd) => stack[p].fd = Some(fd),
                    // The tree moved: count no further.
                    Err(_) => break,
                }
                lo = p;
            }
            lo = lo.min(stack.len());
            continue;
        };
        let dirfd = stack[top].fd.as_ref().map_or(-1, |f| f.as_raw_fd());
        let Ok(name_c) = CString::new(ent.name.as_bytes()) else {
            continue;
        };
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstatat(dirfd, name_c.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) } != 0
        {
            continue;
        }
        let own = (st.st_blocks as u64).saturating_mul(512);
        if (st.st_mode as libc::mode_t & libc::S_IFMT) != libc::S_IFDIR {
            if !counted_before(seen, st.st_nlink as u64, st.st_dev as u64, st.st_ino as u64) {
                total = total.saturating_add(own);
            }
            continue;
        }
        total = total.saturating_add(own);
        let fd = unsafe { libc::openat(dirfd, name_c.as_ptr(), flags) };
        if fd < 0 {
            continue;
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let Ok(entries) = read_dirfd_entries(fd.as_raw_fd()) else {
            continue;
        };
        stack.push(UsageFrame {
            fd: Some(fd),
            id: (0, 0),
            entries,
        });
        if stack.len() - lo > FD_BUDGET {
            let f = &mut stack[lo];
            if let Some(old) = f.fd.take() {
                match fd_id(old.as_raw_fd()) {
                    Ok(id) => f.id = id,
                    Err(_) => break,
                }
            }
            lo += 1;
        }
    }
    total
}

type Seen = std::sync::Mutex<std::collections::HashSet<(u64, u64)>>;

/// Levels [`usage_in`] recurses through by path before handing a subtree
/// to [`deep_usage`].
const USAGE_PATH_DEPTH: usize = 256;

/// Whether a hard-linked file was counted already; records it if not.
fn counted_before(seen: &Seen, nlink: u64, dev: u64, ino: u64) -> bool {
    nlink > 1
        && !seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert((dev, ino))
}

fn usage_in(path: &Path, seen: &Seen, depth: usize) -> u64 {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(_) => return 0,
    };
    let own = meta.blocks().saturating_mul(512);
    if !meta.is_dir() {
        let repeat = counted_before(seen, meta.nlink(), meta.dev(), meta.ino());
        return if repeat { 0 } else { own };
    }
    // Near PATH_MAX, paths stop opening, and deep trees would grow the
    // stack: walk the rest by descriptor, without recursion.
    let limit = (libc::PATH_MAX as usize).saturating_sub(256);
    if depth >= USAGE_PATH_DEPTH || path.as_os_str().len() >= limit {
        return own.saturating_add(deep_usage(path, seen));
    }
    // Directory tree: sum children (parallel at this level when wide).
    let Ok(rd) = fs::read_dir(path) else {
        return own;
    };
    let children: Vec<_> = rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    let child_sum: u64 = if children.len() >= 4 {
        children
            .par_iter()
            .map(|p| usage_in(p, seen, depth + 1))
            .sum()
    } else {
        children.iter().map(|p| usage_in(p, seen, depth + 1)).sum()
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

    /// Levels in the deep-tree tests. On macOS each step down a chain
    /// costs more the deeper it lies: in CI, 4099 levels take over a
    /// minute there and 20000 levels ran past 25 minutes. 1000 levels
    /// still pass its 1024-byte `PATH_MAX` and the fd budget. Linux
    /// keeps the depth that overflowed the stack.
    const DEEP: usize = if cfg!(target_os = "macos") {
        1000
    } else {
        20_000
    };
    const _: () = assert!(DEEP * 2 > libc::PATH_MAX as usize && DEEP > FD_BUDGET * 4);

    /// A chain of `levels` directories named `d` under `root`, with a
    /// file at every level in `files` and the level `locked` left at
    /// mode 0500 once the chain is built. Built with `mkdirat`, since its paths pass `PATH_MAX`.
    fn deep_chain(root: &Path, levels: usize, files: &[usize], locked: usize) {
        let c = CString::new(root.as_os_str().as_bytes()).unwrap();
        // SAFETY: plain descriptor calls on NUL-terminated names; each
        // descriptor is closed once the next one is open.
        unsafe {
            let mut fd = libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY);
            assert!(fd >= 0);
            let mut lock = -1;
            for i in 0..levels {
                assert_eq!(libc::mkdirat(fd, c"d".as_ptr(), 0o755), 0);
                let next = libc::openat(fd, c"d".as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY);
                assert!(next >= 0);
                libc::close(fd);
                fd = next;
                if files.contains(&i) {
                    let f = libc::openat(fd, c"f".as_ptr(), libc::O_WRONLY | libc::O_CREAT, 0o644);
                    assert!(f >= 0);
                    libc::close(f);
                }
                if i == locked {
                    lock = libc::dup(fd);
                }
            }
            libc::close(fd);
            if lock >= 0 {
                assert_eq!(libc::fchmod(lock, 0o500), 0);
                libc::close(lock);
            }
        }
    }

    #[test]
    fn deep_trees_go_without_recursion_or_an_fd_per_level() {
        // Paths pass PATH_MAX. On Linux, 20000 levels with a frame per
        // level would overflow a 2 MiB rayon stack and need 20000 fds.
        let root = tmp_root("deep");
        let one = root.join("one");
        fs::create_dir(&one).unwrap();
        let locked = DEEP * 3 / 4;
        deep_chain(&one, DEEP, &[0, 99, locked, DEEP - 1], locked);
        remove_path(&one).unwrap();
        assert!(!one.exists());
        // The parallel wipe runs the same walk on rayon worker threads.
        let files = root.join("files");
        fs::create_dir(&files).unwrap();
        for n in ["a", "b"] {
            fs::create_dir(files.join(n)).unwrap();
            deep_chain(&files.join(n), DEEP, &[DEEP - 1], usize::MAX);
        }
        let w = wipe_children_parallel(&files).unwrap();
        assert!(w.failed.is_empty(), "{:?}", w.failed);
        assert_eq!(w.removed, 2);
        assert!(fs::read_dir(&files).unwrap().next().is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn disk_usage_counts_trees_past_path_max() {
        // Files near the top and at the bottom: paths pass PATH_MAX,
        // where opening by path used to fail and count nothing.
        let root = tmp_root("deep-usage");
        let tree = root.join("t");
        fs::create_dir(&tree).unwrap();
        let levels = DEEP;
        deep_chain(&tree, levels, &[5, levels - 1], usize::MAX);
        // The expected sum, from one more walk down the chain.
        let c = CString::new(tree.as_os_str().as_bytes()).unwrap();
        let mut want = fs::symlink_metadata(&tree).unwrap().blocks() * 512;
        // SAFETY: plain descriptor calls on NUL-terminated names.
        unsafe {
            let mut fd = libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY);
            for i in 0..levels {
                let next = libc::openat(fd, c"d".as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY);
                libc::close(fd);
                fd = next;
                let mut st: libc::stat = std::mem::zeroed();
                assert_eq!(libc::fstat(fd, &mut st), 0);
                want += st.st_blocks as u64 * 512;
                if i == 5 || i == levels - 1 {
                    let f = libc::openat(fd, c"f".as_ptr(), libc::O_WRONLY);
                    assert_eq!(libc::write(f, [1u8; 9000].as_ptr().cast(), 9000), 9000);
                    assert_eq!(libc::fsync(f), 0);
                    assert_eq!(libc::fstat(f, &mut st), 0);
                    libc::close(f);
                    want += st.st_blocks as u64 * 512;
                }
            }
            libc::close(fd);
        }
        assert!(want > 0);
        assert_eq!(disk_usage(&tree), want);
        remove_path(&tree).unwrap();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_reopened_parent_must_be_the_same_directory() {
        let root = tmp_root("reopen");
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::create_dir(root.join("c")).unwrap();
        let open = |p: &Path| {
            let c = CString::new(p.as_os_str().as_bytes()).unwrap();
            let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY) };
            assert!(fd >= 0);
            unsafe { OwnedFd::from_raw_fd(fd) }
        };
        let a = fd_id(open(&root.join("a")).as_raw_fd()).unwrap();
        let b = open(&root.join("a/b"));
        assert_eq!(
            fd_id(reopen_parent(b.as_raw_fd(), a).unwrap().as_raw_fd()).unwrap(),
            a
        );
        // Moved under another parent: `..` now leads elsewhere.
        fs::rename(root.join("a/b"), root.join("c/b")).unwrap();
        let err = reopen_parent(b.as_raw_fd(), a).unwrap_err();
        assert!(err.to_string().contains("moved"), "{err}");
        let _ = fs::remove_dir_all(&root);
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
