//! Windows paths past MAX_PATH (260 characters) and verbatim `\\?\`
//! operands through put, list, restore, rm, empty and du, on the scratch Recycle
//! Bin of `common::rtrash`. Every path prints and is recorded without the
//! verbatim prefix, as users type it and the shell records it. A payload
//! another process holds open stays in the bin, and restore names it.
#![cfg(windows)]

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;

/// A scratch home in its long, plain form. %TEMP% may be a short 8.3 name
/// such as `RUNNER~1`, and canonicalize gives the verbatim form.
fn home(tag: &str) -> PathBuf {
    let dir = fs::canonicalize(common::scratch_dir("winpaths", tag)).unwrap();
    let text = s(&dir);
    PathBuf::from(text.strip_prefix(r"\\?\").unwrap_or(&text))
}

fn s(p: &Path) -> String {
    p.to_str().unwrap().to_string()
}

/// `p` as a JSON string body: each backslash doubled.
fn json(p: &str) -> String {
    p.replace('\\', r"\\")
}

/// A directory below `home` whose path runs past 300 characters.
fn deep_dir(home: &Path) -> PathBuf {
    let mut d = home.to_path_buf();
    for i in 0..5 {
        d.push(format!("{i}-{}", "d".repeat(60)));
    }
    fs::create_dir_all(&d).unwrap();
    assert!(d.as_os_str().len() > 300, "{}", d.display());
    d
}

fn run(home: &Path, args: &[&str]) -> Output {
    common::rtrash(home)
        .args(args)
        .current_dir(home)
        .output()
        .unwrap()
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

fn ok(o: &Output) -> String {
    assert!(o.status.success(), "{}", text(o));
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
fn put_list_rm_and_restore_reach_a_file_past_max_path() {
    let home = home("deep-file");
    let f = deep_dir(&home).join("leaf-deep.txt");
    fs::write(&f, "deep").unwrap();
    let path = s(&f);
    ok(&run(&home, &["put", &path]));
    assert!(!f.exists(), "put left {path}");
    let shown = ok(&run(&home, &["list"]));
    assert!(shown.lines().any(|l| l.ends_with(&path)), "{shown}");
    assert!(!shown.contains(r"\\?\"), "{shown}");
    let matched = ok(&run(&home, &["rm", "-n", "leaf-deep.txt"]));
    assert_eq!(matched.trim_end(), path, "{matched}");
    ok(&run(&home, &["restore", &path]));
    assert_eq!(fs::read_to_string(&f).unwrap(), "deep");
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn a_directory_with_paths_past_max_path_goes_and_comes_back_whole() {
    let home = home("deep-dir");
    let top = home.join("tree");
    let f = deep_dir(&top).join("leaf.txt");
    fs::write(&f, "leaf").unwrap();
    let du = run(&home, &["du", "--json", "--files", "-n", "100", &s(&top)]);
    let scan = common::du_scan(ok(&du));
    assert!(scan.contains(&json(&s(&f))), "{scan}");
    ok(&run(&home, &["put", &s(&top)]));
    assert!(!top.exists(), "put left the tree");
    ok(&run(&home, &["restore", &s(&top)]));
    assert_eq!(fs::read_to_string(&f).unwrap(), "leaf");
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn verbatim_operands_are_recorded_and_shown_plain() {
    let home = home("verbatim");
    let f = home.join("verb.txt");
    fs::write(&f, "v").unwrap();
    let plain = s(&f);
    let verbatim = format!(r"\\?\{plain}");
    ok(&run(&home, &["put", &verbatim]));
    assert!(!f.exists(), "put left {plain}");
    let dir = format!(r"--directory=\\?\{}", s(&home));
    let shown = ok(&run(&home, &["list", &dir]));
    assert!(shown.lines().any(|l| l.ends_with(&plain)), "{shown}");
    assert!(!shown.contains(r"\\?\"), "{shown}");
    let back = ok(&run(&home, &["restore", "--json", &verbatim]));
    let want = format!("\"restored_to\":\"{}\"", json(&plain));
    assert!(back.contains(&want), "{back}");
    assert_eq!(fs::read_to_string(&f).unwrap(), "v");

    // --to takes a verbatim directory and restores to its plain form.
    let dest = home.join("dest");
    fs::create_dir_all(&dest).unwrap();
    ok(&run(&home, &["put", &plain]));
    let to = format!(r"--to=\\?\{}", s(&dest));
    let moved = ok(&run(&home, &["restore", "--json", &to, &plain]));
    let want = format!("\"restored_to\":\"{}\"", json(&s(&dest.join("verb.txt"))));
    assert!(moved.contains(&want), "{moved}");

    // du names a verbatim operand's rows in the plain form.
    let sub = home.join("sub");
    fs::create_dir_all(&sub).unwrap();
    fs::write(sub.join("in.txt"), "in").unwrap();
    let operand = format!(r"\\?\{}", s(&sub));
    let du = run(&home, &["du", "--json", "--files", "-n", "100", &operand]);
    let scan = common::du_scan(ok(&du));
    assert!(scan.contains(&json(&s(&sub.join("in.txt")))), "{scan}");
    let _ = fs::remove_dir_all(&home);
}

/// trashy #114: a payload another process holds open. The move back
/// fails, nothing is copied out, the record stays, and restore names the
/// holder as put does; once it is closed the item restores.
#[test]
fn a_held_payload_stays_in_the_bin_and_restore_names_the_holder() {
    use std::os::windows::fs::OpenOptionsExt;
    let home = home("held");
    let f = home.join("held.txt");
    fs::write(&f, "held").unwrap();
    let path = s(&f);
    ok(&run(&home, &["put", &path]));
    let payload = fs::read_dir(common::test_bin(&home))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.file_name().unwrap().to_string_lossy().starts_with("$R"))
        .expect("a $R payload");
    // FILE_SHARE_READ alone: others may read it, but not move or delete it.
    let held = fs::OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(&payload)
        .unwrap();
    let out = run(&home, &["restore", &path]);
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(err.contains("cannot restore"), "{err}");
    // The holder is this test; tests/windows_process.rs checks the pid.
    assert!(err.contains("in use by pid "), "{err}");
    assert!(!f.exists(), "a held payload was copied out");
    let shown = ok(&run(&home, &["list"]));
    assert!(shown.lines().any(|l| l.ends_with(&path)), "{shown}");
    drop(held);
    ok(&run(&home, &["restore", &path]));
    assert_eq!(fs::read_to_string(&f).unwrap(), "held");
    let _ = fs::remove_dir_all(&home);
}

/// dua #410 and #406: a tree whose files sit past MAX_PATH, one of them
/// read-only, leaves the bin for good through rm and through empty. Both
/// clear the read-only bit through a Win32 call, which takes the verbatim
/// form from util::win_path. The plain `empty` goes ahead because the
/// last put moved everything in the scratch bin; that record lives in
/// the private `LOCALAPPDATA` of `common::rtrash`, which the test checks.
#[test]
fn a_read_only_tree_past_max_path_leaves_the_bin_by_rm_and_empty() {
    let home = home("deep-gone");
    for (name, how) in [("by-rm", "rm"), ("by-empty", "empty")] {
        let top = home.join(name);
        let f = deep_dir(&top).join("ro.txt");
        fs::write(&f, "ro").unwrap();
        let mut perm = fs::metadata(&f).unwrap().permissions();
        perm.set_readonly(true);
        fs::set_permissions(&f, perm).unwrap();
        ok(&run(&home, &["put", &s(&top)]));
        let record = common::local_app_data(&home).join(r"rtrash\last-put.txt");
        let last = fs::read_to_string(&record).expect("a private last-put record");
        let want = s(&top).to_lowercase();
        assert!(last.to_lowercase().contains(&want), "{how}: {last}");
        let args: Vec<&str> = if how == "rm" {
            vec!["rm", name]
        } else {
            vec!["empty"]
        };
        ok(&run(&home, &args));
        let shown = ok(&run(&home, &["list"]));
        assert!(!shown.contains(&s(&top)), "{how}: {shown}");
        let left: Vec<_> = fs::read_dir(common::test_bin(&home))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("$R"))
            .collect();
        assert!(left.is_empty(), "{how} left {left:?}");
    }
    let _ = fs::remove_dir_all(&home);
}

/// 34(i): a restore to another volume makes a junction in the folder
/// again, to the same target, and never copies through it. On the CI
/// runners the build folder (CARGO_TARGET_TMPDIR) and the temp dir sit
/// on different volumes; elsewhere the test says so and stops.
#[test]
fn a_restore_across_volumes_makes_a_junction_again() {
    if std::env::var_os("WINEPREFIX").is_some() {
        return;
    }
    let home = home("junction-away");
    let away = Path::new(env!("CARGO_TARGET_TMPDIR")).join(home.file_name().unwrap());
    let drive = |p: &Path| s(p).chars().next().map(|c| c.to_ascii_lowercase());
    if drive(&away) == drive(&home) {
        common::skip("CARGO_TARGET_TMPDIR shares a volume with the temp dir; skipping");
        let _ = fs::remove_dir_all(&home);
        return;
    }
    let target = home.join("target");
    fs::create_dir_all(&target).unwrap();
    fs::write(target.join("t.txt"), "t").unwrap();
    let tree = home.join("tree");
    fs::create_dir_all(&tree).unwrap();
    fs::write(tree.join("f.txt"), "f").unwrap();
    common::mklink_junction(&tree.join("link"), &target);
    ok(&run(&home, &["put", &s(&tree)]));
    fs::create_dir_all(&away).unwrap();
    let to = format!("--to={}", s(&away));
    ok(&run(&home, &["restore", &to, &s(&tree)]));
    let back = away.join("tree");
    assert_eq!(fs::read_to_string(back.join("f.txt")).unwrap(), "f");
    let link = back.join("link");
    let meta = fs::symlink_metadata(&link).unwrap();
    assert!(
        meta.file_type().is_symlink(),
        "the junction was copied through"
    );
    assert_eq!(
        fs::canonicalize(&link).unwrap(),
        fs::canonicalize(&target).unwrap()
    );
    assert_eq!(fs::read_to_string(target.join("t.txt")).unwrap(), "t");
    let left = fs::read_dir(common::test_bin(&home))
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("$R"))
        .count();
    assert_eq!(left, 0);
    let _ = fs::remove_dir_all(&away);
    let _ = fs::remove_dir_all(&home);
}
