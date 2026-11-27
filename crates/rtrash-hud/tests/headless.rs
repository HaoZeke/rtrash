//! The `rtrash-hud` binary, run with no terminal and no compositor.

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

fn bin() -> &'static str {
    // Cargo sets this from the binary file name, hyphen included.
    env!("CARGO_BIN_EXE_rtrash-hud")
}

struct Tmp(PathBuf);

impl Tmp {
    fn trash(tag: &str) -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("rtrash-hud-bin-{tag}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(path.join("files")).unwrap();
        fs::create_dir_all(path.join("info")).unwrap();
        Tmp(path)
    }
}

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
    assert!(text.contains("--offline"), "{text}");
    assert!(text.contains("rtrash status"), "{text}");

    let version = output(&["--version"]);
    let line = String::from_utf8_lossy(&version.stdout);
    assert!(version.status.success(), "{line}");
    assert!(line.contains("rtrash-hud"), "{line}");
    assert!(line.contains(rtrash::version()), "{line}");
}

#[cfg(unix)]
#[test]
fn offline_prints_the_library_count_for_a_pinned_trash() {
    let tmp = Tmp::trash("catalog");
    fs::write(tmp.0.join("files").join("hello.txt"), b"hello-payload").unwrap();
    fs::write(
        tmp.0.join("info").join("hello.txt.trashinfo"),
        "[Trash Info]\nPath=/tmp/hello.txt\nDeletionDate=2026-01-02T03:04:05\n",
    )
    .unwrap();
    fs::write(tmp.0.join("files").join("loose.txt"), b"orphan-payload").unwrap();

    let out = Command::new(bin())
        .arg("--offline")
        .arg("--trash-dir")
        .arg(&tmp.0)
        .output()
        .expect("run rtrash-hud --offline");
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
    assert!(stderr.contains("no valid"), "{stderr}");
}
