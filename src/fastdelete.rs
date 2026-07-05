//! Fast tree deletion for trash payloads.
//!
//! Inspired by the empty-source `rsync --delete` pattern (destination-side
//! readdir + unlink of every name under a root), but fully in-process:
//! depth-first `openat`/`unlinkat`/`rmdir` without shelling out to rsync.
//!
//! On btrfs, if a path is a real subvolume root (inode 256), use
//! `BTRFS_IOC_SNAP_DESTROY` on the parent — much cheaper than walking the
//! tree. Ordinary directories on btrfs still use the generic bulk walk.

use std::ffi::{CStr, CString, OsStr};
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use rayon::prelude::*;

/// Linux btrfs super magic (`statfs.f_type`).
const BTRFS_SUPER_MAGIC: i64 = 0x9123683E;
/// First free object id — subvolume roots report this as their inode.
const BTRFS_FIRST_FREE_OBJECTID: u64 = 256;
const BTRFS_IOCTL_MAGIC: u8 = 0x94;
/// `_IOW(BTRFS_IOCTL_MAGIC, 15, struct btrfs_ioctl_vol_args)`
const BTRFS_IOC_SNAP_DESTROY: libc::c_ulong = {
    // _IOC(_IOC_WRITE, type, nr, size) — size of btrfs_ioctl_vol_args
    // struct is 8 + 4088 = 4096 on Linux.
    const SIZE: libc::c_ulong = 4096;
    (1u64 << 30) | ((SIZE) << 16) | ((BTRFS_IOCTL_MAGIC as u64) << 8) | 15
};

#[repr(C)]
struct BtrfsIoctlVolArgs {
    fd: i64,
    name: [u8; 4088],
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
    // sockets, devices, fifos: unlink like a non-dir
    fs::remove_file(path)
}

/// True when `path` lives on a btrfs filesystem.
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

/// If `path` is a btrfs subvolume root, destroy it via ioctl on the parent.
/// Returns `Ok(true)` when the ioctl removed it.
pub fn try_btrfs_subvolume_delete(path: &Path) -> io::Result<bool> {
    if !is_btrfs(path) {
        return Ok(false);
    }
    // metadata (not symlink_metadata): subvolume roots are real directories.
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

    let rc = unsafe {
        libc::ioctl(
            parent_fd.as_raw_fd(),
            BTRFS_IOC_SNAP_DESTROY,
            &args as *const _ as *mut libc::c_void,
        )
    };
    if rc == 0 {
        return Ok(true);
    }
    // Not a subvolume or ioctl unsupported — fall through to tree walk.
    let err = io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::ENOTTY) | Some(libc::EINVAL) | Some(libc::ENOENT) | Some(libc::ENOTDIR) => {
            Ok(false)
        }
        _ => {
            // Permission or busy: report failure rather than double-delete.
            if err.kind() == io::ErrorKind::PermissionDenied {
                Err(err)
            } else {
                Ok(false)
            }
        }
    }
}

/// Depth-first delete of a directory tree using openat/unlinkat (rsync-style
/// destination wipe: readdir each level, unlink leaves, then rmdir).
fn remove_dir_tree(path: &Path) -> io::Result<()> {
    let dir_c = cstring_path(path)?;
    let fd = unsafe { libc::open(dir_c.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW) };
    if fd < 0 {
        let e = io::Error::last_os_error();
        // Race: already gone.
        if e.kind() == io::ErrorKind::NotFound {
            return Ok(());
        }
        // O_NOFOLLOW on a symlink-to-dir: treat as unlink the symlink.
        if e.raw_os_error() == Some(libc::ELOOP) || e.raw_os_error() == Some(libc::ENOTDIR) {
            return fs::remove_file(path);
        }
        return Err(e);
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    remove_dirfd_contents(fd.as_raw_fd())?;
    drop(fd);
    match fs::remove_dir(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn remove_dirfd_contents(dirfd: RawFd) -> io::Result<()> {
    // Snapshot names first so we do not invalidate the stream while unlinking.
    let names = read_dirfd_names(dirfd)?;
    for name in names {
        remove_child(dirfd, &name)?;
    }
    Ok(())
}

fn remove_child(dirfd: RawFd, name: &OsStr) -> io::Result<()> {
    let name_c = CString::new(name.as_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;

    // Prefer directory open; on ENOTDIR/ELOOP unlink as non-dir.
    let child_fd = unsafe {
        libc::openat(
            dirfd,
            name_c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        )
    };
    if child_fd >= 0 {
        let child_fd = unsafe { OwnedFd::from_raw_fd(child_fd) };
        // Nested subvolume? try ioctl relative to parent dirfd.
        if try_btrfs_subvol_at(dirfd, name)? {
            return Ok(());
        }
        remove_dirfd_contents(child_fd.as_raw_fd())?;
        drop(child_fd);
        let rc = unsafe { libc::unlinkat(dirfd, name_c.as_ptr(), libc::AT_REMOVEDIR) };
        if rc != 0 {
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::NotFound {
                return Err(e);
            }
        }
        return Ok(());
    }

    let err = io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::ENOTDIR) | Some(libc::ELOOP) | Some(libc::EACCES) => {
            // file, symlink, or unreadable dir: try plain unlink
            let rc = unsafe { libc::unlinkat(dirfd, name_c.as_ptr(), 0) };
            if rc != 0 {
                let e = io::Error::last_os_error();
                if e.kind() != io::ErrorKind::NotFound {
                    return Err(e);
                }
            }
            Ok(())
        }
        Some(libc::ENOENT) => Ok(()),
        _ => Err(err),
    }
}

fn try_btrfs_subvol_at(parent_fd: RawFd, name: &OsStr) -> io::Result<bool> {
    // Cheap check: fstatat inode == 256 and is dir.
    let name_c = CString::new(name.as_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::fstatat(parent_fd, name_c.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) };
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
            BTRFS_IOC_SNAP_DESTROY,
            &args as *const _ as *mut libc::c_void,
        )
    };
    Ok(rc == 0)
}

fn read_dirfd_names(dirfd: RawFd) -> io::Result<Vec<std::ffi::OsString>> {
    // Dup so fdopendir can take ownership of a separate fd.
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
        // readdir is MT-safe per-DIR* on Linux; we hold exclusive ownership.
        let ent = unsafe { libc::readdir(dir) };
        if ent.is_null() {
            break;
        }
        let d_name = unsafe { CStr::from_ptr((*ent).d_name.as_ptr()) };
        let bytes = d_name.to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        names.push(std::ffi::OsString::from(OsStr::from_bytes(bytes)));
    }
    unsafe { libc::closedir(dir) };
    Ok(names)
}

/// Wipe every top-level child of `dir` in parallel (full-empty of `files/` or
/// `info/`). Missing `dir` is success. Recreates nothing — caller owns layout.
pub fn wipe_children_parallel(dir: &Path) -> io::Result<u64> {
    let rd = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    let children: Vec<_> = rd
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    let n = children.len() as u64;
    let errors: Vec<_> = children
        .par_iter()
        .filter_map(|p| remove_path(p).err())
        .collect();
    if let Some(e) = errors.into_iter().next() {
        return Err(e);
    }
    Ok(n)
}

fn cstring_path(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

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
        let n = wipe_children_parallel(&root).unwrap();
        assert_eq!(n, 2);
        assert!(root.is_dir());
        assert!(fs::read_dir(&root).unwrap().next().is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_is_ok() {
        let root = tmp_root("miss");
        remove_path(&root.join("nope")).unwrap();
        let _ = fs::remove_dir_all(&root);
    }
}
