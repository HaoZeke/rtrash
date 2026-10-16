//! The trash-cli names on Windows: a copy of rtrash.exe named
//! trash-list.exe or trash-put.exe runs as that command.
#![cfg(windows)]

use std::fs;
use std::process::Command;

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
