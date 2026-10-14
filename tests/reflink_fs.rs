//! Shared extents on a real reflink file system (btrfs or XFS).
//!
//! These need a directory on such a file system in `RTRASH_REFLINK_DIR`,
//! which CI makes from a loop image. Without it they pass without checking
//! anything, so a plain `cargo test` stays the same everywhere.

#![cfg(target_os = "linux")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_rtrash")
}

/// A fresh directory under `RTRASH_REFLINK_DIR` holding `a.bin` (2 MiB of
/// varied bytes) and `clone.bin`, its reflink copy; `None` when unset.
fn cloned_pair(tag: &str) -> Option<PathBuf> {
    let base = std::env::var_os("RTRASH_REFLINK_DIR")?;
    let dir = PathBuf::from(base).join(format!("{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let body: Vec<u8> = (0..2u32 << 20)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    fs::write(dir.join("a.bin"), body).unwrap();
    let ok = Command::new("cp")
        .arg("--reflink=always")
        .arg(dir.join("a.bin"))
        .arg(dir.join("clone.bin"))
        .status()
        .unwrap()
        .success();
    assert!(
        ok,
        "RTRASH_REFLINK_DIR is set but cp --reflink=always failed there"
    );
    Some(dir)
}

fn run(dir: &Path, args: &[&str]) -> String {
    let out: Output = Command::new(bin())
        .args(args)
        .current_dir(dir)
        // A home beside the files, not the directory reaped: reap refuses
        // to reap a home directory.
        .env("HOME", home(dir))
        .env("XDG_DATA_HOME", home(dir).join("xdg"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// An empty home for the run, next to `dir`.
fn home(dir: &Path) -> PathBuf {
    let h = dir.with_extension("home");
    fs::create_dir_all(&h).unwrap();
    h
}

/// The number after the first `"key":` that follows `after` in `json`.
fn num(json: &str, after: &str, key: &str) -> u64 {
    let at = json
        .find(after)
        .unwrap_or_else(|| panic!("{after} missing: {json}"));
    let rest = &json[at..];
    let pat = format!("\"{key}\":");
    let v = &rest[rest
        .find(&pat)
        .unwrap_or_else(|| panic!("{key} missing: {json}"))
        + pat.len()..];
    v[..v.find(|c: char| !c.is_ascii_digit()).unwrap()]
        .parse()
        .unwrap()
}

#[test]
fn du_unique_and_shared_see_a_reflink_copy_as_one() {
    let Some(dir) = cloned_pair("du") else {
        eprintln!("RTRASH_REFLINK_DIR unset; skipping");
        return;
    };
    let mib2 = 2u64 << 20;
    let json = run(&dir, &["du", "--unique", "--no-git", "--json", "."]);
    // The copies own the same extents, which count at most once between them.
    let mut unique = 0;
    for f in ["a.bin\"", "clone.bin\""] {
        assert!(num(&json, f, "disk") >= mib2, "{json}");
        unique += num(&json, f, "unique");
    }
    assert!(unique < mib2 * 5 / 4, "{json}");
    assert!(
        num(&json, "\"shared\":{", "distinct") >= mib2 * 3 / 4,
        "{json}"
    );
    // --shared counts the extents once in the total.
    let json = run(&dir, &["du", "--shared", "--no-git", "--json", "."]);
    let (once, all) = (
        num(&json, "\"roots\"", "disk"),
        num(&json, "\"roots\"", "disk_all"),
    );
    assert!(all >= 2 * mib2 && once < all * 3 / 4, "{json}");
    let _ = fs::remove_dir_all(&dir);
    let _ = fs::remove_dir_all(dir.with_extension("home"));
}

#[test]
fn reap_shared_frees_little_for_a_reflink_copy() {
    let Some(dir) = cloned_pair("reap") else {
        eprintln!("RTRASH_REFLINK_DIR unset; skipping");
        return;
    };
    let day_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(86_400);
    for f in ["a.bin", "clone.bin"] {
        fs::File::options()
            .write(true)
            .open(dir.join(f))
            .unwrap()
            .set_times(
                fs::FileTimes::new()
                    .set_accessed(day_ago)
                    .set_modified(day_ago),
            )
            .unwrap();
    }
    let arg = dir.to_str().unwrap().to_owned();
    let json = run(
        &dir,
        &[
            "reap",
            "--older-than=1h",
            "--age-by=m",
            "--dry-run",
            "--shared",
            "--json",
            &arg,
        ],
    );
    let mib2 = 2u64 << 20;
    // The copies own the same extents, which count at most once between them.
    let mut unique = 0;
    for f in ["a.bin\"", "clone.bin\""] {
        assert!(num(&json, f, "bytes") >= mib2, "{json}");
        unique += num(&json, f, "unique");
    }
    assert!(unique < mib2 * 5 / 4, "{json}");
    assert!(dir.join("clone.bin").exists(), "a dry run removed a file");
    let _ = fs::remove_dir_all(&dir);
    let _ = fs::remove_dir_all(dir.with_extension("home"));
}

#[test]
fn dupes_credit_a_reflink_copy_with_nothing_freed() {
    let Some(dir) = cloned_pair("dupes") else {
        eprintln!("RTRASH_REFLINK_DIR unset; skipping");
        return;
    };
    let ok = Command::new("cp")
        .arg("--reflink=never")
        .arg(dir.join("a.bin"))
        .arg(dir.join("copy.bin"))
        .status()
        .unwrap()
        .success();
    assert!(ok);
    // a.bin is the oldest, so it stays.
    let day_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(86_400);
    fs::File::options()
        .write(true)
        .open(dir.join("a.bin"))
        .unwrap()
        .set_modified(day_ago)
        .unwrap();
    let mib2 = 2u64 << 20;
    let json = run(&dir, &["du", "--dupes", "--json", "."]);
    assert!(json.contains("\"keep\":\"./a.bin\""), "{json}");
    assert!(
        json.contains("clone.bin") && json.contains("copy.bin"),
        "{json}"
    );
    assert_eq!(num(&json, "\"total\"", "extras"), 2, "{json}");
    // Only the plain copy owns its blocks; trashing the clone frees little.
    let reclaim = num(&json, "\"total\"", "reclaim");
    assert!(reclaim >= mib2 * 3 / 4 && reclaim < mib2 * 5 / 4, "{json}");
    let _ = fs::remove_dir_all(&dir);
    let _ = fs::remove_dir_all(dir.with_extension("home"));
}
