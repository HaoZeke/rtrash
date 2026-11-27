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
        common::rtrash(&self.root)
            .args(["list", &self.pin()])
            .current_dir(self.work())
            .env("XDG_CACHE_HOME", self.root.join(".cache"))
            .env("RTRASH_TEST_CATALOG_LOG", self.log())
            .output()
            .expect("run rtrash list")
    }

    fn put(&self, name: &str) {
        fs::write(self.work().join(name), b"payload").unwrap();
        let pin = self.pin();
        let out = self.run(&["put", &pin, name]);
        assert!(out.status.success(), "{}", stderr(&out));
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
