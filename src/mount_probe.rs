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
    let table = TABLE.get_or_init(|| {
        std::fs::read_to_string("/proc/self/mounts")
            .map(|t| crate::trashdir::mount_table(&t))
            .unwrap_or_default()
    });
    let points: Vec<PathBuf> = table.iter().map(|(m, _)| m.clone()).collect();
    let top = crate::trashdir::longest_mount_prefix(path, &points);
    table
        .iter()
        .find(|(m, _)| *m == top)
        .map(|(_, t)| t.clone())
        .unwrap_or_default()
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

    #[test]
    fn a_network_look_that_answers_is_kept() {
        let got = probe_typed(Path::new("/"), "nfs4", |p| p.as_os_str().len());
        assert_eq!(got, Some(1));
    }

    #[test]
    fn a_network_look_that_never_answers_is_given_up() {
        let start = std::time::Instant::now();
        let got = probe_typed(Path::new("/nowhere"), "nfs", |_| {
            std::thread::sleep(Duration::from_secs(3600));
        });
        assert_eq!(got, None);
        assert!(start.elapsed() < wait() + Duration::from_secs(30));
    }
}
