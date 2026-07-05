use std::collections::HashSet;
use std::fs;
use std::io::{self, Write};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::info;

/// A `files/` + `info/` pair per the freedesktop.org Trash spec.
#[derive(Clone, Debug)]
pub struct TrashDir {
    /// Directory containing `files/` and `info/`.
    pub root: PathBuf,
    /// For per-mount trash dirs, the mount point; `Path=` values are relative to it.
    pub topdir: Option<PathBuf>,
}

impl TrashDir {
    pub fn files(&self) -> PathBuf {
        self.root.join("files")
    }
    pub fn info(&self) -> PathBuf {
        self.root.join("info")
    }

    fn ensure(&self) -> io::Result<()> {
        let mut b = fs::DirBuilder::new();
        b.recursive(true).mode(0o700);
        b.create(self.files())?;
        b.create(self.info())?;
        Ok(())
    }

    /// Original path recorded in `info`, resolved to absolute.
    pub fn resolve_original(&self, recorded: &Path) -> PathBuf {
        match (&self.topdir, recorded.is_absolute()) {
            (Some(top), false) => top.join(recorded),
            _ => recorded.to_path_buf(),
        }
    }
}

pub fn uid() -> u32 {
    unsafe { libc::getuid() }
}

pub fn home_trash() -> PathBuf {
    let data = std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default();
            home.join(".local/share")
        });
    data.join("Trash")
}

fn dev_of(path: &Path) -> Option<u64> {
    fs::symlink_metadata(path).ok().map(|m| m.dev())
}

/// Device of the deepest existing ancestor (the fs the path would land on).
fn dev_of_deepest_existing(path: &Path) -> Option<u64> {
    let mut cur = path;
    loop {
        if let Some(d) = dev_of(cur) {
            return Some(d);
        }
        cur = cur.parent()?;
    }
}

/// Walk up from `start` until the parent lives on a different device.
fn mount_top_of(start: &Path, dev: u64) -> PathBuf {
    let mut cur = start.to_path_buf();
    while let Some(parent) = cur.parent() {
        match dev_of(parent) {
            Some(d) if d == dev => cur = parent.to_path_buf(),
            _ => break,
        }
    }
    cur
}

fn sticky_and_dir(meta: &fs::Metadata) -> bool {
    meta.is_dir() && (meta.permissions().mode() & 0o1000) != 0
}

/// Trash dir for a file on device `dev` at absolute path `abs`.
/// Home trash when on the same fs; otherwise `$top/.Trash/$uid` (must be a
/// sticky non-symlink dir) or `$top/.Trash-$uid`; home trash as last resort
/// (put then copies across devices).
pub fn select(abs: &Path, dev: u64) -> io::Result<TrashDir> {
    let home = TrashDir {
        root: home_trash(),
        topdir: None,
    };
    if dev_of_deepest_existing(&home.root) == Some(dev) {
        home.ensure()?;
        return Ok(home);
    }

    let parent = abs.parent().unwrap_or(Path::new("/"));
    let top = mount_top_of(parent, dev);
    let uid = uid();

    let shared = top.join(".Trash");
    if let Ok(meta) = fs::symlink_metadata(&shared) {
        if sticky_and_dir(&meta) {
            let td = TrashDir {
                root: shared.join(uid.to_string()),
                topdir: Some(top.clone()),
            };
            if td.ensure().is_ok() {
                return Ok(td);
            }
        }
    }

    let td = TrashDir {
        root: top.join(format!(".Trash-{uid}")),
        topdir: Some(top),
    };
    if td.ensure().is_ok() {
        return Ok(td);
    }

    home.ensure()?;
    Ok(home)
}

/// All trash dirs visible to this user: home trash plus per-mount dirs.
pub fn all() -> Vec<TrashDir> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    let home = home_trash();
    if home.is_dir() {
        seen.insert(home.clone());
        out.push(TrashDir {
            root: home,
            topdir: None,
        });
    }
    let uid = uid();
    for mp in mount_points() {
        for cand in [
            mp.join(".Trash").join(uid.to_string()),
            mp.join(format!(".Trash-{uid}")),
        ] {
            if !seen.contains(&cand) && cand.is_dir() {
                seen.insert(cand.clone());
                out.push(TrashDir {
                    root: cand,
                    topdir: Some(mp.clone()),
                });
            }
        }
    }
    out
}

const PSEUDO_FS: &[&str] = &[
    "proc",
    "sysfs",
    "devtmpfs",
    "devpts",
    "securityfs",
    "cgroup",
    "cgroup2",
    "pstore",
    "bpf",
    "autofs",
    "mqueue",
    "hugetlbfs",
    "debugfs",
    "tracefs",
    "fusectl",
    "configfs",
    "binfmt_misc",
    "rpc_pipefs",
    "nsfs",
    "efivarfs",
    "fuse.portal",
    "fuse.gvfsd-fuse",
    "squashfs",
    "ramfs",
];

/// Mount-point escapes per fstab(5): `\040` space, `\011` tab, `\012` newline, `\134` backslash.
fn unescape_mount(s: &str) -> PathBuf {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() {
            let oct = &b[i + 1..i + 4];
            if oct.iter().all(|c| (b'0'..=b'7').contains(c)) {
                let v = (oct[0] - b'0') * 64 + (oct[1] - b'0') * 8 + (oct[2] - b'0');
                out.push(v);
                i += 4;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    PathBuf::from(std::ffi::OsString::from_vec(out))
}

fn mount_points() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(content) = fs::read_to_string("/proc/self/mounts") else {
        return out;
    };
    let mut seen = HashSet::new();
    for line in content.lines() {
        let mut fields = line.split_whitespace();
        let _dev = fields.next();
        let (Some(mp), Some(fstype)) = (fields.next(), fields.next()) else {
            continue;
        };
        if PSEUDO_FS.contains(&fstype) || fstype.starts_with("cgroup") {
            continue;
        }
        let mp = unescape_mount(mp);
        if mp == Path::new("/") || !seen.insert(mp.clone()) {
            continue;
        }
        out.push(mp);
    }
    out
}

/// Absolute path without resolving the final component (so symlinks trash as
/// symlinks): canonicalize the parent, re-attach the file name.
pub fn abs_nofollow(path: &Path) -> io::Result<PathBuf> {
    let file_name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?;
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.canonicalize()?,
        _ => std::env::current_dir()?,
    };
    Ok(parent.join(file_name))
}

/// Move `abs` into `trash`, reserving a unique name via O_EXCL on the
/// `.trashinfo` (the spec's atomicity requirement). Returns the name used.
pub fn trash_move(abs: &Path, meta: &fs::Metadata, trash: &TrashDir) -> io::Result<String> {
    let base = abs
        .file_name()
        .and_then(|n| n.to_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            String::from_utf8_lossy(abs.file_name().unwrap_or_default().as_encoded_bytes())
                .into_owned()
        });

    let recorded: PathBuf = match &trash.topdir {
        Some(top) => abs
            .strip_prefix(top)
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|_| abs.to_path_buf()),
        None => abs.to_path_buf(),
    };
    let body = info::render(&recorded, &info::now_local_string());

    let mut attempt: u64 = 0;
    let (name, mut info_file) = loop {
        let name = if attempt == 0 {
            base.clone()
        } else {
            format!("{base}.{}", attempt + 1)
        };
        let info_path = trash.info().join(format!("{name}.trashinfo"));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&info_path)
        {
            Ok(f) => break (name, f),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                attempt += 1;
                if attempt > 10_000 {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "could not reserve a unique trash name",
                    ));
                }
            }
            Err(e) => return Err(e),
        }
    };
    info_file.write_all(body.as_bytes())?;
    drop(info_file);

    let dest = trash.files().join(&name);
    let moved = match fs::rename(abs, &dest) {
        Ok(()) => Ok(()),
        Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
            copy_recursive(abs, &dest, meta).and_then(|_| remove_any(abs, meta))
        }
        Err(e) => Err(e),
    };
    if let Err(e) = moved {
        let _ = fs::remove_file(trash.info().join(format!("{name}.trashinfo")));
        let _ = remove_any_path(&dest);
        return Err(e);
    }
    Ok(name)
}

fn remove_any(path: &Path, meta: &fs::Metadata) -> io::Result<()> {
    if meta.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

/// Remove a path whatever it is; used by empty, restore force-overwrite, and error cleanup.
pub fn remove_any_path(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Move `src` to `dest`, falling back to copy+delete across devices (EXDEV).
/// Used by restore when the trash entry lives on a different filesystem than
/// the original path (e.g. home-trash fallback after a volume was full).
pub fn relocate(src: &Path, dest: &Path) -> io::Result<()> {
    match fs::rename(src, dest) {
        Ok(()) => Ok(()),
        Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
            let meta = fs::symlink_metadata(src)?;
            copy_recursive(src, dest, &meta).and_then(|_| remove_any(src, &meta))
        }
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relocate_same_fs_moves_file() {
        let root = std::env::temp_dir().join(format!(
            "rtrash-relocate-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let src = root.join("src");
        let dst = root.join("dst");
        fs::write(&src, b"payload").unwrap();
        relocate(&src, &dst).unwrap();
        assert!(!src.exists());
        assert_eq!(fs::read(&dst).unwrap(), b"payload");
        let _ = fs::remove_dir_all(&root);
    }
}

/// Cross-device fallback: copy preserving symlinks and permissions.
fn copy_recursive(src: &Path, dst: &Path, meta: &fs::Metadata) -> io::Result<()> {
    let ftype = meta.file_type();
    if ftype.is_symlink() {
        let target = fs::read_link(src)?;
        std::os::unix::fs::symlink(target, dst)
    } else if ftype.is_dir() {
        fs::create_dir(dst)?;
        fs::set_permissions(dst, meta.permissions())?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            let emeta = fs::symlink_metadata(entry.path())?;
            copy_recursive(&entry.path(), &dst.join(entry.file_name()), &emeta)?;
        }
        Ok(())
    } else {
        fs::copy(src, dst).map(|_| ())
    }
}
