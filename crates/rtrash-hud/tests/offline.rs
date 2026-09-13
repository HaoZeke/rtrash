//! `rtrash-hud --offline --trash-dir=FIXTURE` paints the catalog without a TTY.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn hud() -> &'static str {
    env!("CARGO_BIN_EXE_rtrash-hud")
}

fn fixture(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "rtrash-hud-offline-{}-{tag}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("files")).unwrap();
    fs::create_dir_all(root.join("info")).unwrap();
    fs::write(root.join("files/hello.txt"), b"payload").unwrap();
    fs::write(
        root.join("info/hello.txt.trashinfo"),
        "[Trash Info]\n\
         Path=/tmp/rtrash-hud-fixture/hello.txt\n\
         DeletionDate=2026-01-02T03:04:05\n",
    )
    .unwrap();
    root
}

#[test]
fn offline_paints_fixture_catalog_without_a_tty() {
    let trash = fixture("catalog");
    let pin = format!("--trash-dir={}", trash.display());
    let out = Command::new(hud())
        .args(["--offline", &pin])
        .output()
        .expect("run rtrash-hud --offline");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "offline paint failed: stdout={stdout} stderr={stderr}"
    );
    assert!(
        stdout.contains("rtrash-hud"),
        "must name the HUD: {stdout}"
    );
    assert!(
        stdout.contains("pane:list"),
        "must paint the list pane: {stdout}"
    );
    assert!(
        stdout.contains("pane:restore")
            && stdout.contains("pane:empty")
            && stdout.contains("pane:put")
            && stdout.contains("pane:status"),
        "must name every pane: {stdout}"
    );
    assert!(
        stdout.contains("hello.txt"),
        "must paint the fixture catalog: {stdout}"
    );
    assert!(
        stdout.contains("2026-01-02T03:04:05"),
        "must paint the fixture date: {stdout}"
    );
    assert!(
        stdout.contains("offline"),
        "must mark the offline paint: {stdout}"
    );
    let _ = fs::remove_dir_all(&trash);
}

#[test]
fn help_names_the_hud_and_offline() {
    let out = Command::new(hud())
        .arg("--help")
        .output()
        .expect("run rtrash-hud --help");
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success());
    assert!(s.contains("rtrash-hud"));
    assert!(s.contains("--offline"));
    assert!(s.contains("icedtea"));
}
