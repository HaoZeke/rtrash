//! The trash-cli names on Windows: a copy of rtrash.exe named
//! trash-list.exe or trash-put.exe runs as that command.
#![cfg(windows)]

use std::fs;
use std::io::Write;
use std::process::{Command, Output, Stdio};

/// Run `exe` in `dir` with `args`, feeding `input` on stdin.
fn run_with(exe: &std::path::Path, dir: &std::path::Path, args: &[&str], input: &str) -> Output {
    let mut child = Command::new(exe)
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn trash_cli_names_run_without_their_exe_suffix() {
    let dir = std::env::temp_dir().join(format!("rtrash-names-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let list = dir.join("trash-list.exe");
    fs::copy(env!("CARGO_BIN_EXE_rtrash"), &list).unwrap();
    let out = Command::new(&list).arg("--help").output().unwrap();
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(help.contains("Usage: trash-list"), "{help}");
    // The suffix matches in any case, and trash-put keeps trash-cli's 74.
    let put = dir.join("trash-put.EXE");
    fs::copy(env!("CARGO_BIN_EXE_rtrash"), &put).unwrap();
    let out = Command::new(&put)
        .arg(dir.join("no-such-file"))
        .output()
        .unwrap();
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(
        out.status.code(),
        Some(74),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn trash_cli_names_match_in_any_case() {
    let dir = std::env::temp_dir().join(format!("rtrash-names-case-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let list = dir.join("TRASH-LIST.exe");
    fs::copy(env!("CARGO_BIN_EXE_rtrash"), &list).unwrap();
    let out = Command::new(&list).arg("--help").output().unwrap();
    let _ = fs::remove_dir_all(&dir);
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(help.contains("Usage: trash-list"), "{help}");
}

#[test]
fn trash_restore_scopes_to_the_current_directory() {
    let dir = std::env::temp_dir().join(format!("rtrash-names-cwd-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let (here, away) = (dir.join("here"), dir.join("away"));
    fs::create_dir_all(&here).unwrap();
    fs::create_dir_all(&away).unwrap();
    // An item trashed from elsewhere stays out of the current directory scope.
    let gone = away.join("elsewhere.txt");
    fs::write(&gone, "x").unwrap();
    let put = Command::new(env!("CARGO_BIN_EXE_rtrash"))
        .arg("put")
        .arg(&gone)
        .output()
        .unwrap();
    assert!(
        put.status.success(),
        "{}",
        String::from_utf8_lossy(&put.stderr)
    );
    let restore = dir.join("trash-restore.exe");
    fs::copy(env!("CARGO_BIN_EXE_rtrash"), &restore).unwrap();
    let out = Command::new(&restore).current_dir(&here).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    // Tidy the Recycle Bin item before the asserts.
    let _ = Command::new(env!("CARGO_BIN_EXE_rtrash"))
        .arg("restore")
        .arg(&gone)
        .output();
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(out.status.code(), Some(1), "{stdout}");
    assert!(
        stdout.starts_with("No files trashed from current dir ('"),
        "{stdout}"
    );
    assert!(stdout.contains("here"), "{stdout}");
}

#[test]
fn trash_restore_takes_path_as_a_scope() {
    let dir = std::env::temp_dir().join(format!("rtrash-names-scope-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let sub = dir.join("sub");
    fs::create_dir_all(&sub).unwrap();
    // One item from under sub/: trash-restore sub asks, as trash-cli does.
    let inner = sub.join("inner.txt");
    fs::write(&inner, "in").unwrap();
    let put = Command::new(env!("CARGO_BIN_EXE_rtrash"))
        .arg("put")
        .arg(&inner)
        .output()
        .unwrap();
    assert!(
        put.status.success(),
        "{}",
        String::from_utf8_lossy(&put.stderr)
    );
    let restore = dir.join("trash-restore.exe");
    fs::copy(env!("CARGO_BIN_EXE_rtrash"), &restore).unwrap();
    // A blank answer restores nothing.
    let blank = run_with(&restore, &dir, &["sub"], "\n");
    let kept = !inner.exists();
    let out = run_with(&restore, &dir, &["sub"], "0\n");
    let back = fs::read(&inner).ok();
    if back.is_none() {
        let _ = Command::new(env!("CARGO_BIN_EXE_rtrash"))
            .arg("restore")
            .arg(&inner)
            .output();
    }
    let _ = fs::remove_dir_all(&dir);
    let said = String::from_utf8_lossy(&blank.stdout).into_owned();
    assert_eq!(blank.status.code(), Some(0), "{said}");
    // trash-cli's row: a width-4 index, then one space between columns.
    let row = said.lines().find(|l| l.starts_with("   0 ")).unwrap_or("");
    assert_eq!(row.as_bytes().get(15), Some(&b' '), "{said}");
    assert!(row.ends_with("inner.txt"), "{said}");
    let asked = String::from_utf8_lossy(&blank.stderr).into_owned();
    assert!(asked.contains("What file to restore [0..0]"), "{asked}");
    assert!(said.contains("No files were restored"), "{said}");
    assert!(kept, "a blank answer restored the item");
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(back.as_deref(), Some(&b"in"[..]));
}

#[test]
fn trash_restore_takes_a_range_pick() {
    let dir = std::env::temp_dir().join(format!("rtrash-names-pick-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let sub = dir.join("sub");
    fs::create_dir_all(&sub).unwrap();
    let names = ["a.txt", "b.txt", "c.txt"];
    for n in names {
        fs::write(sub.join(n), n).unwrap();
        let put = Command::new(env!("CARGO_BIN_EXE_rtrash"))
            .arg("put")
            .arg(sub.join(n))
            .output()
            .unwrap();
        assert!(
            put.status.success(),
            "{}",
            String::from_utf8_lossy(&put.stderr)
        );
        // Deletion times on Windows are whole seconds.
        std::thread::sleep(std::time::Duration::from_millis(1100));
    }
    let restore = dir.join("trash-restore.exe");
    fs::copy(env!("CARGO_BIN_EXE_rtrash"), &restore).unwrap();
    // Rows run oldest first: 0 is a.txt, 2 is c.txt.
    let out = run_with(&restore, &dir, &["sub"], "0,2\n");
    let back: Vec<bool> = names.iter().map(|n| sub.join(n).exists()).collect();
    for n in names {
        if !sub.join(n).exists() {
            let _ = Command::new(env!("CARGO_BIN_EXE_rtrash"))
                .arg("restore")
                .arg(sub.join(n))
                .output();
        }
    }
    let _ = fs::remove_dir_all(&dir);
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(back, [true, false, true]);
}
