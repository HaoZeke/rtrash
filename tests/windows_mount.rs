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

/// A private `%LOCALAPPDATA%` per test, so the last-put records of tests
/// running side by side stay apart.
fn local_app_data(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("rtrash-lad-{tag}-{}", std::process::id()));
    fs::create_dir_all(&d).unwrap();
    d
}

/// The binary with a private `%LOCALAPPDATA%` and debug output on.
fn cmd(lad: &Path) -> Command {
    let mut c = Command::new(bin());
    c.env("LOCALAPPDATA", lad).env("RTRASH_DEBUG", "1");
    c
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

#[test]
fn restore_of_one_txt_keeps_its_extension() {
    let Some(m) = mounted() else {
        return;
    };
    let lad = local_app_data("keep-ext");
    let f = m.join(format!("keep-ext-{}.txt", std::process::id()));
    fs::write(&f, b"x").unwrap();
    let out = cmd(&lad).arg("put").arg(&f).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!f.exists());
    let out = cmd(&lad).args(["restore", "-n"]).arg(&f).output().unwrap();
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(said.starts_with("would restore '"), "{said}");
    assert!(!f.exists(), "a dry run restored the file");
    let out = cmd(&lad)
        .args(["restore", "--json"])
        .arg(&f)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json = String::from_utf8_lossy(&out.stdout);
    assert!(json.starts_with("{\"restored\":[{\"original\":"), "{json}");
    assert!(
        json.trim_end().ends_with("\"failed\":0,\"skipped\":0}"),
        "{json}"
    );
    assert!(f.exists(), "restored without its extension");
    assert!(!f.with_extension("").exists());
    fs::remove_file(&f).unwrap();
    let _ = fs::remove_dir_all(&lad);
}

#[test]
fn restore_batch_last_brings_back_the_latest_put() {
    let Some(m) = mounted() else {
        return;
    };
    let lad = local_app_data("batch-last");
    let id = std::process::id();
    let a = m.join(format!("run-a-{id}.txt"));
    let b = m.join(format!("run-b-{id}.txt"));
    for p in [&a, &b] {
        fs::write(p, b"x").unwrap();
    }
    let out = cmd(&lad).arg("put").arg(&a).arg(&b).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!a.exists() && !b.exists());
    let out = cmd(&lad)
        .args(["restore", "--dry-run", "--batch=last"])
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(said.matches("would restore '").count(), 2, "{said}");
    assert!(!a.exists() && !b.exists(), "a dry run restored the run");
    let out = cmd(&lad)
        .args(["restore", "--batch=last"])
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{err}");
    assert!(a.exists() && b.exists(), "{err}");
    let out = cmd(&lad).args(["restore", "--batch=7"]).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    for p in [&a, &b] {
        fs::remove_file(p).unwrap();
    }
    let _ = fs::remove_dir_all(&lad);
}

#[test]
fn restore_conflict_policies_skip_rename_and_refuse() {
    let Some(m) = mounted() else {
        return;
    };
    let lad = local_app_data("conflict");
    let f = m.join(format!("conflict-{}.txt", std::process::id()));
    let renamed = f.with_extension("1.txt");
    fs::write(&f, b"old").unwrap();
    let out = cmd(&lad).arg("put").arg(&f).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    fs::write(&f, b"live").unwrap();
    let run = |extra: &[&str]| {
        cmd(&lad)
            .arg("restore")
            .args(extra)
            .arg(&f)
            .output()
            .unwrap()
    };
    // Refuse is the default: exit 1, the live file stays.
    let out = run(&["--json"]);
    assert_eq!(out.status.code(), Some(1));
    let json = String::from_utf8_lossy(&out.stdout);
    assert!(json.contains("\"failed\":1"), "{json}");
    assert_eq!(fs::read(&f).unwrap(), b"live");
    let out = run(&["--conflict=skip", "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("\"skipped\":1"));
    let out = run(&["--conflict=rename", "--dry-run", "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json = String::from_utf8_lossy(&out.stdout);
    assert!(json.starts_with("{\"dry_run\":true,"), "{json}");
    assert!(!renamed.exists(), "a dry run renamed");
    let out = run(&["--conflict=rename"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(fs::read(&f).unwrap(), b"live");
    assert_eq!(fs::read(&renamed).unwrap(), b"old");
    assert_eq!(run(&["--conflict=nope"]).status.code(), Some(2));
    let _ = fs::remove_file(&f);
    let _ = fs::remove_file(&renamed);
    let _ = fs::remove_dir_all(&lad);
}

#[test]
fn restore_matchers_with_latest_and_all() {
    let Some(m) = mounted() else {
        return;
    };
    let lad = local_app_data("matchers");
    let id = std::process::id();
    let a = m.join(format!("match-a-{id}.log"));
    let b = m.join(format!("match-b-{id}.log"));
    for p in [&a, &b] {
        fs::write(p, b"x").unwrap();
        let out = cmd(&lad).arg("put").arg(p).output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        std::thread::sleep(std::time::Duration::from_millis(1100));
    }
    let glob = format!("--glob=match-*-{id}.log");
    let run = |args: &[&str]| cmd(&lad).arg("restore").args(args).output().unwrap();
    assert_eq!(run(&["--latest", "--all", &glob]).status.code(), Some(2));
    assert_eq!(
        run(&["--name=x", "--regex=y"]).status.code(),
        Some(2),
        "two matchers"
    );
    let out = run(&["--dry-run", "--latest", &glob]);
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(said.matches("would restore").count(), 1, "{said}");
    assert!(said.contains(&format!("match-b-{id}.log")), "{said}");
    let out = run(&["--latest", &glob]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(b.exists() && !a.exists());
    let out = run(&["--all", &format!("--name=match-a-{id}.log")]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(a.exists());
    let out = run(&["--json", &glob]);
    assert_eq!(out.status.code(), Some(1), "nothing left to match");
    for p in [&a, &b] {
        let _ = fs::remove_file(p);
    }
    let _ = fs::remove_dir_all(&lad);
}

#[test]
fn restore_to_a_directory_keeps_the_name() {
    let Some(m) = mounted() else {
        return;
    };
    let lad = local_app_data("to-dir");
    let id = std::process::id();
    let f = m.join(format!("to-dir-{id}.txt"));
    let dir = m.join(format!("to-dir-dest-{id}"));
    fs::create_dir_all(&dir).unwrap();
    let there = dir.join(format!("to-dir-{id}.txt"));
    fs::write(&f, b"old").unwrap();
    let out = cmd(&lad).arg("put").arg(&f).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    fs::write(&there, b"live").unwrap();
    let to = format!("--to={}", dir.display());
    let run = |extra: &[&str]| {
        cmd(&lad)
            .arg("restore")
            .arg(&to)
            .args(extra)
            .arg(&f)
            .output()
            .unwrap()
    };
    let bad = cmd(&lad)
        .arg("restore")
        .arg(format!("--to={}", dir.join("missing").display()))
        .arg(&f)
        .output()
        .unwrap();
    assert_eq!(bad.status.code(), Some(2));
    // The name is taken in DIR: refuse, then a rename dry run, then rename.
    assert_eq!(run(&[]).status.code(), Some(1));
    let out = run(&["--conflict=rename", "--dry-run", "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json = String::from_utf8_lossy(&out.stdout);
    assert!(json.starts_with("{\"dry_run\":true,"), "{json}");
    assert!(json.contains(&format!("to-dir-{id}.1.txt")), "{json}");
    let out = run(&["--conflict=rename", "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(fs::read(&there).unwrap(), b"live");
    assert_eq!(
        fs::read(dir.join(format!("to-dir-{id}.1.txt"))).unwrap(),
        b"old"
    );
    assert!(!f.exists(), "--to restored to the original place");
    let _ = fs::remove_dir_all(&dir);
    let _ = fs::remove_dir_all(&lad);
}

#[test]
fn restore_sort_orders_the_numbered_list() {
    let Some(m) = mounted() else {
        return;
    };
    let lad = local_app_data("sort");
    let id = std::process::id();
    // b goes first, so it is the oldest; a sorts first by path.
    let b = m.join(format!("sort-b-{id}.txt"));
    let a = m.join(format!("sort-a-{id}.txt"));
    for p in [&b, &a] {
        fs::write(p, b"x").unwrap();
        let out = cmd(&lad).arg("put").arg(p).output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        std::thread::sleep(std::time::Duration::from_millis(1100));
    }
    let glob = format!("--glob=sort-?-{id}.txt");
    let pick = |extra: &[&str]| {
        use std::io::Write;
        let mut child = cmd(&lad)
            .args(["restore", "--plain", "--dry-run", &glob])
            .args(extra)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(b"0\n").unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        text.lines()
            .find(|l| l.starts_with("would restore"))
            .unwrap_or_default()
            .to_string()
    };
    assert!(
        pick(&[]).contains("sort-b-"),
        "date order puts the oldest first"
    );
    assert!(pick(&["--sort=path"]).contains("sort-a-"), "path order");
    assert!(pick(&["--sort=date", "--newest-first"]).contains("sort-a-"));
    let bad = cmd(&lad).args(["restore", "--sort=size"]).output().unwrap();
    assert_eq!(bad.status.code(), Some(2));
    for p in [&a, &b] {
        let _ = cmd(&lad).arg("restore").arg(p).output();
        let _ = fs::remove_file(p);
    }
    let _ = fs::remove_dir_all(&lad);
}

#[test]
fn restore_cwd_only_keeps_items_from_here() {
    let Some(m) = mounted() else {
        return;
    };
    let lad = local_app_data("cwd-only");
    let id = std::process::id();
    let here = m.join(format!("cwd-here-{id}"));
    fs::create_dir_all(&here).unwrap();
    let inside = here.join("in.txt");
    let outside = m.join(format!("cwd-out-{id}.txt"));
    for p in [&inside, &outside] {
        fs::write(p, b"x").unwrap();
        let out = cmd(&lad).arg("put").arg(p).output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let out = cmd(&lad)
        .args(["restore", "--cwd-only", "--all", "--dry-run"])
        .current_dir(&here)
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(said.matches("would restore").count(), 1, "{said}");
    assert!(said.contains("in.txt"), "{said}");
    // list --cwd-only sees the same item from the mounted folder.
    let out = cmd(&lad)
        .args(["list", "--cwd-only"])
        .current_dir(&here)
        .output()
        .unwrap();
    let listed = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(listed.contains("in.txt"), "{listed}");
    assert!(!listed.contains(&format!("cwd-out-{id}")), "{listed}");
    // A single match restores right away.
    let out = cmd(&lad)
        .args(["restore", "--cwd-only"])
        .current_dir(&here)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(inside.exists() && !outside.exists());
    let _ = cmd(&lad).arg("restore").arg(&outside).output();
    let _ = fs::remove_file(&outside);
    let _ = fs::remove_dir_all(&here);
    let _ = fs::remove_dir_all(&lad);
}

#[test]
fn directory_scopes_list_and_restore_on_the_mounted_volume() {
    let Some(m) = mounted() else {
        return;
    };
    let lad = local_app_data("directory");
    let id = std::process::id();
    let proj = m.join(format!("dir-proj-{id}"));
    fs::create_dir_all(&proj).unwrap();
    let inside = proj.join("in.txt");
    let outside = m.join(format!("dir-out-{id}.txt"));
    for p in [&inside, &outside] {
        fs::write(p, b"x").unwrap();
        let out = cmd(&lad).arg("put").arg(p).output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    // The folder is gone; its items still match by DIR.
    fs::remove_dir_all(&proj).unwrap();
    let dir = format!("--directory={}", proj.display());
    let out = cmd(&lad).args(["list", &dir]).output().unwrap();
    let listed = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(listed.contains("in.txt"), "{listed}");
    assert!(!listed.contains(&format!("dir-out-{id}")), "{listed}");
    // A relative DIR counts from the current directory.
    let rel = format!("--directory=dir-proj-{id}");
    let out = cmd(&lad)
        .args(["restore", &rel, "--all", "--dry-run"])
        .current_dir(&m)
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(said.matches("would restore").count(), 1, "{said}");
    for bad in [
        vec!["list", "--directory="],
        vec!["restore", "--directory="],
        vec!["list", "--cwd-only", dir.as_str()],
        vec!["restore", "--cwd-only", dir.as_str()],
    ] {
        let out = cmd(&lad).args(&bad).output().unwrap();
        assert_eq!(out.status.code(), Some(2), "{bad:?}");
    }
    // A single match restores right away and recreates the folder.
    let out = cmd(&lad).args(["restore", &dir]).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(inside.exists() && !outside.exists());
    let _ = cmd(&lad).arg("restore").arg(&outside).output();
    let _ = fs::remove_file(&outside);
    let _ = fs::remove_dir_all(&proj);
    let _ = fs::remove_dir_all(&lad);
}

/// Odd but valid NTFS names through put, list, restore and a vanished
/// Recycle Bin payload; no command may panic.
#[test]
fn odd_names_survive_the_recycle_bin() {
    let Some(m) = mounted() else {
        return;
    };
    let id = std::process::id();
    let names = [
        format!("-leading {id}.txt"),
        format!("%2F not a slash {id}"),
        format!("e\u{301}cole {id}.md"),
        format!("\u{202e}txt.exe {id}"),
        format!("emoji \u{1f5d1} {id}.tar.gz"),
        format!("two  spaces {id}"),
    ];
    let run = |args: &[&std::ffi::OsStr]| {
        let o = Command::new(bin()).args(args).output().unwrap();
        let e = String::from_utf8_lossy(&o.stderr).into_owned();
        assert!(
            o.status.code() != Some(101) && !e.contains("panicked"),
            "{args:?} panicked: {e}"
        );
        o
    };
    let os = |s: &str| std::ffi::OsString::from(s);
    let paths: Vec<PathBuf> = names.iter().map(|n| m.join(n)).collect();
    for p in &paths {
        fs::write(p, p.to_string_lossy().as_bytes()).unwrap();
    }
    let mut put = vec![os("put"), os("--")];
    put.extend(paths.iter().map(|p| p.clone().into_os_string()));
    let refs: Vec<&std::ffi::OsStr> = put.iter().map(|s| s.as_os_str()).collect();
    let o = run(&refs);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let o = run(&[os("list").as_os_str(), os("--format={path}\\n").as_os_str()]);
    let text = String::from_utf8_lossy(&o.stdout).into_owned();
    for p in &paths {
        assert!(
            text.lines().any(|l| Path::new(l) == p),
            "{p:?} not in {text}"
        );
    }
    for args in [
        vec!["list", "--size"],
        vec!["list", "--glob=*"],
        vec!["status"],
    ] {
        let a: Vec<std::ffi::OsString> = args.iter().map(|s| os(s)).collect();
        let r: Vec<&std::ffi::OsStr> = a.iter().map(|s| s.as_os_str()).collect();
        assert!(run(&r).status.success(), "{args:?}");
    }
    let o = run(&[os("restore").as_os_str(), paths[2].as_os_str()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(
        fs::read(&paths[2]).unwrap(),
        paths[2].to_string_lossy().as_bytes()
    );
    let o = run(&[os("restore").as_os_str(), os("--batch=last").as_os_str()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    for p in &paths {
        assert!(p.exists(), "{p:?}");
        let _ = fs::remove_file(p);
    }
}

/// A name holding an unpaired surrogate, which NTFS stores as it is but
/// which is not valid text: put, list and restore by path must keep the
/// exact name (and its extension), and nothing may panic.
#[test]
fn unpaired_surrogate_names_survive_the_recycle_bin() {
    use std::os::windows::ffi::OsStringExt;
    let Some(m) = mounted() else {
        return;
    };
    let lad = local_app_data("surrogate");
    let id = std::process::id();
    let units: Vec<u16> = "lone-"
        .encode_utf16()
        .chain([0xD800])
        .chain(format!(" {id}.txt").encode_utf16())
        .collect();
    let f = m.join(std::ffi::OsString::from_wide(&units));
    if let Err(e) = fs::write(&f, b"surrogate") {
        eprintln!("skipped: the volume refused the name: {e}");
        return;
    }
    let out = cmd(&lad).arg("put").arg(&f).output().unwrap();
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(!err.contains("panicked"), "{err}");
    assert!(out.status.success(), "{err}");
    assert!(!f.exists());
    let out = cmd(&lad)
        .args(["list", "--format={name}\\n"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = cmd(&lad).arg("restore").arg(&f).output().unwrap();
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(!err.contains("panicked"), "{err}");
    assert!(out.status.success(), "{err}");
    assert_eq!(fs::read(&f).unwrap(), b"surrogate", "{err}");
    let out = cmd(&lad).arg("put").arg(&f).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = cmd(&lad)
        .args(["restore", "--batch=last"])
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(out.status.success(), "{err}");
    assert!(f.exists(), "{err}");
    fs::remove_file(&f).unwrap();
    let _ = fs::remove_dir_all(&lad);
}
