//! `rtrash completions` and `rtrash man` on Windows, where the PowerShell
//! script is the one that matters.
#![cfg(windows)]

use std::path::Path;
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_rtrash")
}

fn read(rel: &str) -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(rel)).unwrap()
}

#[test]
fn completions_and_man_print_the_embedded_files() {
    for (shell, file) in [
        ("powershell", "completions/rtrash.ps1"),
        ("pwsh", "completions/rtrash.ps1"),
        ("bash", "completions/rtrash.bash"),
    ] {
        let out = Command::new(bin())
            .args(["completions", shell])
            .output()
            .unwrap();
        assert!(out.status.success(), "completions {shell}");
        assert_eq!(String::from_utf8_lossy(&out.stdout), read(file), "{shell}");
    }
    let out = Command::new(bin()).arg("man").output().unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains(".TH RTRASH 1"));
    let out = Command::new(bin())
        .args(["completions", "tcsh"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
}
