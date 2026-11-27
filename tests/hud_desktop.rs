//! The optional desktop launcher. The HUD crate is not a default workspace
//! member, so this test only reads the launcher file.

mod common;

use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn launcher() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("crates/rtrash-hud/rtrash-hud.desktop")
}

#[test]
fn rtrash_hud_desktop_launcher_validates() {
    let path = launcher();
    let body = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    for needle in [
        "[Desktop Entry]",
        "Type=Application",
        "Name=rtrash HUD",
        "Exec=rtrash-hud",
        "Terminal=true",
        "Categories=Utility;FileTools;",
    ] {
        assert!(body.contains(needle), "launcher missing {needle}:\n{body}");
    }
    assert!(body.ends_with('\n'), "desktop file must end with a newline");
    let out = match Command::new("desktop-file-validate").arg(&path).output() {
        Ok(out) => out,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            common::skip("desktop-file-validate is not installed");
            return;
        }
        Err(e) => panic!("desktop-file-validate: {e}"),
    };
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "desktop-file-validate failed: {stderr}{stdout}"
    );
    assert!(
        stderr.trim().is_empty() && stdout.trim().is_empty(),
        "desktop-file-validate: {stderr}{stdout}"
    );
}

#[test]
fn rtrash_hud_crate_version_matches_rtrash() {
    let toml = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("crates/rtrash-hud/Cargo.toml"),
    )
    .unwrap();
    let needle = format!("version = \"{}\"", rtrash::version());
    assert!(
        toml.lines().any(|row| row == needle),
        "rtrash-hud version should be {needle}"
    );
}
