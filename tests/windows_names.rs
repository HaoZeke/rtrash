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
    let put = common::rtrash_real(common::Real::RecycleBin)
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
    let _ = common::rtrash_real(common::Real::RecycleBin)
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
    let put = common::rtrash_real(common::Real::RecycleBin)
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
        let _ = common::rtrash_real(common::Real::RecycleBin)
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
        let put = common::rtrash_real(common::Real::RecycleBin)
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
            let _ = common::rtrash_real(common::Real::RecycleBin)
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
        let put = common::rtrash_real(common::Real::RecycleBin)
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
        let _ = common::rtrash_real(common::Real::RecycleBin)
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
    let put = common::rtrash_real(common::Real::RecycleBin)
        .arg("put")
        .arg(&file)
        .output()
        .unwrap();
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
        let _ = common::rtrash_real(common::Real::RecycleBin)
            .arg("restore")
            .arg(&file)
            .output();
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
        let put = common::rtrash_real(common::Real::RecycleBin)
            .arg("put")
            .arg(f)
            .output()
            .unwrap();
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
            let _ = common::rtrash_real(common::Real::RecycleBin)
                .arg("restore")
                .arg(f)
                .output();
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
        let put = common::rtrash_real(common::Real::RecycleBin)
            .arg("put")
            .arg(f)
            .output()
            .unwrap();
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
            let _ = common::rtrash_real(common::Real::RecycleBin)
                .arg("restore")
                .arg(f)
                .output();
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
        let _ = common::rtrash_real(common::Real::RecycleBin)
            .arg("restore")
            .arg(&proj)
            .output();
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
    let put = common::rtrash_real(common::Real::RecycleBin)
        .arg("put")
        .arg(&proj)
        .output()
        .unwrap();
    let scope = format!("--directory={}", dir.display());
    let out = run_with(rtrash, &dir, &["list", &scope, "--depth=2"], "");
    let bad = run_with(rtrash, &dir, &["list", "--depth=x"], "");
    let _ = common::rtrash_real(common::Real::RecycleBin)
        .arg("restore")
        .arg(&proj)
        .output();
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
    let put = common::rtrash_real(common::Real::RecycleBin)
        .arg("put")
        .arg(&file)
        .output()
        .unwrap();
    fs::write(&file, "newer").unwrap();
    let guarded = |args: &[&str]| {
        common::rtrash_real(common::Real::RecycleBin)
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
    let back = common::rtrash_real(common::Real::RecycleBin)
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

/// The cap on permanent deletions counts Recycle Bin matches first, so an
/// rm past it purges nothing. The PATTERN names files only (no folder),
/// so a short 8.3 %TEMP% and the long path the bin records both match.
#[test]
fn max_count_refuses_a_recycle_bin_rm_past_the_cap() {
    let dir = common::scratch_dir("names", "maxcount");
    let tag = dir.file_name().unwrap().to_string_lossy().into_owned();
    let files: Vec<_> = ["a", "b", "c"]
        .iter()
        .map(|n| dir.join(format!("{tag}-{n}.txt")))
        .collect();
    for f in &files {
        fs::write(f, "x").unwrap();
    }
    let put = common::rtrash_real(common::Real::RecycleBin)
        .arg("put")
        .args(&files)
        .output()
        .unwrap();
    let pattern = format!("{tag}-*.txt");
    let rm = |args: &[&str], cap: Option<&str>| {
        let mut c = common::rtrash_real(common::Real::RecycleBin);
        c.arg("rm").args(args).arg(&pattern).current_dir(&dir);
        match cap {
            Some(v) => c.env("RTRASH_MAX_PERMANENT", v),
            None => c.env_remove("RTRASH_MAX_PERMANENT"),
        };
        c.output().unwrap()
    };
    // A dry run prints one line per matched item, its original path. The
    // tag names the folder and starts each file name, so count lines.
    let listed = |o: &Output| {
        String::from_utf8_lossy(&o.stdout)
            .lines()
            .filter(|l| l.contains(&tag))
            .count()
    };
    let before = listed(&rm(&["-n"], None));
    let flag = rm(&["--max-count=2"], None);
    let env = rm(&[], Some("2"));
    let still = listed(&rm(&["-n"], Some("2")));
    // Within the cap the purge runs, which also leaves the bin as found.
    let done = rm(&["--max-count=3"], Some("2"));
    let after = listed(&rm(&["-n"], None));
    let _ = fs::remove_dir_all(&dir);
    let text = |o: &Output| String::from_utf8_lossy(&o.stderr).into_owned();
    assert!(put.status.success(), "{}", text(&put));
    assert_eq!(before, 3, "the put did not leave three items");
    for o in [&flag, &env] {
        assert_eq!(o.status.code(), Some(2), "{}", text(o));
        assert!(text(o).contains("3 items would be deleted"), "{}", text(o));
    }
    assert_eq!(still, before, "a refused rm purged items");
    assert!(done.status.success(), "{}", text(&done));
    assert_eq!(after, 0);
}

/// `RTRASH_ALLOW_ROOTS` on the Recycle Bin. The root and the paths may
/// differ in letter case and in the short 8.3 %TEMP% against the long path
/// the bin records, and still compare equal; a path outside is refused
/// with nothing moved, and rm purges only items trashed from inside.
#[test]
fn allow_roots_scopes_the_recycle_bin_on_windows() {
    let dir = common::scratch_dir("names", "allowroots");
    let tag = dir.file_name().unwrap().to_string_lossy().into_owned();
    let (inside, outside) = (dir.join("in"), dir.join("out"));
    fs::create_dir_all(&inside).unwrap();
    fs::create_dir_all(&outside).unwrap();
    let long = fs::canonicalize(&inside).unwrap();
    let long = long.to_string_lossy();
    let long = long.trim_start_matches(r"\\?\").to_string();
    let file = inside.join(format!("{tag}-x.txt"));
    let stray = outside.join(format!("{tag}-y.txt"));
    fs::write(&file, "x").unwrap();
    fs::write(&stray, "y").unwrap();
    let run = |args: &[&std::ffi::OsStr], root: Option<&str>| {
        let mut c = common::rtrash_real(common::Real::RecycleBin);
        c.args(args).current_dir(&dir);
        match root {
            Some(v) => c.env("RTRASH_ALLOW_ROOTS", v),
            None => c.env_remove("RTRASH_ALLOW_ROOTS"),
        };
        c.output().unwrap()
    };
    let os = |s: &str| std::ffi::OsString::from(s);
    let upper = inside.to_string_lossy().to_uppercase();
    let in_form = inside.to_string_lossy().into_owned();
    // The root upper-cased, in whatever form temp_dir gave.
    let put_in = run(
        &[os("put").as_os_str(), file.as_os_str()],
        Some(upper.as_str()),
    );
    let put_out = run(
        &[os("put").as_os_str(), stray.as_os_str()],
        Some(in_form.as_str()),
    );
    let stray_kept = stray.exists();
    // The root long, the destination as temp_dir spells it.
    let back = run(
        &[os("restore").as_os_str(), file.as_os_str()],
        Some(long.as_str()),
    );
    let restored = file.exists();
    // Trash it unscoped, then rm it scoped to the other folder and to its own.
    let again = run(&[os("put").as_os_str(), file.as_os_str()], None);
    let pattern = os(&format!("{tag}-x.txt"));
    let listed = |o: &Output| {
        String::from_utf8_lossy(&o.stdout)
            .lines()
            .filter(|l| l.contains(&tag))
            .count()
    };
    let (rm_s, dry_s) = (os("rm"), os("-n"));
    let dry = [rm_s.as_os_str(), dry_s.as_os_str(), pattern.as_os_str()];
    let rm = [rm_s.as_os_str(), pattern.as_os_str()];
    let (out_root, low_root) = (outside.to_string_lossy().into_owned(), long.to_lowercase());
    let before = listed(&run(&dry, None));
    let refused = run(&rm, Some(out_root.as_str()));
    let still = listed(&run(&dry, None));
    let done = run(&rm, Some(low_root.as_str()));
    let after = listed(&run(&dry, None));
    let _ = fs::remove_dir_all(&dir);
    let text = |o: &Output| String::from_utf8_lossy(&o.stderr).into_owned();
    assert!(put_in.status.success(), "{}", text(&put_in));
    assert_eq!(put_out.status.code(), Some(2), "{}", text(&put_out));
    assert_eq!(common::outside_lines(&put_out), 1, "{}", text(&put_out));
    assert!(stray_kept, "a refused put moved the file");
    assert!(back.status.success(), "{}", text(&back));
    assert!(restored, "the scoped restore did not bring the file back");
    assert!(again.status.success(), "{}", text(&again));
    assert_eq!(before, 1, "the put did not leave one item");
    assert_eq!(refused.status.code(), Some(2), "{}", text(&refused));
    assert_eq!(common::outside_lines(&refused), 1, "{}", text(&refused));
    assert_eq!(still, before, "a refused rm purged the item");
    assert!(done.status.success(), "{}", text(&done));
    assert_eq!(after, 0);
}
