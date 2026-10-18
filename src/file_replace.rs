//! Whole-file writes. Every file rtrash writes whole goes through here:
//! the bytes go into a new file beside the target, which is flushed to
//! disk and renamed over it. A full disk, a crash or a fill that fails
//! leaves the file that was there (or none), never a cut-off one.
//!
//! `scripts/precheck.py` refuses `fs::write`, `File::create` and
//! `.truncate(true)` anywhere else outside tests. Appends (a history
//! line, a shell profile block) and reservations with `create_new` are
//! not whole-file writes and stay where they are.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// What a path that names no regular file gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Special {
    /// Written in place: a pipe, `/dev/stdout` or a symlink the user
    /// named as an output, which a rename would replace.
    InPlace,
    /// Replaced by the new file: rtrash's own files, where a link that
    /// someone else planted must never be written through.
    Replace,
}

/// Write `path` whole or not at all: `fill` writes into a new file
/// beside it, which is flushed to disk and renamed over `path`. A path
/// that names no regular file (a pipe, `/dev/stdout`, a symlink) is
/// written in place, since a rename would replace the link or device
/// itself. `fill` is also handed the path of the new file, so an export
/// that scans the directory it is written to can leave that file out.
pub fn replace_file<T>(
    path: &Path,
    fill: impl FnOnce(&mut dyn Write, Option<&Path>) -> io::Result<T>,
) -> io::Result<T> {
    replace_with(path, Special::InPlace, fill)
}

/// [`replace_file`] with `data` as the whole body, for a file the user
/// asked for (a completion script, a timer unit).
pub fn write_whole(path: &Path, data: &[u8]) -> io::Result<()> {
    replace_file(path, |w, _| w.write_all(data))
}

/// `data` as the whole body of a file rtrash keeps for itself (a cache,
/// a total, a log). Unlike [`write_whole`], a link or any other non-file
/// at `path` is replaced, never written through: a trash on a shared
/// volume may hold a link another user put there.
pub fn write_own(path: &Path, data: &[u8]) -> io::Result<()> {
    replace_with(path, Special::Replace, |w, _| w.write_all(data))
}

fn replace_with<T>(
    path: &Path,
    special: Special,
    fill: impl FnOnce(&mut dyn Write, Option<&Path>) -> io::Result<T>,
) -> io::Result<T> {
    if special == Special::InPlace
        && fs::symlink_metadata(path).is_ok_and(|m| !m.file_type().is_file())
    {
        let mut w = io::BufWriter::new(fs::File::create(path)?);
        let done = fill(&mut w, None)?;
        w.flush()?;
        return Ok(done);
    }
    let tmp = temp_beside(path);
    let f = match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
    {
        // Left by a run that crashed with this process id.
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            fs::remove_file(&tmp)?;
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)?
        }
        other => other?,
    };
    let done = fill_and_rename(f, &tmp, path, fill);
    if done.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    done
}

/// The body of [`replace_with`]: `fill` into `f`, opened at `tmp`, then
/// `tmp` over `path`.
fn fill_and_rename<T>(
    f: fs::File,
    tmp: &Path,
    path: &Path,
    fill: impl FnOnce(&mut dyn Write, Option<&Path>) -> io::Result<T>,
) -> io::Result<T> {
    let mut w = io::BufWriter::new(f);
    let done = fill(&mut w, Some(tmp))?;
    let f = w.into_inner().map_err(io::IntoInnerError::into_error)?;
    f.sync_all()?;
    drop(f);
    // A regular file written over keeps its mode.
    if let Ok(m) = fs::symlink_metadata(path) {
        if m.file_type().is_file() {
            fs::set_permissions(tmp, m.permissions())?;
        }
    }
    fs::rename(tmp, path)?;
    Ok(done)
}

/// `.NAME.PID.tmp` in the directory of `path`.
pub(crate) fn temp_beside(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map_or_else(|| "rtrash".into(), |n| n.to_string_lossy());
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    dir.join(format!(".{name}.{}.tmp", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rtrash-replace-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn names(d: &Path) -> Vec<String> {
        let mut out: Vec<String> = fs::read_dir(d)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        out.sort();
        out
    }

    /// Both writers put the whole body in place and leave no temporary,
    /// over an old file, over a temporary a crashed run left, and where
    /// nothing was.
    #[test]
    fn a_whole_write_replaces_the_file_and_leaves_no_temporary() {
        let d = scratch("whole");
        let user = d.join("user.txt");
        let own = d.join("own.txt");
        fs::write(&user, "old").unwrap();
        fs::write(temp_beside(&own), "stale").unwrap();
        write_whole(&user, b"new user").unwrap();
        write_own(&own, b"new own").unwrap();
        assert_eq!(fs::read_to_string(&user).unwrap(), "new user");
        assert_eq!(fs::read_to_string(&own).unwrap(), "new own");
        assert_eq!(names(&d), ["own.txt", "user.txt"]);
        // A folder that is missing fails before anything is written.
        assert!(write_own(&d.join("no/such.txt"), b"x").is_err());
        assert_eq!(names(&d), ["own.txt", "user.txt"]);
        let _ = fs::remove_dir_all(&d);
    }

    /// A fill that fails halfway, as a scan for a snapshot can, keeps the
    /// old file and removes its temporary.
    #[test]
    fn a_failed_fill_keeps_the_old_file() {
        let d = scratch("half");
        let out = d.join("snapshot");
        fs::write(&out, "old").unwrap();
        let half = replace_file(&out, |w, tmp| {
            assert!(tmp.is_some_and(Path::is_file));
            w.write_all(b"cut")?;
            Err::<(), _>(io::Error::other("disk full"))
        });
        assert_eq!(half.unwrap_err().to_string(), "disk full");
        assert_eq!(fs::read_to_string(&out).unwrap(), "old");
        assert_eq!(names(&d), ["snapshot"]);
        let _ = fs::remove_dir_all(&d);
    }

    /// A link at the path: write_whole writes through it, as for an
    /// output the user named; write_own replaces the link and leaves
    /// what it points to alone.
    #[cfg(unix)]
    #[test]
    fn write_own_replaces_a_link_that_write_whole_writes_through() {
        let d = scratch("link");
        let target = d.join("target.txt");
        fs::write(&target, "theirs").unwrap();
        let link = d.join("link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        write_whole(&link, b"through").unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "through");
        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        write_own(&link, b"mine").unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "through");
        let meta = fs::symlink_metadata(&link).unwrap();
        assert!(meta.file_type().is_file());
        assert_eq!(fs::read_to_string(&link).unwrap(), "mine");
        assert_eq!(names(&d), ["link.txt", "target.txt"]);
        let _ = fs::remove_dir_all(&d);
    }
}
