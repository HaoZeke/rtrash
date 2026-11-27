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
        "Terminal=true",
        "Categories=Utility;FileTools;",
    ] {
        assert!(body.contains(needle), "launcher missing {needle}:\n{body}");
    }
    assert!(
        body.lines().any(|line| line == "Exec=rtrash-hud"),
        "Exec= must be the whole line:\n{body}"
    );
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
    for row in toml.lines().filter(|row| row.starts_with("rtrash = ")) {
        if let Some(pin) = pinned_version(row) {
            assert_eq!(pin, rtrash::version(), "pinned rtrash in {row}");
        }
    }
}

/// The version quoted on an `rtrash = { ..., version = "..." }` line.
fn pinned_version(row: &str) -> Option<&str> {
    let at = row.find("version = \"")?;
    let rest = &row[at + "version = \"".len()..];
    let end = rest.find('"')?;
    Some(&rest[..end])
}

/// The cog bump hooks, applied to copies, move the hud package version and
/// a pinned `rtrash` dependency. The lockstep script then accepts the
/// copies, and rejects a hud package version or pin left behind.
#[test]
fn a_bump_keeps_the_hud_crate_on_the_same_version() {
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root = common::scratch_dir("hud", "version-bump");
    struct Gone(PathBuf);
    impl Drop for Gone {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    let _gone = Gone(root.clone());
    for rel in [
        "Cargo.toml",
        "pyproject.toml",
        "docs/source/conf.py",
        "crates/rtrash-hud/Cargo.toml",
        "scripts/check-version-lockstep.sh",
        "scripts/bump-version.pl",
    ] {
        let dest = root.join(rel);
        fs::create_dir_all(dest.parent().unwrap()).unwrap();
        fs::copy(repo.join(rel), &dest).unwrap();
    }
    let hud = root.join("crates/rtrash-hud/Cargo.toml");
    let body = fs::read_to_string(&hud).unwrap().replace(
        "rtrash = { path = \"../..\", default-features = false }",
        "rtrash = { path = \"../..\", version = \"0.0.1\", default-features = false }",
    );
    assert!(
        body.contains("version = \"0.0.1\""),
        "the pin was not injected"
    );
    fs::write(&hud, &body).unwrap();

    let cog = fs::read_to_string(repo.join("cog.toml")).unwrap();
    let hooks = bump_hooks(&cog);
    assert!(
        hooks.iter().any(|h| h.contains("scripts/bump-version.pl")),
        "cog.toml pre_bump_hooks do not run scripts/bump-version.pl: {hooks:?}"
    );
    assert!(
        hooks.iter().all(|h| !h.contains("sed")),
        "a bump hook still calls sed: {hooks:?}"
    );
    let bumper = fs::read_to_string(repo.join("scripts/bump-version.pl")).unwrap();
    assert!(
        bumper.contains("crates/rtrash-hud/Cargo.toml"),
        "bump-version.pl does not name the hud package"
    );
    for hook in &hooks {
        let cmd = hook.replace("{{version}}", "9.9.9");
        let out = Command::new("sh")
            .arg("-c")
            .arg(&cmd)
            .current_dir(&root)
            .output()
            .unwrap_or_else(|e| panic!("run {cmd}: {e}"));
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{cmd} failed: {stderr}");
    }
    let bumped = fs::read_to_string(&hud).unwrap();
    assert!(
        bumped.lines().any(|row| row == "version = \"9.9.9\""),
        "the hud package version was not bumped:\n{bumped}"
    );
    assert!(
        bumped.contains("version = \"9.9.9\""),
        "the pinned rtrash dependency was not bumped:\n{bumped}"
    );
    assert!(
        !bumped.contains("version = \"0.0.1\""),
        "a pin stayed at 0.0.1:\n{bumped}"
    );
    let script = root.join("scripts/check-version-lockstep.sh");
    let matched = Command::new("bash")
        .arg(&script)
        .output()
        .expect("run check-version-lockstep.sh");
    let text = String::from_utf8_lossy(&matched.stdout);
    let err = String::from_utf8_lossy(&matched.stderr);
    assert!(matched.status.success(), "{text}{err}");
    assert!(text.contains("9.9.9"), "{text}");
    let conf = fs::read_to_string(root.join("docs/source/conf.py")).unwrap();
    assert!(
        conf.lines().any(|row| row == "release = \"9.9.9\""),
        "conf.py release was not bumped:\n{conf}"
    );
    assert!(
        conf.lines().any(|row| row == "version = \"0.1\""),
        "conf.py short version moved:\n{conf}"
    );

    let pin_left = bumped.replace(
        "rtrash = { path = \"../..\", version = \"9.9.9\", default-features = false }",
        "rtrash = { path = \"../..\", version = \"0.0.1\", default-features = false }",
    );
    assert!(pin_left.contains("version = \"9.9.9\""), "{pin_left}");
    fs::write(&hud, &pin_left).unwrap();
    let drifted_pin = Command::new("bash").arg(&script).output().unwrap();
    assert!(
        !drifted_pin.status.success(),
        "the lockstep script ignored a pinned rtrash dependency left at 0.0.1: {}",
        String::from_utf8_lossy(&drifted_pin.stdout)
    );

    fs::write(
        &hud,
        bumped.replace("version = \"9.9.9\"", "version = \"0.0.1\""),
    )
    .unwrap();
    let drifted = Command::new("bash").arg(&script).output().unwrap();
    assert!(
        !drifted.status.success(),
        "the lockstep script accepted a hud crate left at 0.0.1: {}",
        String::from_utf8_lossy(&drifted.stdout)
    );
}

/// The quoted commands in `pre_bump_hooks`, with TOML escapes undone.
fn bump_hooks(cog: &str) -> Vec<String> {
    let mut hooks = Vec::new();
    let mut inside = false;
    for line in cog.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("pre_bump_hooks") {
            inside = true;
            continue;
        }
        if !inside {
            continue;
        }
        if trimmed.starts_with(']') {
            break;
        }
        let Some(start) = trimmed.find('"') else {
            continue;
        };
        let rest = &trimmed[start + 1..];
        let Some(end) = rest.rfind('"') else {
            continue;
        };
        hooks.push(rest[..end].replace("\\\"", "\""));
    }
    hooks
}
