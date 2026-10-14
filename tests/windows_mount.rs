//! A real volume mounted on a folder (Windows).
//!
//! These need such a folder in `RTRASH_MOUNTED_DIR`, which CI makes from
//! a VHD with diskpart. Without it they pass without checking anything,
//! so a plain `cargo test` stays the same everywhere.

#![cfg(windows)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_rtrash")
}

fn mounted() -> Option<PathBuf> {
    std::env::var_os("RTRASH_MOUNTED_DIR").map(PathBuf::from)
}

fn put(p: &Path) {
    fs::write(p, p.to_string_lossy().as_bytes()).unwrap();
    let out = Command::new(bin()).arg("put").arg(p).output().unwrap();
    assert!(
        out.status.success(),
        "put {}: {:?}",
        p.display(),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn a_mounted_folder_is_a_volume_of_its_own() {
    let Some(m) = mounted() else {
        return;
    };
    let root = rtrash::win_recycle::volume_root(&m).expect("volume root");
    let trim = |p: &Path| p.to_string_lossy().trim_end_matches('\\').to_lowercase();
    assert_eq!(trim(&root), trim(&m), "{root:?}");
    let k = rtrash::win_recycle::volume_key(&m);
    assert_ne!(k & 1 << 62, 0, "no serial for {}: {k:x}", m.display());
    assert_ne!(k, rtrash::win_recycle::volume_key(&std::env::temp_dir()));
    assert_eq!(k, rtrash::win_recycle::volume_key(&m.join("sub")));
}

/// The RTRASH_TEST_FS seam makes the mounted folder look short of space,
/// so the dry run lists its Recycle Bin: the item put there, and not one
/// put on the drive that holds the folder.
#[cfg(debug_assertions)]
#[test]
fn purge_takes_the_mounted_volume_recycle_bin_only() {
    let Some(m) = mounted() else {
        return;
    };
    let id = std::process::id();
    let dir = m.join(format!("rtrash-mount-{id}"));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let here = format!("rtrash-on-mount-{id}.txt");
    let there = format!("rtrash-on-drive-{id}.txt");
    put(&m.join(&here));
    put(&std::env::temp_dir().join(&there));
    let out = Command::new(bin())
        .args([
            "reap",
            "--older-than=1h",
            "--when-below=999999G",
            "--purge-trash",
            "--dry-run",
        ])
        .arg(&dir)
        .env("RTRASH_TEST_FS", format!("1000000:{}", dir.display()))
        .output()
        .unwrap();
    let (stdout, stderr) = (
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(out.status.success(), "{stderr:?}");
    let listed = |n: &str| {
        stdout
            .lines()
            .any(|l| l.starts_with("would remove ") && l.contains(n))
    };
    assert!(listed(&here), "{here} not listed: {stdout:?} {stderr:?}");
    assert!(!listed(&there), "{there} is on another volume: {stdout:?}");
    let label = m.to_string_lossy().trim_end_matches('\\').to_lowercase();
    assert!(
        stderr
            .to_lowercase()
            .contains(&format!("from the recycle bin of {label}")),
        "{stderr:?}"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// A put on the mounted folder adds a line to that volume's own history
/// file, named by its serial, not to the history of drive C.
#[test]
fn put_on_a_mounted_volume_writes_its_own_history() {
    let Some(m) = mounted() else {
        return;
    };
    let local = std::env::temp_dir().join(format!("rtrash-mount-hist-{}", std::process::id()));
    let _ = fs::remove_dir_all(&local);
    let p = m.join(format!("rtrash-hist-{}.txt", std::process::id()));
    fs::write(&p, "h").unwrap();
    let out = Command::new(bin())
        .arg("put")
        .arg(&p)
        .env("LOCALAPPDATA", &local)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    let key = rtrash::win_recycle::volume_key(&m);
    let file = local
        .join("rtrash")
        .join("history")
        .join(format!("vol-{:08x}.txt", key & 0xffff_ffff));
    let body = fs::read_to_string(&file).unwrap_or_else(|e| {
        let have: Vec<_> = fs::read_dir(local.join("rtrash").join("history"))
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        panic!("{}: {e}; history files: {have:?}", file.display())
    });
    assert!(body.lines().any(|l| l.contains(" put ")), "{body:?}");
    let _ = fs::remove_dir_all(&local);
}
