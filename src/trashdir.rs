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

    pub fn ensure(&self) -> io::Result<()> {
        let mut b = fs::DirBuilder::new();
        b.recursive(true).mode(0o700);
        b.create(self.files())?;
        b.create(self.info())?;
        Ok(())
    }

    /// Original path recorded in `info`, resolved to absolute.
    ///
    /// Relative volume paths are joined to `topdir` only if they do not escape
    /// it via `..` (hostile/corrupt `.trashinfo` fail closed to the joined path
    /// rejected by callers that use [`resolve_original_checked`]).
    pub fn resolve_original(&self, recorded: &Path) -> PathBuf {
        match self.resolve_original_checked(recorded) {
            Ok(p) => p,
            // Fail soft for list display: show the raw joined/lex path, but
            // restore must use the checked form.
            Err(_) => match (&self.topdir, recorded.is_absolute()) {
                (Some(top), false) => top.join(recorded),
                _ => recorded.to_path_buf(),
            },
        }
    }

    /// Like [`resolve_original`] but returns an error if a relative `Path=`
    /// escapes the volume topdir (contains `..` that leaves the mount root).
    pub fn resolve_original_checked(&self, recorded: &Path) -> io::Result<PathBuf> {
        match (&self.topdir, recorded.is_absolute()) {
            (Some(top), false) => {
                if recorded
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
                {
                    // Also reject after lexical clean escapes top.
                    let joined = lexical_join_under(top, recorded);
                    if !joined.starts_with(top) {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "trash Path= escapes volume topdir",
                        ));
                    }
                    // ParentDir present is enough to refuse even if still under top
                    // after clean (spec paths should not need ..).
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "trash Path= must not contain '..'",
                    ));
                }
                Ok(top.join(recorded))
            }
            _ => Ok(recorded.to_path_buf()),
        }
    }
}

/// Join `rel` under `top` with lexical `..` resolution (no symlink walk).
fn lexical_join_under(top: &Path, rel: &Path) -> PathBuf {
    let mut out = top.to_path_buf();
    for c in rel.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
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

/// Mount point of `path`: longest prefix among non-pseudo mounts (btrfs-safe when
/// many subvolumes share one `st_dev`). Falls back to an `st_dev` parent walk only
/// when `/proc/self/mounts` is unreadable.
pub fn mount_top_of_path(path: &Path) -> PathBuf {
    let mounts = mount_points();
    if mounts.is_empty() {
        let dev = dev_of(path).or_else(|| dev_of_deepest_existing(path));
        return match dev {
            Some(d) => mount_top_of_by_dev(path, d),
            None => PathBuf::from("/"),
        };
    }
    longest_mount_prefix(path, &mounts)
}

/// Pure helper: longest mount-point prefix of `path` from a mount list.
/// Used by put selection and unit-tested with synthetic tables (btrfs multi-subvol).
pub fn longest_mount_prefix(path: &Path, mounts: &[PathBuf]) -> PathBuf {
    let mut best: Option<&Path> = None;
    let mut best_comps = 0usize;
    for m in mounts {
        if path.starts_with(m) {
            let comps = m.components().count();
            // Prefer deeper mounts; for equal depth prefer longer OsStr (ties rare).
            let better = match best {
                None => true,
                Some(b) => {
                    let bc = b.components().count();
                    comps > bc || (comps == bc && m.as_os_str().len() > b.as_os_str().len())
                }
            };
            if better {
                best = Some(m.as_path());
                best_comps = comps;
            }
        }
    }
    let _ = best_comps;
    best.unwrap_or(Path::new("/")).to_path_buf()
}

/// Legacy fallback: walk parents while `st_dev` matches.
fn mount_top_of_by_dev(start: &Path, dev: u64) -> PathBuf {
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
///
/// Volume `top` is the **mount point** of `abs` (longest `/proc/self/mounts`
/// prefix), not a pure `st_dev` walk — required when btrfs subvolumes share one
/// device id.
pub fn select(abs: &Path, dev: u64) -> io::Result<TrashDir> {
    let home = TrashDir {
        root: home_trash(),
        topdir: None,
    };
    if dev_of_deepest_existing(&home.root) == Some(dev) {
        home.ensure()?;
        return Ok(home);
    }

    let top = mount_top_of_path(abs);
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

/// Infer the FreeDesktop volume topdir from a trash root path.
///
/// - `$top/.Trash-$uid` → `Some(top)`
/// - `$top/.Trash/$uid` → `Some(top)`
/// - home-style trash (or unrecognized layout) → `None` (Path= is absolute)
pub fn infer_topdir(root: &Path) -> Option<PathBuf> {
    let name = root.file_name()?.to_string_lossy();
    let parent = root.parent()?;

    // $topdir/.Trash-$uid
    if let Some(rest) = name.strip_prefix(".Trash-") {
        if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) {
            return Some(parent.to_path_buf());
        }
    }

    // $topdir/.Trash/$uid
    let parent_name = parent.file_name()?.to_string_lossy();
    if parent_name == ".Trash" && !name.is_empty() && name.chars().all(|c| c.is_ascii_digit()) {
        return parent.parent().map(|p| p.to_path_buf());
    }

    None
}

/// Resolve the set of trash directories to operate on: explicit pins, or every
/// visible trash dir when `pins` is empty.
///
/// Pins that look like volume trash roots (`.Trash-$uid` or `.Trash/$uid`) get
/// a `topdir` so relative `Path=` values resolve the same as in [`all`].
/// Explicit pins must look like a FreeDesktop trash root (real `files/` and
/// `info/` directories, not symlinks) so a mistaken `--trash-dir=./project` with
/// co-incidental child names cannot mass-wipe.
pub fn resolve_dirs(pins: &[PathBuf]) -> Vec<TrashDir> {
    resolve_dirs_opts(pins, false)
}

/// Like [`resolve_dirs`], but `home_only` restricts unpinned discovery to the
/// home trash (scripts that must not touch volume trash on USB mounts).
pub fn resolve_dirs_opts(pins: &[PathBuf], home_only: bool) -> Vec<TrashDir> {
    if pins.is_empty() {
        return if home_only { home_only_dirs() } else { all() };
    }
    pins.iter()
        .filter_map(|p| {
            let root = if p.is_absolute() {
                p.clone()
            } else {
                std::env::current_dir().ok()?.join(p)
            };
            if !is_valid_trash_root(&root) {
                eprintln!(
                    "rtrash: ignoring --trash-dir='{}' (need non-symlink files/ and info/ directories)",
                    p.display()
                );
                return None;
            }
            Some(TrashDir {
                root: root.clone(),
                topdir: infer_topdir(&root),
            })
        })
        .collect()
}

/// Home trash only (if it exists), for `--home-only`.
pub fn home_only_dirs() -> Vec<TrashDir> {
    let home = home_trash();
    if home.is_dir() {
        vec![TrashDir {
            root: home,
            topdir: None,
        }]
    } else {
        Vec::new()
    }
}

/// True when `root` has non-symlink directory children `files` and `info`.
pub fn is_valid_trash_root(root: &Path) -> bool {
    for name in ["files", "info"] {
        let p = root.join(name);
        let Ok(meta) = fs::symlink_metadata(&p) else {
            return false;
        };
        if meta.file_type().is_symlink() || !meta.is_dir() {
            return false;
        }
    }
    true
}

/// All trash dirs visible to this user: home trash plus every non-pseudo
/// mounted volume’s existing user trash (trash-cli multi-volume default).
/// Does not create missing volume trash dirs — only discovers existing ones.
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

/// Exclusive per-trash-root lock (flock on `root/.rtrash.lock`) so put and empty
/// cannot interleave a half-written pair under the same root.
pub struct TrashLock {
    _file: fs::File,
}

impl TrashLock {
    /// Blocking exclusive lock for the critical section of put/empty.
    pub fn acquire(root: &Path) -> io::Result<Self> {
        Self::acquire_flags(root, libc::LOCK_EX)
    }

    /// Non-blocking attempt (tests).
    pub fn try_acquire(root: &Path) -> io::Result<Self> {
        Self::acquire_flags(root, libc::LOCK_EX | libc::LOCK_NB)
    }

    fn acquire_flags(root: &Path, flags: i32) -> io::Result<Self> {
        fs::create_dir_all(root)?;
        let path = root.join(".rtrash.lock");
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)?;
        use std::os::unix::io::AsRawFd;
        let rc = unsafe { libc::flock(file.as_raw_fd(), flags) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(TrashLock { _file: file })
    }
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

/// Non-pseudo mount points from `/proc/self/mounts`, **including `/`**, for
/// trash-cli-compatible multi-volume discovery (home + every mount that may
/// host `.Trash-$uid` / `.Trash/$uid`).
pub fn mount_points() -> Vec<PathBuf> {
    let Ok(content) = fs::read_to_string("/proc/self/mounts") else {
        return vec![PathBuf::from("/")];
    };
    mount_points_from_table(&content)
}

/// Parse an fstab/mounts-style table into non-pseudo mount points (testable).
pub fn mount_points_from_table(content: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
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
        if !seen.insert(mp.clone()) {
            continue;
        }
        out.push(mp);
    }
    if !seen.contains(Path::new("/")) {
        out.insert(0, PathBuf::from("/"));
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
///
/// Holds [`TrashLock`] for the duration of reserve → fsync → payload move so
/// concurrent empty of the same root cannot tear the pair.
pub fn trash_move(abs: &Path, meta: &fs::Metadata, trash: &TrashDir) -> io::Result<String> {
    trash.ensure()?;
    let _lock = TrashLock::acquire(&trash.root)?;
    trash_move_locked(abs, meta, trash)
}

/// Inner put path (caller holds the trash-root lock). Exposed for tests.
pub fn trash_move_locked(abs: &Path, meta: &fs::Metadata, trash: &TrashDir) -> io::Result<String> {
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
        // Skip names already occupied by an orphan payload (no .trashinfo).
        if trash.files().join(&name).symlink_metadata().is_ok() {
            attempt += 1;
            if attempt > 10_000 {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "could not reserve a unique trash name",
                ));
            }
            continue;
        }
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
    // Durable reservation before payload move (crash mid-put must not leave a
    // truncated .trashinfo as the only recoverability record).
    sync_file(&mut info_file)?;
    drop(info_file);
    let _ = sync_dir(&trash.info());

    let dest = trash.files().join(&name);
    let moved = match rename_noreplace(abs, &dest) {
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
    // FreeDesktop directorysizes cache: only for trashed directories.
    if meta.is_dir() {
        let size = dir_tree_size(&dest).unwrap_or(0);
        let mtime = info_mtime_secs(&trash.info().join(format!("{name}.trashinfo"))).unwrap_or(0);
        let _ = directorysizes_upsert(trash, &name, size, mtime);
    }
    Ok(name)
}

/// Flush file data+metadata to stable storage.
pub fn sync_file(file: &mut fs::File) -> io::Result<()> {
    file.sync_all()
}

fn sync_dir(path: &Path) -> io::Result<()> {
    let f = fs::File::open(path)?;
    f.sync_all()
}

/// `rename(2)` that refuses to replace an existing path when the kernel supports
/// `RENAME_NOREPLACE` (Linux); falls back to plain `rename` otherwise.
fn rename_noreplace(src: &Path, dest: &Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let src_c = std::ffi::CString::new(src.as_os_str().as_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let dest_c = std::ffi::CString::new(dest.as_os_str().as_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    // RENAME_NOREPLACE = 1 on Linux. Use the renameat2 syscall (available under
    // both glibc and musl libc crates; renameat2 is not always exported as a
    // function symbol on musl).
    let rc = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            src_c.as_ptr(),
            libc::AT_FDCWD,
            dest_c.as_ptr(),
            1usize,
        )
    };
    if rc == 0 {
        return Ok(());
    }
    let err = io::Error::last_os_error();
    // Older kernels / non-Linux: fall back (orphan pre-check still applies).
    if err.raw_os_error() == Some(libc::EINVAL) || err.raw_os_error() == Some(libc::ENOSYS) {
        return fs::rename(src, dest);
    }
    Err(err)
}

/// Total byte size of a directory tree (symlink targets not followed).
fn dir_tree_size(path: &Path) -> io::Result<u64> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_dir() {
        return Ok(meta.len());
    }
    let mut total = 0u64;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let emeta = fs::symlink_metadata(entry.path())?;
        if emeta.is_dir() {
            total = total.saturating_add(dir_tree_size(&entry.path())?);
        } else {
            total = total.saturating_add(emeta.len());
        }
    }
    Ok(total)
}

fn info_mtime_secs(path: &Path) -> io::Result<i64> {
    use std::time::SystemTime;
    let meta = fs::metadata(path)?;
    let d = meta
        .modified()?
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    Ok(d.as_secs() as i64)
}

/// Insert or replace a `directorysizes` line: `size mtime percent-encoded-name`.
/// Writes via temp file + rename (FreeDesktop: avoid concurrent cache corruption).
pub fn directorysizes_upsert(
    trash: &TrashDir,
    name: &str,
    size: u64,
    mtime: i64,
) -> io::Result<()> {
    use crate::util::url_encode;
    let path = trash.root.join("directorysizes");
    let enc = url_encode(name.as_bytes());
    let line = format!("{size} {mtime} {enc}\n");
    let mut out = String::new();
    if let Ok(existing) = fs::read_to_string(&path) {
        for l in existing.lines() {
            let keep = l.rsplit(' ').next().is_some_and(|e| e != enc.as_str());
            if keep && !l.is_empty() {
                out.push_str(l);
                out.push('\n');
            }
        }
    }
    out.push_str(&line);
    atomic_write(&path, out.as_bytes())
}

/// Drop a single name from `directorysizes` (used by selective trash-rm).
pub fn directorysizes_remove(trash: &TrashDir, name: &str) {
    use crate::util::url_encode;
    let path = trash.root.join("directorysizes");
    let Ok(existing) = fs::read_to_string(&path) else {
        return;
    };
    let enc = url_encode(name.as_bytes());
    let filtered: String = existing
        .lines()
        .filter(|l| l.rsplit(' ').next().is_some_and(|e| e != enc.as_str()))
        .map(|l| format!("{l}\n"))
        .collect();
    if filtered.is_empty() {
        let _ = fs::remove_file(&path);
    } else {
        let _ = atomic_write(&path, filtered.as_bytes());
    }
}

/// Parse one FreeDesktop `directorysizes` line: `size mtime percent-encoded-name`.
/// Returns `None` on any malformed line (callers fall back to a real walk).
pub fn directorysizes_parse_line(line: &str) -> Option<(u64, i64, String)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let mut parts = line.splitn(3, ' ');
    let size: u64 = parts.next()?.parse().ok()?;
    let mtime: i64 = parts.next()?.parse().ok()?;
    let enc = parts.next()?.to_string();
    if enc.is_empty() {
        return None;
    }
    Some((size, mtime, enc))
}

/// Cached directory payload size from `$trash/directorysizes` when the line is
/// still valid for `name`:
/// - a matching percent-encoded name is present;
/// - the payload under `files/` exists and is a directory (cache is for dirs);
/// - the cached mtime equals the current `.trashinfo` mtime (FreeDesktop intent).
///
/// Any doubt → `None` so callers use [`crate::fastdelete::disk_usage`].
pub fn directorysizes_cached_size(trash: &TrashDir, name: &str) -> Option<u64> {
    use crate::util::url_encode;
    let path = trash.root.join("directorysizes");
    let body = fs::read_to_string(path).ok()?;
    let want = url_encode(name.as_bytes());
    let mut hit: Option<(u64, i64)> = None;
    for line in body.lines() {
        let Some((size, mtime, enc)) = directorysizes_parse_line(line) else {
            continue;
        };
        if enc == want {
            hit = Some((size, mtime));
            // Last matching line wins (upsert rewrites whole file with one line,
            // but tolerate hand-edited duplicates conservatively).
        }
    }
    let (size, mtime) = hit?;
    let payload = trash.files().join(name);
    let meta = fs::symlink_metadata(&payload).ok()?;
    if !meta.is_dir() {
        return None;
    }
    let info = trash.info().join(format!("{name}.trashinfo"));
    let info_mt = info_mtime_secs(&info).ok()?;
    if info_mt != mtime {
        return None;
    }
    Some(size)
}

/// Approximate reclaimable bytes for one named trash entry (payload + its
/// `.trashinfo` when present). Directory payloads use a valid `directorysizes`
/// cache line when available; files, orphans, and invalid cache always walk.
pub fn entry_reclaim_bytes(trash: &TrashDir, name: &str) -> u64 {
    let payload = trash.files().join(name);
    let info = trash.info().join(format!("{name}.trashinfo"));
    let payload_sz = directorysizes_cached_size(trash, name)
        .unwrap_or_else(|| crate::fastdelete::disk_usage(&payload));
    let info_sz = if info.exists() {
        crate::fastdelete::disk_usage(&info)
    } else {
        0
    };
    payload_sz.saturating_add(info_sz)
}

/// Reclaimable bytes for a path under `files/` (top-level name), using the
/// directorysizes cache when the payload is a directory with a valid line.
pub fn files_child_reclaim_bytes(trash: &TrashDir, path: &Path) -> u64 {
    let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
        return crate::fastdelete::disk_usage(path);
    };
    // Prefer cache only when this is the trash files/ child (same parent as files()).
    if path.parent().is_some_and(|p| p == trash.files()) {
        if let Some(sz) = directorysizes_cached_size(trash, &name) {
            return sz;
        }
    }
    crate::fastdelete::disk_usage(path)
}

/// Write `data` to `path` via a same-dir temp file and rename (atomic on POSIX).
pub fn atomic_write(path: &Path, data: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let parent = path.parent().unwrap_or(Path::new("."));
    let tmp = parent.join(format!(
        ".{}.tmp.{}",
        path.file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "rtrash".into()),
        std::process::id()
    ));
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    match fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

fn remove_any(path: &Path, meta: &fs::Metadata) -> io::Result<()> {
    if meta.is_dir() {
        crate::fastdelete::remove_path(path)
    } else {
        fs::remove_file(path)
    }
}

/// Remove a path whatever it is; used by empty, restore force-overwrite, and error cleanup.
/// Directory trees use the bulk unlinkat walker (and btrfs subvolume destroy when applicable).
pub fn remove_any_path(path: &Path) -> io::Result<()> {
    crate::fastdelete::remove_path(path)
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

/// Cross-device fallback: copy preserving symlinks-as-links, content, mode, and mtime.
/// Shared by put (EXDEV into trash) and restore (`relocate`).
pub fn copy_recursive(src: &Path, dst: &Path, meta: &fs::Metadata) -> io::Result<()> {
    let ftype = meta.file_type();
    if ftype.is_symlink() {
        let target = fs::read_link(src)?;
        std::os::unix::fs::symlink(&target, dst)?;
        // Symlink mtime: best-effort via lutimes/utimensat AT_SYMLINK_NOFOLLOW.
        let _ = set_times_nofollow(dst, meta);
        Ok(())
    } else if ftype.is_dir() {
        fs::create_dir(dst)?;
        fs::set_permissions(dst, meta.permissions())?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            let emeta = fs::symlink_metadata(entry.path())?;
            copy_recursive(&entry.path(), &dst.join(entry.file_name()), &emeta)?;
        }
        set_times(dst, meta)?;
        Ok(())
    } else {
        fs::copy(src, dst)?;
        fs::set_permissions(dst, meta.permissions())?;
        set_times(dst, meta)?;
        Ok(())
    }
}

fn set_times(path: &Path, meta: &fs::Metadata) -> io::Result<()> {
    let modified = meta.modified()?;
    let accessed = meta.accessed().unwrap_or(modified);
    if meta.is_dir() {
        return set_times_path(path, accessed, modified, false);
    }
    match fs::OpenOptions::new().write(true).open(path) {
        Ok(f) => {
            let times = fs::FileTimes::new()
                .set_accessed(accessed)
                .set_modified(modified);
            f.set_times(times)
        }
        Err(_) => set_times_path(path, accessed, modified, false),
    }
}

fn set_times_nofollow(path: &Path, meta: &fs::Metadata) -> io::Result<()> {
    let modified = meta.modified()?;
    let accessed = meta.accessed().unwrap_or(modified);
    set_times_path(path, accessed, modified, true)
}

fn set_times_path(
    path: &Path,
    accessed: std::time::SystemTime,
    modified: std::time::SystemTime,
    nofollow: bool,
) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    use std::time::UNIX_EPOCH;
    let to_timespec = |t: std::time::SystemTime| -> libc::timespec {
        let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
        libc::timespec {
            tv_sec: d.as_secs() as libc::time_t,
            tv_nsec: d.subsec_nanos() as libc::c_long,
        }
    };
    let times = [to_timespec(accessed), to_timespec(modified)];
    let c = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let flags = if nofollow {
        libc::AT_SYMLINK_NOFOLLOW
    } else {
        0
    };
    let rc = unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), times.as_ptr(), flags) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

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

    #[test]
    fn infer_topdir_trash_dash_uid() {
        let root = PathBuf::from("/mnt/usb/.Trash-1000");
        assert_eq!(infer_topdir(&root).as_deref(), Some(Path::new("/mnt/usb")));
    }

    #[test]
    fn infer_topdir_shared_trash_uid() {
        let root = PathBuf::from("/mnt/usb/.Trash/1000");
        assert_eq!(infer_topdir(&root).as_deref(), Some(Path::new("/mnt/usb")));
    }

    #[test]
    fn infer_topdir_home_style_is_none() {
        assert!(infer_topdir(Path::new("/home/u/.local/share/Trash")).is_none());
        assert!(infer_topdir(Path::new("/tmp/xdg/Trash")).is_none());
    }

    #[test]
    fn resolve_dirs_pin_sets_topdir_for_volume_layout() {
        let root = std::env::temp_dir().join(format!(
            "rtrash-pin-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let pin = root.join("vol/.Trash-42");
        fs::create_dir_all(pin.join("files")).unwrap();
        fs::create_dir_all(pin.join("info")).unwrap();
        let dirs = resolve_dirs(std::slice::from_ref(&pin));
        assert_eq!(dirs.len(), 1);
        assert_eq!(dirs[0].root, pin);
        assert_eq!(dirs[0].topdir.as_deref(), Some(root.join("vol").as_path()));
        let abs = dirs[0].resolve_original(Path::new("docs/x.txt"));
        assert_eq!(abs, root.join("vol/docs/x.txt"));
        assert!(resolve_original_escape_rejected());
        let _ = fs::remove_dir_all(&root);
    }

    fn resolve_original_escape_rejected() -> bool {
        let td = TrashDir {
            root: PathBuf::from("/mnt/usb/.Trash-1"),
            topdir: Some(PathBuf::from("/mnt/usb")),
        };
        td.resolve_original_checked(Path::new("../../../etc/passwd"))
            .is_err()
    }

    #[test]
    fn longest_mount_prefix_prefers_nested_over_root() {
        // btrfs: / and /home share one st_dev; longest prefix must win.
        let mounts = vec![
            PathBuf::from("/"),
            PathBuf::from("/home"),
            PathBuf::from("/mnt/data"),
        ];
        assert_eq!(
            longest_mount_prefix(Path::new("/home/u/file"), &mounts),
            Path::new("/home")
        );
        assert_eq!(
            longest_mount_prefix(Path::new("/mnt/data/x"), &mounts),
            Path::new("/mnt/data")
        );
        assert_eq!(
            longest_mount_prefix(Path::new("/opt/x"), &mounts),
            Path::new("/")
        );
    }

    #[test]
    fn mount_points_from_table_includes_root_and_btrfs_subs() {
        let table = "\
/dev/sda1 / btrfs rw 0 0
/dev/sda1 /home btrfs rw,subvol=@home 0 0
/dev/sdb1 /mnt/usb ext4 rw 0 0
proc /proc proc rw 0 0
";
        let mps = mount_points_from_table(table);
        assert!(mps.iter().any(|p| p == Path::new("/")));
        assert!(mps.iter().any(|p| p == Path::new("/home")));
        assert!(mps.iter().any(|p| p == Path::new("/mnt/usb")));
        assert!(!mps.iter().any(|p| p == Path::new("/proc")));
    }

    #[test]
    fn copy_recursive_preserves_bytes_mode_mtime_symlink() {
        let root = std::env::temp_dir().join(format!(
            "rtrash-copy-{}-{}",
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
        fs::write(&src, b"hello-exdev").unwrap();
        let mode = fs::Permissions::from_mode(0o640);
        fs::set_permissions(&src, mode.clone()).unwrap();
        // Set a distinctive mtime (2001-01-01 UTC-ish).
        let old = std::time::UNIX_EPOCH + std::time::Duration::from_secs(978_307_200);
        {
            let f = fs::OpenOptions::new().write(true).open(&src).unwrap();
            let times = fs::FileTimes::new().set_modified(old).set_accessed(old);
            f.set_times(times).unwrap();
        }
        let meta = fs::symlink_metadata(&src).unwrap();
        copy_recursive(&src, &dst, &meta).unwrap();
        assert_eq!(fs::read(&dst).unwrap(), b"hello-exdev");
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

        let link = root.join("link");
        let link_dst = root.join("link-out");
        std::os::unix::fs::symlink("target-name", &link).unwrap();
        let lmeta = fs::symlink_metadata(&link).unwrap();
        copy_recursive(&link, &link_dst, &lmeta).unwrap();
        assert!(link_dst
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read_link(&link_dst).unwrap(), Path::new("target-name"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn trash_lock_is_exclusive() {
        let root = std::env::temp_dir().join(format!(
            "rtrash-lock-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let a = TrashLock::acquire(&root).unwrap();
        let b = TrashLock::try_acquire(&root);
        assert!(b.is_err(), "second exclusive lock must fail with LOCK_NB");
        drop(a);
        let c = TrashLock::try_acquire(&root);
        assert!(c.is_ok());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn trashinfo_sync_file_flushes() {
        let root = std::env::temp_dir().join(format!(
            "rtrash-sync-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let p = root.join("x.trashinfo");
        let mut f = fs::File::create(&p).unwrap();
        use std::io::Write;
        f.write_all(b"[Trash Info]\nPath=/a\nDeletionDate=2020-01-01T00:00:00\n")
            .unwrap();
        sync_file(&mut f).unwrap();
        drop(f);
        let body = fs::read_to_string(&p).unwrap();
        assert!(body.contains("Path=/a"));
        assert!(body.contains("[Trash Info]"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn discovery_all_sees_volume_trash_fixture() {
        // Structural: mount_points_from_table feeds longest_mount_prefix; all()
        // probes each mp for .Trash-uid. Here we only assert the table includes
        // / and an extra mount as required for trash-cli parity.
        let mps = mount_points_from_table(
            "/dev/a / btrfs rw 0 0\n/dev/a /home btrfs rw 0 0\n/dev/b /data ext4 rw 0 0\n",
        );
        assert!(mps.contains(&PathBuf::from("/")));
        assert!(mps.contains(&PathBuf::from("/home")));
        assert!(mps.contains(&PathBuf::from("/data")));
    }

    #[test]
    fn directorysizes_parse_line_ok_and_rejects_garbage() {
        let (sz, mt, enc) = directorysizes_parse_line("1234 1700000000 mydir").unwrap();
        assert_eq!(sz, 1234);
        assert_eq!(mt, 1_700_000_000);
        assert_eq!(enc, "mydir");
        assert!(directorysizes_parse_line("").is_none());
        assert!(directorysizes_parse_line("not-a-number 1 x").is_none());
        assert!(directorysizes_parse_line("1 2").is_none()); // missing name
    }

    #[test]
    fn directorysizes_cached_size_hit_and_stale_fallback() {
        let root = std::env::temp_dir().join(format!(
            "rtrash-ds-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("files/tree/nested")).unwrap();
        fs::create_dir_all(root.join("info")).unwrap();
        fs::write(root.join("files/tree/nested/blob"), vec![b'x'; 4096]).unwrap();
        let info_path = root.join("info/tree.trashinfo");
        fs::write(
            &info_path,
            "[Trash Info]\nPath=/orig/tree\nDeletionDate=2020-01-01T00:00:00\n",
        )
        .unwrap();
        let mtime = info_mtime_secs(&info_path).unwrap();
        let trash = TrashDir {
            root: root.clone(),
            topdir: None,
        };
        // Write a deliberately huge cached size with matching mtime.
        fs::write(
            root.join("directorysizes"),
            format!("999000000 {mtime} tree\n"),
        )
        .unwrap();
        assert_eq!(
            directorysizes_cached_size(&trash, "tree"),
            Some(999_000_000),
            "valid cache line must be used"
        );
        assert_eq!(
            entry_reclaim_bytes(&trash, "tree"),
            999_000_000 + crate::fastdelete::disk_usage(&info_path)
        );
        // Stale mtime → None → walk.
        fs::write(
            root.join("directorysizes"),
            format!("999000000 {} tree\n", mtime.saturating_sub(99)),
        )
        .unwrap();
        assert_eq!(directorysizes_cached_size(&trash, "tree"), None);
        let walked = entry_reclaim_bytes(&trash, "tree");
        assert!(
            walked < 999_000_000,
            "stale cache must fall back to walk, got {walked}"
        );
        // File entry must not use a directorysizes line even if forged.
        fs::write(root.join("files/onlyfile"), b"hi").unwrap();
        fs::write(
            root.join("info/onlyfile.trashinfo"),
            "[Trash Info]\nPath=/orig/onlyfile\nDeletionDate=2020-01-01T00:00:00\n",
        )
        .unwrap();
        let fmt = info_mtime_secs(&root.join("info/onlyfile.trashinfo")).unwrap();
        fs::write(
            root.join("directorysizes"),
            format!("888000000 {fmt} onlyfile\n"),
        )
        .unwrap();
        assert_eq!(
            directorysizes_cached_size(&trash, "onlyfile"),
            None,
            "file payloads must not use directorysizes"
        );
        let _ = fs::remove_dir_all(&root);
    }
}
