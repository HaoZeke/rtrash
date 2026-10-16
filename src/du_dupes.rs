//! `du --dupes`: files with the same contents under the scanned paths.
//!
//! Candidates narrow in three passes, so most files are never read in
//! full: same size first, then the same first and last 4 KiB, then the
//! same 128-bit hash of the whole file (two SipHash streams, one salted).
//! Hard links to one inode count as one file, since removing a link frees
//! nothing while another remains. On file systems that share extents
//! (btrfs, XFS, APFS), a copy whose extents all belong to a reflink frees
//! nothing either; its group says how much would really come back.
//!
//! In each group the oldest file (by modification time, then the shortest
//! path) is the one to keep; the others are the extras that `--print0`
//! lists and `--trash-dupes` moves to the trash.

use std::collections::HashMap;
use std::fs;
use std::hash::Hasher;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use rayon::prelude::*;

use crate::du_fs;

/// Bytes read from each end of a file for the partial pass.
const EDGE: usize = 4096;

/// One regular file met by the scan.
#[derive(Debug, Clone)]
pub struct File {
    pub path: PathBuf,
    pub size: u64,
    pub disk: u64,
    pub dev: u64,
    pub ino: u64,
    pub mtime: i64,
    /// Other names of the same inode under the scanned paths: removing
    /// the file frees its space only once they go too.
    pub links: Vec<PathBuf>,
}

/// One set of files with the same contents.
#[derive(Debug, Clone)]
pub struct Group {
    pub size: u64,
    pub keep: File,
    pub extras: Vec<File>,
    /// Bytes that removing the extras would free: their disk use, less
    /// what reflinks share.
    pub reclaim: u64,
}

/// Options of a scan.
#[derive(Debug, Clone, Copy)]
pub struct Opts {
    pub min_size: u64,
    pub one_file_system: bool,
    /// Also walk proc, sysfs and the like below a root (`--include-kernfs`).
    pub include_kernfs: bool,
}

/// Trash directories hold what is already on its way out, so the walk
/// skips them: the home trash, by identity, and any directory named like
/// a volume trash or a Recycle Bin.
fn trash_dir(name: &std::ffi::OsStr, path: &Path, home: Option<(u64, u64)>) -> bool {
    let n = name.to_string_lossy();
    if n == ".Trash" || n.starts_with(".Trash-") || n.eq_ignore_ascii_case("$Recycle.Bin") {
        return true;
    }
    home.is_some_and(|h| du_fs::lstat(path).is_ok_and(|s| (s.dev, s.ino) == h && s.ino != 0))
}

fn home_trash_id() -> Option<(u64, u64)> {
    let t = crate::platform::home_trash_from(
        std::env::var_os("XDG_DATA_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    );
    du_fs::lstat(&t).ok().map(|s| (s.dev, s.ino))
}

/// Every regular file at or below `roots`, links not followed, outside
/// trash directories.
pub fn walk(roots: &[PathBuf], opts: Opts) -> Vec<File> {
    let home = home_trash_id();
    roots
        .par_iter()
        .flat_map(|r| {
            let Ok(st) = du_fs::lstat(r) else {
                return Vec::new();
            };
            if st.is_file() {
                return file_of(r, &st, opts).into_iter().collect();
            }
            if !st.is_dir() {
                return Vec::new();
            }
            walk_dir(r, st.dev, opts, home)
        })
        .collect()
}

fn file_of(path: &Path, st: &du_fs::Stat, opts: Opts) -> Option<File> {
    (st.is_file() && st.size >= opts.min_size.max(1)).then(|| File {
        path: path.to_path_buf(),
        size: st.size,
        disk: st.sizes().1,
        dev: st.dev,
        ino: st.ino,
        mtime: st.mtime,
        links: Vec::new(),
    })
}

fn walk_dir(dir: &Path, root_dev: u64, opts: Opts, home: Option<(u64, u64)>) -> Vec<File> {
    let Ok(rd) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files = Vec::new();
    let mut subdirs = Vec::new();
    for e in rd.flatten() {
        let Ok(t) = e.file_type() else { continue };
        let p = e.path();
        if t.is_dir() {
            if !trash_dir(&e.file_name(), &p, home) {
                subdirs.push(p);
            }
        } else if t.is_file() {
            if let Ok(st) = du_fs::lstat(&p) {
                files.extend(file_of(&p, &st, opts));
            }
        }
    }
    let deeper: Vec<File> = subdirs
        .par_iter()
        .flat_map(|d| {
            let other_fs = du_fs::lstat(d).is_ok_and(|s| s.dev != root_dev);
            if other_fs && opts.one_file_system {
                return Vec::new();
            }
            // As a scan does: proc, sysfs and the like stay out.
            let kernfs = || du_fs::Dir::open(d).is_ok_and(|x| x.is_kernfs());
            if other_fs && !opts.include_kernfs && kernfs() {
                return Vec::new();
            }
            walk_dir(d, root_dev, opts, home)
        })
        .collect();
    files.extend(deeper);
    files
}

/// One file per inode: the other names become its link count.
fn one_per_inode(files: Vec<File>) -> Vec<File> {
    let mut by: HashMap<(u64, u64), File> = HashMap::new();
    for f in files {
        // Windows may report no inode number; such a file stands alone.
        if f.ino == 0 {
            by.insert((u64::MAX, by.len() as u64), f);
            continue;
        }
        match by.get_mut(&(f.dev, f.ino)) {
            Some(k) => {
                if (f.path.as_os_str().len(), &f.path) < (k.path.as_os_str().len(), &k.path) {
                    let old = std::mem::replace(&mut k.path, f.path);
                    k.links.push(old);
                } else {
                    k.links.push(f.path);
                }
                k.links.sort();
            }
            None => {
                by.insert((f.dev, f.ino), f);
            }
        }
    }
    by.into_values().collect()
}

/// The first and last [`EDGE`] bytes of a file.
fn edges(f: &File) -> Option<Vec<u8>> {
    let mut h = fs::File::open(&f.path).ok()?;
    let mut buf = vec![0u8; EDGE.min(f.size as usize)];
    h.read_exact(&mut buf).ok()?;
    if f.size > EDGE as u64 {
        let tail = EDGE.min((f.size - EDGE as u64) as usize);
        h.seek(SeekFrom::End(-(tail as i64))).ok()?;
        let mut t = vec![0u8; tail];
        h.read_exact(&mut t).ok()?;
        buf.extend(t);
    }
    Some(buf)
}

/// A 128-bit hash of the whole file: two SipHash streams, one salted.
fn full_hash(f: &File) -> Option<(u64, u64)> {
    let mut h = fs::File::open(&f.path).ok()?;
    // The hashes live only for this run, so the unspecified (but fixed
    // within one build) algorithm of DefaultHasher is fine.
    let mut a = std::hash::DefaultHasher::new();
    let mut b = std::hash::DefaultHasher::new();
    b.write(b"rtrash dupes second stream");
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = h.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        a.write(&buf[..n]);
        b.write(&buf[..n]);
    }
    Some((a.finish(), b.finish()))
}

/// Split `files` into groups by `key`, dropping keys with one file and
/// files whose key cannot be read.
fn regroup<K, F>(groups: Vec<Vec<File>>, key: F) -> Vec<Vec<File>>
where
    K: std::hash::Hash + Eq + Send,
    F: Fn(&File) -> Option<K> + Sync,
{
    groups
        .into_par_iter()
        .flat_map(|g| {
            let keyed: Vec<(K, File)> = g
                .into_par_iter()
                .filter_map(|f| key(&f).map(|k| (k, f)))
                .collect();
            let mut by: HashMap<K, Vec<File>> = HashMap::new();
            for (k, f) in keyed {
                by.entry(k).or_default().push(f);
            }
            by.into_values().filter(|v| v.len() > 1).collect::<Vec<_>>()
        })
        .collect()
}

/// What deleting `f` would free: its disk use, less the extents a reflink
/// shares, where the file system can say.
fn freed(f: &File) -> u64 {
    #[cfg(unix)]
    {
        if let Ok(h) = fs::File::open(&f.path) {
            if crate::du_extent::fs_may_share(&h) {
                return crate::du_extent::file_share(&h, f.disk).unique;
            }
        }
    }
    f.disk
}

/// The groups of duplicate files among `files`, largest reclaim first.
pub fn find(files: Vec<File>) -> Vec<Group> {
    let files = one_per_inode(files);
    let mut by_size: HashMap<u64, Vec<File>> = HashMap::new();
    for f in files {
        by_size.entry(f.size).or_default().push(f);
    }
    let same_size: Vec<Vec<File>> = by_size.into_values().filter(|v| v.len() > 1).collect();
    let same_edges = regroup(same_size, edges);
    // A file no longer than both edges is already read in full.
    let (small, big): (Vec<_>, Vec<_>) = same_edges
        .into_iter()
        .partition(|g| g[0].size <= 2 * EDGE as u64);
    let mut same = regroup(big, full_hash);
    same.extend(small);
    let mut groups: Vec<Group> = same
        .into_par_iter()
        .map(|mut g| {
            g.sort_by(|a, b| {
                (a.mtime, a.path.as_os_str().len(), &a.path).cmp(&(
                    b.mtime,
                    b.path.as_os_str().len(),
                    &b.path,
                ))
            });
            let keep = g.remove(0);
            let reclaim = g.iter().map(freed).sum();
            Group {
                size: keep.size,
                keep,
                extras: g,
                reclaim,
            }
        })
        .collect();
    groups.sort_by(|a, b| {
        b.reclaim
            .cmp(&a.reclaim)
            .then(b.size.cmp(&a.size))
            .then_with(|| a.keep.path.cmp(&b.keep.path))
    });
    groups
}

fn json_str(p: &Path) -> String {
    format!("\"{}\"", crate::util::json_escape(&p.to_string_lossy()))
}

/// `{groups:[{size, reclaim, keep, keep_links, extras, extra_links}],
/// total:{groups, extras, reclaim}}`; the links are the other names of
/// the kept file and of the extras.
pub fn to_json(groups: &[Group]) -> String {
    let gs: Vec<String> = groups
        .iter()
        .map(|g| {
            let extras: Vec<String> = g.extras.iter().map(|f| json_str(&f.path)).collect();
            let links: Vec<String> = g
                .extras
                .iter()
                .flat_map(|f| &f.links)
                .map(|p| json_str(p))
                .collect();
            let keep_links: Vec<String> = g.keep.links.iter().map(|p| json_str(p)).collect();
            format!(
                r#"{{"size":{},"reclaim":{},"keep":{},"keep_links":[{}],"extras":[{}],"extra_links":[{}]}}"#,
                g.size,
                g.reclaim,
                json_str(&g.keep.path),
                keep_links.join(","),
                extras.join(","),
                links.join(",")
            )
        })
        .collect();
    let extras: usize = groups.iter().map(|g| g.extras.len()).sum();
    let reclaim: u64 = groups.iter().map(|g| g.reclaim).sum();
    format!(
        r#"{{"groups":[{}],"total":{{"groups":{},"extras":{extras},"reclaim":{reclaim}}}}}"#,
        gs.join(","),
        groups.len()
    )
}

/// The text report: the `top` groups, then the total.
/// What `link_extras` did: names replaced, bytes freed, and one message
/// per name left alone.
#[derive(Debug, Default)]
pub struct Linked {
    pub names: usize,
    pub freed: u64,
    pub skipped: Vec<String>,
}

/// Replace every extra name with a hard link to the kept file, so one copy
/// of the contents stays on disk and every path still reads it.
///
/// A name is only replaced when it still is the file the scan saw (same
/// inode, size and mtime) and sits on the kept file's device. The link is
/// made under a temporary name in the same directory and renamed over the
/// extra, so the path is never missing.
pub fn link_extras(groups: &[Group]) -> Linked {
    replace_extras(groups, Replace::HardLink)
}

/// How `replace_extras` makes the new name.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Replace {
    /// A hard link to the kept file: the extra's metadata goes.
    HardLink,
    /// A reflink clone of the kept file's extents: a file of its own that
    /// keeps the extra's mode, owner and times, sharing the blocks until
    /// either side is written.
    Clone,
}

/// Replace every extra name with a hard link or a reflink clone of the
/// kept file. See `link_extras`; a clone needs a file system that shares
/// extents (btrfs, XFS, APFS and others).
pub fn replace_extras(groups: &[Group], how: Replace) -> Linked {
    let mut out = Linked::default();
    for (gi, g) in groups.iter().enumerate() {
        let mut whole = true;
        for f in &g.extras {
            if f.dev != g.keep.dev {
                out.skipped.push(format!(
                    "'{}' is on another device than '{}'",
                    f.path.display(),
                    g.keep.path.display()
                ));
                whole = false;
                continue;
            }
            for (ni, name) in std::iter::once(&f.path).chain(&f.links).enumerate() {
                if let Err(e) = link_one(&g.keep.path, name, f, (gi, ni), how) {
                    out.skipped.push(format!("'{}': {e}", name.display()));
                    whole = false;
                } else {
                    out.names += 1;
                }
            }
        }
        // Partly linked groups free an unknown share; count only whole ones.
        if whole {
            out.freed += g.reclaim;
        }
    }
    out
}

fn link_one(
    keep: &Path,
    name: &Path,
    seen: &File,
    tag: (usize, usize),
    how: Replace,
) -> std::io::Result<()> {
    let st = du_fs::lstat(name)?;
    if st.size != seen.size || st.mtime != seen.mtime || (seen.ino != 0 && st.ino != seen.ino) {
        return Err(std::io::Error::other("changed since the scan"));
    }
    let dir = name.parent().unwrap_or(Path::new("."));
    let tmp = dir.join(format!(
        ".rtrash-link-{}-{}-{}",
        std::process::id(),
        tag.0,
        tag.1
    ));
    match how {
        Replace::HardLink => fs::hard_link(keep, &tmp)?,
        Replace::Clone => {
            clone_file(keep, &tmp)?;
            if let Err(e) = keep_metadata(name, &tmp) {
                let _ = fs::remove_file(&tmp);
                return Err(e);
            }
        }
    }
    fs::rename(&tmp, name).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })
}

/// A new file `to` sharing every extent of `from`.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn clone_file(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    let src = fs::File::open(from)?;
    let dst = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(to)?;
    // SAFETY: both fds are open; FICLONE takes the source fd.
    if unsafe { libc::ioctl(dst.as_raw_fd(), libc::FICLONE, src.as_raw_fd()) } == 0 {
        return Ok(());
    }
    let e = std::io::Error::last_os_error();
    drop(dst);
    let _ = fs::remove_file(to);
    Err(clone_error(e))
}

#[cfg(target_vendor = "apple")]
fn clone_file(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c = |p: &Path| {
        std::ffi::CString::new(p.as_os_str().as_bytes())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))
    };
    let (a, b) = (c(from)?, c(to)?);
    // SAFETY: both paths are NUL-terminated; flag 0 follows no link at `to`.
    if unsafe { libc::clonefile(a.as_ptr(), b.as_ptr(), 0) } == 0 {
        return Ok(());
    }
    Err(clone_error(std::io::Error::last_os_error()))
}

/// ReFS block cloning: copy the integrity setting and sparseness, size the
/// new file, then map the source clusters into it with
/// `FSCTL_DUPLICATE_EXTENTS_TO_FILE`, one gigabyte per call.
#[cfg(windows)]
fn clone_file(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::windows::fs::MetadataExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::Ioctl::{
        DUPLICATE_EXTENTS_DATA, FSCTL_DUPLICATE_EXTENTS_TO_FILE, FSCTL_GET_INTEGRITY_INFORMATION,
        FSCTL_GET_INTEGRITY_INFORMATION_BUFFER, FSCTL_SET_INTEGRITY_INFORMATION,
        FSCTL_SET_INTEGRITY_INFORMATION_BUFFER, FSCTL_SET_SPARSE,
    };
    const SPARSE: u32 = 0x200;
    const CHUNK: u64 = 1 << 30;
    let src = fs::File::open(from)?;
    let meta = src.metadata()?;
    let dst = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(to)?;
    let run = || -> std::io::Result<()> {
        // SAFETY: a plain C struct; all zero is a valid value.
        let mut info: FSCTL_GET_INTEGRITY_INFORMATION_BUFFER = unsafe { std::mem::zeroed() };
        fsctl(
            &src,
            FSCTL_GET_INTEGRITY_INFORMATION,
            &[],
            as_bytes_mut(&mut info),
        )?;
        if meta.file_attributes() & SPARSE != 0 {
            fsctl(&dst, FSCTL_SET_SPARSE, &[], &mut [])?;
        }
        let set = FSCTL_SET_INTEGRITY_INFORMATION_BUFFER {
            ChecksumAlgorithm: info.ChecksumAlgorithm,
            Reserved: 0,
            Flags: info.Flags,
        };
        fsctl(
            &dst,
            FSCTL_SET_INTEGRITY_INFORMATION,
            as_bytes(&set),
            &mut [],
        )?;
        dst.set_len(meta.len())?;
        // The last range may run past the end up to a whole cluster.
        let cluster = u64::from(info.ClusterSizeInBytes.max(1));
        let total = meta.len().div_ceil(cluster) * cluster;
        let mut off = 0;
        while off < total {
            let n = CHUNK.min(total - off);
            let d = DUPLICATE_EXTENTS_DATA {
                FileHandle: src.as_raw_handle(),
                SourceFileOffset: off as i64,
                TargetFileOffset: off as i64,
                ByteCount: n as i64,
            };
            fsctl(&dst, FSCTL_DUPLICATE_EXTENTS_TO_FILE, as_bytes(&d), &mut [])?;
            off += n;
        }
        Ok(())
    };
    if let Err(e) = run() {
        drop(dst);
        let _ = fs::remove_file(to);
        return Err(clone_error(e));
    }
    Ok(())
}

#[cfg(windows)]
fn as_bytes<T>(v: &T) -> &[u8] {
    // SAFETY: reads the bytes of a plain C struct for the ioctl.
    unsafe { std::slice::from_raw_parts((v as *const T).cast(), std::mem::size_of::<T>()) }
}

#[cfg(windows)]
fn as_bytes_mut<T>(v: &mut T) -> &mut [u8] {
    // SAFETY: the ioctl fills a plain C struct whose every bit pattern is valid.
    unsafe { std::slice::from_raw_parts_mut((v as *mut T).cast(), std::mem::size_of::<T>()) }
}

#[cfg(windows)]
fn fsctl(f: &fs::File, code: u32, input: &[u8], out: &mut [u8]) -> std::io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    let mut got = 0u32;
    let ptr = |b: &[u8]| {
        if b.is_empty() {
            std::ptr::null()
        } else {
            b.as_ptr().cast()
        }
    };
    let out_ptr = if out.is_empty() {
        std::ptr::null_mut()
    } else {
        out.as_mut_ptr().cast()
    };
    // SAFETY: the handle is open and both buffers outlive the call.
    let ok = unsafe {
        windows_sys::Win32::System::IO::DeviceIoControl(
            f.as_raw_handle(),
            code,
            ptr(input),
            input.len() as u32,
            out_ptr,
            out.len() as u32,
            &mut got,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(windows)]
fn clone_error(e: std::io::Error) -> std::io::Error {
    // Invalid function, not same device, not supported: NTFS, FAT, a
    // network share, or two volumes.
    match e.raw_os_error() {
        Some(1 | 17 | 50) => {
            std::io::Error::other("the file system cannot share extents (no reflink support)")
        }
        _ => e,
    }
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple",
    windows
)))]
fn clone_file(_from: &Path, _to: &Path) -> std::io::Result<()> {
    Err(std::io::Error::other(
        "reflink clones are not supported on this platform",
    ))
}

#[cfg(unix)]
fn clone_error(e: std::io::Error) -> std::io::Error {
    match e.raw_os_error() {
        Some(libc::EOPNOTSUPP | libc::EINVAL | libc::EXDEV | libc::ENOTTY) => {
            std::io::Error::other("the file system cannot share extents (no reflink support)")
        }
        _ => e,
    }
}

/// Give the clone `to` the mode, owner and times of `from`, the file it
/// replaces.
#[cfg(unix)]
fn keep_metadata(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let m = fs::symlink_metadata(from)?;
    // Only root may give a file away; a user's own files keep their owner.
    let _ = std::os::unix::fs::lchown(to, Some(m.uid()), Some(m.gid()));
    fs::set_permissions(to, m.permissions())?;
    let f = fs::File::options().write(true).open(to)?;
    f.set_times(
        fs::FileTimes::new()
            .set_accessed(m.accessed()?)
            .set_modified(m.modified()?),
    )
}

/// The read-only bit and times of `from` on the clone `to`.
#[cfg(not(unix))]
fn keep_metadata(from: &Path, to: &Path) -> std::io::Result<()> {
    let m = fs::symlink_metadata(from)?;
    {
        let f = fs::File::options().write(true).open(to)?;
        f.set_times(
            fs::FileTimes::new()
                .set_accessed(m.accessed()?)
                .set_modified(m.modified()?),
        )?;
    }
    fs::set_permissions(to, m.permissions())
}

pub fn print_text(groups: &[Group], top: usize) {
    let fmt = crate::util::format_bytes;
    if !groups.is_empty() {
        println!("Duplicate files (by space freed)");
    }
    for g in groups.iter().take(if top == 0 { usize::MAX } else { top }) {
        let shared = if g.reclaim == 0 {
            ", shared by reflinks"
        } else {
            ""
        };
        println!(
            "{} copies of {}, {} to free{shared}",
            g.extras.len() + 1,
            fmt(g.size),
            fmt(g.reclaim)
        );
        println!("  keep  {}", crate::util::term_path(&g.keep.path));
        for p in &g.keep.links {
            println!("  keep  {} (hard link)", crate::util::term_path(p));
        }
        for f in &g.extras {
            println!("        {}", crate::util::term_path(&f.path));
            for p in &f.links {
                println!("        {} (hard link)", crate::util::term_path(p));
            }
        }
    }
    let extras: usize = groups.iter().map(|g| g.extras.len()).sum();
    let reclaim: u64 = groups.iter().map(|g| g.reclaim).sum();
    println!(
        "Total: {} with {}, {} to free",
        crate::util::plural(groups.len() as u64, "group", "groups"),
        crate::util::plural(extras as u64, "extra copy", "extra copies"),
        fmt(reclaim)
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(name: &str) -> PathBuf {
        let t = std::env::temp_dir().join(format!("rtrash-dupes-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&t);
        fs::create_dir_all(t.join("a/b")).unwrap();
        t
    }

    #[test]
    fn groups_by_contents_and_counts_hard_links_once() {
        let t = tree("unit");
        let big: Vec<u8> = (0..20_000u32).map(|i| (i % 251) as u8).collect();
        let mut other = big.clone();
        other[10_000] ^= 1; // same size and edges, different middle
        fs::write(t.join("one.bin"), &big).unwrap();
        fs::write(t.join("a/two.bin"), &big).unwrap();
        fs::write(t.join("a/b/near.bin"), &other).unwrap();
        fs::write(t.join("small1"), b"hello").unwrap();
        fs::write(t.join("a/small2"), b"hello").unwrap();
        fs::write(t.join("lonely"), b"x").unwrap();
        #[cfg(unix)]
        fs::hard_link(t.join("one.bin"), t.join("a/b/one-link.bin")).unwrap();
        let opts = Opts {
            min_size: 1,
            one_file_system: false,
            include_kernfs: false,
        };
        let groups = find(walk(std::slice::from_ref(&t), opts));
        assert_eq!(groups.len(), 2, "{groups:?}");
        let big_g = groups.iter().find(|g| g.size == 20_000).unwrap();
        assert_eq!(big_g.extras.len(), 1, "{big_g:?}");
        let names: Vec<_> = std::iter::once(&big_g.keep)
            .chain(&big_g.extras)
            .map(|f| f.path.file_name().unwrap().to_owned())
            .collect();
        assert!(!names.iter().any(|n| n == "near.bin"), "{names:?}");
        let small = groups.iter().find(|g| g.size == 5).unwrap();
        assert_eq!(small.extras.len(), 1);
        let j = to_json(&groups);
        assert!(j.contains(r#""total":{"groups":2,"extras":2,"#), "{j}");
        let _ = fs::remove_dir_all(&t);
    }
}
