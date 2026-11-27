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

/// Removes the junction before the tree, including when a test panics.
/// A junction to a drive root must not be walked.
struct DropJunction {
    link: PathBuf,
    home: PathBuf,
}

impl Drop for DropJunction {
    fn drop(&mut self) {
        let _ = fs::remove_dir(&self.link);
        let _ = fs::remove_dir_all(&self.home);
    }
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
    let matched = ok(&run(&home, &["rm", "-n", "--match=glob", "leaf-deep.txt"]));
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
        let top_s = s(&top);
        let args: Vec<&str> = if how == "rm" {
            vec!["rm", &top_s]
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
    common::mklink_junction(&tree.join("link"), &target).expect("mklink /J");
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

fn lines(text: &str) -> usize {
    text.lines().filter(|l| !l.is_empty()).count()
}

fn folded(p: &Path) -> String {
    s(p).replace('/', "\\").to_ascii_lowercase()
}

/// Exact `rm` matches the original through letter case, `/`, a relative
/// name, and the short 8.3 form of a parent. The file's own 8.3 name,
/// read before `put`, matches nothing: that entry is gone, and the `$I`
/// record keeps the one path put stored.
#[test]
fn rm_matches_the_exact_path_across_windows_spellings() {
    let home = home("spellings");
    let dir = home.join("LongDirectoryName");
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join("ExactFile.TXT");
    fs::write(&file, "body").unwrap();
    let long = s(&file);
    let short_file = rtrash::win_recycle::short_path(&file);
    let short_dir = rtrash::win_recycle::short_path(&dir);
    ok(&run(&home, &["put", &long]));
    assert!(!file.exists(), "put left {long}");

    let case = s(&dir.join("exactfile.txt"));
    assert_eq!(lines(&ok(&run(&home, &["rm", "-n", &case]))), 1, "{case}");
    let slashed = long.replace('\\', "/");
    assert_eq!(
        lines(&ok(&run(&home, &["rm", "-n", &slashed]))),
        1,
        "{slashed}"
    );
    let rel = common::rtrash(&home)
        .args(["rm", "-n", "ExactFile.TXT"])
        .current_dir(&dir)
        .output()
        .unwrap();
    assert!(rel.status.success(), "{}", text(&rel));
    let rel_out = String::from_utf8_lossy(&rel.stdout);
    assert_eq!(lines(&rel_out), 1, "{}", text(&rel));

    match &short_dir {
        Some(short) if folded(short) != folded(&dir) => {
            let via = s(&short.join("ExactFile.TXT"));
            assert_eq!(lines(&ok(&run(&home, &["rm", "-n", &via]))), 1, "{via}");
            let other = dir.join("OtherFile.TXT");
            fs::write(&other, "other").unwrap();
            let short_other = s(&short.join("OtherFile.TXT"));
            ok(&run(&home, &["put", &short_other]));
            assert_eq!(
                lines(&ok(&run(&home, &["rm", "-n", &s(&other)]))),
                1,
                "{short_other}"
            );
            ok(&run(&home, &["rm", &s(&other)]));
        }
        _ => common::skip("this volume has no 8.3 name for the directory"),
    }
    match short_file {
        Some(short) if folded(&short) != folded(&file) => {
            let dry = run(&home, &["rm", "-n", &s(&short)]);
            assert_eq!(dry.status.code(), Some(1), "{}", text(&dry));
            assert_eq!(
                lines(&String::from_utf8_lossy(&dry.stdout)),
                0,
                "a short final name matched: {}",
                text(&dry)
            );
            assert!(
                String::from_utf8_lossy(&dry.stderr).contains("no trash items match"),
                "{}",
                text(&dry)
            );
        }
        _ => common::skip("this volume has no 8.3 name for the file"),
    }
    let shown = ok(&run(&home, &["list"]));
    assert!(
        shown.to_ascii_lowercase().contains("exactfile.txt"),
        "{shown}"
    );
    ok(&run(&home, &["rm", &case]));
    let shown = ok(&run(&home, &["list"]));
    assert!(
        !shown.to_ascii_lowercase().contains("exactfile.txt"),
        "{shown}"
    );
    let _ = fs::remove_dir_all(&home);
}

/// A file symlink when the process may make one, otherwise a junction.
/// `None` when neither can be made; `why` is the junction error.
fn final_link(home: &Path) -> Result<(PathBuf, PathBuf), String> {
    let file = home.join("target.txt");
    fs::write(&file, b"t").unwrap();
    let link = home.join("link.txt");
    if std::os::windows::fs::symlink_file(&file, &link).is_ok() {
        return Ok((link, file));
    }
    let _ = fs::remove_file(&link);
    let dir = home.join("target");
    fs::create_dir(&dir).unwrap();
    let junction = home.join("link");
    match common::mklink_junction(&junction, &dir) {
        Ok(()) => Ok((junction, dir)),
        Err(e) => Err(format!("cannot make a final symlink or a junction: {e}")),
    }
}

/// An exact operand whose final component is a symlink or a junction
/// selects that link's own record. Following the final component would
/// select the target's record instead.
#[test]
fn an_exact_final_link_selects_the_link_not_the_target() {
    let home = home("finallink");
    let (link, target) = match final_link(&home) {
        Ok(pair) => pair,
        Err(why) => {
            common::skip(&why);
            let _ = fs::remove_dir_all(&home);
            return;
        }
    };
    let wants = rtrash::orig_path::forms(&s(&link));
    assert!(
        rtrash::orig_path::selects(&wants, &link),
        "the link's own record: {wants:?}"
    );
    assert!(
        !rtrash::orig_path::selects(&wants, &target),
        "the target's record was selected: {wants:?}"
    );
    let _ = fs::remove_dir_all(&home);
}

/// The first drive-letter mount from `volume_mounts`, the table
/// `path_key` reads. `C:\` stays `C:`.
fn drive_mount() -> Option<(String, String)> {
    rtrash::win_recycle::volume_mounts()
        .into_iter()
        .find(|(_, mount)| {
            let m = mount.trim_end_matches(['\\', '/']);
            let b = m.as_bytes();
            b.len() == 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
        })
}

/// A drive-letter original and the `\\?\Volume{GUID}\` spelling of it
/// are one path for `norm`, `selects` and `allow_roots::under`.
#[test]
fn a_volume_guid_operand_selects_the_drive_letter_original() {
    let Some((guid, mount)) = drive_mount() else {
        common::skip("volume_mounts listed no drive-letter mount");
        return;
    };
    let guid = guid.trim_end_matches(['\\', '/']);
    let mount = mount.trim_end_matches(['\\', '/']);
    let letter = format!(r"{mount}\rtrash-exact\File.TXT");
    let volume = format!(r"{guid}\rtrash-exact\File.TXT");
    let letter_p = Path::new(&letter);
    let volume_p = Path::new(&volume);
    assert_eq!(
        rtrash::orig_path::norm(letter_p),
        rtrash::orig_path::norm(volume_p),
        "guid {guid} mount {mount}"
    );
    let wants = vec![rtrash::orig_path::norm(volume_p)];
    assert!(
        rtrash::orig_path::selects(&wants, letter_p),
        "{wants:?} {letter}"
    );
    let letter_root = PathBuf::from(format!(r"{mount}\"));
    let volume_root = PathBuf::from(format!(r"{guid}\"));
    assert!(rtrash::allow_roots::under(letter_p, &letter_root));
    assert!(rtrash::allow_roots::under(volume_p, &letter_root));
    assert!(rtrash::allow_roots::under(letter_p, &volume_root));
    assert!(rtrash::allow_roots::under(volume_p, &volume_root));
}

/// `--` ends options, but its operands still have to name paths.
#[test]
fn rm_refuses_an_empty_operand_after_a_double_dash() {
    let home = home("rm-dash-blank");
    let kept = home.join("keep.txt");
    fs::write(&kept, b"keep").unwrap();
    ok(&run(&home, &["put", &s(&kept)]));
    let out = run(&home, &["rm", "--", ""]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    assert!(text(&out).contains("'' is not a path"), "{}", text(&out));
    let listed = ok(&run(&home, &["list"]));
    assert!(listed.to_ascii_lowercase().contains("keep.txt"), "{listed}");
}

/// `rm ""` is not a path, and it removes nothing.
#[test]
fn rm_refuses_an_empty_operand() {
    let home = home("rm-blank");
    let kept = home.join("keep.txt");
    fs::write(&kept, b"keep").unwrap();
    ok(&run(&home, &["put", &s(&kept)]));
    let out = run(&home, &["rm", ""]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    assert!(text(&out).contains("'' is not a path"), "{}", text(&out));
    let listed = ok(&run(&home, &["list"]));
    assert!(listed.to_ascii_lowercase().contains("keep.txt"), "{listed}");
}

/// `empty ""` is not a path, and it removes nothing.
#[test]
fn empty_refuses_an_empty_operand() {
    let home = home("empty-blank");
    let kept = home.join("keep.txt");
    fs::write(&kept, b"keep").unwrap();
    ok(&run(&home, &["put", &s(&kept)]));
    let out = run(&home, &["empty", ""]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    assert!(text(&out).contains("'' is not a path"), "{}", text(&out));
    let listed = ok(&run(&home, &["list"]));
    assert!(listed.to_ascii_lowercase().contains("keep.txt"), "{listed}");
}

/// An empty operand after `--` is still not a path.
#[test]
fn empty_refuses_an_empty_operand_after_a_double_dash() {
    let home = home("empty-dash-blank");
    let kept = home.join("keep.txt");
    fs::write(&kept, b"keep").unwrap();
    ok(&run(&home, &["put", &s(&kept)]));
    let out = run(&home, &["empty", "--", ""]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    assert!(text(&out).contains("'' is not a path"), "{}", text(&out));
    let listed = ok(&run(&home, &["list"]));
    assert!(listed.to_ascii_lowercase().contains("keep.txt"), "{listed}");
}

#[test]
fn windows_empty_exact_keeps_a_protected_path() {
    let home = home("empty-exact-keep");
    fs::create_dir_all(home.join(".config/rtrash")).unwrap();
    let kept = home.join("kept.txt");
    let other = home.join("other.txt");
    fs::write(
        home.join(".config/rtrash/keep"),
        format!("{}\n", kept.display()),
    )
    .unwrap();
    fs::write(&kept, b"kept").unwrap();
    fs::write(&other, b"other").unwrap();
    ok(&run(&home, &["put", &s(&kept), &s(&other)]));

    let out = run(&home, &["empty", "-f", &s(&kept)]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        text(&out).contains("listed in the keep file"),
        "{}",
        text(&out)
    );
    let listed = ok(&run(&home, &["list"]));
    assert!(listed.contains(&s(&kept)), "{listed}");
    assert!(listed.contains(&s(&other)), "{listed}");
    let json = run(&home, &["empty", "--json", &s(&kept)]);
    assert!(json.status.success(), "{}", text(&json));
    let body: serde_json::Value =
        serde_json::from_slice(&json.stdout).unwrap_or_else(|e| panic!("{e}: {}", text(&json)));
    assert_eq!(body["removed"], 0, "{}", text(&json));
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn windows_empty_exact_refuses_duplicate_originals_until_an_id_selects_one() {
    let home = home("empty-exact-ambiguous");
    let same = home.join("same.txt");
    fs::write(&same, b"first").unwrap();
    ok(&run(&home, &["put", &s(&same)]));
    fs::write(&same, b"second").unwrap();
    ok(&run(&home, &["put", &s(&same)]));

    let refused = run(&home, &["empty", "-f", &s(&same)]);
    assert_eq!(refused.status.code(), Some(2), "{}", text(&refused));
    assert!(
        text(&refused).contains("refusing 2 trash items with original path"),
        "{}",
        text(&refused)
    );
    let ids = ok(&run(&home, &["list", "--format={id}\\n"]));
    let id = ids.lines().next().unwrap();
    let removed = run(&home, &["rm", "--yes", "--match=id", id]);
    assert!(removed.status.success(), "{}", text(&removed));
    assert_eq!(
        ok(&run(&home, &["list", "--format={id}\\n"]))
            .lines()
            .count(),
        1
    );
    let _ = fs::remove_dir_all(&home);
}

/// `--` ends empty's options, so `-n` after it is a path and the item stays.
#[test]
fn empty_takes_a_double_dash_as_the_end_of_its_options() {
    let home = home("empty-ddash");
    let kept = home.join("keep.txt");
    fs::write(&kept, b"keep").unwrap();
    ok(&run(&home, &["put", &s(&kept)]));
    let out = run(&home, &["empty", "--", "-n"]);
    let shown = text(&out);
    assert_eq!(out.status.code(), Some(1), "{shown}");
    assert!(!shown.contains("unrecognized option"), "{shown}");
    assert!(!shown.contains("would remove"), "{shown}");
    assert!(!shown.contains("Would remove"), "{shown}");
    assert!(shown.contains("no trash items match"), "{shown}");
    let json = run(&home, &["empty", "--json", "--", "-n"]);
    assert_eq!(json.status.code(), Some(1), "{}", text(&json));
    let body: serde_json::Value =
        serde_json::from_slice(&json.stdout).unwrap_or_else(|e| panic!("{e}: {}", text(&json)));
    assert_eq!(body["removed"], 0, "{}", text(&json));
    let listed = ok(&run(&home, &["list"]));
    assert!(listed.to_ascii_lowercase().contains("keep.txt"), "{listed}");
}

/// `--prefix` of a junction whose target is a drive root canonicalizes to
/// that root and would match every Recycle Bin item on the drive. Without
/// `--force` rm refuses it and leaves the item. With `--force` the dry run
/// lists the item, so the canonical key is what the guard stopped.
#[test]
fn a_prefix_of_a_junction_to_a_drive_root_is_refused() {
    let home = home("drive-root");
    let link = home.join("safe-junction");
    let _cleanup = DropJunction {
        link: link.clone(),
        home: home.clone(),
    };
    let text_home = s(&home);
    let bytes = text_home.as_bytes();
    if bytes.len() < 3 || !bytes[0].is_ascii_alphabetic() || bytes[1] != b':' || bytes[2] != b'\\' {
        common::skip("the scratch dir is not on a drive letter; skipping");
        return;
    }
    let root = format!(r"{}:\", char::from(bytes[0].to_ascii_uppercase()));
    let narrow_dir = home.join("project");
    fs::create_dir(&narrow_dir).unwrap();
    let file = narrow_dir.join("keep.txt");
    fs::write(&file, "k").unwrap();
    ok(&run(&home, &["put", &s(&file)]));
    common::mklink_junction(&link, Path::new(&root)).unwrap();
    let canon = s(&fs::canonicalize(&link).unwrap());
    let canon = canon
        .trim_start_matches(r"\\?\")
        .trim_end_matches('\\')
        .to_ascii_lowercase();
    assert_eq!(
        canon,
        root.trim_end_matches('\\').to_ascii_lowercase(),
        "the junction did not resolve to the drive root"
    );
    let payloads = || {
        fs::read_dir(common::test_bin(&home))
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("$R"))
            .count()
    };
    assert_eq!(payloads(), 1);
    let junction = s(&link);
    let refused = run(&home, &["rm", "--prefix", &junction]);
    let err = String::from_utf8_lossy(&refused.stderr);
    assert_eq!(refused.status.code(), Some(2), "{err}");
    assert!(err.contains("refusing mass pattern"), "{err}");
    assert!(refused.stdout.is_empty(), "{}", text(&refused));
    assert_eq!(payloads(), 1, "a refused prefix purged the bin");
    let listed = ok(&run(&home, &["rm", "-n", "-f", "--prefix", &junction]));
    assert!(listed.lines().any(|l| l.ends_with("keep.txt")), "{listed}");
    assert_eq!(payloads(), 1, "the dry run purged the bin");
    let narrow = ok(&run(&home, &["rm", "-n", "--prefix", &s(&narrow_dir)]));
    assert!(narrow.lines().any(|l| l.ends_with("keep.txt")), "{narrow}");
}

#[test]
fn a_home_prefix_is_mass_and_a_child_prefix_is_narrow() {
    let home = home("scope-prefix");
    let project = home.join("project");
    fs::create_dir(&project).unwrap();
    let file = project.join("keep.txt");
    fs::write(&file, "k").unwrap();
    ok(&run(&home, &["put", &s(&file)]));

    let refused = run(&home, &["rm", "-n", "--prefix", &s(&home)]);
    let err = text(&refused);
    assert_eq!(refused.status.code(), Some(2), "{err}");
    assert!(err.contains("refusing mass pattern"), "{err}");

    let narrow = ok(&run(&home, &["rm", "-n", "--prefix", &s(&project)]));
    assert!(narrow.lines().any(|l| l.ends_with("keep.txt")), "{narrow}");
}

#[test]
fn a_home_name_does_not_widen_a_basename_glob() {
    let home = home("foldero-glob");
    for name in ["keep.txt", "spared.txt"] {
        fs::write(home.join(name), "x").unwrap();
        ok(&run(&home, &["put", name]));
    }
    let out = run(&home, &["rm", "-n", "--glob", "*o*"]);
    let shown = text(&out);
    assert_eq!(out.status.code(), Some(1), "{shown}");
    assert!(!shown.contains("refusing mass pattern"), "{shown}");
    let listed = ok(&run(&home, &["list"]));
    assert!(listed.contains("keep.txt"), "{listed}");
    assert!(listed.contains("spared.txt"), "{listed}");
}

/// `--prefix` of a junction keeps the junction's own name. Items whose
/// original path is the target are not matches, and they stay in the bin.
#[test]
fn a_prefix_of_a_final_junction_does_not_take_its_target() {
    let home = home("junction-final");
    let link = home.join("vendor");
    let _cleanup = DropJunction {
        link: link.clone(),
        home: home.clone(),
    };
    let real = home.join("real");
    fs::create_dir_all(&real).unwrap();
    let file = real.join("keep.txt");
    fs::write(&file, "k").unwrap();
    ok(&run(&home, &["put", &s(&file)]));
    common::mklink_junction(&link, &real).unwrap();
    let missed = run(&home, &["rm", "-n", "--prefix", &s(&link)]);
    let missed_text = text(&missed);
    assert_eq!(missed.status.code(), Some(1), "{missed_text}");
    assert!(!missed_text.contains("keep.txt"), "{missed_text}");
    let listed = ok(&run(&home, &["rm", "-n", "--prefix", &s(&real)]));
    assert!(listed.lines().any(|l| l.ends_with("keep.txt")), "{listed}");
    let left = fs::read_dir(common::test_bin(&home))
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("$R"))
        .count();
    assert_eq!(left, 1);
}

/// `C:\Temp\x.txt` and `c:\temp\X.TXT` are one place. Exact rm refuses
/// both unless `--all-matches` is given.
#[test]
fn two_spellings_of_one_path_need_all_matches() {
    let home = home("two-spellings");
    let bin = common::test_bin(&home);
    fs::create_dir_all(&bin).unwrap();
    let ft = rtrash::win_recycle::FILETIME_UNIX_EPOCH;
    for (id, orig) in [
        ("AAAAAA.txt", r"C:\Temp\x.txt"),
        ("BBBBBB.txt", r"c:\temp\X.TXT"),
    ] {
        let rec = rtrash::win_recycle::encode_recycle_i_v2(Path::new(orig), 1, ft);
        fs::write(bin.join(format!("$I{id}")), rec).unwrap();
        fs::write(bin.join(format!("$R{id}")), b"x").unwrap();
    }
    let out = run(&home, &["rm", "-n", r"C:\Temp\x.txt"]);
    let err = String::from_utf8_lossy(&out.stderr);
    let left = fs::read_dir(&bin)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("$R"))
        .count();
    let _ = fs::remove_dir_all(&home);
    assert_eq!(out.status.code(), Some(2), "{err}");
    assert!(err.contains("refusing 2 trash items"), "{err}");
    assert!(err.contains("--all-matches"), "{err}");
    assert_eq!(left, 2, "a refused rm purged the bin");
}

/// `C:\Users\Alice` and `c:\users\alice` are one home. A prefix of the
/// other spelling is the whole scope and needs `--force`.
#[test]
fn a_differently_cased_home_prefix_is_refused() {
    let home = home("case-scope");
    let out = common::rtrash(&home)
        .env("HOME", r"C:\Users\Alice")
        .env("USERPROFILE", r"C:\Users\Alice")
        .args(["rm", "--prefix", r"c:\users\alice"])
        .current_dir(&home)
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    let _ = fs::remove_dir_all(&home);
    assert_eq!(out.status.code(), Some(2), "{err}");
    assert!(err.contains("refusing mass pattern"), "{err}");
}

/// Every match kept is a successful rm that deleted nothing. Exit 1 would
/// say the path matched nothing.
#[test]
fn rm_of_only_a_kept_item_exits_success() {
    let home = home("kept-only");
    fs::create_dir_all(home.join(".config/rtrash")).unwrap();
    let file = home.join("secret.txt");
    fs::write(&file, b"s").unwrap();
    fs::write(
        home.join(".config/rtrash/keep"),
        format!("{}\n", file.display()),
    )
    .unwrap();
    ok(&run(&home, &["put", &s(&file)]));
    let out = run(&home, &["rm", &s(&file)]);
    let err = String::from_utf8_lossy(&out.stderr);
    let left = fs::read_dir(common::test_bin(&home))
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("$R"))
        .count();
    assert_eq!(out.status.code(), Some(0), "{err}");
    assert!(err.contains("listed in the keep file"), "{err}");
    assert_eq!(left, 1);
    let json = run(&home, &["rm", "--json", &s(&file)]);
    assert!(json.status.success(), "{}", text(&json));
    let body: serde_json::Value =
        serde_json::from_slice(&json.stdout).unwrap_or_else(|e| panic!("{e}: {}", text(&json)));
    assert_eq!(body["removed"], 0, "{}", text(&json));
    assert_eq!(body["matches"], serde_json::json!([]), "{}", text(&json));
    let _ = fs::remove_dir_all(&home);
}

/// The same list reader as `du --files-from`. A CRLF list with a blank
/// line names the files that were written, and `--zero` reads NUL records.
#[test]
fn put_files_from_trashes_the_listed_paths() {
    let home = home("put-files-from");
    let work = home.join("work");
    fs::create_dir_all(&work).unwrap();
    for name in ["a.txt", "b.txt", "c.txt"] {
        fs::write(work.join(name), b"x").unwrap();
    }
    let list = home.join("list");
    fs::write(&list, "a.txt\r\n\nb.txt\n").unwrap();
    let arg = format!("--files-from={}", s(&list));
    let out = common::rtrash(&home)
        .args(["put", &arg])
        .current_dir(&work)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out));
    assert!(!work.join("a.txt").exists());
    assert!(!work.join("b.txt").exists());
    assert!(work.join("c.txt").exists());

    let list0 = home.join("list0");
    fs::write(&list0, "c.txt\0").unwrap();
    let out = common::rtrash(&home)
        .args(["put", "--zero", "--files-from", &s(&list0)])
        .current_dir(&work)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out));
    assert!(!work.join("c.txt").exists());

    fs::write(work.join("keep.txt"), b"k").unwrap();
    let out = common::rtrash(&home)
        .args(["put", "--files-from=missing-list"])
        .current_dir(&work)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(
        text(&out).contains("cannot read --files-from"),
        "{}",
        text(&out)
    );
    assert!(work.join("keep.txt").exists());
    let _ = fs::remove_dir_all(&home);
}

/// `-0` clusters with `-f`, `--files0-from` is the NUL list, and a usage
/// error for these flags names `--help`.
#[test]
fn put_zero_clusters_and_names_help() {
    let home = home("put-zero-cluster");
    let work = home.join("work");
    fs::create_dir_all(&work).unwrap();
    for name in ["a.txt", "b.txt", "c.txt"] {
        fs::write(work.join(name), b"x").unwrap();
    }
    let list = home.join("list");
    fs::write(&list, "a.txt\0").unwrap();
    let arg = format!("--files-from={}", s(&list));
    let out = common::rtrash(&home)
        .args(["put", "-f0", &arg])
        .current_dir(&work)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out));
    assert!(!work.join("a.txt").exists());

    fs::write(&list, "b.txt\0").unwrap();
    let out = common::rtrash(&home)
        .args(["put", "-0f", "--files-from", &s(&list)])
        .current_dir(&work)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out));
    assert!(!work.join("b.txt").exists());

    fs::write(&list, "c.txt\0").unwrap();
    let arg0 = format!("--files0-from={}", s(&list));
    let out = common::rtrash(&home)
        .args(["put", &arg0])
        .current_dir(&work)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out));
    assert!(!work.join("c.txt").exists());

    fs::write(work.join("keep.txt"), b"k").unwrap();
    let out = common::rtrash(&home)
        .args(["put", "--zero", &s(&work.join("keep.txt"))])
        .current_dir(&work)
        .output()
        .unwrap();
    let err = text(&out);
    assert_eq!(out.status.code(), Some(2), "{err}");
    assert!(err.contains("--zero requires --files-from"), "{err}");
    assert!(err.contains("Try"), "{err}");
    assert!(err.contains("--help"), "{err}");
    assert!(work.join("keep.txt").exists());
    let _ = fs::remove_dir_all(&home);
}
