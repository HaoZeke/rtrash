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
    cleanup: PathBuf,
}

impl Fake {
    fn new(tag: &str) -> Self {
        let top = common::scratch_dir("wslbin", tag);
        Self::at(top.clone(), top)
    }

    fn behind_link(tag: &str) -> Self {
        let cleanup = common::scratch_dir("wslbin", tag);
        let real = cleanup.join("real");
        let link = cleanup.join("linked");
        fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        Self::at(link.join("top"), cleanup)
    }

    fn at(top: PathBuf, cleanup: PathBuf) -> Self {
        let root = top.join("drives");
        fs::create_dir_all(root.join("c").join("$Recycle.Bin").join(SID)).unwrap();
        fs::write(top.join("osrelease"), "5.15.0-microsoft-standard-WSL2\n").unwrap();
        fs::write(
            top.join("wsl.conf"),
            format!("[automount]\nroot = {}/\n", root.display()),
        )
        .unwrap();
        Fake { top, root, cleanup }
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
        self.command()
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap()
    }

    fn run_without_scope_roots(&self, args: &[&str]) -> std::process::Output {
        self.command()
            .args(args)
            .env_remove("HOME")
            .env_remove("USERPROFILE")
            .env_remove("XDG_DATA_HOME")
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap()
    }

    fn command(&self) -> std::process::Command {
        let mut command = common::rtrash(&self.top);
        command
            .arg("wsl")
            .env("RTRASH_WSL_OSRELEASE", self.top.join("osrelease"))
            .env("RTRASH_WSL_CONF", self.top.join("wsl.conf"))
            .env("RTRASH_WSL_SID", SID);
        command
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
        let _ = fs::remove_dir_all(&self.cleanup);
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
    assert!(text.contains(r#""id":"#), "{text}");
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
    // A selector with no match exits one; no selector is a usage error.
    let missing = f.run(&["rm", "--name=none"]);
    assert_eq!(missing.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&missing.stderr).contains("no trash items match"),
        "{}",
        String::from_utf8_lossy(&missing.stderr)
    );
    assert_eq!(f.run(&["rm"]).status.code(), Some(2));
}

#[test]
fn wsl_rm_refuses_risky_git_work_without_force_and_dry_run_reports_it() {
    let f = Fake::new("rm-git-risk");
    let id = "AB12CD";
    let payload = f.bin().join(format!("$R{id}"));
    fs::create_dir(&payload).unwrap();
    fs::create_dir(payload.join(".git")).unwrap();
    fs::write(payload.join("f"), b"x").unwrap();
    let rec = encode_recycle_i_v2(
        Path::new(r"C:\repo"),
        0,
        FILETIME_UNIX_EPOCH + 1_700_000_000 * 10_000_000,
    );
    fs::write(f.bin().join(format!("$I{id}")), rec).unwrap();
    f.recycle("EF34GH.txt", r"C:\apart.txt", b"apart", 1_700_000_100);

    let dry = f.run(&["rm", "-n", "--name=repo"]);
    let dry_err = String::from_utf8_lossy(&dry.stderr);
    assert_eq!(dry.status.code(), Some(1), "{dry_err}");
    assert!(dry_err.contains("refusing to remove"), "{dry_err}");
    assert!(payload.exists(), "{dry_err}");

    let blocked = f.run(&["rm", "--name=repo"]);
    let err = String::from_utf8_lossy(&blocked.stderr);
    assert_eq!(blocked.status.code(), Some(1), "{err}");
    assert!(err.contains("refusing to remove"), "{err}");
    assert!(err.contains("pass -f"), "{err}");
    assert!(payload.exists(), "{err}");

    let forced = f.run(&["rm", "-f", "--name=repo"]);
    assert!(
        forced.status.success(),
        "{}",
        String::from_utf8_lossy(&forced.stderr)
    );
    assert!(!payload.exists());
    assert!(!f.bin().join(format!("$I{id}")).exists());
}

#[test]
fn wsl_list_and_rm_accept_match_id() {
    let f = Fake::new("match-id");
    f.recycle("AB12CD.txt", r"C:\a.txt", b"a", 1_700_000_000);
    f.recycle("EF34GH.txt", r"C:\b.txt", b"b", 1_700_000_100);
    let json = f.run(&["list", "--json"]);
    assert!(
        json.status.success(),
        "{}",
        String::from_utf8_lossy(&json.stderr)
    );
    let body = String::from_utf8_lossy(&json.stdout);
    let key = r#""id":""#;
    let start = body.find(key).expect("id field in json") + key.len();
    let end = body[start..].find('"').expect("id close quote") + start;
    let id = body[start..end].to_string();

    let listed = f.run(&["list", "--match=id", &id]);
    let text = String::from_utf8_lossy(&listed.stdout);
    assert!(
        listed.status.success(),
        "{}",
        String::from_utf8_lossy(&listed.stderr)
    );
    assert!(text.contains("a.txt") && !text.contains("b.txt"), "{text}");

    let dry = f.run(&["rm", "-n", "-m", "id", &id]);
    let dry_text = String::from_utf8_lossy(&dry.stdout);
    assert!(
        dry.status.success(),
        "{}",
        String::from_utf8_lossy(&dry.stderr)
    );
    assert!(
        dry_text.contains("a.txt") && !dry_text.contains("b.txt"),
        "{dry_text}"
    );

    let rm = f.run(&["rm", "--match=id", &id]);
    assert!(
        rm.status.success(),
        "{}",
        String::from_utf8_lossy(&rm.stderr)
    );
    assert!(!f.bin().join("$RAB12CD.txt").exists());
    assert!(f.bin().join("$REF34GH.txt").exists());

    let missing_match = f.run(&["list", "--match=id"]);
    assert_eq!(missing_match.status.code(), Some(2));
}

fn exact_kept(tag: &str) -> (Fake, PathBuf) {
    let f = Fake::new(tag);
    let kept = f.root.join("c/Users/me/kept.txt");
    fs::create_dir_all(f.top.join(".config/rtrash")).unwrap();
    fs::write(
        f.top.join(".config/rtrash/keep"),
        format!("{}\n", kept.display()),
    )
    .unwrap();
    f.recycle(
        "AB12CD.txt",
        r"C:\Users\me\kept.txt",
        b"kept",
        1_700_000_000,
    );
    (f, kept)
}

fn assert_kept(f: &Fake, kept: &Path, command: &str) {
    let out = f.run(&[command, &kept.display().to_string()]);
    assert!(
        out.status.success(),
        "{command}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("listed in the keep file"),
        "{command}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        f.bin().join("$RAB12CD.txt").exists(),
        "{command} removed the kept payload"
    );
}

#[test]
fn wsl_rm_keeps_an_exact_protected_path() {
    let (f, kept) = exact_kept("rm-exact-keep");
    assert_kept(&f, &kept, "rm");
}

#[test]
fn wsl_empty_keeps_an_exact_protected_path() {
    let (f, kept) = exact_kept("empty-exact-keep");
    assert_kept(&f, &kept, "empty");
}

#[test]
fn wsl_exact_misses_use_the_shared_diagnostic_and_json_summary() {
    let f = Fake::new("exact-miss");
    f.recycle(
        "AB12CD.txt",
        r"C:\Users\me\kept.txt",
        b"kept",
        1_700_000_000,
    );
    let missing = f.root.join("c/Users/me/kept");
    let path = missing.display().to_string();
    for args in [
        vec!["rm", &path],
        vec!["rm", "-n", &path],
        vec!["rm", "--json", &path],
        vec!["rm", "--json", "--match=exact", &path],
        vec!["empty", &path],
        vec!["empty", "-n", &path],
        vec!["empty", "--json", &path],
    ] {
        let out = f.run(&args);
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(1), "{args:?}: {err}");
        assert!(err.contains("no exact match"), "{args:?}: {err}");
        assert!(err.contains("no trash items match"), "{args:?}: {err}");
        if args.contains(&"--json") {
            let body: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
                panic!("{args:?}: {e}: {}", String::from_utf8_lossy(&out.stdout))
            });
            assert_eq!(body["removed"], 0, "{args:?}: {body}");
        }
    }
    assert!(f.bin().join("$RAB12CD.txt").exists());
}

#[test]
fn wsl_json_summarizes_an_exact_match_filtered_by_the_keep_file() {
    let (f, kept) = exact_kept("json-exact-keep");
    let path = kept.display().to_string();
    let rm = f.run(&["rm", "--json", &path]);
    assert!(
        rm.status.success(),
        "{}",
        String::from_utf8_lossy(&rm.stderr)
    );
    let rm_body: serde_json::Value = serde_json::from_slice(&rm.stdout).unwrap();
    assert_eq!(rm_body["removed"], 0);
    assert_eq!(rm_body["matches"], serde_json::json!([]));

    let empty = f.run(&["empty", "--json", &path]);
    assert!(
        empty.status.success(),
        "{}",
        String::from_utf8_lossy(&empty.stderr)
    );
    let empty_body: serde_json::Value = serde_json::from_slice(&empty.stdout).unwrap();
    assert_eq!(empty_body["removed"], 0);
    assert!(f.bin().join("$RAB12CD.txt").exists());
}

#[test]
fn wsl_exact_keep_matches_paths_below_a_linked_temp_directory() {
    let f = Fake::behind_link("linked-exact-keep");
    let kept = f.root.join("c/Users/me/kept.txt");
    fs::create_dir_all(f.top.join(".config/rtrash")).unwrap();
    fs::write(
        f.top.join(".config/rtrash/keep"),
        format!("{}\n", kept.display()),
    )
    .unwrap();
    f.recycle(
        "AB12CD.txt",
        r"C:\Users\me\kept.txt",
        b"kept",
        1_700_000_000,
    );
    assert_kept(&f, &kept, "rm");
    assert_kept(&f, &kept, "empty");
}

fn exact_duplicates(tag: &str) -> (Fake, PathBuf) {
    let f = Fake::new(tag);
    f.recycle(
        "AB12CD.txt",
        r"C:\Users\me\same.txt",
        b"first",
        1_700_000_000,
    );
    f.recycle(
        "EF34GH.txt",
        r"C:\Users\me\same.txt",
        b"second",
        1_700_000_100,
    );
    let same = f.root.join("c/Users/me/same.txt");
    (f, same)
}

fn assert_duplicates_refused(f: &Fake, same: &Path, command: &str) {
    let out = f.run(&[command, "-n", &same.display().to_string()]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "{command}: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("refusing 2 trash items with original path"),
        "{command}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(f.bin().join("$RAB12CD.txt").exists());
    assert!(f.bin().join("$REF34GH.txt").exists());
}

#[test]
fn wsl_rm_refuses_duplicate_exact_originals_until_id_selects_one() {
    let (f, same) = exact_duplicates("rm-exact-ambiguous");
    assert_duplicates_refused(&f, &same, "rm");
    let exact = f.run(&["rm", "-n", "--match=exact", &same.display().to_string()]);
    assert_eq!(
        exact.status.code(),
        Some(2),
        "{}{}",
        String::from_utf8_lossy(&exact.stdout),
        String::from_utf8_lossy(&exact.stderr)
    );

    let json = f.run(&["list", "--json"]);
    let body = String::from_utf8_lossy(&json.stdout);
    let key = r#""id":""#;
    let start = body.find(key).expect("id field in json") + key.len();
    let end = body[start..].find('"').expect("id close quote") + start;
    let id = &body[start..end];
    let removed = f.run(&["rm", "--match=id", id]);
    assert!(
        removed.status.success(),
        "{}",
        String::from_utf8_lossy(&removed.stderr)
    );
    assert_eq!(
        [f.bin().join("$RAB12CD.txt"), f.bin().join("$REF34GH.txt")]
            .into_iter()
            .filter(|path| path.exists())
            .count(),
        1
    );
}

#[test]
fn wsl_empty_refuses_duplicate_exact_originals() {
    let (f, same) = exact_duplicates("empty-exact-ambiguous");
    assert_duplicates_refused(&f, &same, "empty");
}

fn refusal(out: &std::process::Output) -> String {
    assert_eq!(
        out.status.code(),
        Some(2),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn refusal_reason(out: &std::process::Output) -> String {
    refusal(out)
        .split_once(": ")
        .map(|(_, reason)| reason)
        .unwrap()
        .replace("rtrash wsl", "rtrash")
        .replace("rtrash rm", "rtrash")
}

#[test]
fn wsl_pattern_modes_share_the_mass_scope_and_unknown_root_guards() {
    let f = Fake::new("pattern-guards");
    f.recycle("AB12CD.txt", r"C:\Users\me\a.txt", b"a", 1_700_000_000);
    f.recycle("EF34GH.txt", r"C:\Users\me\b.txt", b"b", 1_700_000_100);
    let slash_glob = format!("{}/*", f.root.join("c/Users/me").display());
    let slash_option = format!("--glob={slash_glob}");
    let mut missed = Vec::new();
    for args in [
        vec!["rm", "-n", "--regex=.*"],
        vec!["rm", "-n", "--match=regex", ".*"],
        vec!["rm", "-n", slash_option.as_str()],
        vec!["rm", "-n", "--match=substring", "Users"],
    ] {
        let out = f.run_without_scope_roots(&args);
        let err = String::from_utf8_lossy(&out.stderr);
        if out.status.code() != Some(2) || !err.contains("When the scope is unknown") {
            missed.push(format!("{args:?}: status {:?}: {err}", out.status.code()));
        }
    }
    for args in [&["rm", "-n", "--glob=*"][..], &["rm", "-n", "--glob=*.txt"]] {
        let out = f.run(args);
        let err = String::from_utf8_lossy(&out.stderr);
        if out.status.code() != Some(2) || !err.contains("refusing mass pattern") {
            missed.push(format!("{args:?}: status {:?}: {err}", out.status.code()));
        }
    }
    let exact = f.root.join("c/Users/me/a.txt").display().to_string();
    let mixed = ["rm", "-n", "--glob=b*", exact.as_str()];
    let out = f.run(&mixed);
    let err = String::from_utf8_lossy(&out.stderr);
    if out.status.code() != Some(2) || !err.contains("refusing mass pattern") {
        missed.push(format!("{mixed:?}: status {:?}: {err}", out.status.code()));
    }
    assert!(missed.is_empty(), "{missed:#?}");

    let narrow = f.run_without_scope_roots(&["rm", "-n", "--glob=a*"]);
    assert!(
        narrow.status.success(),
        "{}",
        String::from_utf8_lossy(&narrow.stderr)
    );
    let shown = String::from_utf8_lossy(&narrow.stdout);
    assert!(
        shown.contains("a.txt") && !shown.contains("b.txt"),
        "{shown}"
    );

    let forced = f.run_without_scope_roots(&["rm", "-n", "-f", "--regex=.*"]);
    assert!(
        forced.status.success(),
        "{}",
        String::from_utf8_lossy(&forced.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&forced.stdout)
            .lines()
            .filter(|line| line.contains("would remove"))
            .count(),
        2
    );
}

#[test]
fn wsl_rm_and_freedesktop_rm_give_the_same_pattern_refusals() {
    let f = Fake::new("pattern-parity");
    f.recycle("AB12CD.txt", r"C:\Users\me\a.txt", b"a", 1_700_000_000);
    f.recycle("EF34GH.txt", r"C:\Users\me\b.txt", b"b", 1_700_000_100);
    let source = f.root.join("c/Users/me");
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("a.txt"), b"a").unwrap();
    fs::write(source.join("b.txt"), b"b").unwrap();
    let trash = f.top.join("freedesktop-trash");
    let pin = format!("--trash-dir={}", trash.display());
    let put = common::rtrash(&f.top)
        .args([
            "put",
            pin.as_str(),
            source.join("a.txt").to_str().unwrap(),
            source.join("b.txt").to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        put.status.success(),
        "{}",
        String::from_utf8_lossy(&put.stderr)
    );

    let slash_glob = format!("{}/*", source.display());
    let slash_option = format!("--glob={slash_glob}");
    let cases = [
        (vec!["--regex=.*"], vec!["--regex", ".*"], false),
        (
            vec![slash_option.as_str()],
            vec!["--match=glob", slash_glob.as_str()],
            false,
        ),
        (
            vec!["--match=substring", "Users"],
            vec!["--match=substring", "Users"],
            false,
        ),
        (vec!["--glob=*"], vec!["--match=glob", "*"], true),
        (vec!["--glob=*.txt"], vec!["--match=glob", "*.txt"], true),
    ];
    for (wsl_selection, plain_selection, roots_known) in cases {
        let mut wsl_args = vec!["rm", "-n"];
        wsl_args.extend(wsl_selection.iter().copied());
        let wsl = if roots_known {
            f.run(&wsl_args)
        } else {
            f.run_without_scope_roots(&wsl_args)
        };

        let mut plain = common::rtrash(&f.top);
        plain.args(["rm", "-n", pin.as_str()]);
        plain.args(&plain_selection);
        if !roots_known {
            plain
                .env_remove("HOME")
                .env_remove("USERPROFILE")
                .env_remove("XDG_DATA_HOME");
        }
        let plain = plain.output().unwrap();
        let wsl_err = refusal_reason(&wsl);
        let plain_err = refusal_reason(&plain);
        assert_eq!(wsl_err, plain_err, "{wsl_selection:?}");
    }
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
    f.recycle("EF34GH.txt", r"C:\apart.txt", b"apart", 1_700_000_100);
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
