//! The trash-cli names on Windows: a copy of rtrash.exe named
//! trash-list.exe or trash-put.exe runs as that command.
#![cfg(windows)]

mod common;

use std::fs;
use std::io::Write;
use std::process::{Command, Output, Stdio};

/// Run `exe` in `dir` with `args`, feeding `input` on stdin.
fn run_with(exe: &std::path::Path, dir: &std::path::Path, args: &[&str], input: &str) -> Output {
    let mut child = Command::new(exe)
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

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

#[test]
fn trash_cli_names_match_in_any_case() {
    let dir = std::env::temp_dir().join(format!("rtrash-names-case-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let list = dir.join("TRASH-LIST.exe");
    fs::copy(env!("CARGO_BIN_EXE_rtrash"), &list).unwrap();
    let out = Command::new(&list).arg("--help").output().unwrap();
    let _ = fs::remove_dir_all(&dir);
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(help.contains("Usage: trash-list"), "{help}");
}

#[test]
fn trash_restore_scopes_to_the_current_directory() {
    let dir = std::env::temp_dir().join(format!("rtrash-names-cwd-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let (here, away) = (dir.join("here"), dir.join("away"));
    fs::create_dir_all(&here).unwrap();
    fs::create_dir_all(&away).unwrap();
    // An item trashed from elsewhere stays out of the current directory scope.
    let gone = away.join("elsewhere.txt");
    fs::write(&gone, "x").unwrap();
    let put = Command::new(env!("CARGO_BIN_EXE_rtrash"))
        .arg("put")
        .arg(&gone)
        .output()
        .unwrap();
    assert!(
        put.status.success(),
        "{}",
        String::from_utf8_lossy(&put.stderr)
    );
    let restore = dir.join("trash-restore.exe");
    fs::copy(env!("CARGO_BIN_EXE_rtrash"), &restore).unwrap();
    let out = Command::new(&restore).current_dir(&here).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    // --all says the same, as trash-cli does.
    let all = Command::new(&restore)
        .arg("--all")
        .current_dir(&here)
        .output()
        .unwrap();
    let all_out = String::from_utf8_lossy(&all.stdout).into_owned();
    // Tidy the Recycle Bin item before the asserts.
    let _ = Command::new(env!("CARGO_BIN_EXE_rtrash"))
        .arg("restore")
        .arg(&gone)
        .output();
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(out.status.code(), Some(1), "{stdout}");
    assert!(
        stdout.starts_with("No files trashed from current dir ('"),
        "{stdout}"
    );
    assert!(stdout.contains("here"), "{stdout}");
    assert_eq!(all.status.code(), Some(1), "{all_out}");
    assert!(
        all_out.starts_with("No files trashed from current dir ('"),
        "{all_out}"
    );
}

#[test]
fn trash_restore_takes_path_as_a_scope() {
    let dir = std::env::temp_dir().join(format!("rtrash-names-scope-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let sub = dir.join("sub");
    fs::create_dir_all(&sub).unwrap();
    // One item from under sub/: trash-restore sub asks, as trash-cli does.
    let inner = sub.join("inner.txt");
    fs::write(&inner, "in").unwrap();
    let put = Command::new(env!("CARGO_BIN_EXE_rtrash"))
        .arg("put")
        .arg(&inner)
        .output()
        .unwrap();
    assert!(
        put.status.success(),
        "{}",
        String::from_utf8_lossy(&put.stderr)
    );
    let restore = dir.join("trash-restore.exe");
    fs::copy(env!("CARGO_BIN_EXE_rtrash"), &restore).unwrap();
    // A blank answer restores nothing.
    let blank = run_with(&restore, &dir, &["sub"], "\n");
    let kept = !inner.exists();
    let out = run_with(&restore, &dir, &["sub"], "0\n");
    let back = fs::read(&inner).ok();
    if back.is_none() {
        let _ = Command::new(env!("CARGO_BIN_EXE_rtrash"))
            .arg("restore")
            .arg(&inner)
            .output();
    }
    let _ = fs::remove_dir_all(&dir);
    let said = String::from_utf8_lossy(&blank.stdout).into_owned();
    assert_eq!(blank.status.code(), Some(0), "{said}");
    // trash-cli's row: a width-4 index, then one space between columns.
    let row = said.lines().find(|l| l.starts_with("   0 ")).unwrap_or("");
    assert_eq!(row.as_bytes().get(15), Some(&b' '), "{said}");
    assert!(row.ends_with("inner.txt"), "{said}");
    let asked = String::from_utf8_lossy(&blank.stderr).into_owned();
    // The answer comes from a pipe, so no prompt shows, as on FreeDesktop.
    assert!(!asked.contains("What file to restore"), "{asked}");
    assert!(said.contains("No files were restored"), "{said}");
    assert!(kept, "a blank answer restored the item");
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(back.as_deref(), Some(&b"in"[..]));
}

#[test]
fn trash_restore_takes_a_range_pick() {
    let dir = std::env::temp_dir().join(format!("rtrash-names-pick-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let sub = dir.join("sub");
    fs::create_dir_all(&sub).unwrap();
    let names = ["a.txt", "b.txt", "c.txt"];
    for n in names {
        fs::write(sub.join(n), n).unwrap();
        let put = Command::new(env!("CARGO_BIN_EXE_rtrash"))
            .arg("put")
            .arg(sub.join(n))
            .output()
            .unwrap();
        assert!(
            put.status.success(),
            "{}",
            String::from_utf8_lossy(&put.stderr)
        );
        common::next_trash_second();
    }
    let restore = dir.join("trash-restore.exe");
    fs::copy(env!("CARGO_BIN_EXE_rtrash"), &restore).unwrap();
    // Rows run oldest first: 0 is a.txt, 2 is c.txt.
    let out = run_with(&restore, &dir, &["sub"], "0,2\n");
    let back: Vec<bool> = names.iter().map(|n| sub.join(n).exists()).collect();
    for n in names {
        if !sub.join(n).exists() {
            let _ = Command::new(env!("CARGO_BIN_EXE_rtrash"))
                .arg("restore")
                .arg(sub.join(n))
                .output();
        }
    }
    let _ = fs::remove_dir_all(&dir);
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(back, [true, false, true]);
}

#[test]
fn a_blank_pick_asks_for_an_index() {
    let dir = std::env::temp_dir().join(format!("rtrash-names-blank-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let sub = dir.join("sub");
    fs::create_dir_all(&sub).unwrap();
    let names = ["one.txt", "two.txt"];
    for n in names {
        fs::write(sub.join(n), n).unwrap();
        let put = Command::new(env!("CARGO_BIN_EXE_rtrash"))
            .arg("put")
            .arg(sub.join(n))
            .output()
            .unwrap();
        assert!(
            put.status.success(),
            "{}",
            String::from_utf8_lossy(&put.stderr)
        );
        common::next_trash_second();
    }
    let scope = format!("--directory={}", sub.display());
    let out = run_with(
        std::path::Path::new(env!("CARGO_BIN_EXE_rtrash")),
        &dir,
        &["restore", &scope],
        "\n",
    );
    let kept = names.iter().all(|n| !sub.join(n).exists());
    for n in names {
        let _ = Command::new(env!("CARGO_BIN_EXE_rtrash"))
            .arg("restore")
            .arg(sub.join(n))
            .output();
    }
    let _ = fs::remove_dir_all(&dir);
    let said = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(out.status.code(), Some(1), "{said}");
    assert!(
        said.contains("2 matches; provide an index 0..1 on stdin"),
        "{said}"
    );
    assert!(kept, "a blank answer restored something");
}

#[test]
fn restore_takes_age_filters_on_windows() {
    let dir = std::env::temp_dir().join(format!("rtrash-names-age-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join("fresh.txt");
    fs::write(&file, "fresh").unwrap();
    let rtrash = std::path::Path::new(env!("CARGO_BIN_EXE_rtrash"));
    let put = Command::new(rtrash).arg("put").arg(&file).output().unwrap();
    assert!(
        put.status.success(),
        "{}",
        String::from_utf8_lossy(&put.stderr)
    );
    let scope = format!("--directory={}", dir.display());
    // Trashed just now, so nothing is older than a day.
    let old = run_with(
        rtrash,
        &dir,
        &["restore", &scope, "--older-than=1", "--all"],
        "",
    );
    let kept = !file.exists();
    let bad = run_with(rtrash, &dir, &["restore", "--older-than=soon"], "");
    let new = run_with(
        rtrash,
        &dir,
        &["restore", &scope, "--newer-than=1h", "--all"],
        "",
    );
    let back = fs::read(&file).ok();
    if back.is_none() {
        let _ = Command::new(rtrash).arg("restore").arg(&file).output();
    }
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(
        old.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&old.stderr)
    );
    assert!(kept, "--older-than=1 restored a fresh item");
    assert_eq!(
        bad.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&bad.stderr)
    );
    assert!(
        new.status.success(),
        "{}",
        String::from_utf8_lossy(&new.stderr)
    );
    assert_eq!(back.as_deref(), Some(&b"fresh"[..]));
}

#[test]
fn restore_takes_size_filters_on_windows() {
    let dir = std::env::temp_dir().join(format!("rtrash-names-size-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let (big, small) = (dir.join("big.bin"), dir.join("small.txt"));
    fs::write(&big, vec![7u8; 1 << 20]).unwrap();
    fs::write(&small, "s").unwrap();
    let rtrash = std::path::Path::new(env!("CARGO_BIN_EXE_rtrash"));
    for f in [&big, &small] {
        let put = Command::new(rtrash).arg("put").arg(f).output().unwrap();
        assert!(
            put.status.success(),
            "{}",
            String::from_utf8_lossy(&put.stderr)
        );
    }
    let scope = format!("--directory={}", dir.display());
    let out = run_with(
        rtrash,
        &dir,
        &["restore", &scope, "--smaller-than", "100k", "--all"],
        "",
    );
    let (big_back, small_back) = (big.exists(), small.exists());
    let bad = run_with(rtrash, &dir, &["restore", "--larger-than=lots"], "");
    for f in [&big, &small] {
        if !f.exists() {
            let _ = Command::new(rtrash).arg("restore").arg(f).output();
        }
    }
    let _ = fs::remove_dir_all(&dir);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        small_back && !big_back,
        "small {small_back}, big {big_back}"
    );
    assert_eq!(
        bad.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&bad.stderr)
    );
}

#[test]
fn restore_sorts_by_size_on_windows() {
    let dir = std::env::temp_dir().join(format!("rtrash-names-sortsize-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let (big, small) = (dir.join("big.bin"), dir.join("small.txt"));
    fs::write(&small, "s").unwrap();
    fs::write(&big, vec![7u8; 1 << 20]).unwrap();
    let rtrash = std::path::Path::new(env!("CARGO_BIN_EXE_rtrash"));
    // The small file goes first, so date order would list it at 0.
    for f in [&small, &big] {
        let put = Command::new(rtrash).arg("put").arg(f).output().unwrap();
        assert!(
            put.status.success(),
            "{}",
            String::from_utf8_lossy(&put.stderr)
        );
        common::next_trash_second();
    }
    let scope = format!("--directory={}", dir.display());
    let out = run_with(
        rtrash,
        &dir,
        &["restore", &scope, "--plain", "--sort=size"],
        "0\n",
    );
    let (big_back, small_back) = (big.exists(), small.exists());
    for f in [&big, &small] {
        if !f.exists() {
            let _ = Command::new(rtrash).arg("restore").arg(f).output();
        }
    }
    let _ = fs::remove_dir_all(&dir);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        big_back && !small_back,
        "big {big_back}, small {small_back}"
    );
}

#[test]
fn put_inspect_asks_with_the_size_on_windows() {
    let name = format!("rtrash-names-inspect-{}", std::process::id());
    let dir = std::env::temp_dir().join(name);
    let _ = fs::remove_dir_all(&dir);
    let proj = dir.join("proj");
    fs::create_dir_all(proj.join("sub")).unwrap();
    for f in ["a.txt", "b.txt", "sub/c.txt"] {
        fs::write(proj.join(f), "data").unwrap();
    }
    let rtrash = std::path::Path::new(env!("CARGO_BIN_EXE_rtrash"));
    // A "no" keeps it, and the prompt names the file count.
    let no = run_with(rtrash, &dir, &["put", "--inspect", "proj"], "n\n");
    let err = String::from_utf8_lossy(&no.stderr).to_string();
    let kept = proj.exists();
    // A "yes" moves it to the Recycle Bin.
    let yes = run_with(rtrash, &dir, &["put", "--inspect", "proj"], "y\n");
    let gone = !proj.exists();
    if gone {
        let _ = Command::new(rtrash).arg("restore").arg(&proj).output();
    }
    let _ = fs::remove_dir_all(&dir);
    assert!(err.contains("remove directory '"), "{err}");
    assert!(err.contains(", 3 files)? "), "{err}");
    assert!(kept, "{err}");
    assert!(
        yes.status.success(),
        "{}",
        String::from_utf8_lossy(&yes.stderr)
    );
    assert!(gone);
}

#[test]
fn list_depth_shows_a_deleted_tree_on_windows() {
    let name = format!("rtrash-names-depth-{}", std::process::id());
    let dir = std::env::temp_dir().join(name);
    let _ = fs::remove_dir_all(&dir);
    let proj = dir.join("proj");
    fs::create_dir_all(proj.join("sub").join("deeper")).unwrap();
    fs::write(proj.join("a.txt"), "a").unwrap();
    fs::write(proj.join("sub").join("c.txt"), "c").unwrap();
    fs::write(proj.join("sub").join("deeper").join("x.txt"), "x").unwrap();
    let rtrash = std::path::Path::new(env!("CARGO_BIN_EXE_rtrash"));
    let put = Command::new(rtrash).arg("put").arg(&proj).output().unwrap();
    let scope = format!("--directory={}", dir.display());
    let out = run_with(rtrash, &dir, &["list", &scope, "--depth=2"], "");
    let bad = run_with(rtrash, &dir, &["list", "--depth=x"], "");
    let _ = Command::new(rtrash).arg("restore").arg(&proj).output();
    let _ = fs::remove_dir_all(&dir);
    assert!(
        put.status.success(),
        "{}",
        String::from_utf8_lossy(&put.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let lines: Vec<&str> = text.lines().collect();
    let row = lines.iter().position(|l| l.ends_with("proj"));
    let row = row.unwrap_or_else(|| panic!("{text}"));
    let want = ["    a.txt", "    sub\\", "      c.txt", "      deeper\\"];
    assert_eq!(lines[row + 1..], want, "{text}");
    assert_eq!(bad.status.code(), Some(2));
}

/// Under `RTRASH_NO_PERMANENT` rm refuses before it starts, and a restore
/// that would overwrite a file fails with both copies intact.
#[test]
fn no_permanent_guard_keeps_the_recycle_bin_on_windows() {
    let dir = common::scratch_dir("names", "noperm");
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join("pg.txt");
    fs::write(&file, "trashed").unwrap();
    let rtrash = env!("CARGO_BIN_EXE_rtrash");
    let put = Command::new(rtrash).arg("put").arg(&file).output().unwrap();
    fs::write(&file, "newer").unwrap();
    let guarded = |args: &[&str]| {
        Command::new(rtrash)
            .args(args)
            .current_dir(&dir)
            .env("RTRASH_NO_PERMANENT", "1")
            .output()
            .unwrap()
    };
    let path = file.to_str().unwrap();
    let rm = guarded(&["rm", path]);
    let over = guarded(&["restore", "-f", path]);
    let kept = fs::read_to_string(&file).unwrap_or_default();
    // Put the trashed copy back so the Recycle Bin is left as found.
    let _ = fs::remove_file(&file);
    let back = Command::new(rtrash)
        .arg("restore")
        .arg(&file)
        .output()
        .unwrap();
    let restored = fs::read_to_string(&file).unwrap_or_default();
    let _ = fs::remove_dir_all(&dir);
    let text = |o: &Output| String::from_utf8_lossy(&o.stderr).into_owned();
    assert!(put.status.success(), "{}", text(&put));
    assert_eq!(rm.status.code(), Some(2), "{}", text(&rm));
    assert!(text(&rm).contains("RTRASH_NO_PERMANENT"), "{}", text(&rm));
    assert!(!over.status.success(), "{}", text(&over));
    assert!(
        text(&over).contains("RTRASH_NO_PERMANENT"),
        "{}",
        text(&over)
    );
    assert_eq!(kept, "newer");
    assert!(back.status.success(), "{}", text(&back));
    assert_eq!(restored, "trashed", "the guard lost the trashed copy");
}
