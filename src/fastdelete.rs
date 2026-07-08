//! Fast tree deletion for trash payloads.
//!
//! In-process readdir + `unlinkat` (rsync empty-source style, no shell-out).
//! Hot path optimisations for full trash empty:
//! - use `d_type` to unlink regular files without a failed `openat` probe
//! - parallel sibling deletion (rayon) at each directory level with many names
//! - btrfs subvolume destroy when a directory is a real subvolume root (ino 256)

use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use rayon::prelude::*;

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
    let fd = unsafe {
        libc::open(
            dir_c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        let e = io::Error::last_os_error();
        if e.kind() == io::ErrorKind::NotFound {
            return Ok(());
        }
        if e.raw_os_error() == Some(libc::ELOOP) || e.raw_os_error() == Some(libc::ENOTDIR) {
            return fs::remove_file(path);
        }
        return Err(e);
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    remove_dirfd_contents(fd.as_raw_fd(), 0)?;
    drop(fd);
    match fs::remove_dir(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn remove_dirfd_contents(dirfd: RawFd, depth: u32) -> io::Result<()> {
    let entries = read_dirfd_entries(dirfd)?;
    // Nested levels stay sequential: parallelising every directory oversubscribes
    // rayon when the top-level full-empty wipe is already highly parallel.
    // (Top-level `wipe_children_parallel` is the parallel boundary.)
    let _ = (depth, PAR_SIBLING_THRESHOLD, PAR_MAX_DEPTH);
    for e in &entries {
        remove_entry(dirfd, e, depth)?;
    }
    Ok(())
}

fn remove_entry(dirfd: RawFd, ent: &DirEnt, depth: u32) -> io::Result<()> {
    let name_c = CString::new(ent.name.as_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;

    // Fast path: known non-directories — single unlinkat, no openat probe.
    match ent.dtype {
        DT_REG | DT_LNK | DT_FIFO | DT_SOCK | DT_CHR | DT_BLK => {
            return unlinkat_file(dirfd, &name_c);
        }
        DT_DIR => return remove_dir_child(dirfd, &ent.name, &name_c, depth),
        DT_UNKNOWN => {}
        _ => {}
    }

    // DT_UNKNOWN or exotic: probe with openat as directory.
    let child_fd = unsafe {
        libc::openat(
            dirfd,
            name_c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        )
    };
    if child_fd >= 0 {
        // We already opened; finish as directory (subvol check on parent).
        unsafe { libc::close(child_fd) };
        return remove_dir_child(dirfd, &ent.name, &name_c, depth);
    }
    let err = io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::ENOTDIR) | Some(libc::ELOOP) | Some(libc::EACCES) => {
            unlinkat_file(dirfd, &name_c)
        }
        Some(libc::ENOENT) => Ok(()),
        _ => Err(err),
    }
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
    name: &OsStr,
    name_c: &CString,
    depth: u32,
) -> io::Result<()> {
    // Subvolume destroy only at the first level of a tree (cheap for the common
    // non-subvolume case: one failed/skipped ioctl path per top-level payload,
    // not per nested directory).
    if depth == 0 && try_btrfs_subvol_at(dirfd, name)? {
        return Ok(());
    }
    let child_fd = unsafe {
        libc::openat(
            dirfd,
            name_c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        )
    };
    if child_fd < 0 {
        let e = io::Error::last_os_error();
        if e.kind() == io::ErrorKind::NotFound {
            return Ok(());
        }
        // Race: became a non-dir.
        if e.raw_os_error() == Some(libc::ENOTDIR) || e.raw_os_error() == Some(libc::ELOOP) {
            return unlinkat_file(dirfd, name_c);
        }
        return Err(e);
    }
    let child_fd = unsafe { OwnedFd::from_raw_fd(child_fd) };
    remove_dirfd_contents(child_fd.as_raw_fd(), depth.saturating_add(1))?;
    drop(child_fd);
    let rc = unsafe { libc::unlinkat(dirfd, name_c.as_ptr(), libc::AT_REMOVEDIR) };
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
    let rc =
        unsafe { libc::fstatat(parent_fd, name_c.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) };
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

/// Allocated disk usage of `path` in bytes (`st_blocks * 512`), like a fast
/// in-process `du -sB1` / `du --apparent-size` hybrid: we use block allocation
/// so sparse files report reclaimable space, not logical length.
///
/// Symlinks count only the link inode (not the target). Missing paths are 0.
pub fn disk_usage(path: &Path) -> u64 {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(_) => return 0,
    };
    let own = meta.blocks().saturating_mul(512);
    if !meta.is_dir() {
        return own;
    }
    // Directory tree: sum children (parallel at this level when wide).
    let Ok(rd) = fs::read_dir(path) else {
        return own;
    };
    let children: Vec<_> = rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    let child_sum: u64 = if children.len() >= 4 {
        children.par_iter().map(|p| disk_usage(p)).sum()
    } else {
        children.iter().map(|p| disk_usage(p)).sum()
    };
    own.saturating_add(child_sum)
}

/// Human-readable binary units (KiB/MiB/GiB) for CLI summaries.
pub fn format_bytes(n: u64) -> String {
    const K: f64 = 1024.0;
    if n < 1024 {
        format!("{n} B")
    } else if (n as f64) < K * K {
        format!("{:.1} KiB", n as f64 / K)
    } else if (n as f64) < K * K * K {
        format!("{:.1} MiB", n as f64 / (K * K))
    } else {
        format!("{:.2} GiB", n as f64 / (K * K * K))
    }
}

/// Wipe every top-level child of `dir` in parallel. Missing `dir` is success.
/// Returns the number of top-level children removed.
pub fn wipe_children_parallel(dir: &Path) -> io::Result<u64> {
    let dir_c = match cstring_path(dir) {
        Ok(c) => c,
        Err(e) => return Err(e),
    };
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
            return Ok(0);
        }
        return Err(e);
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let entries = read_dirfd_entries(fd.as_raw_fd())?;
    let n = entries.len() as u64;
    if n == 0 {
        return Ok(0);
    }
    // Always parallelise top-level wipe (full empty of files/ or info/).
    let err_slot: AtomicU64 = AtomicU64::new(0);
    // Store first error message is hard without mutex; collect errors.
    let mut first_err: Option<io::Error> = None;
    let results: Vec<io::Result<()>> = entries
        .par_iter()
        .map(|e| remove_entry(fd.as_raw_fd(), e, 0))
        .collect();
    for r in results {
        if let Err(e) = r {
            if first_err.is_none() {
                first_err = Some(e);
            }
            err_slot.fetch_add(1, Ordering::Relaxed);
        }
    }
    if let Some(e) = first_err {
        return Err(e);
    }
    Ok(n)
}

/// Wipe `files/` then `info/` (sequential roots, parallel children each).
/// Always attempts **both** roots even if the first returns an error, so a
/// partial failure does not leave one side fully intact and the other half-gone
/// without trying to finish the pair.
pub fn wipe_two_parallel(a: &Path, b: &Path) -> io::Result<(u64, u64)> {
    let ra = wipe_children_parallel(a);
    let rb = wipe_children_parallel(b);
    match (ra, rb) {
        (Ok(na), Ok(nb)) => Ok((na, nb)),
        (Err(e), Ok(nb)) => {
            let _ = nb;
            Err(e)
        }
        (Ok(na), Err(e)) => {
            let _ = na;
            Err(e)
        }
        (Err(e), Err(_)) => Err(e),
    }
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
        let (na, nb) = wipe_two_parallel(&a, &b).unwrap();
        assert_eq!(na, 20);
        assert_eq!(nb, 20);
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
