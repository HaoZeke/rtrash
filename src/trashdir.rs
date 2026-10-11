use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::info;
use crate::util::{join_info, join_name};

/// A `files/` + `info/` pair per the freedesktop.org Trash spec.
#[derive(Clone, Debug)]
pub struct TrashDir {
    /// Directory containing `files/` and `info/`.
    pub root: PathBuf,
    /// For per-mount trash dirs, the mount point; `Path=` values are relative to it.
    pub topdir: Option<PathBuf>,
}

impl TrashDir {
    /// A trash dir given by its root path (a `--trash-dir` pin), with the
    /// directories above it resolved the way [`abs_nofollow`] resolves an
    /// original location. A volume layout keeps its own `.Trash-$uid` or
    /// `.Trash/$uid` names and gets a resolved topdir, so relative `Path=`
    /// values match originals whose parents went through a symlink.
    pub fn from_root(root: &Path) -> io::Result<TrashDir> {
        let root = if root.is_absolute() {
            root.to_path_buf()
        } else {
            std::env::current_dir()?.join(root)
        };
        Ok(match infer_topdir(&root) {
            Some(top) => {
                let rest = root
                    .strip_prefix(&top)
                    .unwrap_or(Path::new(""))
                    .to_path_buf();
                let top = resolve_existing(&top);
                TrashDir {
                    root: top.join(rest),
                    topdir: Some(top),
                }
            }
            None => TrashDir {
                root: resolve_parent(&root),
                topdir: None,
            },
        })
    }

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

/// The home trash, `$XDG_DATA_HOME/Trash`, with symlinks above `Trash`
/// resolved. Original locations record resolved parents (see
/// [`abs_nofollow`]), so the trash root they are reported next to is
/// resolved the same way (on macOS `/var` is a symlink to `/private/var`).
pub fn home_trash() -> PathBuf {
    resolve_parent(&crate::platform::home_trash_from(
        std::env::var_os("XDG_DATA_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    ))
}

/// `path` with symlinks resolved in its longest existing ancestor; the
/// components that do not exist yet are appended as written.
pub fn resolve_existing(path: &Path) -> PathBuf {
    for ancestor in path.ancestors() {
        if ancestor.as_os_str().is_empty() {
            break;
        }
        if let Ok(real) = ancestor.canonicalize() {
            return match path.strip_prefix(ancestor) {
                Ok(rest) if !rest.as_os_str().is_empty() => real.join(rest),
                _ => real,
            };
        }
    }
    path.to_path_buf()
}

/// `path` with its parent resolved by [`resolve_existing`] and its last
/// component kept, so a symlinked trash root stays as named.
fn resolve_parent(path: &Path) -> PathBuf {
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) if !parent.as_os_str().is_empty() => {
            resolve_existing(parent).join(name)
        }
        _ => path.to_path_buf(),
    }
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

/// Why a `$topdir/.Trash` that exists cannot serve as the shared trash,
/// or `None` when it can.
fn shared_trash_problem(meta: &fs::Metadata) -> Option<&'static str> {
    if meta.file_type().is_symlink() {
        Some("is a symbolic link")
    } else if !meta.is_dir() {
        Some("is not a directory")
    } else if meta.permissions().mode() & 0o1000 == 0 {
        Some("lacks the sticky bit")
    } else {
        None
    }
}

/// Why a per-user volume trash (`$top/.Trash/$uid` or `$top/.Trash-$uid`)
/// that exists must not be used by `uid`, or `None` when it can. Only the
/// owner's own real directory will do: a directory another user made
/// there first, or a symlink, would hand them whatever gets trashed.
fn user_trash_problem(meta: &fs::Metadata, uid: u32) -> Option<&'static str> {
    if meta.file_type().is_symlink() {
        Some("is a symbolic link")
    } else if !meta.is_dir() {
        Some("is not a directory")
    } else if meta.uid() != uid {
        Some("belongs to another user")
    } else {
        None
    }
}

/// Warn at most once per path and process, so trashing or listing many
/// files from one mount prints one line.
fn warn_once(path: &Path, message: String) {
    static WARNED: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());
    let mut warned = WARNED.lock().unwrap_or_else(|e| e.into_inner());
    if warned.iter().any(|p| p == path) {
        return;
    }
    warned.push(path.to_path_buf());
    eprintln!("rtrash: warning: {message}");
}

/// Warn that `shared` cannot serve, naming the trash used instead.
fn warn_shared_trash_once(shared: &Path, why: &str, fallback: &Path) {
    warn_once(
        shared,
        format!(
            "'{}' {why}; using '{}'",
            shared.display(),
            fallback.display()
        ),
    );
}

/// Make the per-user volume trash at `root` ready for `uid`: create it
/// (mode 0700) when missing, then use it only when it is that user's own
/// real directory. Refusing returns an error and warns once.
fn ensure_user_trash(td: &TrashDir, uid: u32, fallback: &Path) -> io::Result<()> {
    match fs::DirBuilder::new().mode(0o700).create(&td.root) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let meta = fs::symlink_metadata(&td.root)?;
    if let Some(why) = user_trash_problem(&meta, uid) {
        warn_shared_trash_once(&td.root, why, fallback);
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, why));
    }
    td.ensure()
}

/// The trash `uid` uses on the volume mounted at `top`: `$top/.Trash/$uid`
/// when `$top/.Trash` is a sticky directory and not a symlink, else
/// `$top/.Trash-$uid`. Either must be the user's own real directory, made
/// here when missing. `None` sends the caller to the home trash.
fn select_on_volume(top: &Path, uid: u32) -> Option<TrashDir> {
    let own = top.join(format!(".Trash-{uid}"));
    let shared = top.join(".Trash");
    if let Ok(meta) = fs::symlink_metadata(&shared) {
        if sticky_and_dir(&meta) {
            let td = TrashDir {
                root: shared.join(uid.to_string()),
                topdir: Some(top.to_path_buf()),
            };
            if ensure_user_trash(&td, uid, &own).is_ok() {
                return Some(td);
            }
        } else if let Some(why) = shared_trash_problem(&meta) {
            // The spec asks for the failed check to be reported.
            warn_shared_trash_once(&shared, why, &own);
        }
    }
    let td = TrashDir {
        root: own,
        topdir: Some(top.to_path_buf()),
    };
    ensure_user_trash(&td, uid, &home_trash())
        .is_ok()
        .then_some(td)
}

/// The existing trash directories of `uid` on the volume mounted at `mp`,
/// under the rules [`select_on_volume`] puts by: anything else there is
/// skipped with a warning, so `list`, `empty` and `rm` never act through
/// a symlink or in a directory another user planted.
fn volume_trashes_of(mp: &Path, uid: u32) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let shared = mp.join(".Trash");
    let shared_ok = fs::symlink_metadata(&shared).is_ok_and(|m| sticky_and_dir(&m));
    let mut consider = |cand: PathBuf| {
        let Ok(meta) = fs::symlink_metadata(&cand) else {
            return;
        };
        match user_trash_problem(&meta, uid) {
            None => out.push(cand),
            Some(why) => warn_once(&cand, format!("skipping '{}', which {why}", cand.display())),
        }
    };
    if shared_ok {
        consider(shared.join(uid.to_string()));
    }
    consider(mp.join(format!(".Trash-{uid}")));
    out
}

/// Trash dir for a file on device `dev` at absolute path `abs`.
/// Home trash when on the same fs; otherwise `$top/.Trash/$uid` (must be a
/// sticky non-symlink dir) or `$top/.Trash-$uid`; home trash as last resort
/// (put then copies across devices).
///
/// Volume `top` is the **mount point** of `abs` (longest `/proc/self/mounts`
/// prefix on Linux), not a pure `st_dev` walk — required when btrfs subvolumes
/// share one device id.
///
/// **macOS (experimental):** always the FreeDesktop home trash under
/// `$XDG_DATA_HOME/Trash` or `~/.local/share/Trash`. This is **not** Finder Trash;
/// volume `.Trash-$uid` discovery is disabled for the experimental path.
pub fn select(abs: &Path, dev: u64) -> io::Result<TrashDir> {
    if crate::platform::is_macos_fdo_experimental() {
        let _ = (abs, dev);
        let home = TrashDir {
            root: home_trash(),
            topdir: None,
        };
        home.ensure()?;
        return Ok(home);
    }

    let home = TrashDir {
        root: home_trash(),
        topdir: None,
    };
    if dev_of_deepest_existing(&home.root) == Some(dev) {
        home.ensure()?;
        return Ok(home);
    }

    if let Some(td) = select_on_volume(&mount_top_of_path(abs), uid()) {
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
            let td = TrashDir::from_root(p).ok()?;
            if !is_valid_trash_root(&td.root) {
                eprintln!(
                    "rtrash: ignoring --trash-dir='{}' (need non-symlink files/ and info/ directories)",
                    p.display()
                );
                return None;
            }
            Some(td)
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
/// mounted volume’s existing user trash (trash-cli multi-volume default),
/// when it passes the checks a put would make ([`volume_trashes_of`]).
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
        for cand in volume_trashes_of(&mp, uid) {
            if seen.insert(cand.clone()) {
                out.push(TrashDir {
                    root: cand,
                    topdir: Some(mp.clone()),
                });
            }
        }
    }
    out
}

/// Every trash this process can read, for `list --all-users`: each
/// account's home trash at the default `~/.local/share/Trash` (another
/// user's `XDG_DATA_HOME` is unknowable), plus every `.Trash/$uid` and
/// `.Trash-$uid` on each mounted volume, plus this user's own trash. Root
/// reads them all; anyone else gets the ones that let them in. Nothing is
/// created or changed.
pub fn all_users() -> Vec<TrashDir> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    let mut push = |root: PathBuf, topdir: Option<PathBuf>| {
        if seen.insert(root.clone())
            && is_valid_trash_root(&root)
            && fs::read_dir(root.join("info")).is_ok()
        {
            out.push(TrashDir { root, topdir });
        }
    };
    for td in all() {
        push(td.root, td.topdir);
    }
    for home in user_homes() {
        push(resolve_parent(&home.join(".local/share/Trash")), None);
    }
    for mp in mount_points() {
        for root in volume_user_trashes(&mp) {
            push(root, Some(mp.clone()));
        }
    }
    out
}

/// The per-user trash roots on the volume mounted at `mp`: `.Trash/$uid`
/// and `.Trash-$uid` for any numeric uid.
pub fn volume_user_trashes(mp: &Path) -> Vec<PathBuf> {
    let numeric = |s: &std::ffi::OsStr| {
        let b = s.as_bytes();
        !b.is_empty() && b.iter().all(u8::is_ascii_digit)
    };
    let mut out = Vec::new();
    if let Ok(rd) = fs::read_dir(mp.join(".Trash")) {
        out.extend(
            rd.flatten()
                .filter(|e| numeric(&e.file_name()))
                .map(|e| e.path()),
        );
    }
    if let Ok(rd) = fs::read_dir(mp) {
        out.extend(
            rd.flatten()
                .filter(|e| {
                    let name = e.file_name();
                    name.as_bytes()
                        .strip_prefix(b".Trash-")
                        .is_some_and(|rest| numeric(std::ffi::OsStr::from_bytes(rest)))
                })
                .map(|e| e.path()),
        );
    }
    out.sort();
    out
}

/// Home directories from the user database (`getpwent`, so NSS sources
/// such as LDAP count too).
fn user_homes() -> Vec<PathBuf> {
    let mut out = Vec::new();
    unsafe {
        libc::setpwent();
        loop {
            let pw = libc::getpwent();
            if pw.is_null() {
                break;
            }
            let dir = (*pw).pw_dir;
            if !dir.is_null() {
                let home = std::ffi::CStr::from_ptr(dir).to_bytes();
                if home.first() == Some(&b'/') && home != b"/" {
                    out.push(PathBuf::from(std::ffi::OsStr::from_bytes(home)));
                }
            }
        }
        libc::endpwent();
    }
    out.sort();
    out.dedup();
    out
}

/// The account name owning `path`, or its uid when the user database has
/// no name for it.
pub fn owner_name(path: &Path) -> String {
    let Ok(meta) = fs::metadata(path) else {
        return "?".to_string();
    };
    uid_name(meta.uid())
}

/// The account name of `uid`, or the number when the user database has no
/// name for it.
pub fn uid_name(uid: u32) -> String {
    let mut buf = vec![0 as libc::c_char; 1024];
    let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
    let mut res: *mut libc::passwd = std::ptr::null_mut();
    let rc = unsafe { libc::getpwuid_r(uid, &mut pw, buf.as_mut_ptr(), buf.len(), &mut res) };
    if rc == 0 && !res.is_null() && !pw.pw_name.is_null() {
        let name = unsafe { std::ffi::CStr::from_ptr(pw.pw_name) };
        return name.to_string_lossy().into_owned();
    }
    uid.to_string()
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

/// Non-pseudo mount points for multi-volume FreeDesktop discovery.
///
/// - **Linux:** `/proc/self/mounts` (including `/`).
/// - **macOS experimental / other non-Linux Unix:** empty list so [`all`] stays
///   home-only; [`mount_top_of_path`] still falls back to an `st_dev` parent walk
///   when put needs a mount top on non-experimental builds.
pub fn mount_points() -> Vec<PathBuf> {
    if !crate::platform::volume_trash_discovery() {
        return Vec::new();
    }
    #[cfg(target_os = "linux")]
    {
        let Ok(content) = fs::read_to_string("/proc/self/mounts") else {
            return vec![PathBuf::from("/")];
        };
        mount_points_from_table(&content)
    }
    #[cfg(not(target_os = "linux"))]
    {
        // No /proc: home-only discovery; st_dev walk remains available for select.
        Vec::new()
    }
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
///
/// A trashed directory then gets its `directorysizes` line. The size walk
/// runs in parallel and outside the lock, so a big tree does not hold up
/// other puts and empties of the same root; the lock is taken again only
/// to write the line.
pub fn trash_move(abs: &Path, meta: &fs::Metadata, trash: &TrashDir) -> io::Result<String> {
    trash.ensure()?;
    let name = {
        let _lock = TrashLock::acquire(&trash.root)?;
        trash_move_locked(abs, meta, trash)?
    };
    if meta.is_dir() {
        record_dir_size(trash, &name);
    }
    Ok(name)
}

/// Write the `directorysizes` line for the trashed directory `name`.
/// Best effort: a cache that cannot be written is rebuilt by `status`.
pub fn record_dir_size(trash: &TrashDir, name: &str) {
    let dest = join_name(&trash.files(), name);
    let size = crate::fastdelete::disk_usage(&dest);
    let Ok(mtime) = info_mtime_secs(&join_info(&trash.info(), name)) else {
        return;
    };
    if let Ok(_lock) = TrashLock::acquire(&trash.root) {
        let _ = directorysizes_upsert(trash, name, size, mtime);
    }
}

/// Inner put path (caller holds the trash-root lock). Exposed for tests.
pub fn trash_move_locked(abs: &Path, meta: &fs::Metadata, trash: &TrashDir) -> io::Result<String> {
    // Keep the bytes of a name that is not UTF-8, as other tools do, so
    // two such names never fold into one replacement-character name.
    let base = crate::util::str_from_os_ref(abs.file_name().unwrap_or_default());

    let recorded: PathBuf = match &trash.topdir {
        Some(top) => abs
            .strip_prefix(top)
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|_| abs.to_path_buf()),
        None => abs.to_path_buf(),
    };
    let body = info::render_batch(&recorded, &info::now_local_string(), Some(info::batch_id()));

    let mut attempt: u64 = 0;
    let (name, mut info_file) = loop {
        let name = if attempt == 0 {
            base.clone()
        } else {
            format!("{base}.{}", attempt + 1)
        };
        // Skip names already occupied by an orphan payload (no .trashinfo).
        if join_name(&trash.files(), &name).symlink_metadata().is_ok() {
            attempt += 1;
            if attempt > 10_000 {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "could not reserve a unique trash name",
                ));
            }
            continue;
        }
        let info_path = join_info(&trash.info(), &name);
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

    let dest = join_name(&trash.files(), &name);
    // Ok(true) when the payload is a copy and the original still exists.
    let moved = match rename_noreplace(abs, &dest) {
        Ok(()) => Ok(false),
        Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
            // A directory inside that this user can't empty would stop the
            // removal halfway; refuse before copying, as a rename would.
            tree_removable(abs, meta).and_then(|()| copy_recursive(abs, &dest, meta).map(|()| true))
        }
        Err(e) => Err(e),
    };
    let copied = match moved {
        Ok(c) => c,
        Err(e) => {
            let _ = fs::remove_file(join_info(&trash.info(), &name));
            make_tree_writable(&dest);
            let _ = remove_any_path(&dest);
            return Err(e);
        }
    };
    if copied {
        // The copy is whole. If the original then can't go (a read-only
        // directory inside it), part of it may already be gone, so the
        // trash entry stays: dropping it would lose those files.
        if let Err(e) = remove_any(abs, meta) {
            return Err(io::Error::new(
                e.kind(),
                format!(
                    "copied into the trash as '{}', but removing the original failed: {e}",
                    crate::util::show_name(&name)
                ),
            ));
        }
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
/// `RENAME_NOREPLACE` (Linux); falls back to plain `rename` otherwise (macOS/BSD).
fn rename_noreplace(src: &Path, dest: &Path) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
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
        // Older kernels: fall back (orphan pre-check still applies).
        if err.raw_os_error() == Some(libc::EINVAL) || err.raw_os_error() == Some(libc::ENOSYS) {
            return fs::rename(src, dest);
        }
        Err(err)
    }
    #[cfg(not(target_os = "linux"))]
    {
        // macOS/BSD: no RENAME_NOREPLACE in this path; collision handled by
        // O_EXCL .trashinfo reservation before the payload rename.
        fs::rename(src, dest)
    }
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
    let enc = url_encode(crate::util::os_from_str(name).as_bytes());
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
    let enc = url_encode(crate::util::os_from_str(name).as_bytes());
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

/// The `directorysizes` cache of one trash root, read once. Sizes are
/// block usage as `du -B1` reports it ([`crate::fastdelete::disk_usage`]),
/// which the spec asks for.
///
/// Lookups that miss walk the payload and remember the result;
/// [`DirSizes::write_back`] then adds those lines and drops lines whose
/// directory is gone, the spec's suggested algorithm. Safe to share
/// between threads.
pub struct DirSizes<'a> {
    trash: &'a TrashDir,
    /// Percent-encoded name to (size, trashinfo mtime).
    lines: HashMap<String, (u64, i64)>,
    misses: std::sync::Mutex<Vec<(String, u64, i64)>>,
}

impl<'a> DirSizes<'a> {
    pub fn load(trash: &'a TrashDir) -> Self {
        let mut lines = HashMap::new();
        if let Ok(body) = fs::read_to_string(trash.root.join("directorysizes")) {
            for line in body.lines() {
                // A later duplicate wins, as a rewrite would keep it.
                if let Some((size, mtime, enc)) = directorysizes_parse_line(line) {
                    lines.insert(enc, (size, mtime));
                }
            }
        }
        DirSizes {
            trash,
            lines,
            misses: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// The cached size of directory payload `name`, when its line is still
    /// valid: the payload is a directory and the line's mtime equals the
    /// current `.trashinfo` mtime.
    pub fn cached(&self, name: &str) -> Option<u64> {
        let meta = fs::symlink_metadata(join_name(&self.trash.files(), name)).ok()?;
        if !meta.is_dir() {
            return None;
        }
        self.cached_dir(name)
    }

    /// [`Self::cached`] for a payload already known to be a directory.
    fn cached_dir(&self, name: &str) -> Option<u64> {
        let key = crate::util::url_encode(crate::util::os_from_str(name).as_bytes());
        let (size, mtime) = *self.lines.get(&key)?;
        let info = join_info(&self.trash.info(), name);
        (info_mtime_secs(&info).ok()? == mtime).then_some(size)
    }

    /// Bytes of the payload `files/NAME`: the cache for a directory with a
    /// valid line, otherwise a walk, remembered for [`Self::write_back`]
    /// when the payload is a directory with a `.trashinfo`.
    pub fn payload_bytes(&self, name: &str) -> u64 {
        let payload = join_name(&self.trash.files(), name);
        let Ok(meta) = fs::symlink_metadata(&payload) else {
            return 0;
        };
        if !meta.is_dir() {
            // One stat answers for a file, a symlink or a special file.
            return meta.blocks().saturating_mul(512);
        }
        if let Some(size) = self.cached_dir(name) {
            return size;
        }
        let size = crate::fastdelete::disk_usage(&payload);
        let info = join_info(&self.trash.info(), name);
        {
            if let Ok(mtime) = info_mtime_secs(&info) {
                self.misses.lock().unwrap_or_else(|e| e.into_inner()).push((
                    name.to_string(),
                    size,
                    mtime,
                ));
            }
        }
        size
    }

    /// Payload plus `.trashinfo` bytes for one entry.
    pub fn entry_bytes(&self, name: &str) -> u64 {
        let info = join_info(&self.trash.info(), name);
        let info_sz = fs::symlink_metadata(&info).map_or(0, |m| m.blocks().saturating_mul(512));
        self.payload_bytes(name).saturating_add(info_sz)
    }

    /// Write the sizes walked since [`Self::load`] into the cache, and drop
    /// lines whose payload is no longer a directory. Nothing is written when
    /// nothing changed. Best effort: a trash root this user cannot write
    /// keeps its old cache.
    pub fn write_back(self) {
        self.write_back_with(true);
    }

    /// [`Self::write_back`] for a caller that already holds the root's
    /// [`TrashLock`].
    pub fn write_back_locked(self) {
        self.write_back_with(false);
    }

    fn write_back_with(self, lock: bool) {
        let misses = self.misses.into_inner().unwrap_or_else(|e| e.into_inner());
        let files = self.trash.files();
        let gone = |enc: &str| {
            let name = crate::util::url_decode(enc);
            let name = std::ffi::OsStr::from_bytes(&name);
            !fs::symlink_metadata(files.join(name)).is_ok_and(|m| m.is_dir())
        };
        if misses.is_empty() && !self.lines.keys().any(|enc| gone(enc)) {
            return;
        }
        let _lock = if lock {
            match TrashLock::acquire(&self.trash.root) {
                Ok(l) => Some(l),
                Err(_) => return,
            }
        } else {
            None
        };
        // Re-read under the lock: a put may have added a line meanwhile.
        let path = self.trash.root.join("directorysizes");
        let mut kept: Vec<(String, u64, i64)> = fs::read_to_string(&path)
            .unwrap_or_default()
            .lines()
            .filter_map(directorysizes_parse_line)
            .map(|(size, mtime, enc)| (enc, size, mtime))
            .filter(|(enc, _, _)| !gone(enc))
            .collect();
        for (name, size, mtime) in misses {
            let enc = crate::util::url_encode(crate::util::os_from_str(&name).as_bytes());
            kept.retain(|(e, _, _)| *e != enc);
            kept.push((enc, size, mtime));
        }
        if kept.is_empty() {
            let _ = fs::remove_file(&path);
            return;
        }
        let body: String = kept
            .iter()
            .map(|(enc, size, mtime)| format!("{size} {mtime} {enc}\n"))
            .collect();
        let _ = atomic_write(&path, body.as_bytes());
    }
}

/// Cached directory payload size from `$trash/directorysizes` when the line
/// is still valid for `name` (see [`DirSizes::cached`]). Any doubt → `None`
/// so callers walk instead.
pub fn directorysizes_cached_size(trash: &TrashDir, name: &str) -> Option<u64> {
    DirSizes::load(trash).cached(name)
}

/// Approximate reclaimable bytes for one named trash entry (payload + its
/// `.trashinfo` when present). A directory payload uses a valid
/// `directorysizes` line; callers sizing many entries should hold one
/// [`DirSizes`] and write it back instead.
pub fn entry_reclaim_bytes(trash: &TrashDir, name: &str) -> u64 {
    DirSizes::load(trash).entry_bytes(name)
}

/// Reclaimable bytes for a path under `files/` (top-level name), using the
/// directorysizes cache when the payload is a directory with a valid line.
pub fn files_child_reclaim_bytes(trash: &TrashDir, path: &Path) -> u64 {
    match path.file_name() {
        Some(name) if path.parent() == Some(trash.files().as_path()) => {
            DirSizes::load(trash).payload_bytes(&crate::util::str_from_os_ref(name))
        }
        _ => crate::fastdelete::disk_usage(path),
    }
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

/// Ok when every directory under `path` lets this user remove entries.
fn tree_removable(path: &Path, meta: &fs::Metadata) -> io::Result<()> {
    if !meta.is_dir() {
        return Ok(());
    }
    let c = path_cstr(path)?;
    if unsafe { libc::access(c.as_ptr(), libc::W_OK | libc::X_OK) } != 0 {
        return Err(io::Error::last_os_error());
    }
    for e in fs::read_dir(path)? {
        let e = e?;
        if e.file_type()?.is_dir() {
            tree_removable(&e.path(), &e.metadata()?)?;
        }
    }
    Ok(())
}

/// Give the owner write access to every directory under `path`, so a
/// half-made copy of a read-only tree can be removed again.
fn make_tree_writable(path: &Path) {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return;
    };
    if !meta.is_dir() {
        return;
    }
    let mode = meta.permissions().mode();
    if mode & 0o700 != 0o700 {
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode | 0o700));
    }
    if let Ok(rd) = fs::read_dir(path) {
        for e in rd.flatten() {
            make_tree_writable(&e.path());
        }
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
            tree_removable(src, &meta)?;
            copy_recursive(src, dest, &meta).and_then(|_| remove_any(src, &meta))
        }
        Err(e) => Err(e),
    }
}

/// Cross-device fallback, shared by put (EXDEV into trash) and restore
/// ([`relocate`]). It keeps what a rename would have kept, as far as the
/// target file system allows:
///
/// - bytes, holes in sparse files, and a reflink when both sides share a
///   btrfs or XFS file system (a rename across btrfs subvolumes fails with
///   EXDEV, yet a clone works there)
/// - mode, owner (best effort), times and extended attributes, which carry
///   ACLs, file capabilities and macOS Finder data
/// - symlinks as links, named pipes as pipes, and device nodes when the
///   caller may create them
/// - hard links inside the tree, as links to one copy
///
/// A socket can't be copied, so moving one fails and leaves the source in
/// place. A directory gets its mode only after its children, so a
/// read-only tree copies too.
pub fn copy_recursive(src: &Path, dst: &Path, meta: &fs::Metadata) -> io::Result<()> {
    copy_node(src, dst, meta, &mut HashMap::new())
}

/// Copies made so far of files with more than one link, by (device, inode).
type LinkMap = HashMap<(u64, u64), PathBuf>;

fn copy_node(src: &Path, dst: &Path, meta: &fs::Metadata, links: &mut LinkMap) -> io::Result<()> {
    use std::os::unix::fs::FileTypeExt;
    let ftype = meta.file_type();
    if ftype.is_symlink() {
        let target = fs::read_link(src)?;
        std::os::unix::fs::symlink(&target, dst)?;
        let _ = std::os::unix::fs::lchown(dst, Some(meta.uid()), Some(meta.gid()));
        copy_xattrs(src, dst);
        // Symlink mtime: best-effort via utimensat AT_SYMLINK_NOFOLLOW.
        let _ = set_times_nofollow(dst, meta);
        return Ok(());
    }
    if ftype.is_dir() {
        fs::create_dir(dst)?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            let emeta = fs::symlink_metadata(entry.path())?;
            copy_node(&entry.path(), &dst.join(entry.file_name()), &emeta, links)?;
        }
        finish_meta(src, dst, meta)?;
        return set_times(dst, meta);
    }
    if ftype.is_socket() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "cannot copy a socket to another file system",
        ));
    }
    if meta.nlink() > 1 {
        let key = (meta.dev(), meta.ino());
        if let Some(first) = links.get(&key) {
            return fs::hard_link(first, dst);
        }
        links.insert(key, dst.to_path_buf());
    }
    if ftype.is_fifo() || ftype.is_char_device() || ftype.is_block_device() {
        make_node(dst, meta)?;
    } else {
        copy_file_data(src, dst, meta)?;
    }
    finish_meta(src, dst, meta)?;
    set_times(dst, meta)
}

/// Owner, extended attributes and mode, in that order: a chown clears
/// set-id bits, and the mode then trims an ACL mask the way chmod does.
fn finish_meta(src: &Path, dst: &Path, meta: &fs::Metadata) -> io::Result<()> {
    let _ = std::os::unix::fs::lchown(dst, Some(meta.uid()), Some(meta.gid()));
    copy_xattrs(src, dst);
    fs::set_permissions(dst, meta.permissions())
}

/// A named pipe or device node like `meta`. Device nodes need privilege,
/// so an unprivileged move of one fails and leaves the source alone.
fn make_node(dst: &Path, meta: &fs::Metadata) -> io::Result<()> {
    use std::os::unix::fs::FileTypeExt;
    let c = path_cstr(dst)?;
    let rc = if meta.file_type().is_fifo() {
        unsafe { libc::mkfifo(c.as_ptr(), 0o600) }
    } else {
        unsafe { libc::mknod(c.as_ptr(), meta.mode() as libc::mode_t, meta.rdev() as _) }
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn path_cstr(p: &Path) -> io::Result<std::ffi::CString> {
    std::ffi::CString::new(p.as_os_str().as_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))
}

/// The bytes of a regular file. Linux tries a reflink, then copies only
/// the data runs of a sparse file, then falls back to the kernel copy
/// (`copy_file_range` or `sendfile`) that `io::copy` uses. Elsewhere
/// `fs::copy` already clones where it can (macOS `fclonefileat`).
#[cfg(any(target_os = "linux", target_os = "android"))]
fn copy_file_data(src: &Path, dst: &Path, meta: &fs::Metadata) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;
    let mut from = fs::File::open(src)?;
    let mut to = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(dst)?;
    if unsafe { libc::ioctl(to.as_raw_fd(), libc::FICLONE, from.as_raw_fd()) } == 0 {
        return Ok(());
    }
    let len = meta.len();
    if meta.blocks().saturating_mul(512) < len {
        if copy_sparse(&from, &to, len).is_ok() {
            return Ok(());
        }
        // A failed sparse copy may have written part of the file: start
        // over. Only here: truncating on ext4 makes close flush the file.
        to.set_len(0)?;
        // SEEK_DATA moved the read offset; pwrite left the write one at 0.
        io::Seek::rewind(&mut from)?;
    }
    io::copy(&mut from, &mut to)?;
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn copy_file_data(src: &Path, dst: &Path, _meta: &fs::Metadata) -> io::Result<()> {
    fs::copy(src, dst).map(|_| ())
}

/// Copy the data runs SEEK_DATA and SEEK_HOLE report, leave the holes
/// unwritten and set the length, so a mostly empty disk image stays small.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn copy_sparse(from: &fs::File, to: &fs::File, len: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    use std::os::unix::io::AsRawFd;
    let fd_in = from.as_raw_fd();
    let mut buf = vec![0u8; 1 << 20];
    let mut pos: u64 = 0;
    while pos < len {
        let data = unsafe { libc::lseek(fd_in, pos as libc::off_t, libc::SEEK_DATA) };
        if data < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::ENXIO) {
                break; // only a hole is left
            }
            return Err(e);
        }
        let data = data as u64;
        let hole = unsafe { libc::lseek(fd_in, data as libc::off_t, libc::SEEK_HOLE) };
        if hole < 0 {
            return Err(io::Error::last_os_error());
        }
        let hole = (hole as u64).min(len);
        let mut at = data;
        while at < hole {
            let want = ((hole - at) as usize).min(buf.len());
            let n = from.read_at(&mut buf[..want], at)?;
            if n == 0 {
                break; // the file shrank under us
            }
            to.write_all_at(&buf[..n], at)?;
            at += n as u64;
        }
        pos = hole.max(data + 1);
    }
    to.set_len(len)
}

/// Extended attributes, best effort: a target file system without them,
/// or a `security.*` name this user may not set, drops that attribute.
#[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
fn copy_xattrs(src: &Path, dst: &Path) {
    let (Ok(s), Ok(d)) = (path_cstr(src), path_cstr(dst)) else {
        return;
    };
    let Some(names) = xattr_list(&s) else { return };
    for name in names.split(|&b| b == 0).filter(|n| !n.is_empty()) {
        let Ok(name) = std::ffi::CString::new(name) else {
            continue;
        };
        if let Some(value) = xattr_get(&s, &name) {
            xattr_set(&d, &name, &value);
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
fn copy_xattrs(_src: &Path, _dst: &Path) {}

/// Call `f` with a growing buffer until the value fits; `f(null, 0)` sizes it.
#[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
fn xattr_buf(f: impl Fn(*mut libc::c_void, usize) -> isize) -> Option<Vec<u8>> {
    for _ in 0..4 {
        let n = f(std::ptr::null_mut(), 0);
        if n <= 0 {
            return (n == 0).then(Vec::new);
        }
        let mut buf = vec![0u8; n as usize];
        let got = f(buf.as_mut_ptr().cast(), buf.len());
        if got >= 0 {
            buf.truncate(got as usize);
            return Some(buf);
        }
        if io::Error::last_os_error().raw_os_error() != Some(libc::ERANGE) {
            return None;
        }
    }
    None
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn xattr_list(p: &std::ffi::CStr) -> Option<Vec<u8>> {
    xattr_buf(|b, n| unsafe { libc::llistxattr(p.as_ptr(), b.cast(), n) })
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn xattr_get(p: &std::ffi::CStr, name: &std::ffi::CStr) -> Option<Vec<u8>> {
    xattr_buf(|b, n| unsafe { libc::lgetxattr(p.as_ptr(), name.as_ptr(), b, n) })
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn xattr_set(p: &std::ffi::CStr, name: &std::ffi::CStr, v: &[u8]) {
    unsafe { libc::lsetxattr(p.as_ptr(), name.as_ptr(), v.as_ptr().cast(), v.len(), 0) };
}

#[cfg(target_os = "macos")]
fn xattr_list(p: &std::ffi::CStr) -> Option<Vec<u8>> {
    xattr_buf(|b, n| unsafe { libc::listxattr(p.as_ptr(), b.cast(), n, libc::XATTR_NOFOLLOW) })
}

#[cfg(target_os = "macos")]
fn xattr_get(p: &std::ffi::CStr, name: &std::ffi::CStr) -> Option<Vec<u8>> {
    xattr_buf(|b, n| unsafe {
        libc::getxattr(p.as_ptr(), name.as_ptr(), b, n, 0, libc::XATTR_NOFOLLOW)
    })
}

#[cfg(target_os = "macos")]
fn xattr_set(p: &std::ffi::CStr, name: &std::ffi::CStr, v: &[u8]) {
    unsafe {
        libc::setxattr(
            p.as_ptr(),
            name.as_ptr(),
            v.as_ptr().cast(),
            v.len(),
            0,
            libc::XATTR_NOFOLLOW,
        )
    };
}

fn set_times(path: &Path, meta: &fs::Metadata) -> io::Result<()> {
    let modified = meta.modified()?;
    let accessed = meta.accessed().unwrap_or(modified);
    // Opening a pipe for writing would wait for a reader: only regular
    // files go through a file handle.
    if !meta.is_file() {
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
            // `as _` takes the field's own width: libc deprecates naming
            // `time_t` on musl, where it widens to 64 bits in musl 1.2.
            tv_sec: d.as_secs() as _,
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
    fn volume_trash_must_be_the_users_own_real_directory() {
        let top = std::env::temp_dir().join(format!("rtrash-voltop-{}", std::process::id()));
        let _ = fs::remove_dir_all(&top);
        fs::create_dir_all(&top).unwrap();
        let me = uid();
        let own = top.join(format!(".Trash-{me}"));
        // Missing: made as `.Trash-$uid`, mode 0700, when there is no
        // shared `.Trash`.
        let td = select_on_volume(&top, me).unwrap();
        assert_eq!(td.root, own);
        assert_eq!(
            fs::metadata(&own).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert!(td.files().is_dir() && td.info().is_dir());
        assert_eq!(volume_trashes_of(&top, me), std::slice::from_ref(&own));
        // A directory that belongs to someone else: the scan as uid + 1
        // finds the one it makes owned by this user instead.
        assert!(select_on_volume(&top, me + 1).is_none());
        assert!(volume_trashes_of(&top, me + 1).is_empty());
        // A symlink is refused even when it leads to a directory of ours.
        fs::remove_dir_all(&own).unwrap();
        let elsewhere = top.join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &own).unwrap();
        assert!(select_on_volume(&top, me).is_none());
        assert!(volume_trashes_of(&top, me).is_empty());
        assert!(!elsewhere.join("files").exists());
        // A sticky shared `.Trash` gets the uid directory inside it.
        fs::remove_file(&own).unwrap();
        let shared = top.join(".Trash");
        fs::create_dir(&shared).unwrap();
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o1777)).unwrap();
        let td = select_on_volume(&top, me).unwrap();
        assert_eq!(td.root, shared.join(me.to_string()));
        assert_eq!(volume_trashes_of(&top, me), std::slice::from_ref(&td.root));
        // Without the sticky bit the shared one is neither used nor listed.
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(volume_trashes_of(&top, me).is_empty());
        assert_eq!(select_on_volume(&top, me).unwrap().root, own);
        fs::remove_dir_all(&top).unwrap();
    }

    #[test]
    fn shared_trash_problem_names_each_failed_check() {
        let root = std::env::temp_dir().join(format!("rtrash-shared-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let plain = root.join("plain");
        fs::create_dir(&plain).unwrap();
        fs::set_permissions(&plain, fs::Permissions::from_mode(0o777)).unwrap();
        let sticky = root.join("sticky");
        fs::create_dir(&sticky).unwrap();
        fs::set_permissions(&sticky, fs::Permissions::from_mode(0o1777)).unwrap();
        let file = root.join("file");
        fs::write(&file, b"x").unwrap();
        let link = root.join("link");
        std::os::unix::fs::symlink(&sticky, &link).unwrap();
        let check = |p: &Path| shared_trash_problem(&fs::symlink_metadata(p).unwrap());
        assert_eq!(check(&plain), Some("lacks the sticky bit"));
        assert_eq!(check(&sticky), None);
        assert_eq!(check(&file), Some("is not a directory"));
        assert_eq!(check(&link), Some("is a symbolic link"));
        let _ = fs::remove_dir_all(&root);
    }
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
        fs::create_dir_all(&root).unwrap();
        // Pins come back resolved (macOS reaches temp_dir through /var).
        let root = fs::canonicalize(&root).unwrap();
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

    #[test]
    fn resolve_existing_and_from_root_follow_symlinked_parents() {
        let base = std::env::temp_dir().join(format!(
            "rtrash-resolve-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(base.join("real/vol")).unwrap();
        let real = fs::canonicalize(base.join("real")).unwrap();
        let link = base.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        // The existing part resolves; the missing tail stays as written.
        assert_eq!(
            resolve_existing(&link.join("vol/new/x")),
            real.join("vol/new/x")
        );

        // A home-style root resolves its parent but keeps its own name.
        let td = TrashDir::from_root(&link.join("xdg/Trash")).unwrap();
        assert_eq!(td.root, real.join("xdg/Trash"));
        assert!(td.topdir.is_none());

        // Volume roots resolve the topdir, so Path= stays topdir-relative.
        for rest in [".Trash-7", ".Trash/7"] {
            let td = TrashDir::from_root(&link.join("vol").join(rest)).unwrap();
            assert_eq!(td.topdir.as_deref(), Some(real.join("vol").as_path()));
            assert_eq!(td.root, real.join("vol").join(rest));
        }
        let _ = fs::remove_dir_all(&base);
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

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    #[test]
    fn copy_recursive_keeps_pipes_links_holes_xattrs_and_read_only_dirs() {
        use std::os::unix::fs::FileTypeExt;
        let root = std::env::temp_dir().join(format!("rtrash-copy2-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let src = root.join("src");
        fs::create_dir_all(src.join("ro")).unwrap();
        // A named pipe: fs::copy used to block on it forever.
        let fifo = path_cstr(&src.join("pipe")).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) }, 0);
        // Two names for one file, one of them in a subdirectory.
        fs::write(src.join("a"), b"shared").unwrap();
        fs::hard_link(src.join("a"), src.join("ro/b")).unwrap();
        // A sparse file: 8 MiB with 4 KiB of data at the end.
        let sparse = src.join("sparse");
        {
            use std::os::unix::fs::FileExt;
            let f = fs::File::create(&sparse).unwrap();
            f.set_len(8 << 20).unwrap();
            f.write_all_at(&[7u8; 4096], (8 << 20) - 4096).unwrap();
        }
        // A user xattr, when this file system takes one.
        let xname = std::ffi::CString::new("user.rtrash.test").unwrap();
        let src_c = path_cstr(&src.join("a")).unwrap();
        xattr_set(&src_c, &xname, b"v1");
        let has_xattr = xattr_get(&src_c, &xname).as_deref() == Some(&b"v1"[..]);
        // Read-only directory with a child: its mode lands after the child.
        fs::set_permissions(src.join("ro"), fs::Permissions::from_mode(0o555)).unwrap();

        let dst = root.join("dst");
        let meta = fs::symlink_metadata(&src).unwrap();
        copy_recursive(&src, &dst, &meta).unwrap();

        assert!(fs::symlink_metadata(dst.join("pipe"))
            .unwrap()
            .file_type()
            .is_fifo());
        assert_eq!(
            fs::symlink_metadata(dst.join("pipe")).unwrap().mode() & 0o777,
            0o644
        );
        let (a, b) = (
            fs::metadata(dst.join("a")).unwrap(),
            fs::metadata(dst.join("ro/b")).unwrap(),
        );
        assert_eq!(a.ino(), b.ino(), "hard links stay one file");
        assert_eq!(a.nlink(), 2);
        assert_eq!(fs::read(dst.join("ro/b")).unwrap(), b"shared");
        assert_eq!(fs::metadata(dst.join("ro")).unwrap().mode() & 0o777, 0o555);
        let (sm, dm) = (
            fs::metadata(&sparse).unwrap(),
            fs::metadata(dst.join("sparse")).unwrap(),
        );
        assert_eq!(dm.len(), 8 << 20);
        assert_eq!(
            fs::read(dst.join("sparse")).unwrap(),
            fs::read(&sparse).unwrap()
        );
        // Elsewhere fs::copy decides; Linux copies data runs itself.
        if cfg!(target_os = "linux") && sm.blocks() * 512 < sm.len() {
            assert!(
                dm.blocks() <= sm.blocks() + 128,
                "holes kept: {} vs {} blocks",
                dm.blocks(),
                sm.blocks()
            );
        }
        if has_xattr {
            let dst_c = path_cstr(&dst.join("a")).unwrap();
            assert_eq!(xattr_get(&dst_c, &xname).as_deref(), Some(&b"v1"[..]));
        }
        fs::set_permissions(src.join("ro"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(dst.join("ro"), fs::Permissions::from_mode(0o755)).unwrap();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn volume_user_trashes_finds_numeric_uid_dirs_only() {
        let mp = std::env::temp_dir().join(format!("rtrash-vol-{}", std::process::id()));
        let _ = fs::remove_dir_all(&mp);
        for d in [
            ".Trash/1000",
            ".Trash/abc",
            ".Trash-1001",
            ".Trash-x",
            ".Trash-",
            "other",
        ] {
            fs::create_dir_all(mp.join(d)).unwrap();
        }
        assert_eq!(
            volume_user_trashes(&mp),
            [mp.join(".Trash/1000"), mp.join(".Trash-1001")]
        );
        assert!(!owner_name(&mp).is_empty());
        let _ = fs::remove_dir_all(&mp);
    }

    #[test]
    fn tree_removable_spots_a_read_only_directory() {
        let root = std::env::temp_dir().join(format!("rtrash-ro-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("t/ro")).unwrap();
        fs::write(root.join("t/ro/f"), "x").unwrap();
        let t = root.join("t");
        assert!(tree_removable(&t, &fs::metadata(&t).unwrap()).is_ok());
        fs::set_permissions(root.join("t/ro"), fs::Permissions::from_mode(0o555)).unwrap();
        // Root may write anywhere, so only an ordinary user sees the refusal.
        if unsafe { libc::geteuid() } != 0 {
            let err = tree_removable(&t, &fs::metadata(&t).unwrap()).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        }
        make_tree_writable(&t);
        assert_eq!(
            fs::metadata(root.join("t/ro")).unwrap().mode() & 0o700,
            0o700
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn copy_recursive_refuses_a_socket() {
        let root = std::env::temp_dir().join(format!("rtrash-copy3-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let sock = root.join("s");
        let _l = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let meta = fs::symlink_metadata(&sock).unwrap();
        let err = copy_recursive(&sock, &root.join("out"), &meta).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
        assert!(!root.join("out").exists());
        let _ = fs::remove_dir_all(&root);
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
