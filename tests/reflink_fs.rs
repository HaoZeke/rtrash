//! Shared extents on a real reflink file system (btrfs or XFS).
//!
//! These need a directory on such a file system in `RTRASH_REFLINK_DIR`,
//! which CI makes from a loop image. Without it they pass without checking
//! anything, so a plain `cargo test` stays the same everywhere.

#![cfg(target_os = "linux")]

mod common;

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

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
    let out: Output = common::rtrash(&home(dir))
        .args(args)
        .current_dir(dir)
        // A home beside the files, not the directory reaped: reap refuses
        // to reap a home directory.
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

#[test]
fn clone_dupes_share_the_extents_and_keep_each_mode() {
    use std::os::unix::fs::PermissionsExt;
    let Some(dir) = cloned_pair("clone") else {
        eprintln!("RTRASH_REFLINK_DIR unset; skipping");
        return;
    };
    let copy = dir.join("copy.bin");
    let ok = Command::new("cp")
        .arg("--reflink=never")
        .arg(dir.join("a.bin"))
        .arg(&copy)
        .status()
        .unwrap()
        .success();
    assert!(ok);
    fs::set_permissions(&copy, fs::Permissions::from_mode(0o640)).unwrap();
    let day_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(86_400);
    fs::File::options()
        .write(true)
        .open(dir.join("a.bin"))
        .unwrap()
        .set_modified(day_ago)
        .unwrap();
    let before = fs::metadata(&copy).unwrap().modified().unwrap();
    run(&dir, &["du", "--dupes", "--clone-dupes"]);
    let m = fs::metadata(&copy).unwrap();
    assert_eq!(m.permissions().mode() & 0o777, 0o640);
    assert_eq!(m.modified().unwrap(), before);
    assert_eq!(
        fs::read(&copy).unwrap(),
        fs::read(dir.join("a.bin")).unwrap()
    );
    // Every copy now shares the kept file's extents: nothing left to free.
    let json = run(&dir, &["du", "--dupes", "--json", "."]);
    assert!(
        num(&json, "\"total\"", "reclaim") < (2u64 << 20) / 4,
        "{json}"
    );
    let _ = fs::remove_dir_all(&dir);
    let _ = fs::remove_dir_all(dir.with_extension("home"));
}

/// Whether `dir` is on btrfs (the reflink job also runs on XFS).
fn on_btrfs(dir: &Path) -> bool {
    let kind = Command::new("stat")
        .args(["-f", "-c", "%T"])
        .arg(dir)
        .output()
        .unwrap();
    String::from_utf8_lossy(&kind.stdout).trim() == "btrfs"
}

/// Run `btrfs ARGS`, which must succeed.
fn btrfs(args: &[&OsStr]) {
    let out = Command::new("btrfs").args(args).output().unwrap();
    assert!(
        out.status.success(),
        "btrfs {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn du_names_read_only_btrfs_snapshots_and_x_leaves_them_out() {
    let Some(dir) = cloned_pair("snap") else {
        eprintln!("RTRASH_REFLINK_DIR unset; skipping");
        return;
    };
    if !on_btrfs(&dir) {
        eprintln!("RTRASH_REFLINK_DIR is not on btrfs; skipping");
        let _ = fs::remove_dir_all(&dir);
        return;
    }
    let (vol, snap) = (dir.join("vol"), dir.join("snap"));
    btrfs(&[
        OsStr::new("subvolume"),
        OsStr::new("create"),
        vol.as_os_str(),
    ]);
    fs::write(vol.join("data.bin"), vec![7u8; 1 << 20]).unwrap();
    btrfs(&[
        OsStr::new("subvolume"),
        OsStr::new("snapshot"),
        OsStr::new("-r"),
        vol.as_os_str(),
        snap.as_os_str(),
    ]);
    let du = |flags: &[&str]| -> Output {
        common::rtrash(&home(&dir))
            .arg("du")
            .args(flags)
            .arg(&dir)
            .current_dir(&dir)
            .output()
            .unwrap()
    };
    let out = du(&["--no-git", "--json"]);
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(out.status.success(), "{err}");
    assert!(
        err.contains("note: 1 read-only btrfs snapshot counted in full"),
        "{err}"
    );
    assert!(
        err.contains(&format!("snapshot '{}'", snap.display())),
        "{err}"
    );
    // A writable subvolume is no snapshot.
    assert!(!err.contains(&format!("'{}'", vol.display())), "{err}");
    let json = String::from_utf8(out.stdout).unwrap();
    let listed = format!("\"snapshots\":[\"{}\"]", snap.display());
    assert!(json.contains(&listed), "{json}");
    // Counted in full, as du(1) counts it.
    let copy = snap.join("data.bin");
    assert!(json.contains(&format!("\"{}\"", copy.display())), "{json}");
    // Shared extents counted once, or one subvolume only: no note.
    for flag in ["--shared", "-x"] {
        let out = du(&["--no-git", "--json", flag]);
        let err = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(out.status.success(), "{err}");
        assert!(!err.contains("btrfs snapshot"), "{flag}: {err}");
    }
    // -x stays on the subvolume of PATH, so neither copy counts.
    let out = du(&["--no-git", "--json", "-x"]);
    let json = String::from_utf8(out.stdout).unwrap();
    assert!(!json.contains("data.bin"), "{json}");
    assert!(!json.contains("\"snapshots\""), "{json}");
    // Unprivileged deletes may be refused; the image goes with the runner.
    let _ = Command::new("btrfs")
        .args(["subvolume", "delete"])
        .arg(&snap)
        .arg(&vol)
        .output();
    let _ = fs::remove_dir_all(&dir);
    let _ = fs::remove_dir_all(dir.with_extension("home"));
}

/// GNU `du -s` of `dir` with `flags`, as a number.
fn gnu_du(dir: &Path, flags: &[&str]) -> u64 {
    let out = Command::new("du")
        .arg("-s")
        .args(flags)
        .arg(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout).unwrap();
    text.split_whitespace().next().unwrap().parse().unwrap()
}

/// Each btrfs subvolume has a device number of its own, so it is a file
/// system boundary for -x without any mount (gdu #666: gdu -x / and
/// du -shx / disagreed). With and without -x, the totals and the inode
/// count match GNU du's.
#[test]
fn du_x_totals_match_gnu_du_x_across_a_btrfs_subvolume() {
    let Some(dir) = cloned_pair("xdev") else {
        eprintln!("RTRASH_REFLINK_DIR unset; skipping");
        return;
    };
    if !on_btrfs(&dir) {
        eprintln!("RTRASH_REFLINK_DIR is not on btrfs; skipping");
        let _ = fs::remove_dir_all(&dir);
        return;
    }
    let vol = dir.join("vol");
    btrfs(&[
        OsStr::new("subvolume"),
        OsStr::new("create"),
        vol.as_os_str(),
    ]);
    fs::create_dir(vol.join("d")).unwrap();
    fs::write(vol.join("d/inner.bin"), vec![3u8; 300_000]).unwrap();
    fs::write(dir.join("outer.bin"), vec![4u8; 70_000]).unwrap();
    // Settle delayed allocation, so both readers see the same blocks.
    assert!(Command::new("sync").status().unwrap().success());
    let arg = dir.to_str().unwrap().to_owned();
    for x in [true, false] {
        let mut flags = vec!["du", "--json", "--no-git"];
        let mut gnu: Vec<&'static str> = Vec::new();
        if x {
            flags.push("-x");
            gnu.push("-x");
        }
        flags.push(&arg);
        let json = run(&dir, &flags);
        let du_with = |flag: &'static str| {
            let mut v = gnu.clone();
            v.push(flag);
            gnu_du(&dir, &v)
        };
        let (disk, apparent, inodes) = (du_with("-B1"), du_with("-b"), du_with("--inodes"));
        assert_eq!(num(&json, "\"roots\"", "disk"), disk, "-x {x}: {json}");
        assert_eq!(
            num(&json, "\"roots\"", "apparent"),
            apparent,
            "-x {x}: {json}"
        );
        let counted = num(&json, "\"roots\"", "files") + num(&json, "\"roots\"", "dirs");
        assert_eq!(counted, inodes, "-x {x}: {json}");
    }
    // -x leaves the subvolume out: its 300 kB count only without it.
    let with = run(&dir, &["du", "--json", "--no-git", "-x", &arg]);
    let without = run(&dir, &["du", "--json", "--no-git", &arg]);
    let gap = num(&without, "\"roots\"", "apparent") - num(&with, "\"roots\"", "apparent");
    assert_eq!(gap, 300_000, "{with}\n{without}");
    let _ = Command::new("btrfs")
        .args(["subvolume", "delete"])
        .arg(&vol)
        .output();
    let _ = fs::remove_dir_all(&dir);
    let _ = fs::remove_dir_all(dir.with_extension("home"));
}
