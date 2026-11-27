//! Bounded looks at mounted file systems (glib #605).
//!
//! A stat on a hard NFS mount whose server is gone waits until the
//! server answers. `list`, `empty`, `status` and `du --devices` look at
//! every mount, so one dead share could hang them all. Each such look goes
//! through [`probe_typed`] (or [`probe`], which finds the type): on a local
//! file system it runs at once, and on a network one it runs on a thread
//! of its own. A look that has not answered within [`wait`] is given up,
//! with one note on stderr per mount; its thread is left to finish, or to
//! end with the process.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// File system types served over a network, whose server may stop
/// answering.
const NETWORK_FS: &[&str] = &[
    "nfs",
    "nfs4",
    "cifs",
    "smb3",
    "smbfs",
    "ncpfs",
    "afs",
    "afpfs",
    "webdav",
    "ceph",
    "glusterfs",
    "lustre",
    "gpfs",
    "beegfs",
    "9p",
    "davfs",
    "fuse.sshfs",
    "fuse.rclone",
    "fuse.glusterfs",
    "fuse.davfs2",
    "fuse.s3fs",
];

/// Whether `fstype` is served over a network.
pub fn is_network_fs(fstype: &str) -> bool {
    NETWORK_FS.contains(&fstype)
}

/// How long a look at a network mount may take: `RTRASH_MOUNT_WAIT`
/// seconds (fractions allowed, at most an hour), else 3.
pub fn wait() -> Duration {
    static WAIT: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *WAIT.get_or_init(|| {
        std::env::var("RTRASH_MOUNT_WAIT")
            .ok()
            .and_then(|v| v.trim().parse::<f64>().ok())
            .filter(|s| (0.0..=3600.0).contains(s))
            .map(Duration::from_secs_f64)
            .unwrap_or(Duration::from_secs(3))
    })
}

/// The type of the file system `path` is on, from the mount table: that
/// of the deepest mount point above it; empty when unknown.
pub fn fs_type_of(path: &Path) -> String {
    static TABLE: std::sync::OnceLock<Vec<(PathBuf, String)>> = std::sync::OnceLock::new();
    // A test pins the table on this thread. The lookup below is the one
    // both that pin and the host table go through.
    let fixture = fixture_text().map(|text| crate::trashdir::mount_table(&text));
    let table = if let Some(ref rows) = fixture {
        rows
    } else {
        TABLE.get_or_init(|| {
            crate::trashdir::read_mount_table()
                .map(|t| crate::trashdir::mount_table(&t))
                .unwrap_or_default()
        })
    };
    type_from_mounts(path, table)
}

/// The type `table` gives `path`: the deepest mount point above it, or
/// empty when that point is not one of the rows.
fn type_from_mounts(path: &Path, table: &[(PathBuf, String)]) -> String {
    let points: Vec<PathBuf> = table.iter().map(|(m, _)| m.clone()).collect();
    let top = crate::trashdir::longest_mount_prefix(path, &points);
    table
        .iter()
        .find(|(m, _)| *m == top)
        .map(|(_, t)| t.clone())
        .unwrap_or_default()
}

/// Mount-table text a test installs for this thread. Production reads
/// the host table; there is nothing to pin.
fn fixture_text() -> Option<String> {
    #[cfg(test)]
    {
        TABLE_FIXTURE.with(|slot| slot.borrow().clone())
    }
    #[cfg(not(test))]
    {
        None
    }
}

#[cfg(test)]
thread_local! {
    static TABLE_FIXTURE: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

/// [`probe_typed`] with the type the mount table gives `path`.
pub fn probe<T: Send + 'static>(
    path: &Path,
    look: impl FnOnce(&Path) -> T + Send + 'static,
) -> Option<T> {
    probe_typed(path, &fs_type_of(path), look)
}

/// Run `look` on `path`, on a file system of type `fstype`. On a network
/// file system it gets [`wait`] to answer; `None` when it does not, and
/// stderr says once which mount was skipped.
pub fn probe_typed<T: Send + 'static>(
    path: &Path,
    fstype: &str,
    look: impl FnOnce(&Path) -> T + Send + 'static,
) -> Option<T> {
    let stalled = stalls(path);
    if !stalled && !is_network_fs(fstype) {
        return Some(look(path));
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let owned = path.to_path_buf();
    let started = std::thread::Builder::new()
        .name("mount-probe".into())
        .spawn(move || {
            if stalled {
                // A dead server: the look never returns.
                loop {
                    std::thread::sleep(Duration::from_secs(3600));
                }
            }
            let _ = tx.send(look(&owned));
        });
    if started.is_err() {
        skipped(path, fstype, "no thread could be started to look at it");
        return None;
    }
    match rx.recv_timeout(wait()) {
        Ok(v) => Some(v),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            let why = format!("it did not answer within {:.1} s", wait().as_secs_f64());
            skipped(path, fstype, &why);
            None
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => None,
    }
}

/// Say once per mount that a look at `path` was given up.
fn skipped(path: &Path, fstype: &str, why: &str) {
    static SAID: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());
    let mut said = SAID.lock().unwrap_or_else(|e| e.into_inner());
    if said.iter().any(|p| p == path) {
        return;
    }
    said.push(path.to_path_buf());
    let kind = if fstype.is_empty() { "mount" } else { fstype };
    eprintln!(
        "rtrash: warning: skipping {kind} '{}': {why} (RTRASH_MOUNT_WAIT sets the wait)",
        crate::util::term_path(path)
    );
}

/// `RTRASH_TEST_STALL_MOUNT`, in debug builds only: the mount point whose
/// looks never answer, as on a hard NFS mount of a dead server.
#[cfg(debug_assertions)]
fn stalls(path: &Path) -> bool {
    std::env::var_os("RTRASH_TEST_STALL_MOUNT").is_some_and(|v| Path::new(&v) == path)
}

#[cfg(not(debug_assertions))]
fn stalls(_path: &Path) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_types_are_known_and_local_ones_are_not() {
        assert!(is_network_fs("nfs4"));
        assert!(is_network_fs("fuse.sshfs"));
        assert!(!is_network_fs("ext4"));
        assert!(!is_network_fs("tmpfs"));
        assert!(!is_network_fs(""));
    }

    #[test]
    fn a_local_look_runs_at_once() {
        let got = probe_typed(Path::new("/"), "ext4", |p| p.to_path_buf());
        assert_eq!(got, Some(PathBuf::from("/")));
    }

    /// The give-up bound is latched on the first network look. Both tests
    /// pin it first, so a parallel run still waits 0.2 s rather than 3.
    fn pin_mount_wait() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| std::env::set_var("RTRASH_MOUNT_WAIT", "0.2"));
    }

    #[test]
    fn a_network_look_that_answers_is_kept() {
        pin_mount_wait();
        let got = probe_typed(Path::new("/"), "nfs4", |p| p.as_os_str().len());
        assert_eq!(got, Some(1));
    }

    #[test]
    fn a_network_look_that_never_answers_is_given_up() {
        pin_mount_wait();
        let start = std::time::Instant::now();
        let got = probe_typed(Path::new("/nowhere"), "nfs", |_| {
            std::thread::sleep(Duration::from_secs(3600));
        });
        assert_eq!(got, None);
        assert!(start.elapsed() < wait() + Duration::from_secs(30));
    }

    /// A fixture table, not the host's: the deepest mount wins, and a
    /// local look at a scratch directory returns what the closure built.
    #[test]
    fn a_fixture_mount_table_names_the_deeper_type() {
        let table = "\
dev /mnt/data ext4 rw 0 0
dev /mnt/data/vol ext4 rw 0 0
dev /mnt/other nfs4 rw 0 0
";
        let mounts = crate::trashdir::mount_table(table);
        let points: Vec<PathBuf> = mounts.iter().map(|(m, _)| m.clone()).collect();
        let path = Path::new("/mnt/data/vol/file");
        let top = crate::trashdir::longest_mount_prefix(path, &points);
        assert_eq!(top, Path::new("/mnt/data/vol"));
        let kind = mounts
            .iter()
            .find(|(m, _)| *m == top)
            .map(|(_, t)| t.as_str());
        assert_eq!(kind, Some("ext4"));
        let dir = std::env::temp_dir().join(format!("rtrash-mnt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(
            probe_typed(&dir, kind.unwrap(), |p| p.to_path_buf()),
            Some(dir.clone())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `fs_type_of` reads the type off the mount table. A stand-in answer
    /// such as `xyzzy`, or the type of a different row, is not that type.
    /// The table is the fixture on this thread, not the host's mounts.
    #[test]
    fn fs_type_of_reads_the_mounted_type() {
        struct Clear;
        impl Drop for Clear {
            fn drop(&mut self) {
                super::TABLE_FIXTURE.with(|slot| *slot.borrow_mut() = None);
            }
        }
        let _clear = Clear;
        super::TABLE_FIXTURE.with(|slot| {
            *slot.borrow_mut() = Some(
                "\
dev /mnt/data ext4 rw 0 0
dev /mnt/data/vol xfs rw 0 0
dev /mnt/other nfs4 rw 0 0
"
                .to_string(),
            );
        });
        assert_eq!(fs_type_of(Path::new("/mnt/data/vol/file")), "xfs");
        assert_eq!(fs_type_of(Path::new("/mnt/data/file")), "ext4");
        assert_eq!(fs_type_of(Path::new("/mnt/other/file")), "nfs4");
        // Nothing in the fixture covers this path, so the type is empty.
        assert_eq!(fs_type_of(Path::new("/somewhere/else")), "");
    }
}
