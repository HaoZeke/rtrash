use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
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

    /// Make `files/` and `info/` (mode 0700) when missing. Either one
    /// that is a symbolic link is refused, as trash-cli 0.26.9 refuses
    /// it: a put through it would land wherever the link points, and a
    /// `--trash-dir` pin of such a trash is refused by list and empty.
    pub fn ensure(&self) -> io::Result<()> {
        for d in [self.files(), self.info()] {
            if fs::symlink_metadata(&d).is_ok_and(|m| m.file_type().is_symlink()) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "'{}' is a symbolic link; a trash takes only a real folder there",
                        d.display()
                    ),
                ));
            }
        }
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

    /// Like [`resolve_original`] but returns an error when a volume trash's
    /// `Path=` could lead out of its volume: a relative one with any `..`,
    /// or an absolute one with `..` or outside the topdir.
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
            // A volume trash holds what came from its volume, so an
            // absolute Path= must stay inside it, as trash-cli 0.26.9
            // requires: a stick that names `/home/u/.bashrc` restores
            // nothing there. On the root volume every path is inside.
            (Some(top), true) => {
                let climbs = recorded
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir));
                if climbs || !recorded.starts_with(top) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "trash Path= is absolute and outside the volume topdir",
                    ));
                }
                Ok(recorded.to_path_buf())
            }
            (None, _) => Ok(recorded.to_path_buf()),
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

/// How a path meets a trash directory: put refuses to trash such a path,
/// and restore refuses to write to one.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Overlap {
    /// The path is the trash directory or contains it.
    Contains,
    /// The path is inside the trash directory.
    Inside,
}

/// Whether `abs` (absolute, its parent resolved) is, contains or sits inside
/// the trash at `root`, compared both as named and with the trash's
/// symlinks resolved. A `Trash` that is a symlink (glib #1640) is the trash
/// by either name: trashing the link would move it into the trash it
/// points at, and the next put would make a new, empty home trash.
pub(crate) fn trash_overlap(abs: &Path, root: &Path) -> Option<Overlap> {
    let real = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    if real.starts_with(abs) || root.starts_with(abs) {
        Some(Overlap::Contains)
    } else if abs.starts_with(&real) || abs.starts_with(root) {
        Some(Overlap::Inside)
    } else {
        None
    }
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

/// The device the home trash at `root` keeps its items on: where `root`
/// leads when it exists, so a `Trash` symlinked to another disk counts as
/// that disk (glib #1640), else the deepest existing ancestor's.
fn home_dev(root: &Path) -> Option<u64> {
    fs::metadata(root)
        .ok()
        .map(|m| m.dev())
        .or_else(|| dev_of_deepest_existing(root))
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

/// The volume `path` is on, for a trash beside it: on Linux the deepest
/// mount point above it among every mount (btrfs-safe when many
/// subvolumes share one `st_dev`), or `None` when that mount is of a type
/// discovery skips; see [`volume_top_from_table`]. Elsewhere, or with no
/// volume discovery, an `st_dev` parent walk.
pub fn volume_top_of_path(path: &Path) -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        if crate::platform::volume_trash_discovery() {
            return match fs::read_to_string("/proc/self/mounts") {
                Ok(table) => volume_top_from_table(path, &table),
                Err(_) => Some(PathBuf::from("/")),
            };
        }
    }
    let dev = dev_of(path).or_else(|| dev_of_deepest_existing(path));
    Some(match dev {
        Some(d) => mount_top_of_by_dev(path, d),
        None => PathBuf::from("/"),
    })
}

/// The volume `path` is on, from a `/proc/self/mounts`-style table: the
/// deepest mount point above it, whatever its type, or `None` when the
/// mount in view there is one [`is_pseudo_fs`] skips. Such a mount must
/// not hand its files to the trash of the mount around it, which is
/// another device (a document portal under `/run/user/$uid`, say); `None`
/// sends them to the home trash.
pub fn volume_top_from_table(path: &Path, content: &str) -> Option<PathBuf> {
    let table = mount_table(content);
    let mounts: Vec<PathBuf> = table.iter().map(|(m, _)| m.clone()).collect();
    let top = longest_mount_prefix(path, &mounts);
    match table.iter().find(|(m, _)| *m == top) {
        Some((_, fstype)) if is_pseudo_fs(fstype) => None,
        _ => Some(top),
    }
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
/// there first, or a symlink, would hand them whatever gets trashed. One
/// that group or others can write to is refused too, since anyone with
/// write access could swap its `files/` or `info/` for a symlink of their
/// own (trash-cli #433).
fn user_trash_problem(meta: &fs::Metadata, uid: u32) -> Option<&'static str> {
    if meta.file_type().is_symlink() {
        Some("is a symbolic link")
    } else if !meta.is_dir() {
        Some("is not a directory")
    } else if meta.uid() != uid {
        Some("belongs to another user")
    } else if meta.permissions().mode() & 0o022 != 0 {
        Some("is writable by other users")
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
    // A fresh partition owned by root, or a trash made read-only, would
    // otherwise send items to the fallback (a copy across file systems
    // for the home trash) without a word.
    let say = |e: &io::Error| {
        warn_shared_trash_once(&td.root, &format!("cannot be used ({e})"), fallback);
    };
    match fs::DirBuilder::new().mode(0o700).create(&td.root) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => {
            say(&e);
            return Err(e);
        }
    }
    let meta = fs::symlink_metadata(&td.root)?;
    if let Some(why) = user_trash_problem(&meta, uid) {
        warn_shared_trash_once(&td.root, why, fallback);
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, why));
    }
    td.ensure().inspect_err(say)?;
    // files/ and info/ that exist but refuse writes (made read-only by
    // hand, or a read-only remount) would fail each item later; fall back
    // now, once, with the reason.
    for d in [td.files(), td.info()] {
        if !writable(&d) {
            let e = io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("'{}' is not writable", d.display()),
            );
            say(&e);
            return Err(e);
        }
    }
    Ok(())
}

/// Make the home trash ready and check that `files/` and `info/` take
/// writes. The home trash has no fallback, so a read-only one fails here
/// with one clear reason instead of a failure for each item.
pub fn ensure_home(td: &TrashDir) -> io::Result<()> {
    td.ensure()?;
    for d in [td.files(), td.info()] {
        if !writable(&d) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "the home trash is not writable: '{}' refuses new entries",
                    d.display()
                ),
            ));
        }
    }
    Ok(())
}

/// True when this process may create entries in `dir`.
fn writable(dir: &Path) -> bool {
    let Ok(c) = CString::new(dir.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `c` is a valid NUL-terminated path for the call's duration.
    unsafe { libc::access(c.as_ptr(), libc::W_OK | libc::X_OK) == 0 }
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
        ensure_home(&home)?;
        return Ok(home);
    }

    let home = TrashDir {
        root: home_trash(),
        topdir: None,
    };
    if home_dev(&home.root) == Some(dev) {
        ensure_home(&home)?;
        return Ok(home);
    }

    if let Some(td) = volume_top_of_path(abs).and_then(|top| select_on_volume(&top, uid())) {
        return Ok(td);
    }
    ensure_home(&home)?;
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
    // A trash pinned twice, however spelled, is one trash: a command that
    // locks every root at once would otherwise wait on its own lock.
    let mut seen = HashSet::new();
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
        .filter(|td| match fs::metadata(&td.root) {
            Ok(m) => seen.insert((m.dev(), m.ino())),
            Err(_) => true,
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
        // A dead network share must not hang the scan (glib #605).
        let found = crate::mount_probe::probe(&mp, move |mp| volume_trashes_of(mp, uid));
        for cand in found.unwrap_or_default() {
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
        // Homes on a network share answer within the probe's wait or not
        // at all (glib #605).
        let trash = home.join(".local/share/Trash");
        if let Some(root) = crate::mount_probe::probe(&trash, resolve_parent) {
            push(root, None);
        }
    }
    for mp in mount_points() {
        let found = crate::mount_probe::probe(&mp, volume_user_trashes);
        for root in found.unwrap_or_default() {
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

/// Identity of a payload: device, inode and change time, so a new item
/// that got the inode and name of a removed one still differs.
type PayloadId = (u64, u64, i64, i64);

fn payload_id(path: &Path) -> Option<PayloadId> {
    let m = fs::symlink_metadata(path).ok()?;
    Some((m.dev(), m.ino(), m.ctime(), m.ctime_nsec()))
}

/// What a command counted in one trash before it took the lock: each
/// payload in files/ with its identity, and each name in info/. Under the
/// lock it acts only on what it counted, so an item put after the count,
/// or after the user said yes to it, stays (round 245).
pub struct Seen {
    dir: TrashDir,
    files: HashMap<OsString, PayloadId>,
    info: HashSet<OsString>,
}

impl Seen {
    /// Count `dir` now.
    pub fn take(dir: &TrashDir) -> Seen {
        let files_dir = dir.files();
        let files = fs::read_dir(&files_dir)
            .map(|r| {
                r.flatten()
                    .filter_map(|e| Some((e.file_name(), payload_id(&e.path())?)))
                    .collect()
            })
            .unwrap_or_default();
        let info = fs::read_dir(dir.info())
            .map(|r| r.flatten().map(|e| e.file_name()).collect())
            .unwrap_or_default();
        Seen {
            dir: dir.clone(),
            files,
            info,
        }
    }

    /// The trash that was counted.
    pub fn dir(&self) -> &TrashDir {
        &self.dir
    }

    /// How many payloads the count found, which is the item count an
    /// empty reports.
    pub fn items(&self) -> u64 {
        self.files.len() as u64
    }

    /// The payload `name` in files/ is the one counted.
    pub fn saw_payload(&self, name: &OsStr) -> bool {
        self.files
            .get(name)
            .is_some_and(|id| payload_id(&self.dir.files().join(name)) == Some(*id))
    }

    /// The item `name` is as counted: its payload is the counted one, or
    /// there was none then and is none now; and a record it has now was
    /// counted too.
    pub fn saw_item(&self, name: &OsStr) -> bool {
        if payload_id(&self.dir.files().join(name)) != self.files.get(name).copied() {
            return false;
        }
        let mut rec = name.to_os_string();
        rec.push(".trashinfo");
        self.info.contains(&rec) || crate::util::is_gone(&self.dir.info().join(&rec))
    }

    /// The name `rec` in info/ was counted. The record of a payload that
    /// came later may have a counted name; check the payload for that.
    pub fn saw_record(&self, rec: &OsStr) -> bool {
        self.info.contains(rec)
    }
}

/// Exclusive per-trash-root lock (flock on `root/.rtrash.lock`) so put and empty
/// cannot interleave a half-written pair under the same root.
///
/// flock is per open file, so a thread that takes a lock it already holds
/// waits for itself forever. A debug build panics instead, naming where
/// the lock was first taken, so a test fails at once rather than hanging
/// until the runner kills it.
pub struct TrashLock {
    _file: fs::File,
    #[cfg(debug_assertions)]
    _held: held::Mark,
}

impl TrashLock {
    /// Blocking exclusive lock for the critical section of put/empty.
    #[track_caller]
    pub fn acquire(root: &Path) -> io::Result<Self> {
        Self::acquire_flags(root, libc::LOCK_EX, std::panic::Location::caller())
    }

    /// Non-blocking attempt (tests). It fails, and never panics, on a
    /// lock this thread holds.
    #[track_caller]
    pub fn try_acquire(root: &Path) -> io::Result<Self> {
        let at = std::panic::Location::caller();
        Self::acquire_flags(root, libc::LOCK_EX | libc::LOCK_NB, at)
    }

    fn acquire_flags(
        root: &Path,
        flags: i32,
        at: &'static std::panic::Location<'static>,
    ) -> io::Result<Self> {
        fs::create_dir_all(root)?;
        let path = root.join(".rtrash.lock");
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)?;
        // Before the flock, which would never return.
        #[cfg(debug_assertions)]
        let mark = {
            let meta = file.metadata()?;
            let blocking = flags & libc::LOCK_NB == 0;
            held::Mark::take((meta.dev(), meta.ino()), root, blocking, at)
        };
        #[cfg(not(debug_assertions))]
        let _ = at;
        use std::os::unix::io::AsRawFd;
        let rc = unsafe { libc::flock(file.as_raw_fd(), flags) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(TrashLock {
            _file: file,
            #[cfg(debug_assertions)]
            _held: mark,
        })
    }
}

/// Debug builds: the trash locks this thread holds, by lock file (device
/// and inode, however the root was spelled), with where each was taken.
#[cfg(debug_assertions)]
mod held {
    use std::cell::RefCell;
    use std::panic::Location;
    use std::path::Path;

    type Held = Vec<((u64, u64), &'static Location<'static>)>;

    thread_local! {
        static HELD: RefCell<Held> = const { RefCell::new(Vec::new()) };
    }

    /// This thread's claim on one lock file, given up when dropped.
    pub(super) struct Mark(Option<(u64, u64)>);

    impl Mark {
        /// Claim lock file `id` of `root` for a lock taken at `at`. A
        /// blocking take of a file this thread holds panics; a
        /// non-blocking one claims nothing, since its flock fails anyway.
        pub(super) fn take(
            id: (u64, u64),
            root: &Path,
            blocking: bool,
            at: &'static Location<'static>,
        ) -> Mark {
            let first = HELD.with(|h| h.borrow().iter().find(|(i, _)| *i == id).map(|(_, l)| *l));
            match (first, blocking) {
                (Some(first), true) => panic!(
                    "this thread already holds the trash lock on '{}', taken at {first}; \
                     taking it again at {at} would wait forever",
                    root.display()
                ),
                (Some(_), false) => Mark(None),
                (None, _) => {
                    HELD.with(|h| h.borrow_mut().push((id, at)));
                    Mark(Some(id))
                }
            }
        }
    }

    impl Drop for Mark {
        fn drop(&mut self) {
            let Some(id) = self.0 else {
                return;
            };
            // At thread exit the list may be gone already.
            let _ = HELD.try_with(|h| {
                let mut h = h.borrow_mut();
                if let Some(k) = h.iter().position(|(i, _)| *i == id) {
                    h.remove(k);
                }
            });
        }
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
    "fuse.lxcfs",
    "fuse.snapfuse",
    "squashfs",
    "selinuxfs",
    "smackfs",
    "nfsd",
];

/// Whether `fstype` is a pseudo file system: no files of its own to
/// trash or measure (kernel interfaces, `lxcfs`'s view of `/proc`, a
/// portal or `gvfs` view of files kept elsewhere), or a read-only image
/// (`squashfs`, `snapfuse`). Anything else is listed, whether it is a
/// disk, a network share or a VM or container mount: `virtiofs`, `9p`,
/// `fuse.sshfs`, `overlay`, `tmpfs` and `ramfs` all hold files a user
/// can trash (trash-cli #412).
pub fn is_pseudo_fs(fstype: &str) -> bool {
    PSEUDO_FS.contains(&fstype) || fstype.starts_with("cgroup")
}

/// Mount-point escapes per fstab(5): `\040` space, `\011` tab, `\012` newline, `\134` backslash.
pub fn unescape_mount(s: &str) -> PathBuf {
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
///   home-only; [`volume_top_of_path`] still falls back to an `st_dev` parent walk
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

/// Each mount point in an fstab/mounts-style table once, in table order,
/// with the type of the mount in view there: a later mount on the same
/// point hides the earlier one. Nothing is filtered out.
pub(crate) fn mount_table(content: &str) -> Vec<(PathBuf, String)> {
    let mut out: Vec<(PathBuf, String)> = Vec::new();
    for line in content.lines() {
        let mut fields = line.split_whitespace();
        let _dev = fields.next();
        let (Some(mp), Some(fstype)) = (fields.next(), fields.next()) else {
            continue;
        };
        let mp = unescape_mount(mp);
        match out.iter_mut().find(|(m, _)| *m == mp) {
            Some(slot) => slot.1 = fstype.to_string(),
            None => out.push((mp, fstype.to_string())),
        }
    }
    out
}

/// Parse an fstab/mounts-style table into non-pseudo mount points (testable).
/// A mount point whose mount in view is a pseudo file system is left out,
/// even when an earlier mount there was not.
pub fn mount_points_from_table(content: &str) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = mount_table(content)
        .into_iter()
        .filter(|(_, fstype)| !is_pseudo_fs(fstype))
        .map(|(m, _)| m)
        .collect();
    if !out.iter().any(|m| m == Path::new("/")) {
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

/// Move `abs` into `trash`, reserving a unique name with its whole
/// `.trashinfo` (the spec's atomicity requirement; see
/// `reserve_record`). Returns the name used.
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
        // A Ctrl-C here waits until the record, payload and total agree.
        let _critical = crate::interrupt::critical();
        let _lock = TrashLock::acquire(&trash.root)?;
        let before = crate::trash_total::cached(trash);
        let name = trash_move_locked(abs, meta, trash)?;
        // Keep a total that was current before this put current after it.
        if let Some((bytes, items)) = before {
            let added = DirSizes::load(trash).entry_bytes(&name);
            crate::trash_total::store(trash, bytes.saturating_add(added), items + 1);
        } else {
            crate::trash_total::forget(trash);
        }
        crate::trash_total::touched(trash);
        name
    };
    if meta.is_dir() {
        record_dir_size(trash, &name);
    }
    Ok(name)
}

/// Write the `directorysizes` line for the trashed directory `name`.
/// Best effort: a cache that cannot be written is rebuilt by `status`.
pub fn record_dir_size(trash: &TrashDir, name: &str) {
    // A run asked to stop skips the walk; `status` rebuilds the line.
    if crate::interrupt::requested() {
        return;
    }
    let dest = join_name(&trash.files(), name);
    let record = join_info(&trash.info(), name);
    let Some(seen) = RecordLook::take(&record) else {
        return;
    };
    let size = crate::fastdelete::disk_usage(&dest);
    let Ok(mtime) = info_mtime_secs(&record) else {
        return;
    };
    let inodes = crate::trash_inodes::tree_inodes(&dest);
    #[cfg(test)]
    {
        if let Some(f) = tests::AFTER_WALK.take() {
            f();
        }
    }
    if let Ok(_lock) = TrashLock::acquire(&trash.root) {
        // Under the lock, the line goes only to the record the walk
        // sized. A restore or rm since, then a put that gave the name to
        // another item, would leave that item with this size.
        if RecordLook::take(&record).as_ref() != Some(&seen) {
            return;
        }
        let _critical = crate::interrupt::critical();
        let _ = directorysizes_upsert(trash, name, size, mtime);
        crate::trash_inodes::record(trash, name, inodes);
    }
}

/// One look at a record: its bytes, and which file held them since when.
/// Two looks that differ saw two records, even under one name.
#[derive(PartialEq, Eq)]
struct RecordLook {
    body: Vec<u8>,
    file: (u64, u64, i64, i64),
}

impl RecordLook {
    fn take(path: &Path) -> Option<Self> {
        let meta = fs::metadata(path).ok()?;
        let body = info::read_bounded(path, info::RECORD_CAP).ok()?;
        let file = (meta.dev(), meta.ino(), meta.mtime(), meta.mtime_nsec());
        Some(RecordLook { body, file })
    }
}

/// Inner put path (caller holds the trash-root lock). Exposed for tests.
pub fn trash_move_locked(abs: &Path, meta: &fs::Metadata, trash: &TrashDir) -> io::Result<String> {
    // From the name taken to the payload moved, or both undone: a Ctrl-C
    // never leaves a half-written record or a payload without one.
    let _critical = crate::interrupt::critical();
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

    let name = reserve_record(trash, &base, body.as_bytes())?;

    let dest = join_name(&trash.files(), &name);
    let moved = match rename_noreplace(abs, &dest) {
        Ok(()) => Ok(()),
        Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
            // A directory inside that this user can't empty would stop the
            // removal halfway; refuse before copying, as a rename would.
            // A copy that would fill the trash's file system is refused too.
            tree_removable(abs, meta)
                .and_then(|()| crate::space::guard_copy(abs, meta, &trash.files()))
                .and_then(|()| {
                    // A Ctrl-C stops the copy, which then takes back what
                    // it made; the error path below removes the record.
                    crate::interrupt::abortable(|| move_by_copy(abs, &dest, meta))
                })
        }
        Err(e) => Err(e),
    };
    if let Err(e) = moved {
        if let Some(left) = source_left(&e) {
            // The copy is whole and part of the original may be gone, so
            // the trash entry stays: dropping it would lose those files.
            return Err(io::Error::new(
                e.kind(),
                format!(
                    "copied into the trash as '{}', but removing the original failed: {}",
                    crate::util::show_name(&name),
                    left.err
                ),
            ));
        }
        let _ = fs::remove_file(join_info(&trash.info(), &name));
        return Err(e);
    }
    Ok(name)
}

/// The start of the name of the file in `info/` that a put writes its
/// record into before it links the record to its trash name. `status
/// --check --fix` removes one that a killed put left behind.
pub const PUT_TEMP_PREFIX: &str = ".rtrash-put.";

/// How many names past the first a put tries for one item.
const NAME_TRIES: u64 = 10_000;

/// The trash names a put tries for `base`, in order: `base`, `base.2`
/// and on, skipping one a payload with no record already holds. `take`
/// claims a name (true), finds it taken (false) or fails; the first name
/// it claims is returned.
fn claim_name(
    trash: &TrashDir,
    base: &str,
    mut take: impl FnMut(&str) -> io::Result<bool>,
) -> io::Result<String> {
    let files = trash.files();
    for attempt in 0..=NAME_TRIES {
        let name = if attempt == 0 {
            base.to_string()
        } else {
            format!("{base}.{}", attempt + 1)
        };
        if join_name(&files, &name).symlink_metadata().is_ok() {
            continue;
        }
        if take(&name)? {
            return Ok(name);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not reserve a unique trash name",
    ))
}

/// Take a trash name for `base` in `trash` and give it the record
/// `body`, which appears whole or not at all.
///
/// The record is written and synced under a temporary name in `info/`
/// first, then hard-linked to `NAME.trashinfo`. link(2) fails when that
/// name exists, so it reserves the name as O_EXCL does, and a tool that
/// reads `info/` meanwhile never meets an empty or cut-off record. A put
/// killed before the link leaves only the temporary, which no other tool
/// reads as a record. On a file system with no hard links (FAT, exFAT,
/// some network mounts) the record is made in place with O_EXCL.
fn reserve_record(trash: &TrashDir, base: &str, body: &[u8]) -> io::Result<String> {
    let _critical = crate::interrupt::critical();
    let info_dir = trash.info();
    let (temp, mut file) = create_put_temp(&info_dir)?;
    crate::interrupt::test_seam("in-record");
    let written = file.write_all(body).and_then(|()| sync_file(&mut file));
    drop(file);
    if let Err(e) = written {
        let _ = fs::remove_file(&temp);
        return Err(e);
    }
    let mut no_links = false;
    let linked = claim_name(trash, base, |name| {
        match link_record(&temp, &join_info(&info_dir, name)) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(false),
            Err(e) => {
                no_links = hard_links_unsupported(&e);
                Err(e)
            }
        }
    });
    let _ = fs::remove_file(&temp);
    let name = match linked {
        Err(_) if no_links => reserve_in_place(trash, base, body)?,
        other => other?,
    };
    // Durable reservation before payload move (crash mid-put must not leave a
    // truncated .trashinfo as the only recoverability record).
    let _ = sync_dir(&info_dir);
    Ok(name)
}

/// A fresh, empty `.rtrash-put.PID.N` in `info_dir` for a record.
fn create_put_temp(info_dir: &Path) -> io::Result<(PathBuf, fs::File)> {
    let pid = std::process::id();
    for n in 0..NAME_TRIES {
        let temp = info_dir.join(format!("{PUT_TEMP_PREFIX}{pid}.{n}"));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
        {
            Ok(f) => return Ok((temp, f)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not make a temporary record",
    ))
}

/// Hard-link the written record `temp` to its trash name `dest`.
fn link_record(temp: &Path, dest: &Path) -> io::Result<()> {
    #[cfg(test)]
    {
        if tests::NO_HARD_LINKS.with(std::cell::Cell::get) {
            return Err(io::Error::from_raw_os_error(libc::EPERM));
        }
    }
    fs::hard_link(temp, dest)
}

/// Whether link(2) failed with `e` because the file system has no hard
/// links: EPERM on FAT and exFAT, EOPNOTSUPP or ENOSYS on some mounts.
fn hard_links_unsupported(e: &io::Error) -> bool {
    let codes = [libc::EPERM, libc::EOPNOTSUPP, libc::ENOTSUP, libc::ENOSYS];
    e.raw_os_error().is_some_and(|c| codes.contains(&c))
}

/// [`reserve_record`] where there are no hard links: create
/// `NAME.trashinfo` with O_EXCL, then write `body` into it. A write that
/// fails takes the record back.
fn reserve_in_place(trash: &TrashDir, base: &str, body: &[u8]) -> io::Result<String> {
    let _critical = crate::interrupt::critical();
    let info_dir = trash.info();
    let mut opened = None;
    let name = claim_name(trash, base, |name| {
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(join_info(&info_dir, name))
        {
            Ok(f) => {
                opened = Some(f);
                Ok(true)
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(false),
            Err(e) => Err(e),
        }
    })?;
    let Some(mut file) = opened else {
        return Err(io::Error::other("no record was opened"));
    };
    let written = file.write_all(body).and_then(|()| sync_file(&mut file));
    drop(file);
    if let Err(e) = written {
        let _ = fs::remove_file(join_info(&info_dir, &name));
        return Err(e);
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

/// Largest `directorysizes` cache read; a bigger one is ignored, and the
/// sizes are walked instead.
const SIZES_CAP: u64 = 64 << 20;

/// One line of a trash root's `directorysizes` cache: `SIZE MTIME NAME`,
/// where SIZE is the directory's block usage in bytes and MTIME the
/// seconds of its `.trashinfo` file's modification time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SizeLine {
    /// The directory's name under `files/`, decoded to its raw bytes.
    pub name: Vec<u8>,
    pub size: u64,
    pub mtime: i64,
}

/// Parse one `directorysizes` line, without its newline. The spec lets a
/// writer percent-encode anything from just `%` and newline up to every
/// byte RFC 2396 reserves, so the name is decoded and every comparison is
/// on its bytes: `My Dir`, `My%20Dir` and `My%20D%69r` name one directory.
///
/// `None` for a line that is not `SIZE MTIME NAME` with decimal numbers
/// (MTIME may be negative), and for a name that cannot be a child of
/// `files/`: empty, `.`, `..`, or with a `/` or NUL byte. The spec refuses
/// a `/` even as `%2F`.
pub fn directorysizes_parse_line(line: &[u8]) -> Option<SizeLine> {
    let mut parts = line.splitn(3, |&b| b == b' ');
    let digits = |f: &[u8]| !f.is_empty() && f.iter().all(u8::is_ascii_digit);
    let size = parts.next().filter(|f| digits(f))?;
    let mtime = parts.next()?;
    let unsigned = mtime.strip_prefix(b"-").unwrap_or(mtime);
    if !digits(unsigned) {
        return None;
    }
    let name = crate::util::url_decode_bytes(parts.next()?);
    let bad = name.is_empty()
        || name == b"."
        || name == b".."
        || name.iter().any(|&b| b == b'/' || b == 0);
    if bad {
        return None;
    }
    Some(SizeLine {
        name,
        size: std::str::from_utf8(size).ok()?.parse().ok()?,
        mtime: std::str::from_utf8(mtime).ok()?.parse().ok()?,
    })
}

/// Every valid line of the `directorysizes` cache at `path`, read by
/// [`info::read_bounded`] so a FIFO or device there cannot hang or flood a
/// command. The file is bytes, not text: a writer that encodes only `%`
/// and newline leaves other bytes of a name as they are. A later line for
/// a name replaces the earlier one, so each name appears once.
pub fn read_size_lines(path: &Path) -> io::Result<Vec<SizeLine>> {
    let body = info::read_bounded(path, SIZES_CAP)?;
    let mut lines: Vec<SizeLine> = Vec::new();
    let mut at: HashMap<Vec<u8>, usize> = HashMap::new();
    for line in body
        .split(|&b| b == b'\n')
        .filter_map(directorysizes_parse_line)
    {
        match at.get(&line.name) {
            Some(&i) => lines[i] = line,
            None => {
                at.insert(line.name.clone(), lines.len());
                lines.push(line);
            }
        }
    }
    Ok(lines)
}

/// Read the `directorysizes` cache of `trash`, let `edit` change its lines,
/// and write the result through a temporary file and a rename, as the spec
/// requires. Every line comes out in one encoding, a name once. A cache
/// that cannot be read counts as empty, since the cache may lose lines;
/// one left with no lines is removed. Nothing is written when `edit`
/// changes nothing.
///
/// Every update of the cache goes through here: a put, rm, restore, empty
/// and the write back of [`DirSizes`].
pub fn directorysizes_rewrite(
    trash: &TrashDir,
    edit: impl FnOnce(&mut Vec<SizeLine>),
) -> io::Result<()> {
    let path = trash.root.join("directorysizes");
    let before = read_size_lines(&path).unwrap_or_default();
    let mut lines = before.clone();
    edit(&mut lines);
    if lines == before {
        return Ok(());
    }
    if lines.is_empty() {
        return fs::remove_file(&path);
    }
    let body: String = lines
        .iter()
        .map(|l| {
            let enc = crate::util::url_encode(&l.name);
            format!("{} {} {enc}\n", l.size, l.mtime)
        })
        .collect();
    crate::file_replace::write_own(&path, body.as_bytes())
}

/// The raw bytes of trash entry `name`, as [`SizeLine::name`] holds them.
fn size_key(name: &str) -> Vec<u8> {
    crate::util::os_from_str(name).as_bytes().to_vec()
}

/// Insert or replace the `directorysizes` line of directory `name`,
/// whichever encoding the old line used.
pub fn directorysizes_upsert(
    trash: &TrashDir,
    name: &str,
    size: u64,
    mtime: i64,
) -> io::Result<()> {
    let name = size_key(name);
    directorysizes_rewrite(trash, |lines| {
        lines.retain(|l| l.name != name);
        lines.push(SizeLine { name, size, mtime });
    })
}

/// Drop the `directorysizes` line of `name`, if there is one (used by
/// selective trash-rm and restore).
pub fn directorysizes_remove(trash: &TrashDir, name: &str) {
    let name = size_key(name);
    let _ = directorysizes_rewrite(trash, |lines| lines.retain(|l| l.name != name));
}

/// Keep only the `directorysizes` lines of the entries in `kept`.
pub fn directorysizes_keep(trash: &TrashDir, kept: &[String]) {
    let keep: HashSet<Vec<u8>> = kept.iter().map(|n| size_key(n.as_str())).collect();
    let _ = directorysizes_rewrite(trash, |lines| lines.retain(|l| keep.contains(&l.name)));
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
    /// Decoded name to (size, trashinfo mtime).
    lines: HashMap<Vec<u8>, (u64, i64)>,
    misses: std::sync::Mutex<Vec<SizeLine>>,
}

impl<'a> DirSizes<'a> {
    pub fn load(trash: &'a TrashDir) -> Self {
        let lines = read_size_lines(&trash.root.join("directorysizes"))
            .unwrap_or_default()
            .into_iter()
            .map(|l| (l.name, (l.size, l.mtime)))
            .collect();
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
        let (size, mtime) = *self.lines.get(&size_key(name))?;
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
                self.misses
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(SizeLine {
                        name: size_key(name),
                        size,
                        mtime,
                    });
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
        let gone = |name: &[u8]| {
            let name = OsStr::from_bytes(name);
            !fs::symlink_metadata(files.join(name)).is_ok_and(|m| m.is_dir())
        };
        if misses.is_empty() && !self.lines.keys().any(|name| gone(name)) {
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
        // The rewrite reads again under the lock: a put may have added a
        // line meanwhile.
        let _ = directorysizes_rewrite(self.trash, |lines| {
            lines.retain(|l| !gone(&l.name));
            for miss in misses {
                lines.retain(|l| l.name != miss.name);
                lines.push(miss);
            }
        });
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

fn remove_any(path: &Path, meta: &fs::Metadata) -> io::Result<()> {
    if meta.is_dir() {
        // scratch: callers decide; permanent_guard::remove gates user data.
        crate::fastdelete::remove_path(path)
    } else {
        fs::remove_file(path)
    }
}

/// Owned directory descriptors one walk keeps open. Deeper frames close
/// their shallowest ancestors and reopen them through `..`.
const WALK_FD_BUDGET: usize = 16;

/// One directory on an explicit walk stack.
struct WalkDir {
    fd: Option<OwnedFd>,
    /// Device and inode, recorded when `fd` is closed.
    id: (u64, u64),
    entries: Vec<OsString>,
}

fn open_dir_nofollow(path: &Path) -> io::Result<OwnedFd> {
    let c = path_cstr(path)?;
    let fd = unsafe {
        libc::open(
            c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

/// A directory opened through any symlinks on the way, for a parent
/// whose own path may be a link (a symlinked trash folder).
fn open_dir(path: &Path) -> io::Result<OwnedFd> {
    let c = path_cstr(path)?;
    let fd = unsafe {
        libc::open(
            c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn openat_dir(dirfd: RawFd, name: &CStr) -> io::Result<OwnedFd> {
    let fd = unsafe {
        libc::openat(
            dirfd,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

fn name_cstr(name: &OsStr) -> io::Result<CString> {
    CString::new(name.as_bytes()).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))
}

fn fstatat_nofollow(dirfd: RawFd, name: &CStr) -> io::Result<libc::stat> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatat(dirfd, name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(st)
}

fn is_type(st: &libc::stat, kind: libc::mode_t) -> bool {
    (st.st_mode & libc::S_IFMT) == kind
}

fn fd_dev_ino(fd: RawFd) -> io::Result<(u64, u64)> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((st.st_dev as u64, st.st_ino as u64))
}

/// Open the parent of `child` through `..` and check it is still `id`.
fn reopen_parent_dir(child: RawFd, id: (u64, u64)) -> io::Result<OwnedFd> {
    let fd = unsafe {
        libc::openat(
            child,
            c"..".as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    if fd_dev_ino(fd.as_raw_fd())? != id {
        return Err(io::Error::other(
            "the directory moved while it was being copied",
        ));
    }
    Ok(fd)
}

/// Names in `dirfd`, not `.` or `..`. The descriptor stays open.
fn read_names(dirfd: RawFd) -> io::Result<Vec<OsString>> {
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
        let bytes = unsafe { CStr::from_ptr((*ent).d_name.as_ptr()) }.to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        names.push(OsString::from(OsStr::from_bytes(bytes)));
    }
    unsafe { libc::closedir(dir) };
    Ok(names)
}

fn walk_fd(frame: &WalkDir) -> RawFd {
    frame.fd.as_ref().map_or(-1, |f| f.as_raw_fd())
}

/// Close the shallowest open frame once more than [`WALK_FD_BUDGET`]
/// directories are open, so depth never costs a descriptor per level.
fn trim_walk(stack: &mut [WalkDir], lo: &mut usize) -> io::Result<()> {
    if stack.len() - *lo <= WALK_FD_BUDGET {
        return Ok(());
    }
    let f = &mut stack[*lo];
    if let Some(old) = f.fd.take() {
        f.id = fd_dev_ino(old.as_raw_fd())?;
    }
    *lo += 1;
    Ok(())
}

/// Reopen `stack[parent]` from `child` when its descriptor was closed.
fn restore_parent(
    stack: &mut [WalkDir],
    parent: usize,
    child: RawFd,
    lo: &mut usize,
) -> io::Result<()> {
    if stack[parent].fd.is_none() {
        stack[parent].fd = Some(reopen_parent_dir(child, stack[parent].id)?);
        *lo = parent;
    }
    *lo = (*lo).min(stack.len());
    Ok(())
}

/// What a cross-device move must know of an entry before it unlinks it:
/// its mode, its owner, and whether a file flag pins it in place.
#[derive(Clone, Copy, Debug)]
struct Node {
    mode: libc::mode_t,
    uid: libc::uid_t,
    pinned: bool,
}

impl Node {
    fn of(st: &libc::stat, pinned: bool) -> Node {
        Node {
            mode: st.st_mode,
            uid: st.st_uid,
            pinned,
        }
    }
}

/// Why this user could not unlink `entry` from the directory `dir`, past
/// the write and search permission an `access` check already covers, or
/// `None` when nothing stands in the way. An immutable or append-only
/// flag stops root too; the sticky bit does not.
fn unlink_blocker(dir: Node, entry: Node, euid: libc::uid_t) -> Option<&'static str> {
    if dir.pinned {
        return Some("its directory is immutable or append-only");
    }
    if entry.pinned {
        return Some("it is immutable or append-only");
    }
    let sticky = dir.mode & libc::S_ISVTX != 0;
    if sticky && euid != 0 && entry.uid != euid && dir.uid != euid {
        return Some("another user owns it in a sticky directory");
    }
    None
}

/// Whether the entry `name` in `dirfd` (`st`, not followed) is immutable
/// or append-only. Linux reads the statx attributes by syscall, since the
/// libc crate has no statx wrapper on every musl; the struct is 256
/// bytes, with `stx_attributes` the u64 at offset 8. A kernel without
/// statx answers no.
#[cfg(target_os = "linux")]
fn pinned_at(dirfd: RawFd, name: &CStr, _st: &libc::stat) -> bool {
    const STATX_TYPE: libc::c_uint = 0x1;
    const IMMUTABLE_OR_APPEND: u64 = 0x10 | 0x20;
    let mut buf = [0u64; 32];
    let rc = unsafe {
        libc::syscall(
            libc::SYS_statx,
            dirfd,
            name.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
            STATX_TYPE,
            buf.as_mut_ptr(),
        )
    };
    rc == 0 && buf[1] & IMMUTABLE_OR_APPEND != 0
}

/// Whether `st` carries a user or system immutable or append-only flag.
#[cfg(target_os = "macos")]
fn pinned_at(_dirfd: RawFd, _name: &CStr, st: &libc::stat) -> bool {
    let flags = libc::UF_IMMUTABLE | libc::UF_APPEND | libc::SF_IMMUTABLE | libc::SF_APPEND;
    st.st_flags & flags != 0
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn pinned_at(_dirfd: RawFd, _name: &CStr, _st: &libc::stat) -> bool {
    false
}

/// A permission error for the entry `name` that `why` stops.
fn blocked(name: &OsStr, why: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!(
            "cannot remove '{}': {why}",
            crate::util::term_text(&name.to_string_lossy())
        ),
    )
}

/// The [`Node`] of `path` once this user may unlink it from its
/// directory: the directory lets it write and search, and
/// [`unlink_blocker`] finds nothing. Checked before a copy, since a
/// rename across devices fails with EXDEV before any permission check
/// and the copy would then leave a whole original behind, or a half one.
fn entry_removable(path: &Path) -> io::Result<Node> {
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let parent_c = path_cstr(parent)?;
    if unsafe { libc::access(parent_c.as_ptr(), libc::W_OK | libc::X_OK) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let dir = open_dir(parent)?;
    let dirfd = dir.as_raw_fd();
    let mut dst: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(dirfd, &mut dst) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let dir_node = Node::of(&dst, pinned_at(dirfd, c".", &dst));
    let name_c = name_cstr(name)?;
    let st = fstatat_nofollow(dirfd, &name_c)?;
    let node = Node::of(&st, pinned_at(dirfd, &name_c, &st));
    if let Some(why) = unlink_blocker(dir_node, node, unsafe { libc::geteuid() }) {
        return Err(blocked(name, why));
    }
    Ok(node)
}

/// Ok when this user may unlink `path` from its directory and every
/// entry under it, as a copy across devices must before it starts: each
/// directory lets it write and search, and [`unlink_blocker`] passes each
/// entry. The walk keeps its own stack: a chain of any depth stays off
/// the thread stack, and paths past `PATH_MAX` are reached by descriptor.
fn tree_removable(path: &Path, meta: &fs::Metadata) -> io::Result<()> {
    let item = entry_removable(path)?;
    if !meta.is_dir() {
        return Ok(());
    }
    let euid = unsafe { libc::geteuid() };
    let c = path_cstr(path)?;
    if unsafe { libc::access(c.as_ptr(), libc::W_OK | libc::X_OK) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let root = open_dir_nofollow(path)?;
    let root_dev = fd_dev_ino(root.as_raw_fd())?.0;
    let entries = read_names(root.as_raw_fd())?;
    let mut stack = vec![WalkDir {
        fd: Some(root),
        id: (0, 0),
        entries,
    }];
    // The node of each frame's directory, in step with `stack`.
    let mut nodes = vec![item];
    // Frames below `lo` have their descriptors closed; the root never does.
    let mut lo = 1usize;
    loop {
        let top = stack.len() - 1;
        let Some(name) = stack[top].entries.pop() else {
            if top == 0 {
                return Ok(());
            }
            let done = stack.pop().expect("child frame");
            nodes.pop();
            let parent = top - 1;
            restore_parent(&mut stack, parent, walk_fd(&done), &mut lo)?;
            continue;
        };
        let dirfd = walk_fd(&stack[top]);
        let name_c = name_cstr(&name)?;
        let st = fstatat_nofollow(dirfd, &name_c)?;
        let node = Node::of(&st, pinned_at(dirfd, &name_c, &st));
        if let Some(why) = unlink_blocker(nodes[top], node, euid) {
            return Err(blocked(&name, why));
        }
        // A symlink is not a directory, even when its target is one.
        if !is_type(&st, libc::S_IFDIR) {
            continue;
        }
        if st.st_dev as u64 != root_dev {
            return Err(io::Error::other(
                "refusing to enter a directory on another device",
            ));
        }
        if unsafe { libc::faccessat(dirfd, name_c.as_ptr(), libc::W_OK | libc::X_OK, 0) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let child = openat_dir(dirfd, &name_c)?;
        let entries = read_names(child.as_raw_fd())?;
        stack.push(WalkDir {
            fd: Some(child),
            id: (0, 0),
            entries,
        });
        nodes.push(node);
        trim_walk(&mut stack, &mut lo)?;
    }
}

/// Give the owner write access to every directory under `path`, so a
/// half-made copy of a read-only tree can be removed again. Same walk as
/// [`tree_removable`]: no recursion, and no path long enough to fail.
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
    let Ok(root) = open_dir_nofollow(path) else {
        return;
    };
    let Ok(entries) = read_names(root.as_raw_fd()) else {
        return;
    };
    let mut stack = vec![WalkDir {
        fd: Some(root),
        id: (0, 0),
        entries,
    }];
    let mut lo = 1usize;
    loop {
        let top = stack.len() - 1;
        let Some(name) = stack[top].entries.pop() else {
            if top == 0 {
                return;
            }
            let done = stack.pop().expect("child frame");
            let parent = top - 1;
            if restore_parent(&mut stack, parent, walk_fd(&done), &mut lo).is_err() {
                return;
            }
            continue;
        };
        let dirfd = walk_fd(&stack[top]);
        let Ok(name_c) = name_cstr(&name) else {
            continue;
        };
        let Ok(st) = fstatat_nofollow(dirfd, &name_c) else {
            continue;
        };
        if !is_type(&st, libc::S_IFDIR) {
            continue;
        }
        let Ok(child) = openat_dir(dirfd, &name_c) else {
            continue;
        };
        chmod_dir_fd(child.as_raw_fd());
        let Ok(entries) = read_names(child.as_raw_fd()) else {
            continue;
        };
        stack.push(WalkDir {
            fd: Some(child),
            id: (0, 0),
            entries,
        });
        if trim_walk(&mut stack, &mut lo).is_err() {
            return;
        }
    }
}

fn chmod_dir_fd(fd: RawFd) {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut st) } != 0 {
        return;
    }
    let mode = st.st_mode;
    if (mode & 0o700) == 0o700 {
        return;
    }
    unsafe { libc::fchmod(fd, (mode & 0o7777) | 0o700) };
}

/// Remove a path whatever it is; used by empty, restore force-overwrite, and error cleanup.
/// Directory trees use the bulk unlinkat walker (and btrfs subvolume destroy when applicable).
pub fn remove_any_path(path: &Path) -> io::Result<()> {
    // scratch: the primitive; permanent_guard::remove gates user data.
    crate::fastdelete::remove_path(path)
}

/// Whether `path` is a regular file, not followed if it is a link: the
/// only thing a `.trashinfo` record can be. A folder, link, fifo or
/// socket by that name is something else, and no record removal takes it.
pub fn is_record_file(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_file())
}

/// Remove the `.trashinfo` record at `path`, and only a record: anything
/// [`is_record_file`] does not take is refused and left in place. The
/// removal is one unlink, which cannot take a folder even if one replaces
/// the file after the look, and takes only the link itself if a link
/// does. Callers that delete for good go through
/// [`crate::permanent_guard::remove_record`].
pub fn remove_record(path: &Path) -> io::Result<()> {
    if !is_record_file(path) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular file, so not a record; left in place",
        ));
    }
    fs::remove_file(path)
}

/// Move `src` to `dest`, falling back to copy+delete across devices (EXDEV).
/// Used by restore when the trash entry lives on a different filesystem than
/// the original path (e.g. home-trash fallback after a volume was full).
pub fn relocate(src: &Path, dest: &Path) -> io::Result<()> {
    relocate_with(src, dest, true)
}

/// [`relocate`] without the free space check, for putting a payload back
/// where it was after a failed restore.
pub fn relocate_back(src: &Path, dest: &Path) -> io::Result<()> {
    relocate_with(src, dest, false)
}

fn relocate_with(src: &Path, dest: &Path, check_space: bool) -> io::Result<()> {
    match fs::rename(src, dest) {
        Ok(()) => Ok(()),
        Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
            let meta = fs::symlink_metadata(src)?;
            tree_removable(src, &meta)?;
            if check_space {
                let parent = dest.parent().filter(|p| !p.as_os_str().is_empty());
                crate::space::guard_copy(src, &meta, parent.unwrap_or(Path::new(".")))?;
            }
            move_by_copy(src, dest, &meta)
        }
        Err(e) => Err(e),
    }
}

/// The error of a [`move_by_copy`] that failed after its copy was whole:
/// the copy at `dest` is complete, and part of the source may be gone.
#[derive(Debug)]
pub struct SourceLeft {
    pub dest: PathBuf,
    pub err: io::Error,
}

impl std::fmt::Display for SourceLeft {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the copy at '{}' is whole, but removing the source failed: {}",
            self.dest.display(),
            self.err
        )
    }
}

impl std::error::Error for SourceLeft {}

/// The [`SourceLeft`] inside `e`, when there is one: the copy is whole,
/// so the caller must keep it.
pub fn source_left(e: &io::Error) -> Option<&SourceLeft> {
    e.get_ref()?.downcast_ref::<SourceLeft>()
}

/// Move `src` to `dest` by copying it there and then removing it, the one
/// place that does both (put into a trash on another device, restore out
/// of one). The move completes or changes as little as it can:
///
/// - a copy that fails or is stopped takes back what it made at `dest`,
///   so the source stays whole and nothing new is left behind;
/// - once the copy is whole the source goes. A removal that fails partway
///   keeps the copy and returns a [`SourceLeft`]: part of the source may
///   be gone, and dropping the copy then would lose it (trash-cli #429).
///
/// Callers ask [`tree_removable`] first, so that removal rarely fails.
fn move_by_copy(src: &Path, dest: &Path, meta: &fs::Metadata) -> io::Result<()> {
    // Only what this copy made is taken back, never a path that was there.
    let fresh = crate::util::is_gone(dest);
    if let Err(e) = copy_recursive(src, dest, meta) {
        if fresh && e.kind() != io::ErrorKind::AlreadyExists {
            make_tree_writable(dest);
            // scratch: the part copied; the source is still whole.
            let _ = remove_any_path(dest);
        }
        return Err(e);
    }
    // scratch: a move; the copy is whole first.
    remove_any(src, meta).map_err(|err| {
        io::Error::new(
            err.kind(),
            SourceLeft {
                dest: dest.to_path_buf(),
                err,
            },
        )
    })
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
        return copy_dir_iter(src, dst, meta, links);
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

/// One directory in [`copy_dir_iter`]'s heap stack.
struct DirCopy {
    src: Option<OwnedFd>,
    dst: Option<OwnedFd>,
    name: OsString,
    src_id: (u64, u64),
    dst_id: (u64, u64),
    entries: Vec<OsString>,
    /// The path of this directory still fits in `PATH_MAX`, so its metadata
    /// can be applied with the same calls a shallow tree uses.
    by_path: bool,
    mode: libc::mode_t,
    uid: libc::uid_t,
    gid: libc::gid_t,
    atime: libc::timespec,
    mtime: libc::timespec,
}

/// Copy the directory `src` onto `dst` without recursion. Children whose
/// paths still fit in `PATH_MAX` use [`copy_node`]; deeper ones are copied
/// by descriptor. At most [`WALK_FD_BUDGET`] directories stay open on each
/// side.
fn copy_dir_iter(
    src: &Path,
    dst: &Path,
    meta: &fs::Metadata,
    links: &mut LinkMap,
) -> io::Result<()> {
    fs::create_dir(dst)?;
    // macOS accepts `/var/...` and then measures `/private/var/...`. The
    // length that has to stay under PATH_MAX is the resolved one.
    let ends = CopyEnds {
        src,
        dst,
        src_len: resolve_existing(src).as_os_str().len(),
        dst_len: resolve_existing(dst).as_os_str().len(),
    };
    let sfd = open_dir_nofollow(src)?;
    let dfd = open_dir_nofollow(dst)?;
    let entries = read_names(sfd.as_raw_fd())?;
    let mut stack = vec![blank_dir(sfd, dfd, entries)];
    let mut lo = 1usize;
    loop {
        let top = stack.len() - 1;
        if let Some(name) = stack[top].entries.pop() {
            crate::interrupt::check()?;
            copy_one_entry(&ends, links, &mut stack, &mut lo, &name)?;
            continue;
        }
        if top == 0 {
            break;
        }
        finish_child_dir(&ends, &mut stack, &mut lo)?;
    }
    finish_meta(src, dst, meta)?;
    set_times(dst, meta)
}

fn blank_dir(src: OwnedFd, dst: OwnedFd, entries: Vec<OsString>) -> DirCopy {
    DirCopy {
        src: Some(src),
        dst: Some(dst),
        name: OsString::new(),
        src_id: (0, 0),
        dst_id: (0, 0),
        entries,
        by_path: true,
        mode: 0,
        uid: 0,
        gid: 0,
        atime: libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        },
        mtime: libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        },
    }
}

/// The two roots of a copy, with the lengths the kernel will measure.
struct CopyEnds<'a> {
    src: &'a Path,
    dst: &'a Path,
    src_len: usize,
    dst_len: usize,
}

fn copy_one_entry(
    ends: &CopyEnds<'_>,
    links: &mut LinkMap,
    stack: &mut Vec<DirCopy>,
    lo: &mut usize,
    name: &OsStr,
) -> io::Result<()> {
    let top = stack.len() - 1;
    let name_c = name_cstr(name)?;
    let src_fd = dir_fd(&stack[top].src)?;
    let dst_fd = dir_fd(&stack[top].dst)?;
    let st = fstatat_nofollow(src_fd, &name_c)?;
    if is_type(&st, libc::S_IFSOCK) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "cannot copy a socket to another file system",
        ));
    }
    let by_path = path_fits(ends.src_len, stack, name) && path_fits(ends.dst_len, stack, name);
    if is_type(&st, libc::S_IFDIR) {
        return push_child_dir(ends.dst, stack, lo, (src_fd, dst_fd), name, &st, by_path);
    }
    if by_path {
        let sp = joined(ends.src, stack, name);
        let dp = joined(ends.dst, stack, name);
        let emeta = fs::symlink_metadata(&sp)?;
        return copy_node(&sp, &dp, &emeta, links);
    }
    copy_entry_deep(src_fd, dst_fd, &name_c, &st)
}

fn dir_fd(fd: &Option<OwnedFd>) -> io::Result<RawFd> {
    fd.as_ref()
        .map(|f| f.as_raw_fd())
        .ok_or_else(|| io::Error::other("directory descriptor closed"))
}

fn push_child_dir(
    dst_root: &Path,
    stack: &mut Vec<DirCopy>,
    lo: &mut usize,
    fds: (RawFd, RawFd),
    name: &OsStr,
    st: &libc::stat,
    by_path: bool,
) -> io::Result<()> {
    let (src_fd, dst_fd) = fds;
    let name_c = name_cstr(name)?;
    if by_path {
        fs::create_dir(joined(dst_root, stack, name))?;
    } else if unsafe { libc::mkdirat(dst_fd, name_c.as_ptr(), 0o700) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let child_src = openat_dir(src_fd, &name_c)?;
    let child_dst = openat_dir(dst_fd, &name_c)?;
    let entries = read_names(child_src.as_raw_fd())?;
    let (atime, mtime) = stat_times(st);
    stack.push(DirCopy {
        src: Some(child_src),
        dst: Some(child_dst),
        name: name.to_os_string(),
        src_id: (0, 0),
        dst_id: (0, 0),
        entries,
        by_path,
        mode: st.st_mode,
        uid: st.st_uid,
        gid: st.st_gid,
        atime,
        mtime,
    });
    trim_dir_copy(stack, lo)
}

fn trim_dir_copy(stack: &mut [DirCopy], lo: &mut usize) -> io::Result<()> {
    if stack.len() - *lo <= WALK_FD_BUDGET {
        return Ok(());
    }
    let f = &mut stack[*lo];
    if let Some(old) = f.src.take() {
        f.src_id = fd_dev_ino(old.as_raw_fd())?;
    }
    if let Some(old) = f.dst.take() {
        f.dst_id = fd_dev_ino(old.as_raw_fd())?;
    }
    *lo += 1;
    Ok(())
}

fn finish_child_dir(
    ends: &CopyEnds<'_>,
    stack: &mut Vec<DirCopy>,
    lo: &mut usize,
) -> io::Result<()> {
    let done = stack.pop().expect("child frame");
    let parent = stack.len() - 1;
    if stack[parent].src.is_none() || stack[parent].dst.is_none() {
        let src = reopen_parent_dir(dir_fd(&done.src)?, stack[parent].src_id)?;
        let dst = reopen_parent_dir(dir_fd(&done.dst)?, stack[parent].dst_id)?;
        stack[parent].src = Some(src);
        stack[parent].dst = Some(dst);
        *lo = parent;
    }
    *lo = (*lo).min(stack.len());
    if done.by_path {
        let sp = joined(ends.src, stack, &done.name);
        let dp = joined(ends.dst, stack, &done.name);
        let sm = fs::symlink_metadata(&sp)?;
        finish_meta(&sp, &dp, &sm)?;
        return set_times(&dp, &sm);
    }
    apply_fd_meta(
        dir_fd(&done.src)?,
        dir_fd(&done.dst)?,
        done.mode,
        done.uid,
        done.gid,
        done.atime,
        done.mtime,
    )
}

fn joined(root: &Path, stack: &[DirCopy], name: &OsStr) -> PathBuf {
    let mut p = root.to_path_buf();
    for f in stack.iter().skip(1) {
        p.push(&f.name);
    }
    if !name.is_empty() {
        p.push(name);
    }
    p
}

fn joined_len(root_len: usize, stack: &[DirCopy], name: &OsStr) -> usize {
    let mut n = root_len;
    for f in stack.iter().skip(1) {
        n = n.saturating_add(1).saturating_add(f.name.len());
    }
    if !name.is_empty() {
        n = n.saturating_add(1).saturating_add(name.len());
    }
    n
}

/// Bytes kept free under `PATH_MAX`. The syscall buffer includes a trailing
/// NUL, and a resolved path can still grow after it is measured.
const PATH_HEADROOM: usize = 256;

/// True when a path call can carry `name`. `root_len` is the resolved length.
fn path_fits(root_len: usize, stack: &[DirCopy], name: &OsStr) -> bool {
    joined_len(root_len, stack, name).saturating_add(PATH_HEADROOM) < libc::PATH_MAX as usize
}

fn stat_times(st: &libc::stat) -> (libc::timespec, libc::timespec) {
    // libc 0.2 keeps seconds and nanoseconds as separate fields on Linux and
    // on macOS. It does not expose `st_atimespec` / `st_mtimespec`.
    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios"
    ))]
    {
        (
            libc::timespec {
                tv_sec: st.st_atime as _,
                tv_nsec: st.st_atime_nsec as _,
            },
            libc::timespec {
                tv_sec: st.st_mtime as _,
                tv_nsec: st.st_mtime_nsec as _,
            },
        )
    }
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios"
    )))]
    {
        (
            libc::timespec {
                tv_sec: st.st_atime as _,
                tv_nsec: 0,
            },
            libc::timespec {
                tv_sec: st.st_mtime as _,
                tv_nsec: 0,
            },
        )
    }
}

fn apply_fd_meta(
    src: RawFd,
    dst: RawFd,
    mode: libc::mode_t,
    uid: libc::uid_t,
    gid: libc::gid_t,
    atime: libc::timespec,
    mtime: libc::timespec,
) -> io::Result<()> {
    unsafe { libc::fchown(dst, uid, gid) };
    copy_xattrs_fd(src, dst);
    if unsafe { libc::fchmod(dst, mode & 0o7777) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let times = [atime, mtime];
    if unsafe { libc::futimens(dst, times.as_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// A non-directory whose path does not fit in `PATH_MAX`.
fn copy_entry_deep(src_dir: RawFd, dst_dir: RawFd, name: &CStr, st: &libc::stat) -> io::Result<()> {
    if is_type(st, libc::S_IFLNK) {
        return copy_symlink_at(src_dir, dst_dir, name, st);
    }
    if is_type(st, libc::S_IFIFO) {
        if unsafe { libc::mkfifoat(dst_dir, name.as_ptr(), 0o600) } != 0 {
            return Err(io::Error::last_os_error());
        }
        return finish_node_at(dst_dir, name, st);
    }
    if is_type(st, libc::S_IFCHR) || is_type(st, libc::S_IFBLK) {
        if unsafe { libc::mknodat(dst_dir, name.as_ptr(), st.st_mode, st.st_rdev) } != 0 {
            return Err(io::Error::last_os_error());
        }
        return finish_node_at(dst_dir, name, st);
    }
    copy_file_at(src_dir, dst_dir, name, st)
}

fn finish_node_at(dirfd: RawFd, name: &CStr, st: &libc::stat) -> io::Result<()> {
    unsafe { libc::fchownat(dirfd, name.as_ptr(), st.st_uid, st.st_gid, 0) };
    if unsafe { libc::fchmodat(dirfd, name.as_ptr(), st.st_mode & 0o7777, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let (atime, mtime) = stat_times(st);
    let times = [atime, mtime];
    if unsafe { libc::utimensat(dirfd, name.as_ptr(), times.as_ptr(), 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn copy_symlink_at(src_dir: RawFd, dst_dir: RawFd, name: &CStr, st: &libc::stat) -> io::Result<()> {
    let mut buf = vec![0u8; (st.st_size as usize).saturating_add(1).max(64)];
    let target = loop {
        let n =
            unsafe { libc::readlinkat(src_dir, name.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        let n = n as usize;
        if n < buf.len() {
            buf.truncate(n);
            break buf;
        }
        buf.resize(buf.len().saturating_mul(2).max(n.saturating_add(1)), 0);
    };
    let target =
        CString::new(target).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    if unsafe { libc::symlinkat(target.as_ptr(), dst_dir, name.as_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    unsafe {
        libc::fchownat(
            dst_dir,
            name.as_ptr(),
            st.st_uid,
            st.st_gid,
            libc::AT_SYMLINK_NOFOLLOW,
        );
    }
    let (atime, mtime) = stat_times(st);
    let times = [atime, mtime];
    if unsafe {
        libc::utimensat(
            dst_dir,
            name.as_ptr(),
            times.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn copy_file_at(src_dir: RawFd, dst_dir: RawFd, name: &CStr, st: &libc::stat) -> io::Result<()> {
    let sfd = unsafe {
        libc::openat(
            src_dir,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if sfd < 0 {
        return Err(io::Error::last_os_error());
    }
    let dfd = unsafe {
        libc::openat(
            dst_dir,
            name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if dfd < 0 {
        let e = io::Error::last_os_error();
        unsafe { libc::close(sfd) };
        return Err(e);
    }
    let mut from = unsafe { fs::File::from_raw_fd(sfd) };
    let mut to = unsafe { fs::File::from_raw_fd(dfd) };
    copy_opened(&mut from, &mut to, st.st_size as u64, st.st_blocks as u64)?;
    unsafe { libc::fchown(to.as_raw_fd(), st.st_uid, st.st_gid) };
    copy_xattrs_fd(from.as_raw_fd(), to.as_raw_fd());
    if unsafe { libc::fchmod(to.as_raw_fd(), st.st_mode & 0o7777) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let (atime, mtime) = stat_times(st);
    let times = [atime, mtime];
    if unsafe { libc::futimens(to.as_raw_fd(), times.as_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
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
    let mut from = fs::File::open(src)?;
    let mut to = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(dst)?;
    copy_opened(&mut from, &mut to, meta.len(), meta.blocks())
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn copy_file_data(src: &Path, dst: &Path, _meta: &fs::Metadata) -> io::Result<()> {
    fs::copy(src, dst).map(crate::interrupt::copied)
}

/// Bytes from `from` into `to`, keeping a reflink or holes on Linux.
fn copy_opened(from: &mut fs::File, to: &mut fs::File, len: u64, blocks: u64) -> io::Result<()> {
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let _ = (len, blocks);
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        if unsafe { libc::ioctl(to.as_raw_fd(), libc::FICLONE, from.as_raw_fd()) } == 0 {
            crate::interrupt::copied(len);
            return Ok(());
        }
        if blocks.saturating_mul(512) < len {
            if copy_sparse(from, to, len).is_ok() {
                return Ok(());
            }
            // A failed sparse copy may have written part of the file: start
            // over. Only here: truncating on ext4 makes close flush the file.
            to.set_len(0)?;
            // SEEK_DATA moved the read offset; pwrite left the write one at 0.
            io::Seek::rewind(from)?;
        }
    }
    // In chunks, so a Ctrl-C or a watch ends a long copy inside
    // `abortable`, and a progress line moves within one file.
    const CHUNK: u64 = 64 << 20;
    loop {
        crate::interrupt::check()?;
        match io::copy(&mut io::Read::take(&mut *from, CHUNK), to)? {
            0 => return Ok(()),
            n => crate::interrupt::copied(n),
        }
    }
}

/// Copy the data runs SEEK_DATA and SEEK_HOLE report, leave the holes
/// unwritten and set the length, so a mostly empty disk image stays small.
/// The bytes count as they go, so a sparse copy that fails and starts
/// over counts its first part twice; only a progress line reads them.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn copy_sparse(from: &fs::File, to: &fs::File, len: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    use std::os::unix::io::AsRawFd;
    let fd_in = from.as_raw_fd();
    let mut buf = vec![0u8; 1 << 20];
    let mut pos: u64 = 0;
    while pos < len {
        crate::interrupt::check()?;
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
            crate::interrupt::copied(n as u64);
            at += n as u64;
        }
        pos = hole.max(data + 1);
    }
    to.set_len(len)
}

/// Extended attributes, best effort: a target file system without them,
/// or a `security.*` name this user may not set, drops that attribute.
/// On macOS the access control list comes along too.
#[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
fn copy_xattrs(src: &Path, dst: &Path) {
    let (Ok(s), Ok(d)) = (path_cstr(src), path_cstr(dst)) else {
        return;
    };
    #[cfg(target_os = "macos")]
    mac_acl::copy_path(&s, &d);
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

#[cfg(any(target_os = "linux", target_os = "android"))]
fn copy_xattrs_fd(src: RawFd, dst: RawFd) {
    let Some(names) = xattr_buf(|b, n| unsafe { libc::flistxattr(src, b.cast(), n) }) else {
        return;
    };
    for name in names.split(|&b| b == 0).filter(|n| !n.is_empty()) {
        let Ok(name) = CString::new(name) else {
            continue;
        };
        if let Some(value) = xattr_buf(|b, n| unsafe { libc::fgetxattr(src, name.as_ptr(), b, n) })
        {
            unsafe { libc::fsetxattr(dst, name.as_ptr(), value.as_ptr().cast(), value.len(), 0) };
        }
    }
}

/// macOS keeps access control lists apart from extended attributes, so the
/// attribute copy misses them; libSystem's `acl_*_np` calls copy them.
#[cfg(target_os = "macos")]
mod mac_acl {
    use std::ffi::{c_char, c_int, c_void, CStr};
    use std::os::fd::RawFd;

    /// `ACL_TYPE_EXTENDED` from `<sys/acl.h>`, the only type macOS uses.
    const ACL_TYPE_EXTENDED: c_int = 0x0000_0100;

    extern "C" {
        fn acl_get_fd_np(fd: c_int, ty: c_int) -> *mut c_void;
        fn acl_set_fd_np(fd: c_int, acl: *mut c_void, ty: c_int) -> c_int;
        fn acl_get_link_np(path: *const c_char, ty: c_int) -> *mut c_void;
        fn acl_set_link_np(path: *const c_char, ty: c_int, acl: *mut c_void) -> c_int;
        fn acl_free(obj: *mut c_void) -> c_int;
    }

    /// Best effort: a source without an ACL, or a target that refuses one,
    /// leaves the target as it is.
    pub fn copy_fd(src: RawFd, dst: RawFd) {
        unsafe {
            let acl = acl_get_fd_np(src, ACL_TYPE_EXTENDED);
            if !acl.is_null() {
                acl_set_fd_np(dst, acl, ACL_TYPE_EXTENDED);
                acl_free(acl);
            }
        }
    }

    /// [`copy_fd`] by path, not following a final symlink.
    pub fn copy_path(src: &CStr, dst: &CStr) {
        unsafe {
            let acl = acl_get_link_np(src.as_ptr(), ACL_TYPE_EXTENDED);
            if !acl.is_null() {
                acl_set_link_np(dst.as_ptr(), ACL_TYPE_EXTENDED, acl);
                acl_free(acl);
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn copy_xattrs_fd(src: RawFd, dst: RawFd) {
    mac_acl::copy_fd(src, dst);
    let Some(names) = xattr_buf(|b, n| unsafe { libc::flistxattr(src, b.cast(), n, 0) }) else {
        return;
    };
    for name in names.split(|&b| b == 0).filter(|n| !n.is_empty()) {
        let Ok(name) = CString::new(name) else {
            continue;
        };
        if let Some(value) =
            xattr_buf(|b, n| unsafe { libc::fgetxattr(src, name.as_ptr(), b, n, 0, 0) })
        {
            unsafe {
                libc::fsetxattr(dst, name.as_ptr(), value.as_ptr().cast(), value.len(), 0, 0);
            }
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
fn copy_xattrs_fd(_src: RawFd, _dst: RawFd) {}

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

    thread_local! {
        /// Set by a test: link(2) of a record fails with EPERM on this
        /// thread, as on a file system with no hard links.
        pub(super) static NO_HARD_LINKS: std::cell::Cell<bool> =
            const { std::cell::Cell::new(false) };
        /// Set by a test: run once on this thread after `record_dir_size`
        /// has sized the item and before it takes the trash lock.
        pub(super) static AFTER_WALK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
            const { std::cell::RefCell::new(None) };
    }

    /// A trash under the temp dir for one put test, fresh.
    fn put_trash(tag: &str) -> (PathBuf, TrashDir) {
        let root = std::env::temp_dir().join(format!("rtrash-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let td = TrashDir {
            root: root.join("Trash"),
            topdir: None,
        };
        td.ensure().unwrap();
        (root, td)
    }

    /// The names in `dir`, sorted.
    fn names(dir: &Path) -> Vec<String> {
        let mut out: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        out.sort();
        out
    }

    /// A put writes its record under a temporary name and links it into
    /// place: the trash then has the record alone, with one link, and a
    /// clash takes the next name the same way.
    #[test]
    fn a_put_links_its_whole_record_into_place_and_leaves_no_temporary() {
        let (root, td) = put_trash("putlink");
        fs::write(td.info().join("a.txt.trashinfo"), b"[Trash Info]\n").unwrap();
        for n in ["a.txt", "b.txt"] {
            let item = root.join(n);
            fs::write(&item, n).unwrap();
            let meta = fs::symlink_metadata(&item).unwrap();
            trash_move_locked(&item, &meta, &td).unwrap();
        }
        assert_eq!(names(&td.files()), ["a.txt.2", "b.txt"]);
        assert_eq!(
            names(&td.info()),
            ["a.txt.2.trashinfo", "a.txt.trashinfo", "b.txt.trashinfo"]
        );
        for (name, file) in [("a.txt.2", "a.txt"), ("b.txt", "b.txt")] {
            let record = join_info(&td.info(), name);
            assert_eq!(fs::metadata(&record).unwrap().nlink(), 1, "{name}");
            let body = fs::read_to_string(&record).unwrap();
            let path = format!("Path={}\n", root.join(file).display());
            assert!(body.starts_with("[Trash Info]\n"), "{body}");
            assert!(
                body.contains(&path) && body.contains("DeletionDate="),
                "{body}"
            );
        }
        let _ = fs::remove_dir_all(&root);
    }

    /// Where link(2) fails as on FAT, a put makes its record in place
    /// with O_EXCL, still takes the next name on a clash, and leaves no
    /// temporary either.
    #[test]
    fn a_put_without_hard_links_makes_its_record_in_place() {
        let (root, td) = put_trash("putnolink");
        fs::write(td.info().join("a.txt.trashinfo"), b"[Trash Info]\n").unwrap();
        let item = root.join("a.txt");
        fs::write(&item, b"a").unwrap();
        let meta = fs::symlink_metadata(&item).unwrap();
        NO_HARD_LINKS.with(|n| n.set(true));
        let name = trash_move_locked(&item, &meta, &td);
        NO_HARD_LINKS.with(|n| n.set(false));
        assert_eq!(name.unwrap(), "a.txt.2");
        assert_eq!(names(&td.files()), ["a.txt.2"]);
        assert_eq!(names(&td.info()), ["a.txt.2.trashinfo", "a.txt.trashinfo"]);
        let body = fs::read_to_string(join_info(&td.info(), "a.txt.2")).unwrap();
        assert!(
            body.contains("Path=") && body.contains("DeletionDate="),
            "{body}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_read_only_files_dir_in_an_owned_volume_trash_falls_back() {
        if uid() == 0 {
            return;
        }
        let top = std::env::temp_dir().join(format!("rtrash-rofiles-{}", std::process::id()));
        let _ = fs::remove_dir_all(&top);
        fs::create_dir_all(&top).unwrap();
        let td = select_on_volume(&top, uid()).expect("a fresh trash");
        fs::set_permissions(td.files(), fs::Permissions::from_mode(0o500)).unwrap();
        assert!(select_on_volume(&top, uid()).is_none());
        fs::set_permissions(td.files(), fs::Permissions::from_mode(0o700)).unwrap();
        assert!(select_on_volume(&top, uid()).is_some());
        let _ = fs::remove_dir_all(&top);
    }

    #[test]
    fn a_volume_top_the_user_cannot_write_falls_back() {
        // Root ignores the mode, so the check means nothing there.
        if uid() == 0 {
            return;
        }
        let top = std::env::temp_dir().join(format!("rtrash-rotop-{}", std::process::id()));
        let _ = fs::remove_dir_all(&top);
        fs::create_dir_all(&top).unwrap();
        fs::set_permissions(&top, fs::Permissions::from_mode(0o500)).unwrap();
        // A fresh partition whose root the user cannot write: no trash
        // there, a warning, and the caller falls back to the home trash.
        assert!(select_on_volume(&top, uid()).is_none());
        fs::set_permissions(&top, fs::Permissions::from_mode(0o700)).unwrap();
        let _ = fs::remove_dir_all(&top);
    }

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
    fn a_volume_trash_other_users_can_write_to_is_refused() {
        let top = std::env::temp_dir().join(format!("rtrash-volmode-{}", std::process::id()));
        let _ = fs::remove_dir_all(&top);
        fs::create_dir_all(&top).unwrap();
        let me = uid();
        let own = top.join(format!(".Trash-{me}"));
        fs::create_dir_all(own.join("files")).unwrap();
        fs::create_dir_all(own.join("info")).unwrap();
        fs::write(own.join("files/kept"), b"kept").unwrap();
        for mode in [0o777, 0o770, 0o702] {
            fs::set_permissions(&own, fs::Permissions::from_mode(mode)).unwrap();
            let meta = fs::symlink_metadata(&own).unwrap();
            assert_eq!(
                user_trash_problem(&meta, me),
                Some("is writable by other users"),
                "mode {mode:o}"
            );
            // Neither listed nor put into: put falls back past it.
            assert!(volume_trashes_of(&top, me).is_empty(), "mode {mode:o}");
            assert!(select_on_volume(&top, me).is_none(), "mode {mode:o}");
        }
        // Readable by others is fine; only write access lets them plant.
        for mode in [0o700, 0o750, 0o755] {
            fs::set_permissions(&own, fs::Permissions::from_mode(mode)).unwrap();
            assert_eq!(
                volume_trashes_of(&top, me),
                std::slice::from_ref(&own),
                "mode {mode:o}"
            );
        }
        assert_eq!(fs::read(own.join("files/kept")).unwrap(), b"kept");
        fs::remove_dir_all(&top).unwrap();
    }

    #[test]
    fn a_volume_trash_another_uid_owns_is_skipped() {
        // A real other owner needs chown: root does it directly, anyone
        // else through passwordless sudo, as on CI.
        let top = std::env::temp_dir().join(format!("rtrash-volowner-{}", std::process::id()));
        let _ = fs::remove_dir_all(&top);
        fs::create_dir_all(&top).unwrap();
        let me = uid();
        let theirs = top.join(format!(".Trash-{me}"));
        fs::create_dir(&theirs).unwrap();
        let mut chown = if me == 0 {
            std::process::Command::new("chown")
        } else {
            let mut c = std::process::Command::new("sudo");
            c.arg("-n").arg("chown");
            c
        };
        let owned = chown
            .arg("65534")
            .arg(&theirs)
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !owned {
            eprintln!("note: chown to uid 65534 unavailable; foreign owner check skipped");
            let _ = fs::remove_dir_all(&top);
            return;
        }
        let meta = fs::symlink_metadata(&theirs).unwrap();
        assert_eq!(meta.uid(), 65534);
        assert_eq!(
            user_trash_problem(&meta, me),
            Some("belongs to another user")
        );
        assert!(volume_trashes_of(&top, me).is_empty());
        assert!(select_on_volume(&top, me).is_none());
        // It refuses before it makes anything inside.
        assert!(!theirs.join("files").exists());
        // Empty, so this user can remove it from its own top directory.
        fs::remove_dir(&theirs).unwrap();
        fs::remove_dir_all(&top).unwrap();
    }

    /// A volume trash takes an absolute Path= only inside its volume, as
    /// trash-cli 0.26.9's restore does; the home trash takes any, and on
    /// the root volume every path is inside.
    #[test]
    fn volume_trash_refuses_an_absolute_path_outside_its_volume() {
        let usb = TrashDir {
            root: PathBuf::from("/mnt/usb/.Trash-1"),
            topdir: Some(PathBuf::from("/mnt/usb")),
        };
        let ok = |td: &TrashDir, p: &str| td.resolve_original_checked(Path::new(p)).ok();
        assert_eq!(
            ok(&usb, "/mnt/usb/a.txt"),
            Some(PathBuf::from("/mnt/usb/a.txt"))
        );
        assert_eq!(
            ok(&usb, "docs/a.txt"),
            Some(PathBuf::from("/mnt/usb/docs/a.txt"))
        );
        for bad in [
            "/home/u/.bashrc",
            "/mnt/usbx/a.txt",
            "/mnt/usb/../etc/passwd",
            "/mnt/usb/a/../../x",
            "/mnt",
        ] {
            assert_eq!(ok(&usb, bad), None, "{bad}");
        }
        // The soft form still shows the record as written.
        assert_eq!(
            usb.resolve_original(Path::new("/home/u/.bashrc")),
            PathBuf::from("/home/u/.bashrc")
        );
        let root = TrashDir {
            root: PathBuf::from("/.Trash-1"),
            topdir: Some(PathBuf::from("/")),
        };
        assert_eq!(ok(&root, "/etc/x"), Some(PathBuf::from("/etc/x")));
        let home = TrashDir {
            root: PathBuf::from("/home/u/.local/share/Trash"),
            topdir: None,
        };
        assert_eq!(
            ok(&home, "/mnt/usb/a.txt"),
            Some(PathBuf::from("/mnt/usb/a.txt"))
        );
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
    fn a_file_on_a_skipped_mount_gets_no_volume_trash_around_it() {
        let table = "\
/dev/sda1 / ext4 rw 0 0
tmpfs /run/user/1000 tmpfs rw 0 0
portal /run/user/1000/doc fuse.portal rw 0 0
/dev/sdb1 /mnt/x ext4 rw 0 0
/dev/loop0 /mnt/x squashfs ro 0 0
/dev/loop1 /mnt/y squashfs ro 0 0
/dev/sdc1 /mnt/y ext4 rw 0 0
";
        let top = |p: &str| volume_top_from_table(Path::new(p), table);
        // The portal is another device than the tmpfs around it, so its
        // files go to the home trash, not to /run/user/1000/.Trash-$uid.
        assert_eq!(top("/run/user/1000/doc/abc/f"), None);
        assert_eq!(
            top("/run/user/1000/f"),
            Some(PathBuf::from("/run/user/1000"))
        );
        // The last mount on a point is the one in view.
        assert_eq!(top("/mnt/x/f"), None);
        assert_eq!(top("/mnt/y/f"), Some(PathBuf::from("/mnt/y")));
        assert_eq!(top("/opt/f"), Some(PathBuf::from("/")));
        let listed = mount_points_from_table(table);
        assert!(!listed.iter().any(|m| m == Path::new("/mnt/x")));
        assert!(listed.iter().any(|m| m == Path::new("/mnt/y")));
        assert!(!listed.iter().any(|m| m == Path::new("/run/user/1000/doc")));
    }

    #[test]
    fn discovery_lists_vm_container_and_network_mounts() {
        let listed = [
            "virtiofs",
            "9p",
            "fuse.sshfs",
            "fuse.rclone",
            "overlay",
            "nfs4",
            "cifs",
            "tmpfs",
            "ramfs",
            "zfs",
            "btrfs",
            "xfs",
            "ext4",
            "vfat",
        ];
        let skipped = [
            "proc",
            "sysfs",
            "selinuxfs",
            "nfsd",
            "fuse.lxcfs",
            "fuse.portal",
            "fuse.gvfsd-fuse",
            "squashfs",
            "fuse.snapfuse",
            "cgroup2",
        ];
        let mut table = String::from("/dev/sda1 / ext4 rw 0 0\n");
        for (i, t) in listed.iter().chain(skipped.iter()).enumerate() {
            table.push_str(&format!("src{i} /m/{i} {t} rw 0 0\n"));
        }
        let mps = mount_points_from_table(&table);
        for (i, t) in listed.iter().enumerate() {
            let mp = PathBuf::from(format!("/m/{i}"));
            assert!(mps.contains(&mp), "{t} is listed");
            assert_eq!(
                volume_top_from_table(&mp.join("f"), &table),
                Some(mp),
                "{t} keeps its files in its own trash"
            );
        }
        for (j, t) in skipped.iter().enumerate() {
            let mp = PathBuf::from(format!("/m/{}", listed.len() + j));
            assert!(!mps.contains(&mp), "{t} is skipped");
            assert_eq!(volume_top_from_table(&mp.join("f"), &table), None, "{t}");
        }
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

    /// Levels for the deep copy. macOS CI hung past 25 minutes at 20,000,
    /// so that runner stops at 1,000. Either depth is past `PATH_MAX`.
    #[cfg(unix)]
    const DEEP_COPY: usize = if cfg!(target_os = "macos") {
        1_000
    } else {
        20_000
    };

    /// A chain of `levels` directories named `d` under `root`, with `leaf`
    /// written at the bottom. Built with `mkdirat`: the path passes `PATH_MAX`.
    #[cfg(unix)]
    fn deep_chain(root: &Path, levels: usize) {
        let c = path_cstr(root).unwrap();
        // SAFETY: descriptor calls on NUL-terminated names; each descriptor
        // is closed once the next one is open.
        unsafe {
            let mut fd = libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY);
            assert!(fd >= 0, "{}", io::Error::last_os_error());
            for _ in 0..levels {
                assert_eq!(libc::mkdirat(fd, c"d".as_ptr(), 0o755), 0);
                let next = libc::openat(fd, c"d".as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY);
                assert!(next >= 0, "{}", io::Error::last_os_error());
                libc::close(fd);
                fd = next;
            }
            let f = libc::openat(
                fd,
                c"leaf".as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_CLOEXEC,
                0o644,
            );
            assert!(f >= 0, "{}", io::Error::last_os_error());
            assert_eq!(libc::write(f, b"leaf".as_ptr().cast(), 4), 4);
            libc::close(f);
            libc::close(fd);
        }
    }

    /// Read `leaf` at the bottom of a `deep_chain` by descriptor.
    #[cfg(unix)]
    fn read_deep_leaf(root: &Path, levels: usize) -> Vec<u8> {
        let c = path_cstr(root).unwrap();
        unsafe {
            let mut fd = libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY);
            assert!(fd >= 0);
            for _ in 0..levels {
                let next = libc::openat(fd, c"d".as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY);
                libc::close(fd);
                fd = next;
                assert!(fd >= 0);
            }
            let f = libc::openat(fd, c"leaf".as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC);
            libc::close(fd);
            assert!(f >= 0);
            let mut buf = [0u8; 8];
            let n = libc::read(f, buf.as_mut_ptr().cast(), buf.len());
            libc::close(f);
            assert!(n >= 0);
            buf[..n as usize].to_vec()
        }
    }

    #[test]
    fn path_copy_stops_short_of_path_max() {
        let limit = libc::PATH_MAX as usize;
        let name = OsStr::new("d");
        assert!(!path_fits(limit - PATH_HEADROOM, &[], name));
        assert!(path_fits(8, &[], name));
    }

    /// A short symlink prefix hides a long directory. Measuring the path as
    /// written would keep using `mkdir` after the resolved path is past the
    /// limit, which is the macOS "File name too long" failure.
    ///
    /// Long names fill the hidden part to a few hundred bytes short of the
    /// limit, so the copy crosses it within some hundred levels. With one
    /// 180-byte name, the chain took about 2000 levels on Linux, each copied
    /// by a path call that walks every level above it: 4 to 8 seconds.
    #[test]
    fn copy_through_a_symlink_prefix_past_the_resolved_limit() {
        let base = std::env::temp_dir().join(format!("rtrash-resolv-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        let limit = libc::PATH_MAX as usize;
        // Stop when one more name would leave less than 300 bytes of room
        // for `/src` and the first levels copied by path.
        let mut hidden = resolve_existing(&base).join("h");
        while hidden.as_os_str().len() + 201 + 300 < limit {
            hidden.push("L".repeat(200));
        }
        fs::create_dir_all(&hidden).unwrap();
        let short = base.join("s");
        std::os::unix::fs::symlink(&hidden, &short).unwrap();
        let src = short.join("src");
        fs::create_dir(&src).unwrap();
        let literal = src.as_os_str().len();
        let resolved = resolve_existing(&src).as_os_str().len();
        assert!(
            resolved >= literal + 100,
            "resolved {resolved} literal {literal}"
        );
        // Deep enough that the resolved path is past the limit, while the
        // path as written is still under it.
        let depth = (limit - resolved) / 2 + 2;
        let literal_at_depth = literal + depth * 2;
        assert!(
            literal_at_depth < limit,
            "literal {literal_at_depth} is already past PATH_MAX"
        );
        assert!(resolved + depth * 2 >= limit);
        // Some levels go by path and the rest by descriptor, and the
        // chain stays short.
        assert!(resolved + PATH_HEADROOM + 8 < limit, "resolved {resolved}");
        assert!(depth < 400, "depth {depth}");
        deep_chain(&src, depth);
        let meta = fs::symlink_metadata(&src).unwrap();
        let dst = short.join("dst");
        copy_recursive(&src, &dst, &meta).unwrap();
        assert_eq!(read_deep_leaf(&dst, depth), b"leaf");
        remove_any_path(&src).unwrap();
        remove_any_path(&dst).unwrap();
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn copy_and_delete_a_tree_deeper_than_the_stack() {
        // A put across devices checks the tree, copies it, and deletes the
        // original. Recursing on any of those three overflowed the stack,
        // and paths this deep no longer fit in PATH_MAX.
        let root = std::env::temp_dir().join(format!(
            "rtrash-deepcopy-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&root);
        let src = root.join("src");
        fs::create_dir_all(&src).unwrap();
        deep_chain(&src, DEEP_COPY);
        let meta = fs::symlink_metadata(&src).unwrap();
        tree_removable(&src, &meta).unwrap();
        let dst = root.join("dst");
        copy_recursive(&src, &dst, &meta).unwrap();
        assert_eq!(read_deep_leaf(&dst, DEEP_COPY), b"leaf");
        // Cleanup of a partial copy takes the same walk.
        make_tree_writable(&dst);
        remove_any_path(&src).unwrap();
        remove_any_path(&dst).unwrap();
        assert!(!src.exists());
        assert!(!dst.exists());
        let _ = fs::remove_dir_all(&root);
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
    fn tree_removable_stops_at_another_device() {
        let root = std::env::temp_dir().join(format!(
            "rtrash-xdev-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&root);
        let tree = root.join("t");
        let mnt = tree.join("mnt");
        fs::create_dir_all(&mnt).unwrap();
        let mounted = std::process::Command::new("sudo")
            .args(["-n", "mount", "-t", "tmpfs", "tmpfs"])
            .arg(&mnt)
            .status()
            .is_ok_and(|s| s.success());
        if !mounted {
            let _ = fs::remove_dir_all(&root);
            return;
        }
        struct Umount<'a>(&'a Path);
        impl Drop for Umount<'_> {
            fn drop(&mut self) {
                let _ = std::process::Command::new("sudo")
                    .args(["-n", "umount"])
                    .arg(self.0)
                    .status();
            }
        }
        let _umount = Umount(&mnt);
        fs::write(mnt.join("precious"), b"keep").unwrap();
        let err = tree_removable(&tree, &fs::symlink_metadata(&tree).unwrap()).unwrap_err();
        assert_eq!(fs::read(mnt.join("precious")).unwrap(), b"keep");
        drop(_umount);
        assert!(
            err.to_string().contains("another device"),
            "refused the mount: {err}"
        );
        let _ = fs::remove_dir_all(&root);
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
    fn unlink_blocker_reads_the_sticky_bit_and_the_file_flags() {
        let node = |mode: libc::mode_t, uid: libc::uid_t, pinned: bool| Node { mode, uid, pinned };
        let (me, other) = (1000, 2000);
        let plain = node(0o40755, me, false);
        let sticky_mine = node(0o41777, me, false);
        let sticky_theirs = node(0o41777, other, false);
        let theirs = node(0o100644, other, false);
        let mine = node(0o100644, me, false);
        assert_eq!(unlink_blocker(plain, theirs, me), None);
        assert_eq!(unlink_blocker(sticky_mine, theirs, me), None);
        assert_eq!(unlink_blocker(sticky_theirs, mine, me), None);
        assert!(unlink_blocker(sticky_theirs, theirs, me).is_some());
        assert_eq!(
            unlink_blocker(sticky_theirs, theirs, 0),
            None,
            "root passes the sticky bit"
        );
        let flagged = node(0o100644, me, true);
        assert!(
            unlink_blocker(plain, flagged, 0).is_some(),
            "a flag binds root too"
        );
        assert!(unlink_blocker(node(0o40755, me, true), mine, me).is_some());
    }

    #[test]
    fn tree_removable_spots_a_parent_that_does_not_allow_deleting() {
        let root = std::env::temp_dir().join(format!("rtrash-ro-parent-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("p/d")).unwrap();
        fs::write(root.join("p/d/f"), "x").unwrap();
        fs::write(root.join("p/x"), "x").unwrap();
        let (d, x) = (root.join("p/d"), root.join("p/x"));
        assert!(tree_removable(&d, &fs::symlink_metadata(&d).unwrap()).is_ok());
        assert!(tree_removable(&x, &fs::symlink_metadata(&x).unwrap()).is_ok());
        fs::set_permissions(root.join("p"), fs::Permissions::from_mode(0o555)).unwrap();
        // Root may write anywhere, so only an ordinary user sees the refusal.
        if unsafe { libc::geteuid() } != 0 {
            for item in [&d, &x] {
                let err = tree_removable(item, &fs::symlink_metadata(item).unwrap()).unwrap_err();
                assert_eq!(
                    err.kind(),
                    io::ErrorKind::PermissionDenied,
                    "{}",
                    item.display()
                );
            }
        }
        fs::set_permissions(root.join("p"), fs::Permissions::from_mode(0o755)).unwrap();
        let _ = fs::remove_dir_all(&root);
    }

    fn move_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("rtrash-mbc-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src/sub")).unwrap();
        fs::write(root.join("src/a"), b"first").unwrap();
        fs::write(root.join("src/sub/b"), b"second").unwrap();
        root
    }

    /// A copy that fails partway takes back what it made, and the source
    /// stays whole; a path already at the destination is never touched.
    #[test]
    fn a_move_by_copy_that_fails_takes_back_its_partial_copy() {
        let root = move_root("partial");
        let (src, dest) = (root.join("src"), root.join("dest"));
        let _l = std::os::unix::net::UnixListener::bind(src.join("sub/s")).unwrap();
        let meta = fs::symlink_metadata(&src).unwrap();
        let err = move_by_copy(&src, &dest, &meta).unwrap_err();
        assert!(source_left(&err).is_none(), "{err}");
        assert!(dest.symlink_metadata().is_err(), "the partial copy went");
        assert_eq!(fs::read(src.join("a")).unwrap(), b"first");
        assert_eq!(fs::read(src.join("sub/b")).unwrap(), b"second");

        fs::write(&dest, b"theirs").unwrap();
        let err = move_by_copy(&src, &dest, &meta).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists, "{err}");
        assert_eq!(fs::read(&dest).unwrap(), b"theirs");
        assert_eq!(fs::read(src.join("sub/b")).unwrap(), b"second");
        let _ = fs::remove_dir_all(&root);
    }

    /// trash-cli #429: once the copy is whole, a removal of the source
    /// that fails partway keeps the copy, and the error says so.
    #[test]
    fn a_move_by_copy_keeps_a_whole_copy_when_the_source_cannot_all_go() {
        let root = move_root("left");
        let (src, dest) = (root.join("src"), root.join("dest"));
        let stuck = OsStr::new("stuck-in-a-move-by-copy");
        fs::write(src.join("sub").join(stuck), b"third").unwrap();
        crate::fastdelete::refuse_in_tests(stuck);
        let meta = fs::symlink_metadata(&src).unwrap();
        let err = move_by_copy(&src, &dest, &meta).unwrap_err();
        let left = source_left(&err).expect("the copy is whole");
        assert_eq!(left.dest, dest);
        assert_eq!(fs::read(dest.join("a")).unwrap(), b"first");
        assert_eq!(fs::read(dest.join("sub/b")).unwrap(), b"second");
        assert_eq!(fs::read(dest.join("sub").join(stuck)).unwrap(), b"third");
        assert!(
            src.join("sub").join(stuck).exists(),
            "the refused entry stays in the source"
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
    fn a_copy_counts_its_bytes_into_the_watch() {
        let root = std::env::temp_dir().join(format!("rtrash-copybytes-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let src = root.join("src");
        fs::create_dir_all(src.join("sub")).unwrap();
        fs::write(src.join("a"), vec![1u8; 9000]).unwrap();
        fs::write(src.join("sub/b"), vec![2u8; 30_000]).unwrap();
        std::os::unix::fs::symlink("a", src.join("link")).unwrap();
        let meta = fs::symlink_metadata(&src).unwrap();
        let w = crate::interrupt::Watch::default();
        crate::interrupt::watched(&w, || copy_recursive(&src, &root.join("dst"), &meta)).unwrap();
        // File bytes only: directories and the link add nothing.
        assert_eq!(w.copied.load(std::sync::atomic::Ordering::SeqCst), 39_000);
        assert_eq!(fs::read(root.join("dst/sub/b")).unwrap().len(), 30_000);
        let _ = fs::remove_dir_all(&root);
    }

    /// The explorer stops a put across devices partway through: the copy
    /// ends at its next check, the part copied and the record go, and the
    /// original stays whole. A file and a directory each, with the stop
    /// set before the first chunk and before the first entry.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_stopped_watch_ends_a_put_across_devices_and_keeps_the_original() {
        use std::os::unix::fs::MetadataExt;
        let root = std::env::temp_dir().join(format!("rtrash-watchstop-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("d/sub")).unwrap();
        fs::write(root.join("f"), vec![3u8; 100_000]).unwrap();
        fs::write(root.join("d/sub/x"), vec![4u8; 5000]).unwrap();
        let shm = Path::new("/dev/shm");
        let dev = fs::metadata(&root).unwrap().dev();
        if !fs::metadata(shm).is_ok_and(|m| m.dev() != dev) {
            let _ = fs::remove_dir_all(&root);
            eprintln!("no second file system here; skipping");
            return;
        }
        let td = TrashDir {
            root: shm.join(format!("rtrash-watchstop-{}", std::process::id())),
            topdir: None,
        };
        let _ = fs::remove_dir_all(&td.root);
        td.ensure().unwrap();
        let w = crate::interrupt::Watch::default();
        w.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        for item in [root.join("f"), root.join("d")] {
            let meta = fs::symlink_metadata(&item).unwrap();
            let err =
                crate::interrupt::watched(&w, || trash_move_locked(&item, &meta, &td)).unwrap_err();
            assert!(err.to_string().contains("original stays"), "{err}");
            assert_eq!(fs::read_dir(td.files()).unwrap().count(), 0, "{err}");
            assert_eq!(fs::read_dir(td.info()).unwrap().count(), 0, "{err}");
        }
        assert_eq!(fs::read(root.join("f")).unwrap().len(), 100_000);
        assert_eq!(fs::read(root.join("d/sub/x")).unwrap().len(), 5000);
        // Without the stop, the same put goes through.
        w.stop.store(false, std::sync::atomic::Ordering::SeqCst);
        let meta = fs::symlink_metadata(root.join("d")).unwrap();
        crate::interrupt::watched(&w, || trash_move_locked(&root.join("d"), &meta, &td)).unwrap();
        assert!(!root.join("d").exists());
        assert_eq!(w.copied.load(std::sync::atomic::Ordering::SeqCst), 5000);
        let _ = fs::remove_dir_all(&td.root);
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
        let l = directorysizes_parse_line(b"1234 1700000000 mydir").unwrap();
        assert_eq!((l.size, l.mtime), (1234, 1_700_000_000));
        assert_eq!(l.name, b"mydir");
        assert!(directorysizes_parse_line(b"").is_none());
        assert!(directorysizes_parse_line(b"not-a-number 1 x").is_none());
        assert!(directorysizes_parse_line(b"1 2").is_none()); // missing name
    }

    /// The spec's example line, and the encodings other writers may use
    /// for one name: none past `%` and newline, lower-case hex, raw bytes
    /// that are not UTF-8. A name that is not a child of `files/` is no
    /// line at all, `%2F` included.
    #[test]
    fn directorysizes_lines_read_every_spec_encoding() {
        let name = |line: &[u8]| directorysizes_parse_line(line).map(|l| l.name);
        let l = directorysizes_parse_line(b"16384 15803468 Documents").unwrap();
        assert_eq!(
            (l.size, l.mtime, &l.name[..]),
            (16384, 15803468, &b"Documents"[..])
        );
        assert_eq!(name(b"1 2 My Dir").unwrap(), b"My Dir");
        assert_eq!(name(b"1 2 My%20Dir").unwrap(), b"My Dir");
        assert_eq!(name(b"1 2  lead").unwrap(), b" lead");
        assert_eq!(name(b"1 2 Zo%c3%ab").unwrap(), "Zo\u{eb}".as_bytes());
        assert_eq!(
            name("1 2 Zo\u{eb}".as_bytes()).unwrap(),
            "Zo\u{eb}".as_bytes()
        );
        assert_eq!(name(b"1 2 d\xff").unwrap(), b"d\xff");
        assert_eq!(name(b"1 2 100%25").unwrap(), b"100%");
        assert_eq!(directorysizes_parse_line(b"1 -5 x").unwrap().mtime, -5);
        for bad in [
            &b"1 2 a%2Fb"[..],
            b"1 2 a/b",
            b"1 2 /abs",
            b"1 2 .",
            b"1 2 %2E%2E",
            b"1 2 a%00b",
            b"1 2 ",
            b"+1 2 x",
            b"-1 2 x",
            b" 1 2 x",
            b"1 2.5 x",
            b"1 x y",
            b"99999999999999999999 2 x",
        ] {
            assert_eq!(name(bad), None, "{:?}", String::from_utf8_lossy(bad));
        }
    }

    fn sizes_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "rtrash-ds-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("files")).unwrap();
        fs::create_dir_all(root.join("info")).unwrap();
        root
    }

    /// Lines another tool wrote in its own encoding are the same lines to
    /// rtrash: a put replaces one, rm drops one, empty keeps one, and the
    /// rewrite leaves one line per name in one encoding.
    #[test]
    fn directorysizes_updates_match_lines_other_writers_encoded() {
        let root = sizes_root("foreign");
        let trash = TrashDir {
            root: root.clone(),
            topdir: None,
        };
        let path = root.join("directorysizes");
        let foreign: &[u8] = b"10 1 My Dir\n20 2 Zo%c3%ab\n30 3 d\xff\n40 4 My%20Dir\n";
        fs::write(&path, foreign).unwrap();
        directorysizes_upsert(&trash, "Zo\u{eb}", 99, 9).unwrap();
        let after = fs::read(&path).unwrap();
        assert_ne!(after, foreign, "the put must rewrite the cache");
        // The later `My%20Dir` line won over `My Dir`; the old Zo\u{eb}
        // line is gone, not kept beside the new one.
        assert_eq!(
            String::from_utf8(after).unwrap(),
            "40 4 My%20Dir\n30 3 d%FF\n99 9 Zo%C3%AB\n"
        );

        let before = fs::read(&path).unwrap();
        directorysizes_remove(&trash, "My Dir");
        let after = fs::read(&path).unwrap();
        assert_ne!(after, before, "rm must drop the line");
        assert_eq!(after, b"30 3 d%FF\n99 9 Zo%C3%AB\n");

        let raw = crate::util::str_from_os_ref(OsStr::from_bytes(b"d\xff"));
        directorysizes_keep(&trash, &[raw]);
        assert_eq!(fs::read(&path).unwrap(), b"30 3 d%FF\n");
        directorysizes_keep(&trash, &[]);
        assert!(!path.exists(), "a cache with no lines goes");
        let _ = fs::remove_dir_all(&root);
    }

    /// A status reads a line another tool wrote with little encoding, and
    /// a line whose `.trashinfo` changed since is walked again.
    #[test]
    fn directorysizes_cache_hits_a_minimally_encoded_line() {
        let root = sizes_root("minimal");
        let trash = TrashDir {
            root: root.clone(),
            topdir: None,
        };
        fs::create_dir_all(root.join("files/My Dir")).unwrap();
        let info = root.join("info/My Dir.trashinfo");
        fs::write(
            &info,
            "[Trash Info]\nPath=/orig/My%20Dir\nDeletionDate=2020-01-01T00:00:00\n",
        )
        .unwrap();
        let mtime = info_mtime_secs(&info).unwrap();
        fs::write(
            root.join("directorysizes"),
            format!("999000000 {mtime} My Dir\n"),
        )
        .unwrap();
        assert_eq!(
            directorysizes_cached_size(&trash, "My Dir"),
            Some(999_000_000)
        );
        fs::write(
            root.join("directorysizes"),
            format!("999000000 {} My Dir\n", mtime - 1),
        )
        .unwrap();
        assert_eq!(directorysizes_cached_size(&trash, "My Dir"), None);
        let _ = fs::remove_dir_all(&root);
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

    /// A trash whose `files/` or `info/` is a symbolic link takes
    /// nothing, as in trash-cli 0.26.9: put fails before it moves the
    /// item, and the folder the link points at stays empty. On a volume
    /// such a trash is skipped, so the item goes to the fallback.
    #[test]
    fn a_symlinked_files_or_info_dir_takes_nothing() {
        let root = std::env::temp_dir().join(format!("rtrash-linkedsub-{}", std::process::id()));
        for sub in ["files", "info"] {
            let _ = fs::remove_dir_all(&root);
            let elsewhere = root.join("elsewhere");
            fs::create_dir_all(&elsewhere).unwrap();
            let td = TrashDir {
                root: root.join("Trash"),
                topdir: None,
            };
            fs::create_dir_all(&td.root).unwrap();
            std::os::unix::fs::symlink(&elsewhere, td.root.join(sub)).unwrap();
            let err = td.ensure().unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{sub}");
            assert!(err.to_string().contains("is a symbolic link"), "{err}");
            // It refuses before it makes the other folder.
            assert_eq!(fs::read_dir(&td.root).unwrap().count(), 1, "{sub}");
            assert!(ensure_home(&td).is_err(), "{sub}");
            let item = root.join("item.txt");
            fs::write(&item, b"keep").unwrap();
            let meta = fs::symlink_metadata(&item).unwrap();
            assert!(trash_move(&item, &meta, &td).is_err(), "{sub}");
            assert!(item.is_file(), "{sub}");
            assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0, "{sub}");
            // A volume trash like that is not used; the caller falls back.
            let top = root.join("vol");
            let vt = top.join(format!(".Trash-{}", uid()));
            fs::create_dir_all(&vt).unwrap();
            std::os::unix::fs::symlink(&elsewhere, vt.join(sub)).unwrap();
            assert!(select_on_volume(&top, uid()).is_none(), "{sub}");
            assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0, "{sub}");
        }
        let _ = fs::remove_dir_all(&root);
    }

    /// A write error other than a name clash ends the put at once, as
    /// trash-cli 0.26.9 stops on EACCES, EPERM, ENOSPC, EDQUOT and EROFS:
    /// only `AlreadyExists` tries the next name. A read-only `info/`
    /// stands in for the rest, which a test cannot cause on demand.
    #[test]
    fn a_write_error_ends_the_put_without_trying_more_names() {
        // Root may write anywhere, so the mode would not refuse it.
        if uid() == 0 {
            return;
        }
        let root = std::env::temp_dir().join(format!("rtrash-infoeacces-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let td = TrashDir {
            root: root.join("Trash"),
            topdir: None,
        };
        td.ensure().unwrap();
        let item = root.join("item.txt");
        fs::write(&item, b"keep").unwrap();
        fs::set_permissions(td.info(), fs::Permissions::from_mode(0o500)).unwrap();
        let meta = fs::symlink_metadata(&item).unwrap();
        let err = trash_move_locked(&item, &meta, &td).unwrap_err();
        fs::set_permissions(td.info(), fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
        assert!(item.is_file());
        assert_eq!(fs::read_dir(td.info()).unwrap().count(), 0);
        assert_eq!(fs::read_dir(td.files()).unwrap().count(), 0);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn ensure_home_refuses_a_read_only_files_dir() {
        let root = std::env::temp_dir().join(format!("rtrash-homero-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let td = TrashDir {
            root: root.join("Trash"),
            topdir: None,
        };
        ensure_home(&td).unwrap();
        fs::set_permissions(td.files(), fs::Permissions::from_mode(0o555)).unwrap();
        // Root may write anywhere, so only an ordinary user sees the refusal.
        if unsafe { libc::geteuid() } != 0 {
            let err = ensure_home(&td).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
            assert!(
                err.to_string().contains("home trash is not writable"),
                "{err}"
            );
        }
        fs::set_permissions(td.files(), fs::Permissions::from_mode(0o700)).unwrap();
        let _ = fs::remove_dir_all(&root);
    }

    /// A record removal takes a regular file and refuses a folder (with
    /// what is in it) and a link, leaving the link's target too.
    #[test]
    fn remove_record_takes_only_a_regular_file() {
        let root = std::env::temp_dir().join(format!("rtrash-rmrecord-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("d.trashinfo")).unwrap();
        fs::write(root.join("d.trashinfo/inner"), b"x").unwrap();
        fs::write(root.join("target"), b"t").unwrap();
        std::os::unix::fs::symlink(root.join("target"), root.join("s.trashinfo")).unwrap();
        fs::write(root.join("f.trashinfo"), b"[Trash Info]\n").unwrap();
        for name in ["d.trashinfo", "s.trashinfo"] {
            let e = remove_record(&root.join(name)).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidInput, "{name}");
            assert!(root.join(name).symlink_metadata().is_ok(), "{name}");
        }
        assert!(root.join("d.trashinfo/inner").is_file());
        assert_eq!(fs::read(root.join("target")).unwrap(), b"t");
        remove_record(&root.join("f.trashinfo")).unwrap();
        assert!(!root.join("f.trashinfo").exists());
        let _ = fs::remove_dir_all(&root);
    }

    /// record_dir_size sizes a trashed directory outside the lock, then
    /// writes its line under it only for the record it sized. A put that
    /// gave the name to another item meanwhile gets no line with the old
    /// size (round 244).
    #[test]
    fn a_dir_size_skips_a_name_taken_during_the_walk() {
        let (root, td) = put_trash("dirsize-retaken");
        let item = root.join("d");
        fs::create_dir(&item).unwrap();
        fs::write(item.join("f"), vec![7u8; 64 * 1024]).unwrap();
        let meta = fs::symlink_metadata(&item).unwrap();
        let name = trash_move_locked(&item, &meta, &td).unwrap();
        let (files, info_dir, again) = (td.files(), td.info(), name.clone());
        AFTER_WALK.set(Some(Box::new(move || {
            // Another run takes the item out, and a put gives its name to
            // another, empty directory.
            fs::remove_dir_all(join_name(&files, &again)).unwrap();
            let record = join_info(&info_dir, &again);
            fs::remove_file(&record).unwrap();
            fs::create_dir(join_name(&files, &again)).unwrap();
            let body = info::render_batch(&item, &info::now_local_string(), Some("1-1"));
            fs::write(&record, body).unwrap();
        })));
        record_dir_size(&td, &name);
        let sizes = td.root.join("directorysizes");
        let lines = fs::read_to_string(&sizes).unwrap_or_default();
        assert!(!lines.contains(&name), "{lines}");
        // With nothing in between, the line is written.
        record_dir_size(&td, &name);
        let lines = fs::read_to_string(&sizes).unwrap_or_default();
        assert!(lines.contains(&name), "{lines}");
        let _ = fs::remove_dir_all(&root);
    }

    /// A second blocking lock of a root this thread holds panics in a
    /// debug build and names both call sites, instead of waiting forever;
    /// try_acquire only fails, and the lock is free again once dropped
    /// (round 245).
    #[cfg(debug_assertions)]
    #[test]
    fn a_second_lock_of_a_held_root_panics_instead_of_waiting() {
        let (root, td) = put_trash("relock-panic");
        let (other_root, other) = put_trash("relock-other");
        let held = TrashLock::acquire(&td.root).unwrap();
        // Another root is another lock.
        drop(TrashLock::acquire(&other.root).unwrap());
        assert!(TrashLock::try_acquire(&td.root).is_err());
        // The same root spelled another way is still the same lock file.
        let spelled = td.root.join("files").join("..");
        let again = std::panic::catch_unwind(|| TrashLock::acquire(&spelled).map(drop));
        let payload = again.expect_err("a second lock must panic, not return");
        let msg = payload
            .downcast_ref::<String>()
            .cloned()
            .unwrap_or_default();
        assert!(msg.contains("already holds the trash lock"), "{msg}");
        assert!(msg.contains("trashdir.rs"), "{msg}");
        drop(held);
        drop(TrashLock::acquire(&td.root).unwrap());
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&other_root);
    }
}
