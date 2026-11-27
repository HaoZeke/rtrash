//! The `rtrash-hud` binary, run with no terminal and no compositor.

use std::process::Command;

#[cfg(unix)]
use std::fs;
#[cfg(target_os = "linux")]
use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;
#[cfg(unix)]
use std::sync::atomic::{AtomicUsize, Ordering};

fn bin() -> &'static str {
    // Cargo sets this from the binary file name, hyphen included.
    env!("CARGO_BIN_EXE_rtrash-hud")
}

#[cfg(unix)]
struct Tmp(PathBuf);

#[cfg(unix)]
impl Tmp {
    fn fresh(tag: &str) -> PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("rtrash-hud-bin-{tag}-{}-{n}", std::process::id()))
    }

    fn dir(tag: &str) -> Self {
        let path = Self::fresh(tag);
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Tmp(path)
    }

    fn trash(tag: &str) -> Self {
        let path = Self::fresh(tag);
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(path.join("files")).unwrap();
        fs::create_dir_all(path.join("info")).unwrap();
        Tmp(path)
    }
}

#[cfg(unix)]
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn output(args: &[&str]) -> std::process::Output {
    Command::new(bin())
        .args(args)
        .output()
        .expect("run rtrash-hud")
}

#[test]
fn help_and_version_name_the_display() {
    let help = output(&["--help"]);
    let text = String::from_utf8_lossy(&help.stdout);
    assert!(help.status.success(), "{text}");
    assert!(text.starts_with("Usage: rtrash-hud"), "{text}");
    assert!(text.contains("item count and reclaimable size"), "{text}");
    assert!(text.contains("RTRASH_MACOS_TRASH=freedesktop"), "{text}");
    assert!(text.contains("no age filter"), "{text}");
    assert!(!text.contains("--offline"), "{text}");
    assert!(text.contains("rtrash status"), "{text}");

    let version = output(&["--version"]);
    let line = String::from_utf8_lossy(&version.stdout);
    assert!(version.status.success(), "{line}");
    assert!(line.contains("rtrash-hud"), "{line}");
    assert!(line.contains(rtrash::version()), "{line}");
}

#[cfg(unix)]
#[test]
fn a_pinned_trash_prints_the_library_count() {
    let tmp = Tmp::trash("catalog");
    fs::write(tmp.0.join("files").join("hello.txt"), b"hello-payload").unwrap();
    fs::write(
        tmp.0.join("info").join("hello.txt.trashinfo"),
        "[Trash Info]\nPath=/tmp/hello.txt\nDeletionDate=2026-01-02T03:04:05\n",
    )
    .unwrap();
    fs::write(tmp.0.join("files").join("loose.txt"), b"orphan-payload").unwrap();

    let out = Command::new(bin())
        .arg("--trash-dir")
        .arg(&tmp.0)
        .output()
        .expect("run rtrash-hud");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stdout={stdout} stderr={stderr}");
    assert!(stdout.contains("2 items"), "{stdout}");
    assert!(stdout.contains("1 without .trashinfo"), "{stdout}");
    assert!(stdout.contains("Total:"), "{stdout}");

    let json = Command::new(bin())
        .arg("--json")
        .arg(format!("--trash-dir={}", tmp.0.display()))
        .output()
        .expect("run rtrash-hud --json");
    let body = String::from_utf8_lossy(&json.stdout);
    assert!(
        json.status.success(),
        "{body}{}",
        String::from_utf8_lossy(&json.stderr)
    );
    assert!(body.contains(r#""items":2"#), "{body}");
    assert!(body.contains(r#""orphans":1"#), "{body}");
}

#[cfg(unix)]
#[test]
fn a_pin_that_is_not_a_trash_exits_with_usage() {
    struct Gone(PathBuf);
    impl Drop for Gone {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    let path = std::env::temp_dir().join(format!("rtrash-hud-bin-bad-{}", std::process::id()));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    let _gone = Gone(path.clone());
    let out = Command::new(bin())
        .arg("--trash-dir")
        .arg(&path)
        .output()
        .expect("run rtrash-hud");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("rtrash: ignoring --trash-dir="), "{stderr}");
    assert!(stderr.contains("rtrash-hud: no valid"), "{stderr}");
}

#[cfg(unix)]
#[test]
fn larger_than_is_refused() {
    let out = Command::new(bin())
        .arg("--larger-than=1")
        .output()
        .expect("run rtrash-hud --larger-than");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("unrecognized option"), "{stderr}");
    assert!(stderr.contains("--larger-than=1"), "{stderr}");
}

#[cfg(unix)]
fn hud_json(root: &std::path::Path) -> (String, String) {
    let out = Command::new(bin())
        .arg(format!("--trash-dir={}", root.display()))
        .arg("--json")
        .env("RTRASH_MACOS_TRASH", "freedesktop")
        .output()
        .expect("run rtrash-hud");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(out.status.success(), "{stdout}{stderr}");
    (stdout, stderr)
}

#[cfg(unix)]
fn status_totals(root: &PathBuf) -> (u64, u64) {
    let dirs = rtrash::trashdir::resolve_dirs_opts(std::slice::from_ref(root), false);
    let (_rows, items, bytes) =
        rtrash::status::summarize_dirs(&dirs, None, None, rtrash::list::SizeFilter::default());
    (items, bytes)
}

#[cfg(unix)]
fn assert_json_totals(body: &str, items: u64, bytes: u64) {
    let prefix = format!(r#"{{"items":{items},"bytes":{bytes},"#);
    assert!(body.starts_with(&prefix), "{body}");
}

#[cfg(unix)]
fn write_dir_record(root: &std::path::Path, name: &str) {
    let payload = root.join("files").join(name);
    fs::create_dir(&payload).unwrap();
    fs::write(payload.join("inside"), b"inside-the-dir").unwrap();
    fs::write(
        root.join("info").join(format!("{name}.trashinfo")),
        format!("[Trash Info]\nPath=/tmp/{name}\nDeletionDate=2026-01-02T03:04:05\n"),
    )
    .unwrap();
}

/// A directory payload and a stale `directorysizes`. The display must not
/// create `.rtrash.lock` or change the cache, and its totals match
/// `rtrash status` (which does rewrite the cache).
#[cfg(unix)]
#[test]
fn a_stale_directorysizes_is_not_rewritten_or_locked() {
    let tmp = Tmp::trash("stale");
    write_dir_record(&tmp.0, "dir");
    let cache = tmp.0.join("directorysizes");
    fs::write(&cache, b"1000004095 1 dir\n").unwrap();
    let before = fs::read(&cache).unwrap();
    let (stdout, stderr) = hud_json(&tmp.0);
    assert!(
        !tmp.0.join(".rtrash.lock").exists(),
        "the display created a lock\n{stderr}"
    );
    assert_eq!(fs::read(&cache).unwrap(), before, "cache bytes changed");
    assert!(!stdout.contains("1000004095"), "{stdout}");
    let (items, bytes) = status_totals(&tmp.0);
    assert_json_totals(&stdout, items, bytes);
    assert!(items >= 1, "{items}");
    assert!(bytes < 1_000_000, "used the stale cache: {bytes}");
}

#[cfg(unix)]
fn wait_bounded(child: &mut std::process::Child) -> std::process::ExitStatus {
    let start = std::time::Instant::now();
    let guard = std::time::Duration::from_secs(8);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status,
            Ok(None) if start.elapsed() >= guard => {
                let _ = child.kill();
                let _ = child.wait();
                panic!(
                    "rtrash-hud was still running after {}s while the trash lock was held \
                     (RTRASH_LOCK_WAIT)",
                    guard.as_secs()
                );
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(50)),
            Err(e) => panic!("wait: {e}"),
        }
    }
}

/// The lock is already held and `RTRASH_LOCK_WAIT` is long. A display that
/// took the lock would sit until that wait; this one finishes and leaves
/// the stale cache alone.
#[cfg(unix)]
#[test]
fn a_held_lock_does_not_stall_the_display() {
    let tmp = Tmp::trash("held");
    write_dir_record(&tmp.0, "dir");
    let cache = tmp.0.join("directorysizes");
    fs::write(&cache, b"1000004095 1 dir\n").unwrap();
    let before = fs::read(&cache).unwrap();
    let _lock = rtrash::trashdir::TrashLock::acquire(&tmp.0).expect("hold the lock");
    let mut child = Command::new(bin())
        .arg(format!("--trash-dir={}", tmp.0.display()))
        .arg("--json")
        .env("RTRASH_MACOS_TRASH", "freedesktop")
        .env("RTRASH_LOCK_WAIT", "30")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn rtrash-hud");
    let status = wait_bounded(&mut child);
    let mut out = Vec::new();
    let mut err = Vec::new();
    use std::io::Read;
    child.stdout.take().unwrap().read_to_end(&mut out).unwrap();
    child.stderr.take().unwrap().read_to_end(&mut err).unwrap();
    let stdout = String::from_utf8_lossy(&out);
    let stderr = String::from_utf8_lossy(&err);
    assert!(status.success(), "{stdout}{stderr}");
    assert!(!stderr.contains("waiting for the trash lock"), "{stderr}");
    assert_eq!(fs::read(&cache).unwrap(), before);
    assert!(!stdout.contains("1000004095"), "{stdout}");
    drop(_lock);
}

/// A `directorysizes` symlink to a line of 1000004095 is not followed, so
/// the total is the walk, and the link is left in place.
#[cfg(unix)]
#[test]
fn a_symlinked_directorysizes_is_ignored() {
    let tmp = Tmp::trash("sizelink");
    write_dir_record(&tmp.0, "dir");
    let info = tmp.0.join("info").join("dir.trashinfo");
    let mtime = fs::metadata(&info)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let outside = tmp.0.join("outside-sizes");
    let body = format!("1000004095 {mtime} dir\n");
    fs::write(&outside, &body).unwrap();
    std::os::unix::fs::symlink(&outside, tmp.0.join("directorysizes")).unwrap();
    let (stdout, _stderr) = hud_json(&tmp.0);
    let link = fs::symlink_metadata(tmp.0.join("directorysizes")).unwrap();
    assert!(link.file_type().is_symlink(), "the cache link was replaced");
    assert_eq!(fs::read(&outside).unwrap(), body.as_bytes());
    assert!(!tmp.0.join(".rtrash.lock").exists());
    assert!(!stdout.contains("1000004095"), "{stdout}");
    let (items, bytes) = status_totals(&tmp.0);
    assert_json_totals(&stdout, items, bytes);
    assert!(bytes < 1_000_000, "followed the cache link: {bytes}");
}

/// A `.trashinfo` symlink is not an item. The file it points at is not read
/// as the record.
#[cfg(unix)]
#[test]
fn a_symlinked_trashinfo_is_not_counted() {
    let tmp = Tmp::trash("infolink");
    fs::write(tmp.0.join("files").join("real.txt"), b"real").unwrap();
    fs::write(
        tmp.0.join("info").join("real.txt.trashinfo"),
        "[Trash Info]\nPath=/tmp/real.txt\nDeletionDate=2026-01-02T03:04:05\n",
    )
    .unwrap();
    let outside = tmp.0.join("outside.trashinfo");
    fs::write(
        &outside,
        "[Trash Info]\nPath=/tmp/evil.txt\nDeletionDate=2026-01-02T03:04:05\n",
    )
    .unwrap();
    std::os::unix::fs::symlink(&outside, tmp.0.join("info").join("evil.txt.trashinfo")).unwrap();
    fs::write(tmp.0.join("files").join("evil.txt"), vec![b'y'; 200_000]).unwrap();
    let (stdout, _stderr) = hud_json(&tmp.0);
    assert!(
        stdout.starts_with(r#"{"items":1,"#),
        "the symlink was counted: {stdout}"
    );
    let (items, bytes) = status_totals(&tmp.0);
    assert_eq!(items, 1);
    assert_json_totals(&stdout, items, bytes);
    assert!(bytes < 100_000, "counted the linked payload: {bytes}");
    assert_eq!(
        fs::read(&outside).unwrap(),
        b"[Trash Info]\nPath=/tmp/evil.txt\nDeletionDate=2026-01-02T03:04:05\n"
    );
}

#[cfg(unix)]
#[test]
fn no_trash_is_reported_on_stderr() {
    let home = Tmp::dir("no-trash");
    let out = Command::new(bin())
        .arg("--home-only")
        .env("XDG_DATA_HOME", &home.0)
        .env("HOME", &home.0)
        .env("RTRASH_MACOS_TRASH", "freedesktop")
        .output()
        .expect("run rtrash-hud");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stdout}{stderr}");
    assert!(
        stdout.is_empty(),
        "status writes these lines to stderr: {stdout}"
    );
    assert_eq!(stderr, "No trash directories found\nTotal: 0 items (0 B)\n");
}

/// `--json` with no trash writes the empty object to stdout and leaves
/// stderr empty. The text display is the path that uses stderr.
#[cfg(unix)]
#[test]
fn no_trash_json_leaves_stderr_empty() {
    let home = Tmp::dir("no-trash-json");
    let out = Command::new(bin())
        .args(["--home-only", "--json"])
        .env("XDG_DATA_HOME", &home.0)
        .env("HOME", &home.0)
        .env("RTRASH_MACOS_TRASH", "freedesktop")
        .output()
        .expect("run rtrash-hud --json");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stdout}{stderr}");
    assert_eq!(stderr, "");
    assert_eq!(stdout, "{\"items\":0,\"bytes\":0,\"roots\":[]}\n");
}

#[cfg(unix)]
#[test]
fn the_finder_default_is_refused_without_a_pin() {
    let out = Command::new(bin())
        .env("RTRASH_MACOS_TRASH", "finder")
        .output()
        .expect("run rtrash-hud");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("Finder Trash"), "{stderr}");
    assert!(
        stderr.contains("RTRASH_MACOS_TRASH=freedesktop"),
        "{stderr}"
    );
    assert!(stderr.contains("macos.trash"), "{stderr}");
}

/// The flock `tests/cli.rs` takes around a real mount, so a bare `empty`
/// in another test process waits until this volume trash is gone.
#[cfg(target_os = "linux")]
struct GlobalState(std::fs::File);

#[cfg(target_os = "linux")]
impl Drop for GlobalState {
    fn drop(&mut self) {
        use std::os::unix::io::AsRawFd;
        // SAFETY: the descriptor is the one `global_state` opened, and
        // LOCK_UN releases the exclusive lock it took.
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

#[cfg(target_os = "linux")]
fn global_state() -> GlobalState {
    use std::os::unix::io::AsRawFd;
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"));
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join("rtrash-global-state.lock");
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .unwrap();
    // SAFETY: `file` is open. LOCK_EX waits for the same file the CLI
    // tests lock, and the guard releases it on drop.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    assert_eq!(rc, 0, "flock {}", path.display());
    GlobalState(file)
}

/// The marker is a regular file. `Path::is_file` follows a link, so a
/// symlink to any file would pass and `remove_dir_all` could wipe a
/// trash this run did not create.
#[cfg(target_os = "linux")]
fn marker_is_regular_file(dir: &Path) -> bool {
    fs::symlink_metadata(dir.join(".rtrash-hud-volume")).is_ok_and(|m| m.file_type().is_file())
}

/// Remove `path` only when it is still the directory recorded at
/// creation and its marker is a regular file.
#[cfg(target_os = "linux")]
fn remove_owned_volume(path: &Path, dev: u64, ino: u64) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(meta) = fs::symlink_metadata(path) else {
        return false;
    };
    if !meta.is_dir() || meta.dev() != dev || meta.ino() != ino {
        return false;
    }
    if !marker_is_regular_file(path) {
        return false;
    }
    fs::remove_dir_all(path).is_ok()
}

/// A marker that is a symlink to a regular file used to pass `is_file`,
/// and the cleanup then deleted the trash. The marker has to be a
/// regular file, and the directory has to be the one this run created.
#[cfg(target_os = "linux")]
#[test]
fn a_symlink_marker_does_not_remove_a_volume_trash() {
    use std::os::unix::fs::MetadataExt;
    let home = Tmp::trash("marker-link");
    let marker = home.0.join(".rtrash-hud-volume");
    let target = home.0.join("files").join("vol.txt");
    let record = home.0.join("info").join("vol.txt.trashinfo");
    fs::write(&target, b"volume-payload").unwrap();
    fs::write(
        &record,
        "[Trash Info]\nPath=/tmp/hud-volume\nDeletionDate=2026-01-02T03:04:05\n",
    )
    .unwrap();
    std::os::unix::fs::symlink(&target, &marker).unwrap();
    assert!(marker.is_file(), "Path::is_file follows the marker link");
    assert!(!marker_is_regular_file(&home.0));
    let meta = fs::symlink_metadata(&home.0).unwrap();
    assert!(!remove_owned_volume(&home.0, meta.dev(), meta.ino()));
    assert!(record.is_file());
    assert!(target.is_file());
}

/// `--home-only` with no pin is the home trash alone. Forcing `home_only`
/// false would also count the volume trash planted here.
#[cfg(target_os = "linux")]
#[test]
fn home_only_with_no_pin_skips_a_volume_trash() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    // Declared first so it drops last, after the planted trash is removed.
    let _global = global_state();
    let uid = rtrash::trashdir::uid();
    let scratch = std::env::temp_dir().join(format!(
        "rtrash-hud-vol-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&scratch).unwrap();
    let mounted = Command::new("sudo")
        .args(["-n", "mount", "--bind"])
        .arg(&scratch)
        .arg(&scratch)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(mounted, "a volume trash needs its own mount point");
    let trash = scratch.join(format!(".Trash-{uid}"));
    assert!(
        fs::symlink_metadata(&trash).is_err(),
        "this run's mount already had a volume trash"
    );
    fs::create_dir(&trash).unwrap();
    fs::set_permissions(&trash, fs::Permissions::from_mode(0o700)).unwrap();
    let made = fs::symlink_metadata(&trash).unwrap();
    let (dev, ino) = (made.dev(), made.ino());
    fs::write(trash.join(".rtrash-hud-volume"), b"fixture").unwrap();
    fs::create_dir_all(trash.join("files")).unwrap();
    fs::create_dir_all(trash.join("info")).unwrap();
    fs::write(trash.join("files").join("vol.txt"), b"volume-payload").unwrap();
    fs::write(
        trash.join("info").join("vol.txt.trashinfo"),
        "[Trash Info]\nPath=/tmp/hud-volume\nDeletionDate=2026-01-02T03:04:05\n",
    )
    .unwrap();
    struct Gone {
        trash: PathBuf,
        dev: u64,
        ino: u64,
        mount: PathBuf,
    }
    impl Drop for Gone {
        fn drop(&mut self) {
            let _ = remove_owned_volume(&self.trash, self.dev, self.ino);
            let _ = Command::new("sudo")
                .args(["-n", "umount"])
                .arg(&self.mount)
                .status();
            let _ = fs::remove_dir_all(&self.mount);
        }
    }
    let _gone = Gone {
        trash: trash.clone(),
        dev,
        ino,
        mount: scratch,
    };
    let volume = trash;
    let seen = rtrash::trashdir::resolve_dirs_opts(&[], false);
    assert!(
        seen.iter().any(|d| d.root == volume),
        "all() did not see {}: {seen:?}",
        volume.display()
    );

    let home = Tmp::trash("home-only");
    fs::write(home.0.join("files").join("home.txt"), b"home-payload").unwrap();
    fs::write(
        home.0.join("info").join("home.txt.trashinfo"),
        "[Trash Info]\nPath=/tmp/home.txt\nDeletionDate=2026-01-02T03:04:05\n",
    )
    .unwrap();
    let xdg = Tmp::dir("xdg");
    // home_trash() is $XDG_DATA_HOME/Trash, so the fixture trash has to
    // sit at that path. Move the prepared trash there.
    let trash = xdg.0.join("Trash");
    fs::rename(&home.0, &trash).unwrap();

    let out = Command::new(bin())
        .arg("--home-only")
        .arg("--json")
        .env("XDG_DATA_HOME", &xdg.0)
        .env("HOME", &xdg.0)
        .env("RTRASH_MACOS_TRASH", "freedesktop")
        .output()
        .expect("run rtrash-hud --home-only");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stdout}{stderr}");
    assert!(
        stdout.starts_with(r#"{"items":1,"#),
        "home_only was not applied: {stdout}"
    );
    assert!(
        !stdout.contains(&volume.display().to_string()),
        "volume trash leaked into --home-only: {stdout}"
    );
}
