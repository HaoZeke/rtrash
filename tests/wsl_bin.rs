//! `rtrash wsl list` and `rtrash wsl restore` against a made-up drive
//! tree: a fake kernel release says WSL, a fake wsl.conf moves the
//! automount root into a scratch folder, and `RTRASH_WSL_SID` stands in
//! for the call to powershell.exe.
#![cfg(unix)]

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use rtrash::win_recycle::{encode_recycle_i_v2, FILETIME_UNIX_EPOCH};

const SID: &str = "S-1-5-21-1-2-3-1001";

struct Fake {
    top: PathBuf,
    root: PathBuf,
}

impl Fake {
    fn new(tag: &str) -> Self {
        let top = common::scratch_dir("wslbin", tag);
        let root = top.join("drives");
        fs::create_dir_all(root.join("c").join("$Recycle.Bin").join(SID)).unwrap();
        fs::write(top.join("osrelease"), "5.15.0-microsoft-standard-WSL2\n").unwrap();
        fs::write(
            top.join("wsl.conf"),
            format!("[automount]\nroot = {}/\n", root.display()),
        )
        .unwrap();
        Fake { top, root }
    }

    fn bin(&self) -> PathBuf {
        self.root.join("c").join("$Recycle.Bin").join(SID)
    }

    /// A recycled file: `$R<id>` holds `body`, `$I<id>` names `win`.
    fn recycle(&self, id: &str, win: &str, body: &[u8], secs: u64) {
        fs::write(self.bin().join(format!("$R{id}")), body).unwrap();
        let ft = FILETIME_UNIX_EPOCH + secs * 10_000_000;
        let rec = encode_recycle_i_v2(Path::new(win), body.len() as u64, ft);
        fs::write(self.bin().join(format!("$I{id}")), rec).unwrap();
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        common::rtrash(&self.top)
            .arg("wsl")
            .args(args)
            .env("RTRASH_WSL_OSRELEASE", self.top.join("osrelease"))
            .env("RTRASH_WSL_CONF", self.top.join("wsl.conf"))
            .env("RTRASH_WSL_SID", SID)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap()
    }

    /// `run` with `RTRASH_ALLOW_ROOTS` set to `roots`.
    fn run_scoped(&self, args: &[&str], roots: &Path) -> std::process::Output {
        common::rtrash(&self.top)
            .arg("wsl")
            .args(args)
            .env("RTRASH_WSL_OSRELEASE", self.top.join("osrelease"))
            .env("RTRASH_WSL_CONF", self.top.join("wsl.conf"))
            .env("RTRASH_WSL_SID", SID)
            .env("RTRASH_ALLOW_ROOTS", roots)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap()
    }

    /// `run` with `RTRASH_NO_PERMANENT=1`.
    fn run_guarded(&self, args: &[&str]) -> std::process::Output {
        common::rtrash(&self.top)
            .arg("wsl")
            .args(args)
            .env("RTRASH_WSL_OSRELEASE", self.top.join("osrelease"))
            .env("RTRASH_WSL_CONF", self.top.join("wsl.conf"))
            .env("RTRASH_WSL_SID", SID)
            .env("RTRASH_NO_PERMANENT", "1")
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap()
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.top);
    }
}

#[test]
fn wsl_list_shows_original_paths_with_their_extension() {
    let f = Fake::new("list");
    f.recycle("AB12CD.txt", r"C:\Users\me\notes.txt", b"n", 1_700_000_000);
    f.recycle("EF34GH", r"C:\Users\me\dir", b"d", 1_700_000_100);
    // A record without its payload is skipped.
    fs::write(
        f.bin().join("$IZZZZZZ.txt"),
        encode_recycle_i_v2(Path::new(r"C:\gone.txt"), 1, FILETIME_UNIX_EPOCH),
    )
    .unwrap();
    let out = f.run(&["list"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "{text}");
    let notes = f.root.join("c/Users/me/notes.txt");
    assert!(lines[0].ends_with(&notes.display().to_string()), "{text}");
    assert!(lines[1].ends_with(&f.root.join("c/Users/me/dir").display().to_string()));
    assert!(!text.contains("gone.txt"));
}

#[test]
fn wsl_restore_moves_the_payload_back_and_drops_the_record() {
    let f = Fake::new("restore");
    f.recycle(
        "AB12CD.txt",
        r"C:\Users\me\notes.txt",
        b"hello",
        1_700_000_000,
    );
    let dest = f.root.join("c/Users/me/notes.txt");
    let out = f.run(&["restore", &dest.display().to_string()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(fs::read(&dest).unwrap(), b"hello");
    assert!(!f.bin().join("$RAB12CD.txt").exists());
    assert!(!f.bin().join("$IAB12CD.txt").exists());
    // Gone now, so a second restore finds nothing.
    let out = f.run(&["restore", &dest.display().to_string()]);
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn wsl_restore_refuses_to_overwrite_without_force() {
    let f = Fake::new("force");
    f.recycle("AB12CD.txt", r"C:\a.txt", b"old", 1_700_000_000);
    let dest = f.root.join("c/a.txt");
    fs::write(&dest, b"new").unwrap();
    let out = f.run(&["restore", &dest.display().to_string()]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(fs::read(&dest).unwrap(), b"new");
    let out = f.run(&["restore", "-f", &dest.display().to_string()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(fs::read(&dest).unwrap(), b"old");
}

#[test]
fn wsl_commands_need_wsl_and_a_known_action() {
    let f = Fake::new("guard");
    assert_eq!(f.run(&["nope"]).status.code(), Some(2));
    assert_eq!(f.run(&[]).status.code(), Some(2));
    assert!(f.run(&["--help"]).status.success());
    let out = common::rtrash(&f.top)
        .args(["wsl", "list"])
        .env("RTRASH_WSL_OSRELEASE", f.top.join("missing"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn wsl_list_filters_and_prints_json() {
    let f = Fake::new("json");
    f.recycle("AB12CD.txt", r"C:\Users\me\notes.txt", b"n", 1_700_000_000);
    f.recycle("EF34GH.log", r"C:\Users\me\run.log", b"ll", 1_700_000_100);
    let out = f.run(&["list", "--glob=*.log"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(text.lines().count(), 1, "{text}");
    assert!(text.contains("run.log"), "{text}");
    let out = f.run(&["list", "--json", "--name=notes.txt"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        text.starts_with("[{") && text.trim_end().ends_with("}]"),
        "{text}"
    );
    assert!(
        text.contains(r#""original":"C:\\Users\\me\\notes.txt""#),
        "{text}"
    );
    assert!(text.contains(r#""deleted":1700000000"#), "{text}");
    assert!(!text.contains("run.log"), "{text}");
    assert_eq!(f.run(&["list", "--regex=("]).status.code(), Some(2));
    assert_eq!(f.run(&["list", "extra"]).status.code(), Some(2));
}

#[test]
fn wsl_list_prints_through_a_format() {
    let f = Fake::new("format");
    f.recycle("AB12CD.txt", r"C:\Users\me\notes.txt", b"n", 1_700_000_000);
    f.recycle("EF34GH.log", r"C:\Users\me\run.log", b"ll", 1_700_000_100);
    let out = f.run(&["list", "--format={epoch}\t{size}\t{name}\t{path}\n"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    let notes = f.root.join("c/Users/me/notes.txt");
    let run = f.root.join("c/Users/me/run.log");
    assert_eq!(
        text,
        format!(
            "1700000000\t1\t$RAB12CD.txt\t{}\n1700000100\t2\t$REF34GH.log\t{}\n",
            notes.display(),
            run.display()
        )
    );
    let out = f.run(&["list", "--name=run.log", "--format={path}\\0"]);
    assert_eq!(out.stdout, format!("{}\0", run.display()).into_bytes());
    assert_eq!(f.run(&["list", "--format={nope}"]).status.code(), Some(2));
    let both = f.run(&["list", "--json", "--format={path}"]);
    assert_eq!(both.status.code(), Some(2));
}

#[test]
fn wsl_rm_deletes_matching_items_for_good() {
    let f = Fake::new("rm");
    f.recycle("AB12CD.txt", r"C:\a.txt", b"a", 1_700_000_000);
    f.recycle("EF34GH.log", r"C:\b.log", b"b", 1_700_000_100);
    let out = f.run(&["rm", "-n", "--glob=*.log"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("would remove"));
    assert!(f.bin().join("$REF34GH.log").exists());
    let out = f.run(&["rm", "--glob=*.log"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!f.bin().join("$REF34GH.log").exists());
    assert!(!f.bin().join("$IEF34GH.log").exists());
    assert!(f.bin().join("$RAB12CD.txt").exists());
    let dest = f.root.join("c/a.txt");
    let out = f.run(&["rm", &dest.display().to_string()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!f.bin().join("$RAB12CD.txt").exists());
    // No match is not an error; no selector is.
    assert!(f.run(&["rm", "--name=none"]).status.success());
    assert_eq!(f.run(&["rm"]).status.code(), Some(2));
}

#[test]
fn wsl_empty_goes_by_age_and_guards_the_whole_bin() {
    let f = Fake::new("empty");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    f.recycle("AB12CD.txt", r"C:\old.txt", b"o", 1_700_000_000);
    f.recycle("EF34GH.txt", r"C:\new.txt", b"n", now);
    let out = f.run(&["empty", "-n", "--older-than=1d"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("old.txt") && !text.contains("new.txt"),
        "{text}"
    );
    assert!(f.bin().join("$RAB12CD.txt").exists());
    let out = f.run(&["empty", "7"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!f.bin().join("$RAB12CD.txt").exists());
    assert!(!f.bin().join("$IAB12CD.txt").exists());
    assert!(f.bin().join("$REF34GH.txt").exists());
    // A whole empty without a terminal refuses unless --all.
    let out = f.run(&["empty"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("--all"));
    assert!(f.bin().join("$REF34GH.txt").exists());
    assert_eq!(f.run(&["empty", "--all", "3"]).status.code(), Some(2));
    assert_eq!(
        f.run(&["empty", "--older-than=soon"]).status.code(),
        Some(2)
    );
    let out = f.run(&["empty", "--all"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!f.bin().join("$REF34GH.txt").exists());
}

#[test]
fn wsl_rm_empty_and_overwrite_refuse_under_no_permanent() {
    let f = Fake::new("noperm");
    f.recycle("AB12CD.txt", r"C:\a.txt", b"old", 1_700_000_000);
    let payload = f.bin().join("$RAB12CD.txt");
    for args in [&["rm", "--name=a.txt"][..], &["empty", "--all"]] {
        let out = f.run_guarded(args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("RTRASH_NO_PERMANENT"), "{args:?}: {err}");
        assert!(payload.exists(), "{args:?} deleted the item");
    }
    // A dry run still lists what would go.
    let out = f.run_guarded(&["rm", "-n", "--name=a.txt"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("would remove"));
    // restore -f over a file would delete that file for good.
    let dest = f.root.join("c/a.txt");
    fs::write(&dest, b"new").unwrap();
    let out = f.run_guarded(&["restore", "-f", &dest.display().to_string()]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("RTRASH_NO_PERMANENT"));
    assert_eq!(fs::read(&dest).unwrap(), b"new");
    assert!(payload.exists());
}

/// `RTRASH_ALLOW_ROOTS` on wsl restore: every destination is checked
/// before the first change, so one outside path refuses the whole run,
/// and `-f` never removes a file outside the scope.
#[test]
fn wsl_restore_keeps_to_the_allowed_roots() {
    let f = Fake::new("wslroots");
    f.recycle("AB12CD.txt", r"C:\in\a.txt", b"a", 1_700_000_000);
    f.recycle("EF34GH.txt", r"C:\out\b.txt", b"b", 1_700_000_100);
    let (inside, outside) = (f.root.join("c/in"), f.root.join("c/out"));
    fs::create_dir_all(&inside).unwrap();
    fs::create_dir_all(&outside).unwrap();
    let (a, b) = (inside.join("a.txt"), outside.join("b.txt"));
    fs::write(&b, b"keep").unwrap();
    let (a_s, b_s) = (a.display().to_string(), b.display().to_string());
    let bin_names = || {
        let mut v: Vec<String> = fs::read_dir(f.bin())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    };
    // A refusal exits 2, names each outside path on its own line, and
    // leaves the bin and both destinations as they were just before.
    let bin = f.bin();
    let refused = |args: &[&str], roots: &Path| {
        let lines = [(common::OUTSIDE, 1)];
        let watch = [bin.as_path(), a.as_path(), b.as_path()];
        common::assert_refused_unchanged(&args, &watch, 2, &lines, || f.run_scoped(args, roots));
    };
    assert_eq!(bin_names().len(), 4);
    // b.txt is outside: nothing is restored, a.txt included, and -f
    // leaves the file at b.txt alone.
    refused(&["restore", "-f", &a_s, &b_s], &inside);
    assert!(!a.exists());
    // Inside the root, a.txt comes back.
    let out = f.run_scoped(&["restore", &a_s], &inside);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(fs::read(&a).unwrap(), b"a");
    assert_eq!(bin_names(), ["$IEF34GH.txt", "$REF34GH.txt"]);
    // A root that does not exist allows nothing.
    let gone = f.top.join("no-such-root");
    refused(&["restore", "-f", &b_s], &gone);
}
