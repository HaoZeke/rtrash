//! The sealed per-root catalog: a second list must use it, a damaged
//! blob falls back to a full read, and an edit made by another tool
//! shows up on the next list.
#![cfg(unix)]

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Output;

struct Box {
    root: PathBuf,
}

impl Box {
    fn open(tag: &str) -> Self {
        let root = fs::canonicalize(common::scratch_dir("catalog", tag)).unwrap();
        fs::create_dir_all(root.join("work")).unwrap();
        let trash = root.join("xdg/Trash");
        fs::create_dir_all(trash.join("files")).unwrap();
        fs::create_dir_all(trash.join("info")).unwrap();
        fs::create_dir_all(root.join(".cache")).unwrap();
        Box { root }
    }

    fn work(&self) -> PathBuf {
        self.root.join("work")
    }

    fn trash(&self) -> PathBuf {
        self.root.join("xdg/Trash")
    }

    fn pin(&self) -> String {
        format!("--trash-dir={}", self.trash().display())
    }

    fn cache(&self) -> PathBuf {
        self.root.join(".cache/rtrash")
    }

    fn log(&self) -> PathBuf {
        self.root.join("catalog.log")
    }

    fn run(&self, args: &[&str]) -> Output {
        common::rtrash(&self.root)
            .args(args)
            .current_dir(self.work())
            .env("XDG_CACHE_HOME", self.root.join(".cache"))
            .output()
            .expect("run rtrash")
    }

    fn list(&self) -> Output {
        self.list_env(&[])
    }

    fn list_env(&self, extra: &[(&str, &str)]) -> Output {
        let mut cmd = common::rtrash(&self.root);
        cmd.args(["list", &self.pin()])
            .current_dir(self.work())
            .env("XDG_CACHE_HOME", self.root.join(".cache"))
            .env("RTRASH_TEST_CATALOG_LOG", self.log());
        for (key, value) in extra {
            cmd.env(key, value);
        }
        cmd.output().expect("run rtrash list")
    }

    fn clear_log(&self) {
        let _ = fs::remove_file(self.log());
    }

    fn put(&self, name: &str) {
        fs::write(self.work().join(name), b"payload").unwrap();
        let pin = self.pin();
        let out = self.run(&["put", &pin, name]);
        assert!(out.status.success(), "{}", stderr(&out));
    }

    /// Modification times two seconds back, past the coarse-clock margin,
    /// so a following list can seal a catalog without waiting on the clock.
    fn backdate(&self) {
        let ago = std::time::SystemTime::now() - std::time::Duration::from_secs(2);
        let set = |p: &Path| {
            let f = fs::OpenOptions::new().read(true).open(p).unwrap();
            let times = fs::FileTimes::new().set_accessed(ago).set_modified(ago);
            f.set_times(times).unwrap();
        };
        let trash = self.trash();
        for sub in ["info", "files"] {
            let dir = trash.join(sub);
            for ent in fs::read_dir(&dir).unwrap().flatten() {
                set(&ent.path());
            }
            set(&dir);
        }
    }
}

impl Drop for Box {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn blobs(cache: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(rd) = fs::read_dir(cache) else {
        return found;
    };
    for ent in rd.flatten() {
        if ent.path().extension().and_then(|s| s.to_str()) == Some("bin") {
            found.push(ent.path());
        }
    }
    found.sort();
    found
}

fn log_lines(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn a_second_list_uses_the_sealed_catalog() {
    let sb = Box::open("seal");
    sb.put("catalog-seal-token.txt");
    let _ = fs::remove_dir_all(sb.cache());
    let _ = fs::remove_file(sb.log());
    sb.backdate();

    let first = sb.list();
    assert!(first.status.success(), "{}", stderr(&first));
    let text = stdout(&first);
    assert!(text.contains("catalog-seal-token.txt"), "{text}");
    assert_eq!(log_lines(&sb.log()), ["scan"]);
    let found = blobs(&sb.cache());
    assert_eq!(found.len(), 1, "list must write one sealed catalog");
    let raw = fs::read(&found[0]).unwrap();
    assert!(raw.starts_with(b"RTR1"), "catalog magic");
    assert!(
        !raw.windows(b"catalog-seal-token.txt".len())
            .any(|w| w == b"catalog-seal-token.txt"),
        "the catalog must not store the path in the clear"
    );
    let key = sb.root.join("xdg/rtrash/cache.key");
    assert!(key.is_file(), "key under XDG_DATA_HOME");
    let mode = fs::metadata(&key).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);

    let second = sb.list();
    assert!(second.status.success(), "{}", stderr(&second));
    assert_eq!(stdout(&second), text);
    assert_eq!(
        log_lines(&sb.log()),
        ["scan", "hit"],
        "the second list must use the catalog"
    );

    let mut damaged = raw;
    let last = damaged.len() - 1;
    damaged[last] ^= 0xff;
    fs::write(&found[0], &damaged).unwrap();
    let third = sb.list();
    assert!(third.status.success(), "{}", stderr(&third));
    assert!(
        stdout(&third).contains("catalog-seal-token.txt"),
        "a damaged catalog falls back to a full read"
    );
    assert_eq!(
        log_lines(&sb.log()).last().map(String::as_str),
        Some("scan")
    );
}

#[test]
fn an_external_edit_of_a_record_is_not_served_from_the_catalog() {
    let sb = Box::open("external");
    sb.put("catalog-seal-token.txt");
    let _ = fs::remove_dir_all(sb.cache());
    let _ = fs::remove_file(sb.log());
    sb.backdate();
    let first = sb.list();
    assert!(first.status.success(), "{}", stderr(&first));
    assert_eq!(log_lines(&sb.log()), ["scan"]);
    assert_eq!(blobs(&sb.cache()).len(), 1);

    let info = fs::read_dir(sb.trash().join("info"))
        .unwrap()
        .flatten()
        .find(|e| e.path().extension().and_then(|s| s.to_str()) == Some("trashinfo"))
        .expect("record")
        .path();
    let body = fs::read_to_string(&info).unwrap();
    let rewritten = body.replacen("catalog-seal-token.txt", "catalog-external-edit.txt", 1);
    assert_ne!(body, rewritten, "the record should name the token");
    fs::write(&info, rewritten).unwrap();

    let second = sb.list();
    assert!(second.status.success(), "{}", stderr(&second));
    let text = stdout(&second);
    assert!(
        text.contains("catalog-external-edit.txt"),
        "the edit must show: {text}"
    );
    assert!(
        !text.contains("catalog-seal-token.txt"),
        "the catalog must not replay the old path: {text}"
    );
    assert_eq!(
        log_lines(&sb.log()).last().map(String::as_str),
        Some("scan")
    );
}

#[test]
fn put_restore_rm_and_empty_change_what_list_shows() {
    let sb = Box::open("commands");
    let pin = sb.pin();
    sb.put("a.txt");
    sb.put("b.txt");
    let listed = stdout(&sb.list());
    assert!(listed.contains("a.txt"), "{listed}");
    assert!(listed.contains("b.txt"), "{listed}");

    let b = sb.work().join("b.txt");
    let out = sb.run(&["restore", &pin, b.to_str().unwrap()]);
    assert!(out.status.success(), "{}", stderr(&out));
    let listed = stdout(&sb.list());
    assert!(listed.contains("a.txt"), "{listed}");
    assert!(!listed.contains("b.txt"), "{listed}");
    assert!(b.is_file(), "restore puts the file back");

    let a = sb.work().join("a.txt");
    let out = sb.run(&["rm", &pin, a.to_str().unwrap()]);
    assert!(out.status.success(), "{}", stderr(&out));
    let listed = stdout(&sb.list());
    assert!(!listed.contains("a.txt"), "{listed}");
    assert!(!a.exists(), "rm removes the original");

    sb.put("c.txt");
    let out = sb.run(&["empty", &pin]);
    assert!(out.status.success(), "{}", stderr(&out));
    let listed = stdout(&sb.list());
    assert!(
        !listed.contains("c.txt"),
        "empty must not leave the catalog showing c.txt: {listed}"
    );
}

#[test]
fn no_cache_reads_every_record_and_writes_no_blob() {
    let sb = Box::open("nocache");
    sb.put("plain.txt");
    let _ = fs::remove_dir_all(sb.cache());
    let _ = fs::remove_file(sb.log());
    let out = common::rtrash(&sb.root)
        .args(["list", &sb.pin()])
        .current_dir(sb.work())
        .env("XDG_CACHE_HOME", sb.root.join(".cache"))
        .env("RTRASH_NO_CACHE", "1")
        .env("RTRASH_TEST_CATALOG_LOG", sb.log())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("plain.txt"));
    assert!(
        blobs(&sb.cache()).is_empty(),
        "RTRASH_NO_CACHE writes no catalog"
    );
    assert!(
        !log_lines(&sb.log()).iter().any(|l| l == "hit"),
        "RTRASH_NO_CACHE must not use a catalog"
    );
}

fn rewrite_record(sb: &Box, from: &str, to: &str) -> PathBuf {
    assert_eq!(from.len(), to.len());
    let info = fs::read_dir(sb.trash().join("info"))
        .unwrap()
        .flatten()
        .find(|e| e.path().extension().and_then(|s| s.to_str()) == Some("trashinfo"))
        .expect("record")
        .path();
    let body = fs::read(&info).unwrap();
    let at = body
        .windows(from.len())
        .position(|w| w == from.as_bytes())
        .expect("path in the record");
    let mut next = body;
    next[at..at + from.len()].copy_from_slice(to.as_bytes());
    fs::write(&info, &next).unwrap();
    info
}

fn set_mtime(path: &Path, modified: std::time::SystemTime) {
    let f = fs::OpenOptions::new().read(true).open(path).unwrap();
    let times = fs::FileTimes::new()
        .set_accessed(modified)
        .set_modified(modified);
    f.set_times(times).unwrap();
}

#[test]
fn a_same_length_rewrite_is_not_served_from_the_catalog() {
    let sb = Box::open("samelen");
    let old = "tick-old-name.txt";
    let new = "tick-new-name.txt";
    sb.put(old);
    let _ = fs::remove_dir_all(sb.cache());
    sb.backdate();
    let first = sb.list();
    assert!(first.status.success(), "{}", stderr(&first));
    assert!(stdout(&first).contains(old), "{}", stdout(&first));
    assert_eq!(log_lines(&sb.log()), ["scan"]);
    let info = sb.trash().join("info");
    let record = fs::read_dir(&info)
        .unwrap()
        .flatten()
        .find(|e| e.path().extension().and_then(|s| s.to_str()) == Some("trashinfo"))
        .unwrap()
        .path();
    let mtime = fs::metadata(&record).unwrap().modified().unwrap();
    let dir_mtime = fs::metadata(&info).unwrap().modified().unwrap();
    rewrite_record(&sb, old, new);
    set_mtime(&record, mtime);
    set_mtime(&info, dir_mtime);
    let second = sb.list();
    assert!(second.status.success(), "{}", stderr(&second));
    let text = stdout(&second);
    assert!(text.contains(new), "the rewrite must show: {text}");
    assert!(
        !text.contains(old),
        "the catalog must not replay the old path: {text}"
    );
    assert_eq!(
        log_lines(&sb.log()).last().map(String::as_str),
        Some("scan")
    );
}

#[test]
fn a_recreated_record_is_not_served_from_the_catalog() {
    let sb = Box::open("inode");
    let old = "inode-old-name.txt";
    let new = "inode-new-name.txt";
    sb.put(old);
    let _ = fs::remove_dir_all(sb.cache());
    sb.backdate();
    let first = sb.list();
    assert!(first.status.success(), "{}", stderr(&first));
    assert_eq!(log_lines(&sb.log()), ["scan"]);
    let info = sb.trash().join("info");
    let record = fs::read_dir(&info)
        .unwrap()
        .flatten()
        .find(|e| e.path().extension().and_then(|s| s.to_str()) == Some("trashinfo"))
        .unwrap()
        .path();
    let mtime = fs::metadata(&record).unwrap().modified().unwrap();
    let dir_mtime = fs::metadata(&info).unwrap().modified().unwrap();
    let body = fs::read(&record).unwrap();
    fs::remove_file(&record).unwrap();
    let at = body
        .windows(old.len())
        .position(|w| w == old.as_bytes())
        .unwrap();
    let mut next = body;
    next[at..at + old.len()].copy_from_slice(new.as_bytes());
    fs::write(&record, &next).unwrap();
    set_mtime(&record, mtime);
    set_mtime(&info, dir_mtime);
    let second = sb.list();
    assert!(second.status.success(), "{}", stderr(&second));
    let text = stdout(&second);
    assert!(text.contains(new), "the recreated record must show: {text}");
    assert!(
        !text.contains(old),
        "the catalog must not replay the old path: {text}"
    );
}

/// Put the mode back on drop, so a failed command can still be cleaned up.
struct RestoreMode {
    path: PathBuf,
    bits: u32,
}

impl RestoreMode {
    fn set(path: &Path, bits: u32) -> Self {
        let path = path.to_path_buf();
        fs::set_permissions(&path, fs::Permissions::from_mode(bits)).unwrap();
        RestoreMode { path, bits: 0o755 }
    }
}

impl Drop for RestoreMode {
    fn drop(&mut self) {
        let _ = fs::set_permissions(&self.path, fs::Permissions::from_mode(self.bits));
    }
}

fn seal(sb: &Box) {
    sb.backdate();
    sb.clear_log();
    let first = sb.list();
    assert!(first.status.success(), "{}", stderr(&first));
    assert_eq!(
        blobs(&sb.cache()).len(),
        1,
        "list must write one sealed catalog"
    );
}

fn assert_blob_gone(sb: &Box, why: &str) {
    assert!(blobs(&sb.cache()).is_empty(), "{why}");
}

/// `put` drops the catalog before it tries to move the file. `files/` is
/// not writable, so the move fails and nothing else deletes the blob.
#[test]
fn put_drops_the_catalog_before_the_move_can_fail() {
    let sb = Box::open("put-drop");
    sb.put("a.txt");
    seal(&sb);
    let _mode = RestoreMode::set(&sb.trash().join("files"), 0o555);
    fs::write(sb.work().join("b.txt"), b"payload").unwrap();
    let pin = sb.pin();
    let out = sb.run(&["put", &pin, "b.txt"]);
    assert!(
        !out.status.success(),
        "put into a read-only files/ should fail"
    );
    assert!(
        sb.work().join("b.txt").is_file(),
        "the file should stay put"
    );
    assert!(
        sb.trash().join("files/a.txt").is_file(),
        "the old item stays"
    );
    assert_blob_gone(&sb, "put left the catalog in place");
}

/// `restore` drops the catalog before it writes the file back.
#[test]
fn restore_drops_the_catalog_before_the_write_can_fail() {
    let sb = Box::open("restore-drop");
    sb.put("a.txt");
    seal(&sb);
    let dest = sb.work().join("a.txt");
    let _mode = RestoreMode::set(&sb.work(), 0o555);
    let pin = sb.pin();
    let out = sb.run(&["restore", &pin, dest.to_str().unwrap()]);
    assert!(
        !out.status.success(),
        "restore into a read-only directory should fail"
    );
    assert!(
        sb.trash().join("files/a.txt").is_file(),
        "the payload stays"
    );
    assert_blob_gone(&sb, "restore left the catalog in place");
}

/// `rm` drops the catalog before it removes the payload.
#[test]
fn rm_drops_the_catalog_before_the_payload_can_fail() {
    let sb = Box::open("rm-drop");
    sb.put("a.txt");
    seal(&sb);
    let _mode = RestoreMode::set(&sb.trash().join("files"), 0o555);
    let dest = sb.work().join("a.txt");
    let pin = sb.pin();
    let out = sb.run(&["rm", &pin, dest.to_str().unwrap()]);
    assert!(
        !out.status.success(),
        "rm of a payload that cannot be unlinked should fail"
    );
    assert!(
        sb.trash().join("files/a.txt").is_file(),
        "the payload stays"
    );
    assert_blob_gone(&sb, "rm left the catalog in place");
}

/// `empty PATH` is the call in `empty` that drops the catalog itself,
/// before it removes the payload. A full empty goes through the total
/// instead, so this test names one path.
#[test]
fn empty_of_a_path_drops_the_catalog_before_the_payload_can_fail() {
    let sb = Box::open("empty-drop");
    sb.put("a.txt");
    seal(&sb);
    let _mode = RestoreMode::set(&sb.trash().join("files"), 0o555);
    let dest = sb.work().join("a.txt");
    let pin = sb.pin();
    let out = sb.run(&["empty", &pin, dest.to_str().unwrap()]);
    assert!(
        !out.status.success(),
        "empty of a payload that cannot be unlinked should fail: {}",
        stderr(&out)
    );
    assert!(
        sb.trash().join("files/a.txt").is_file(),
        "the payload stays"
    );
    assert_blob_gone(&sb, "empty of one path left the catalog in place");
}

#[test]
fn no_cache_ignores_an_existing_catalog() {
    let sb = Box::open("nocache-hit");
    sb.put("plain.txt");
    seal(&sb);
    sb.clear_log();
    let again = sb.list();
    assert!(again.status.success(), "{}", stderr(&again));
    assert_eq!(
        log_lines(&sb.log()),
        ["hit"],
        "the catalog is there to bypass"
    );

    for value in ["1", "TRUE", "yes"] {
        sb.clear_log();
        let out = sb.list_env(&[("RTRASH_NO_CACHE", value)]);
        assert!(out.status.success(), "{value}: {}", stderr(&out));
        assert!(stdout(&out).contains("plain.txt"), "{value}");
        assert!(
            !log_lines(&sb.log()).iter().any(|l| l == "hit"),
            "{value} used the existing catalog: {:?}",
            log_lines(&sb.log())
        );
        assert_eq!(
            blobs(&sb.cache()).len(),
            1,
            "{value} removed the catalog instead of ignoring it"
        );
    }

    sb.clear_log();
    let zero = sb.list_env(&[("RTRASH_NO_CACHE", "0")]);
    assert!(zero.status.success(), "{}", stderr(&zero));
    assert_eq!(log_lines(&sb.log()), ["hit"], "0 leaves the catalog on");
}

#[test]
fn the_suite_pins_the_catalog_away_from_the_real_home() {
    let home = common::scratch_dir("catalog", "pin-env");
    let pinned = common::rtrash(&home);
    let cache = env_path(&pinned, "XDG_CACHE_HOME");
    let data = env_path(&pinned, "XDG_DATA_HOME");
    assert!(cache.starts_with(&home), "{}", cache.display());
    assert!(data.starts_with(&home), "{}", data.display());

    let real = common::rtrash_real(common::Real::NoTrash);
    let cache = env_path(&real, "XDG_CACHE_HOME");
    let user = PathBuf::from(std::env::var_os("HOME").unwrap());
    assert_ne!(cache, user.join(".cache"), "{}", cache.display());
    assert!(
        cache.starts_with(std::env::temp_dir()),
        "{}",
        cache.display()
    );

    let watched = common::real_catalog_dirs();
    assert!(watched.contains(&user.join(".cache/rtrash")), "{watched:?}");
    assert!(
        watched.contains(&user.join(".local/share/rtrash")),
        "{watched:?}"
    );
}

#[test]
fn a_catalog_dir_listing_changes_when_its_file_does() {
    let dir = common::scratch_dir("catalog", "listing").join("gone");
    assert_eq!(common::catalog_dir_listing(&dir), "missing\n");
    fs::create_dir_all(&dir).unwrap();
    let empty = common::catalog_dir_listing(&dir);
    assert_ne!(empty, "missing\n");
    fs::write(dir.join("c.bin"), b"abc").unwrap();
    let filled = common::catalog_dir_listing(&dir);
    assert_ne!(filled, empty);
    fs::write(dir.join("c.bin"), b"abcd").unwrap();
    assert_ne!(common::catalog_dir_listing(&dir), filled);
}

fn env_path(cmd: &std::process::Command, key: &str) -> PathBuf {
    cmd.get_envs()
        .find(|(k, _)| *k == key)
        .and_then(|(_, v)| v.map(PathBuf::from))
        .unwrap_or_else(|| panic!("{key} unset"))
}
