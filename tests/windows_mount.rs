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

/// status and du --devices list the mounted folder as a volume of its
/// own, with its own free space.
#[test]
fn status_and_devices_list_the_mounted_volume() {
    let Some(m) = mounted() else {
        return;
    };
    let label = m.to_string_lossy().trim_end_matches('\\').to_lowercase();
    let status = Command::new(bin()).arg("status").output().unwrap();
    let text = String::from_utf8_lossy(&status.stdout).to_lowercase();
    assert!(
        text.lines()
            .any(|l| l.starts_with(&format!("free on {label}"))),
        "{text:?}"
    );
    let devs = Command::new(bin())
        .args(["du", "--devices", "--json"])
        .output()
        .unwrap();
    let json = String::from_utf8_lossy(&devs.stdout).to_lowercase();
    let escaped = label.replace('\\', "\\\\");
    assert!(json.contains(&format!("\"mount\":\"{escaped}")), "{json:?}");
    assert!(json.contains("\"device\":\"rtrashmnt\""), "{json:?}");
}

/// `put -v` names the Recycle Bin of the mounted volume, and the plain
/// status gives that volume a line of its own with the item in it.
#[test]
fn put_names_the_mounted_bin_and_status_counts_it() {
    let Some(m) = mounted() else {
        return;
    };
    let label = m.to_string_lossy().trim_end_matches('\\').to_lowercase();
    let f = m.join(format!("rtrash-where-{}.txt", std::process::id()));
    fs::write(&f, "x").unwrap();
    let out = Command::new(bin())
        .args(["put", "-v"])
        .arg(&f)
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&out.stdout).to_lowercase();
    assert!(
        out.status.success(),
        "{:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        said.contains(&format!("(recycle bin of {label}")),
        "{said:?}"
    );
    let status = Command::new(bin()).arg("status").output().unwrap();
    let text = String::from_utf8_lossy(&status.stdout).to_lowercase();
    assert!(
        text.lines()
            .any(|l| l.starts_with(&format!("{label}\\\t")) || l.starts_with(&format!("{label}\t"))),
        "{text:?}"
    );
}

#[test]
fn list_format_size_and_matchers_read_the_recycle_bin() {
    let Some(m) = mounted() else {
        return;
    };
    let name = format!("fmt-{}.txt", std::process::id());
    let p = m.join(&name);
    // A dry run checks and moves nothing.
    fs::write(&p, b"x").unwrap();
    let out = Command::new(bin())
        .args(["put", "--dry-run"])
        .arg(&p)
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("would trash"));
    assert!(p.exists());
    put(&p);
    let glob = format!("--glob={name}");
    let out = Command::new(bin())
        .args(["list", &glob, "--format={name}|{size}\\n"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().next().unwrap_or_default();
    let (n, size) = line.split_once('|').expect("one row");
    assert_eq!(n, name);
    assert!(size.parse::<u64>().unwrap() > 0, "{line}");
    let out = Command::new(bin())
        .args(["list", &glob, "--size"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).lines().count(), 1);
    let out = Command::new(bin())
        .args(["list", "--format={nope}"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
}
