//! The FreeDesktop trash end to end: put, list, restore, empty and rm.
//! Windows sends put to the Recycle Bin instead, so these run on Unix only.
#![cfg(unix)]

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_rtrash")
}

/// Isolated sandbox: its own XDG_DATA_HOME and working directory so parallel
/// tests never share a trash.
struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!("rtrash-test-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("work")).unwrap();
        // put records originals with symlinked parents resolved (on macOS
        // the temp dir sits under /var, a symlink to /private/var), so the
        // expected paths use the resolved root too.
        let root = fs::canonicalize(&root).unwrap();
        Sandbox { root }
    }

    fn work(&self) -> PathBuf {
        self.root.join("work")
    }

    fn trash(&self) -> PathBuf {
        self.root.join("xdg/Trash")
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(bin())
            .args(args)
            .current_dir(self.work())
            .env("XDG_DATA_HOME", self.root.join("xdg"))
            .env("XDG_CONFIG_HOME", self.root.join(".config"))
            .env("HOME", &self.root)
            .output()
            .expect("failed to run rtrash")
    }

    /// Like `run`, but feeds `stdin` to the child (interactive restore selection).
    fn run_with_stdin(&self, args: &[&str], stdin: &str) -> Output {
        let mut child = Command::new(bin())
            .args(args)
            .current_dir(self.work())
            .env("XDG_DATA_HOME", self.root.join("xdg"))
            .env("XDG_CONFIG_HOME", self.root.join(".config"))
            .env("HOME", &self.root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn rtrash");
        {
            let mut w = child.stdin.take().expect("stdin");
            w.write_all(stdin.as_bytes()).unwrap();
        }
        child.wait_with_output().expect("failed to wait rtrash")
    }

    fn touch(&self, name: &str) -> PathBuf {
        let p = self.work().join(name);
        fs::write(&p, b"payload").unwrap();
        p
    }

    /// Run `empty` pinned to this sandbox's trash dir; a bare `empty` scans
    /// every mounted volume's trash and would purge the host's real trash.
    fn empty(&self, extra: &[&str]) -> Output {
        let pin = format!("--trash-dir={}", self.trash().display());
        let mut args: Vec<&str> = vec!["empty", &pin];
        args.extend_from_slice(extra);
        self.run(&args)
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn stdout_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn trash_names(sb: &Sandbox) -> Vec<String> {
    match fs::read_dir(sb.trash().join("files")) {
        Ok(rd) => rd
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect(),
        Err(_) => Vec::new(),
    }
}

#[test]
fn put_moves_file_and_writes_info() {
    let sb = Sandbox::new("put-basic");
    let f = sb.touch("hello.txt");
    let out = sb.run(&["put", "hello.txt"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(!f.exists());
    assert!(sb.trash().join("files/hello.txt").exists());
    let info = fs::read_to_string(sb.trash().join("info/hello.txt.trashinfo")).unwrap();
    assert!(info.starts_with("[Trash Info]\n"), "{info}");
    assert!(info.contains(&format!("Path={}", f.display())), "{info}");
    assert!(info.contains("DeletionDate="), "{info}");
}

#[test]
fn put_json_reports_trashed_and_failed() {
    let sb = Sandbox::new("put-json");
    sb.touch("a.txt");
    fs::create_dir(sb.work().join("d")).unwrap();
    fs::write(sb.work().join("d/x"), b"x").unwrap();
    let pin = format!("--trash-dir={}", sb.trash().display());
    let out = sb.run(&["put", &pin, "--json", "-rv", "a.txt", "d", "missing.txt"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    let stdout = stdout_of(&out);
    // -v lines must not mix into the JSON on stdout.
    assert_eq!(stdout.lines().count(), 1, "{stdout}");
    let line = stdout.trim();
    assert!(line.starts_with("{\"trashed\":["), "{line}");
    assert!(line.ends_with(",\"failed\":1}"), "{line}");
    let a = sb.work().join("a.txt");
    assert!(
        line.contains(&format!("\"original\":\"{}\"", a.display())),
        "{line}"
    );
    assert!(line.contains("\"name\":\"d\""), "{line}");
    assert!(
        line.contains(&format!("\"trash_dir\":\"{}\"", sb.trash().display())),
        "{line}"
    );
    assert!(stderr_of(&out).contains("missing.txt"));
}

/// Run rtrash in `cwd` with the trash under `data_home`.
fn run_in(cwd: &Path, data_home: &Path, args: &[&str]) -> Output {
    Command::new(bin())
        .args(args)
        .current_dir(cwd)
        .env("XDG_DATA_HOME", data_home)
        .env("HOME", data_home)
        .output()
        .expect("failed to run rtrash")
}

/// The JSON string value of `key` in `json`.
fn json_str(json: &str, key: &str) -> String {
    let at = json
        .find(&format!("\"{key}\":\""))
        .unwrap_or_else(|| panic!("no {key} in {json}"))
        + key.len()
        + 4;
    json[at..at + json[at..].find('"').unwrap()].to_string()
}

/// A sandbox reached through a symlink, the way macOS reaches its temp
/// dir through /var -> /private/var. Originals and trash dirs both come
/// out resolved, and a path typed through the link still restores.
#[test]
fn symlinked_parents_resolve_alike_for_originals_and_trash() {
    let sb = Sandbox::new("symlinked");
    let link = sb.root.with_extension("link");
    let _ = fs::remove_file(&link);
    std::os::unix::fs::symlink(&sb.root, &link).unwrap();
    let lwork = link.join("work");
    let ldata = link.join("xdg");
    fs::write(lwork.join("a.txt"), b"a").unwrap();

    let out = run_in(&lwork, &ldata, &["put", "--json", "a.txt"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let json = stdout_of(&out);
    let original = sb.work().join("a.txt");
    assert_eq!(json_str(&json, "original"), original.display().to_string());
    assert_eq!(
        json_str(&json, "trash_dir"),
        sb.trash().display().to_string()
    );
    let info = fs::read_to_string(sb.trash().join("info/a.txt.trashinfo")).unwrap();
    assert!(
        info.contains(&format!("Path={}\n", original.display())),
        "{info}"
    );

    // Restore by the path as typed through the link.
    let typed = lwork.join("a.txt");
    let out = run_in(&lwork, &ldata, &["restore", typed.to_str().unwrap()]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(original.exists());

    // A pinned volume trash reached through the link keeps Path= relative
    // to its topdir, and restore finds the file again.
    let uid = {
        use std::os::unix::fs::MetadataExt;
        fs::metadata(&sb.root).unwrap().uid()
    };
    fs::create_dir(sb.root.join("vol")).unwrap();
    fs::write(sb.root.join("vol/f.txt"), b"f").unwrap();
    let pin = link.join(format!("vol/.Trash-{uid}"));
    let pin_arg = format!("--trash-dir={}", pin.display());
    let lvol = link.join("vol");
    let out = run_in(&lvol, &ldata, &["put", &pin_arg, "--json", "f.txt"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let real_pin = sb.root.join(format!("vol/.Trash-{uid}"));
    assert_eq!(
        json_str(&stdout_of(&out), "trash_dir"),
        real_pin.display().to_string()
    );
    let info = fs::read_to_string(real_pin.join("info/f.txt.trashinfo")).unwrap();
    assert!(info.contains("Path=f.txt\n"), "{info}");
    let out = run_in(&lvol, &ldata, &["restore", &pin_arg, "f.txt"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(sb.root.join("vol/f.txt").exists());
    let _ = fs::remove_file(&link);
}

/// `rm` takes an absolute pattern typed through a symlinked directory,
/// as restore takes such a path.
#[test]
fn rm_patterns_through_a_symlinked_parent_match() {
    let sb = Sandbox::new("rm-symlinked");
    let link = sb.root.with_extension("link");
    let _ = fs::remove_file(&link);
    std::os::unix::fs::symlink(&sb.root, &link).unwrap();
    let lwork = link.join("work");
    let ldata = link.join("xdg");
    for f in ["a.txt", "b.o", "c.o"] {
        fs::write(lwork.join(f), b"x").unwrap();
    }
    let out = run_in(&lwork, &ldata, &["put", "a.txt", "b.o", "c.o"]);
    assert!(out.status.success(), "{}", stderr_of(&out));

    let typed = lwork.join("a.txt");
    let out = run_in(&lwork, &ldata, &["rm", typed.to_str().unwrap()]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let glob = format!("{}/*.o", lwork.display());
    let out = run_in(&lwork, &ldata, &["rm", "-n", &glob]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(stdout_of(&out).lines().count(), 2, "{}", stdout_of(&out));
    let left: Vec<_> = fs::read_dir(sb.trash().join("info"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert!(!left.iter().any(|n| n.starts_with("a.txt")), "{left:?}");
    assert_eq!(left.len(), 2, "{left:?}");
    let _ = fs::remove_file(&link);
}

#[test]
fn put_json_force_missing_is_empty_summary() {
    let sb = Sandbox::new("put-json-force");
    let pin = format!("--trash-dir={}", sb.trash().display());
    let out = sb.run(&["put", &pin, "--json", "-f", "nope.txt"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(stdout_of(&out).trim(), r#"{"trashed":[],"failed":0}"#);
}

#[test]
fn put_directory_requires_recursive() {
    let sb = Sandbox::new("put-dir");
    fs::create_dir(sb.work().join("d")).unwrap();
    fs::write(sb.work().join("d/x"), b"x").unwrap();

    let out = sb.run(&["put", "d"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr_of(&out).contains("Is a directory"),
        "{}",
        stderr_of(&out)
    );
    assert!(sb.work().join("d").exists());

    let out = sb.run(&["put", "-r", "d"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(!sb.work().join("d").exists());
    assert!(sb.trash().join("files/d/x").exists());
}

#[test]
fn put_d_flag_only_empty_dirs() {
    let sb = Sandbox::new("put-dflag");
    fs::create_dir(sb.work().join("full")).unwrap();
    fs::write(sb.work().join("full/x"), b"x").unwrap();
    fs::create_dir(sb.work().join("hollow")).unwrap();

    let out = sb.run(&["put", "-d", "full"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr_of(&out).contains("Directory not empty"));

    let out = sb.run(&["put", "-d", "hollow"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(!sb.work().join("hollow").exists());
}

#[test]
fn force_ignores_missing_and_bare_force_exits_zero() {
    let sb = Sandbox::new("put-force");
    let out = sb.run(&["put", "nope"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr_of(&out).contains("No such file or directory"));

    let out = sb.run(&["put", "-f", "nope"]);
    assert!(out.status.success());

    let out = sb.run(&["put", "-f"]);
    assert!(out.status.success());

    let out = sb.run(&["put"]);
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn combined_short_flags_and_rm_fallthrough() {
    let sb = Sandbox::new("put-rf");
    fs::create_dir(sb.work().join("d")).unwrap();
    // No subcommand: `rtrash -rf d` behaves like rm -rf.
    let out = sb.run(&["-rf", "d"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(!sb.work().join("d").exists());
}

#[test]
fn refuses_dot_and_root() {
    let sb = Sandbox::new("put-dots");
    let out = sb.run(&["put", "-rf", "."]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr_of(&out).contains("refusing"));

    let out = sb.run(&["put", "-rf", "/"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr_of(&out).contains("dangerous"));
}

#[cfg(unix)]
#[test]
fn put_skips_safe_rm_paths_and_refuses_home() {
    let sb = Sandbox::new("put-safety");
    fs::create_dir(sb.work().join("precious")).unwrap();
    sb.touch("precious/f");
    fs::create_dir(sb.work().join("other")).unwrap();
    // safe-rm's per-user list lives in ~/.safe-rm; patterns work too.
    fs::write(
        sb.root.join(".safe-rm"),
        format!("# keep\n{}/prec*\n", sb.work().display()),
    )
    .unwrap();
    let out = sb.run(&["put", "-r", "precious", "other"]);
    assert_eq!(out.status.code(), Some(1));
    let err = stderr_of(&out);
    assert!(err.contains("skipping protected path 'precious'"), "{err}");
    assert!(sb.work().join("precious/f").exists());
    assert!(!sb.work().join("other").exists(), "unlisted paths still go");

    let out = sb.run(&["put", "-rf", "/usr"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr_of(&out).contains("safe-rm list entry '/usr'"));

    let home = sb.root.display().to_string();
    let out = sb.run(&["put", "-r", "--preserve-root=all", &home]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr_of(&out).contains("home directory"),
        "{}",
        stderr_of(&out)
    );
    assert!(sb.root.join("work").exists());
}

#[cfg(unix)]
#[test]
fn put_warns_before_trashing_a_repo_with_unsaved_work() {
    let sb = Sandbox::new("put-repo-warn");
    let repo = sb.work().join("repo");
    fs::create_dir(&repo).unwrap();
    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .current_dir(&repo)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("HOME", &sb.root)
            .output()
    };
    if !git(&["init", "-q"]).is_ok_and(|o| o.status.success()) {
        eprintln!("git unavailable; skipping");
        return;
    }
    fs::write(repo.join("f"), b"x").unwrap();
    fs::create_dir(sb.work().join("plain")).unwrap();
    let out = sb.run(&["put", "-r", "repo", "plain"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let err = stderr_of(&out);
    assert!(
        err.contains("'repo' contains a git repository with"),
        "{err}"
    );
    assert!(err.contains("rtrash undo"), "{err}");
    assert!(!err.contains("'plain'"), "{err}");
    assert!(!repo.exists(), "a warning, not a refusal");
}

#[test]
fn collision_gets_suffixed_name() {
    let sb = Sandbox::new("put-collide");
    sb.touch("same.txt");
    assert!(sb.run(&["put", "same.txt"]).status.success());
    sb.touch("same.txt");
    assert!(sb.run(&["put", "same.txt"]).status.success());
    let mut names = trash_names(&sb);
    names.sort();
    assert_eq!(names, vec!["same.txt", "same.txt.2"]);
    assert!(sb.trash().join("info/same.txt.2.trashinfo").exists());
}

#[test]
fn symlink_trashes_link_not_target() {
    let sb = Sandbox::new("put-symlink");
    let target = sb.touch("target.txt");
    std::os::unix::fs::symlink(&target, sb.work().join("link")).unwrap();
    let out = sb.run(&["put", "link"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(target.exists(), "target must survive");
    assert!(sb
        .trash()
        .join("files/link")
        .symlink_metadata()
        .unwrap()
        .is_symlink());
}

#[test]
fn list_shows_original_path() {
    let sb = Sandbox::new("list");
    let f = sb.touch("seen.txt");
    sb.run(&["put", "seen.txt"]);
    let out = sb.run(&["list"]);
    assert!(out.status.success());
    assert!(
        stdout_of(&out).contains(&f.display().to_string()),
        "{}",
        stdout_of(&out)
    );
}

#[test]
fn empty_removes_everything() {
    let sb = Sandbox::new("empty-all");
    sb.touch("a");
    sb.touch("b");
    fs::create_dir(sb.work().join("d")).unwrap();
    fs::write(sb.work().join("d/x"), b"x").unwrap();
    sb.run(&["put", "-r", "a", "b", "d"]);
    assert_eq!(trash_names(&sb).len(), 3);

    let out = sb.empty(&[]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(trash_names(&sb).is_empty());
    assert!(fs::read_dir(sb.trash().join("info"))
        .unwrap()
        .next()
        .is_none());
}

#[test]
fn empty_days_keeps_recent_items() {
    let sb = Sandbox::new("empty-days");
    sb.touch("fresh.txt");
    sb.run(&["put", "fresh.txt"]);

    let out = sb.empty(&["5"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(
        trash_names(&sb),
        vec!["fresh.txt"],
        "recent item must survive"
    );

    // Backdate the info file, then a 5-day empty purges it.
    let info_path = sb.trash().join("info/fresh.txt.trashinfo");
    let body = fs::read_to_string(&info_path).unwrap();
    let old = body
        .lines()
        .map(|l| {
            if l.starts_with("DeletionDate=") {
                "DeletionDate=2001-01-01T00:00:00".to_string()
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&info_path, old).unwrap();

    let out = sb.empty(&["5"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(trash_names(&sb).is_empty());
}

#[test]
fn empty_dry_run_removes_nothing() {
    let sb = Sandbox::new("empty-dry");
    sb.touch("keep.txt");
    sb.run(&["put", "keep.txt"]);
    let out = sb.empty(&["--dry-run"]);
    assert!(out.status.success());
    assert_eq!(trash_names(&sb), vec!["keep.txt"]);
    let err = stderr_of(&out);
    assert!(err.contains("Would remove 1 item"), "{err}");
    assert!(
        err.contains("reclaimable"),
        "dry-run must report reclaimable space: {err}"
    );
}

#[test]
fn directorysizes_atomic_rewrite_survives_put() {
    let sb = Sandbox::new("dirsizes-atomic");
    fs::create_dir(sb.work().join("d")).unwrap();
    fs::write(sb.work().join("d/x"), b"payload-bytes").unwrap();
    assert!(sb.run(&["put", "-r", "d"]).status.success());
    let cache = sb.trash().join("directorysizes");
    assert!(cache.is_file());
    let body = fs::read_to_string(&cache).unwrap();
    assert!(
        body.contains(" d\n") || body.ends_with(" d") || body.split_whitespace().any(|t| t == "d"),
        "{body}"
    );
    // No leftover temp files from atomic write.
    let leftovers: Vec<_> = fs::read_dir(sb.trash())
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
        .collect();
    assert!(
        leftovers.is_empty(),
        "atomic write left temps: {leftovers:?}"
    );
}

#[test]
fn trash_rm_dry_run_keeps_entries_and_reports_reclaim() {
    let sb = Sandbox::new("rm-dry");
    sb.touch("drop.o");
    sb.touch("keep.c");
    assert!(sb.run(&["put", "drop.o", "keep.c"]).status.success());
    let out = sb.run(&["rm", "--dry-run", "*.o"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let err = stderr_of(&out);
    assert!(err.contains("Would permanently remove"), "{err}");
    assert!(err.contains("reclaimable"), "{err}");
    let mut names = trash_names(&sb);
    names.sort();
    assert_eq!(names, vec!["drop.o", "keep.c"]);
}

#[test]
fn status_reports_items_and_size_after_put() {
    let sb = Sandbox::new("status");
    fs::write(sb.work().join("blob.bin"), vec![b'x'; 8192]).unwrap();
    assert!(sb.run(&["put", "blob.bin"]).status.success());
    let out = sb.run(&["status", "--home-only"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let s = stdout_of(&out);
    assert!(s.contains("Total:"), "{s}");
    assert!(
        s.contains("\t1 item\t") && s.contains("Total: 1 item ("),
        "{s}"
    );
    assert!(!s.contains("Total: 0 items"), "{s}");
}

#[test]
fn home_only_empty_ignores_foreign_trash_dir() {
    let sb = Sandbox::new("home-only");
    sb.touch("home.txt");
    assert!(sb.run(&["put", "home.txt"]).status.success());
    // Foreign volume-style trash with a separate entry (not under home discovery
    // when --home-only). We pin is not used; foreign is outside XDG home trash.
    let foreign = sb.root.join("mnt/.Trash-1");
    fs::create_dir_all(foreign.join("files")).unwrap();
    fs::create_dir_all(foreign.join("info")).unwrap();
    fs::write(foreign.join("files/vol.dat"), b"keep-on-volume").unwrap();
    fs::write(
        foreign.join("info/vol.dat.trashinfo"),
        "[Trash Info]\nPath=vol.dat\nDeletionDate=2020-01-01T00:00:00\n",
    )
    .unwrap();
    let out = sb.run(&["empty", "--home-only"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(trash_names(&sb).is_empty());
    assert!(
        foreign.join("files/vol.dat").is_file(),
        "volume trash must survive --home-only empty"
    );
}

#[test]
fn put_writes_complete_trashinfo_after_fsync_path() {
    let sb = Sandbox::new("put-fsync-info");
    let f = sb.touch("syncme.txt");
    assert!(sb.run(&["put", "syncme.txt"]).status.success());
    let info = fs::read_to_string(sb.trash().join("info/syncme.txt.trashinfo")).unwrap();
    assert!(info.starts_with("[Trash Info]\n"), "{info}");
    assert!(info.contains("Path="), "{info}");
    assert!(info.contains("DeletionDate="), "{info}");
    // Full body must be present (not truncated reservation).
    assert!(
        info.lines().count() >= 3,
        "expected full trashinfo lines, got {info:?}"
    );
    assert!(!f.exists());
}

#[test]
fn empty_without_pin_clears_home_trash() {
    // trash-cli parity: default empty visits home trash (no --trash-dir).
    let sb = Sandbox::new("empty-default-home");
    sb.touch("home-only.txt");
    assert!(sb.run(&["put", "home-only.txt"]).status.success());
    assert!(sb.trash().join("files/home-only.txt").exists());
    // Bare empty (no pin) must clear this sandbox home trash via XDG_DATA_HOME.
    let out = sb.run(&["empty"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(trash_names(&sb).is_empty());
}

#[test]
fn restore_force_missing_payload_keeps_dest() {
    let sb = Sandbox::new("restore-missing-payload");
    sb.touch("live.txt");
    assert!(sb.run(&["put", "live.txt"]).status.success());
    // Stale info, missing payload: must not destroy a recreated dest with -f.
    fs::remove_file(sb.trash().join("files/live.txt")).unwrap();
    fs::write(sb.work().join("live.txt"), b"survivor").unwrap();
    let out = sb.run(&["restore", "-f", "live.txt"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    assert_eq!(fs::read(sb.work().join("live.txt")).unwrap(), b"survivor");
}

#[test]
fn trash_dir_pin_rejects_non_trash_layout() {
    let sb = Sandbox::new("bad-pin");
    let fake = sb.root.join("project");
    fs::create_dir_all(fake.join("files")).unwrap();
    fs::create_dir_all(fake.join("info")).unwrap();
    fs::write(fake.join("files/secret"), b"do-not-wipe").unwrap();
    // Valid-looking names but we still require the layout — this IS valid layout.
    // Make info a symlink so validation fails.
    fs::remove_dir(fake.join("info")).unwrap();
    std::os::unix::fs::symlink("/tmp", fake.join("info")).unwrap();
    let pin = format!("--trash-dir={}", fake.display());
    let out = sb.run(&["empty", &pin]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr_of(&out));
    assert_eq!(fs::read(fake.join("files/secret")).unwrap(), b"do-not-wipe");
}

#[test]
fn trash_rm_refuses_star_without_force() {
    let sb = Sandbox::new("rm-star");
    sb.touch("a.txt");
    sb.run(&["put", "a.txt"]);
    let out = sb.run(&["rm", "*"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr_of(&out));
    assert_eq!(trash_names(&sb), vec!["a.txt"]);
}

#[test]
fn empty_dry_run_reports_space_for_tree() {
    let sb = Sandbox::new("empty-dry-du");
    let d = sb.work().join("big");
    fs::create_dir(&d).unwrap();
    // ~64 KiB payload so the summary cannot be "0 B" on any normal FS.
    fs::write(d.join("blob"), vec![b'z'; 64 * 1024]).unwrap();
    assert!(sb.run(&["put", "-r", "big"]).status.success());
    let out = sb.empty(&["--dry-run"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(sb.trash().join("files/big/blob").is_file());
    let err = stderr_of(&out);
    assert!(err.contains("Would remove 1 item"), "{err}");
    assert!(err.contains("reclaimable"), "{err}");
    // Must not claim zero reclaim for a large file.
    assert!(
        !err.contains("(0 B,"),
        "expected non-zero reclaim estimate: {err}"
    );
}

#[test]
fn empty_purges_orphaned_files() {
    let sb = Sandbox::new("empty-orphan");
    sb.touch("x");
    sb.run(&["put", "x"]);
    fs::remove_file(sb.trash().join("info/x.trashinfo")).unwrap();
    let out = sb.empty(&[]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(trash_names(&sb).is_empty());
}

#[test]
fn restore_single_match_roundtrip() {
    let sb = Sandbox::new("restore");
    let f = sb.touch("back.txt");
    sb.run(&["put", "back.txt"]);
    assert!(!f.exists());

    let out = sb.run(&["restore", "back.txt"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(f.exists());
    assert_eq!(fs::read(&f).unwrap(), b"payload");
    assert!(trash_names(&sb).is_empty());
    assert!(!sb.trash().join("info/back.txt.trashinfo").exists());
}

#[test]
fn restore_interactive_picks_by_index_from_full_trash() {
    let sb = Sandbox::new("restore-pick");
    let a = sb.touch("a.txt");
    let b = sb.touch("b.txt");
    assert!(sb.run(&["put", "a.txt"]).status.success());
    assert!(sb.run(&["put", "b.txt"]).status.success());
    assert!(!a.exists() && !b.exists());

    // Bare `restore` lists every trashed item; pick index 0 via piped stdin.
    let list = sb.run(&["list"]);
    assert!(list.status.success(), "{}", stderr_of(&list));
    let list_out = stdout_of(&list);
    let n_lines = list_out.lines().filter(|l| !l.trim().is_empty()).count();
    assert_eq!(n_lines, 2, "list: {list_out}");

    let out = sb.run_with_stdin(&["restore"], "0\n");
    assert!(
        out.status.success(),
        "stdout={} stderr={}",
        stdout_of(&out),
        stderr_of(&out)
    );
    assert!(stdout_of(&out).contains("restored"), "{}", stdout_of(&out));
    let restored = a.exists() as u8 + b.exists() as u8;
    assert_eq!(restored, 1, "exactly one file restored after pick 0");
    assert_eq!(trash_names(&sb).len(), 1);
}

/// Backdate one trashinfo so ordering tests do not depend on put timing.
fn backdate_trashinfo(sb: &Sandbox, name: &str, date: &str) {
    let info = sb.trash().join(format!("info/{name}.trashinfo"));
    let text = fs::read_to_string(&info).unwrap();
    let out: Vec<String> = text
        .lines()
        .map(|l| {
            if l.starts_with("DeletionDate=") {
                format!("DeletionDate={date}")
            } else {
                l.to_string()
            }
        })
        .collect();
    fs::write(&info, out.join("\n") + "\n").unwrap();
}

#[test]
fn restore_plain_order_defaults_oldest_first_and_flags_flip_it() {
    let sb = Sandbox::new("restore-order");
    let old = sb.touch("old.txt");
    let new = sb.touch("new.txt");
    assert!(sb.run(&["put", "old.txt", "new.txt"]).status.success());
    backdate_trashinfo(&sb, "old.txt", "2001-01-01T00:00:00");
    let pin = format!("--trash-dir={}", sb.trash().display());

    // Numbered table keeps oldest first: index 0 is the backdated item.
    let out = sb.run_with_stdin(&["restore", &pin, "--plain"], "0\n");
    assert!(out.status.success(), "{}", stderr_of(&out));
    let table = stdout_of(&out);
    let first = table.lines().next().unwrap_or("");
    assert!(first.contains("old.txt"), "{table}");
    assert!(old.exists() && !new.exists());

    // --newest-first flips the table: index 0 is now the latest item.
    assert!(sb.run(&["put", "old.txt"]).status.success());
    backdate_trashinfo(&sb, "old.txt", "2001-01-01T00:00:00");
    let out = sb.run_with_stdin(&["restore", &pin, "--plain", "--newest-first"], "0\n");
    assert!(out.status.success(), "{}", stderr_of(&out));
    let table = stdout_of(&out);
    let first = table.lines().next().unwrap_or("");
    assert!(first.contains("new.txt"), "{table}");
    assert!(new.exists() && !old.exists());
}

#[test]
fn restore_by_name_latest_json() {
    let sb = Sandbox::new("restore-name");
    let f = sb.touch("note.txt");
    assert!(sb.run(&["put", "note.txt"]).status.success());
    backdate_trashinfo(&sb, "note.txt", "2001-01-01T00:00:00");
    fs::write(&f, b"newer").unwrap();
    assert!(sb.run(&["put", "note.txt"]).status.success());
    assert_eq!(trash_names(&sb).len(), 2);
    let pin = format!("--trash-dir={}", sb.trash().display());

    // Two items share the name; without --latest the table goes to stderr
    // and stdout carries only the JSON summary.
    let out = sb.run_with_stdin(&["restore", &pin, "--json", "--name=note.txt"], "q\n");
    assert!(!out.status.success());
    assert!(stderr_of(&out).contains("note.txt"), "{}", stderr_of(&out));
    assert!(!f.exists());

    let out = sb.run(&["restore", &pin, "--json", "--latest", "--name=note.txt"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let json = stdout_of(&out);
    assert!(json.starts_with("{\"restored\":[{\"original\":"), "{json}");
    assert!(
        json.trim_end().ends_with("\"failed\":0,\"skipped\":0}"),
        "{json}"
    );
    assert!(!json.contains("restored '"), "{json}");
    assert_eq!(fs::read(&f).unwrap(), b"newer");
    assert_eq!(trash_names(&sb), vec!["note.txt".to_string()]);

    let out = sb.run(&["restore", &pin, "--json", "--name=missing.txt"]);
    assert!(!out.status.success());
    assert_eq!(
        stdout_of(&out).trim(),
        "{\"restored\":[],\"failed\":0,\"skipped\":0}"
    );

    let out = sb.run(&["restore", "note.txt", "--name=note.txt"]);
    assert_eq!(out.status.code(), Some(2));
}

#[cfg(unix)]
#[test]
fn restore_glob_regex_ranges_to_and_conflicts() {
    let sb = Sandbox::new("restore-match");
    for n in ["a.rs", "b.rs", "c.txt", "d.txt"] {
        sb.touch(n);
    }
    assert!(sb
        .run(&["put", "a.rs", "b.rs", "c.txt", "d.txt"])
        .status
        .success());
    let pin = format!("--trash-dir={}", sb.trash().display());

    // --glob with --all restores every match, nothing else.
    let out = sb.run(&["restore", &pin, "--glob=*.rs", "--all"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(sb.work().join("a.rs").exists() && sb.work().join("b.rs").exists());
    assert!(!sb.work().join("c.txt").exists());

    // --to restores into another directory; a clash there gets renamed.
    let dest = sb.root.join("dest");
    fs::create_dir(&dest).unwrap();
    fs::write(dest.join("c.txt"), b"keep").unwrap();
    let to = format!("--to={}", dest.display());
    let out = sb.run(&[
        "restore",
        &pin,
        "--regex=c\\.txt$",
        &to,
        "--conflict=rename",
        "--json",
    ]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let json = stdout_of(&out);
    assert!(json.contains("c.1.txt\""), "{json}");
    assert_eq!(fs::read(dest.join("c.txt")).unwrap(), b"keep");
    assert_eq!(fs::read(dest.join("c.1.txt")).unwrap(), b"payload");

    // skip leaves the item in the trash and still exits 0.
    fs::write(sb.work().join("d.txt"), b"live").unwrap();
    let out = sb.run(&["restore", &pin, "--name=d.txt", "--conflict=skip", "--json"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        stdout_of(&out).contains("\"skipped\":1"),
        "{}",
        stdout_of(&out)
    );
    assert_eq!(trash_names(&sb), vec!["d.txt".to_string()]);

    // A range on stdin restores several rows of the numbered table.
    for n in ["e1", "e2", "e3"] {
        sb.touch(n);
    }
    assert!(sb.run(&["put", "e1", "e2", "e3"]).status.success());
    fs::remove_file(sb.work().join("d.txt")).unwrap();
    let out = sb.run_with_stdin(&["restore", &pin, "--plain"], "0-1,3\n");
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(trash_names(&sb).len(), 1, "{}", stdout_of(&out));

    let bad = sb.run(&["restore", &pin, "--conflict=maybe"]);
    assert_eq!(bad.status.code(), Some(2));
    let bad = sb.run(&["restore", &pin, "--glob=*", "--regex=x"]);
    assert_eq!(bad.status.code(), Some(2));
}

#[cfg(unix)]
#[test]
fn list_size_and_sort() {
    let sb = Sandbox::new("list-sort");
    fs::write(sb.work().join("big"), vec![1u8; 64 * 1024]).unwrap();
    fs::write(sb.work().join("small"), b"x").unwrap();
    assert!(sb.run(&["put", "big", "small"]).status.success());
    let pin = format!("--trash-dir={}", sb.trash().display());

    let out = stdout_of(&sb.run(&["list", &pin, "--sort=size"]));
    let rows: Vec<&str> = out.lines().collect();
    assert_eq!(rows.len(), 2, "{out}");
    assert!(
        rows[0].ends_with("/big") && rows[0].contains("KiB"),
        "{out}"
    );
    let out = stdout_of(&sb.run(&["list", &pin, "--sort=name", "--reverse"]));
    assert!(
        out.lines().next().unwrap_or("").ends_with("/small"),
        "{out}"
    );
    let json = stdout_of(&sb.run(&["list", &pin, "--size", "--json"]));
    assert!(json.contains("\"size\":"), "{json}");
    assert_eq!(sb.run(&["list", "--sort=colour"]).status.code(), Some(2));
}

#[test]
fn restore_cwd_only_skips_items_outside_cwd() {
    let sb = Sandbox::new("restore-cwd");
    let in_work = sb.touch("local.txt");
    let outer = sb.root.join("outside.txt");
    fs::write(&outer, b"outer").unwrap();
    assert!(sb.run(&["put", "local.txt"]).status.success());
    // Absolute path so original is outside work/.
    assert!(Command::new(bin())
        .args(["put", outer.to_str().unwrap()])
        .current_dir(sb.work())
        .env("XDG_DATA_HOME", sb.root.join("xdg"))
        .env("HOME", &sb.root)
        .status()
        .unwrap()
        .success());
    assert!(!in_work.exists());
    assert!(!outer.exists());

    // --cwd-only: only local.txt (under work/) is selectable → single match auto-restores.
    let out = sb.run(&["restore", "--cwd-only"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(in_work.exists(), "local restored");
    assert!(!outer.exists(), "outer still trashed with --cwd-only");
}

#[test]
fn restore_refuses_overwrite_without_force() {
    let sb = Sandbox::new("restore-clash");
    sb.touch("c.txt");
    sb.run(&["put", "c.txt"]);
    fs::write(sb.work().join("c.txt"), b"newer").unwrap();

    let out = sb.run(&["restore", "c.txt"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr_of(&out).contains("refusing to overwrite"));
    assert_eq!(fs::read(sb.work().join("c.txt")).unwrap(), b"newer");

    let out = sb.run(&["restore", "-f", "c.txt"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(fs::read(sb.work().join("c.txt")).unwrap(), b"payload");
}

#[test]
fn force_interactive_last_wins_like_gnu_rm() {
    let sb = Sandbox::new("fi-wins");
    // -if: force last → ignore missing, exit 0 (GNU rm last-wins).
    let out = sb.run(&["put", "-if", "nope"]);
    assert!(out.status.success(), "{}", stderr_of(&out));

    // -fi: interactive last clears force → missing still errors.
    let out = sb.run(&["put", "-fi", "nope"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    assert!(stderr_of(&out).contains("No such file or directory"));
}

#[test]
fn restore_force_replaces_directory_at_dest() {
    let sb = Sandbox::new("restore-force-dir");
    sb.touch("x");
    assert!(sb.run(&["put", "x"]).status.success());
    // Recreate original path as a non-empty directory; rename alone cannot replace it.
    fs::create_dir(sb.work().join("x")).unwrap();
    fs::write(sb.work().join("x/inner"), b"blocker").unwrap();

    let out = sb.run(&["restore", "-f", "x"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(fs::read(sb.work().join("x")).unwrap(), b"payload");
    assert!(!sb.work().join("x/inner").exists());
}

#[test]
fn multicall_names_dispatch() {
    let sb = Sandbox::new("multicall");
    let bindir = sb.root.join("bin");
    fs::create_dir_all(&bindir).unwrap();
    for name in [
        "rm",
        "trash",
        "trash-put",
        "trash-empty",
        "trash-list",
        "trash-restore",
        "trash-rm",
    ] {
        std::os::unix::fs::symlink(bin(), bindir.join(name)).unwrap();
    }
    let run_as = |name: &str, args: &[&str]| {
        Command::new(bindir.join(name))
            .args(args)
            .current_dir(sb.work())
            .env("XDG_DATA_HOME", sb.root.join("xdg"))
            .env("HOME", &sb.root)
            .output()
            .unwrap()
    };

    sb.touch("m.txt");
    assert!(run_as("rm", &["m.txt"]).status.success());
    assert!(!sb.work().join("m.txt").exists());
    assert_eq!(trash_names(&sb), vec!["m.txt"]);

    let out = run_as("trash-list", &[]);
    assert!(stdout_of(&out).contains("m.txt"));

    let pin = format!("--trash-dir={}", sb.trash().display());
    assert!(run_as("trash-empty", &[&pin]).status.success());
    assert!(trash_names(&sb).is_empty());

    // `trash` argv0 is an alias for put (trash-cli style).
    sb.touch("via-trash.txt");
    assert!(run_as("trash", &["via-trash.txt"]).status.success());
    assert_eq!(trash_names(&sb), vec!["via-trash.txt"]);

    // trash-rm permanently deletes matching trash entries (not restore).
    let out = run_as("trash-rm", &["via-trash.txt"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(trash_names(&sb).is_empty());
}

#[test]
fn trash_rm_removes_match_keeps_other() {
    let sb = Sandbox::new("trash-rm-select");
    sb.touch("keep.dat");
    sb.touch("drop.o");
    sb.touch("also.o");
    assert!(sb
        .run(&["put", "keep.dat", "drop.o", "also.o"])
        .status
        .success());
    assert_eq!(trash_names(&sb).len(), 3);

    let out = sb.run(&["rm", "*.o"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let mut names = trash_names(&sb);
    names.sort();
    assert_eq!(names, vec!["keep.dat"], "only non-matching entry remains");
    assert!(sb.trash().join("info/keep.dat.trashinfo").exists());
    assert!(!sb.trash().join("info/drop.o.trashinfo").exists());
    assert!(!sb.trash().join("files/drop.o").exists());
}

#[test]
fn trash_rm_literal_basename() {
    let sb = Sandbox::new("trash-rm-literal");
    sb.touch("foo");
    sb.touch("foobar");
    assert!(sb.run(&["put", "foo", "foobar"]).status.success());
    let out = sb.run(&["rm", "foo"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let mut names = trash_names(&sb);
    names.sort();
    assert_eq!(names, vec!["foobar"]);
}

#[test]
fn list_and_restore_respect_trash_dir_pin() {
    let sb = Sandbox::new("pin-list-restore");
    let f = sb.touch("pinned.txt");
    assert!(sb.run(&["put", "pinned.txt"]).status.success());

    let pin = format!("--trash-dir={}", sb.trash().display());
    let out = sb.run(&["list", &pin]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        stdout_of(&out).contains(&f.display().to_string()),
        "{}",
        stdout_of(&out)
    );

    // Pin to an empty foreign trash dir: list must not show the real entry.
    let foreign = sb.root.join("other-trash");
    fs::create_dir_all(foreign.join("files")).unwrap();
    fs::create_dir_all(foreign.join("info")).unwrap();
    let foreign_pin = format!("--trash-dir={}", foreign.display());
    let out = sb.run(&["list", &foreign_pin]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        !stdout_of(&out).contains("pinned.txt"),
        "pinned foreign trash must hide home entries: {}",
        stdout_of(&out)
    );

    // Restore with the correct pin brings the file back.
    let out = sb.run(&["restore", &pin, "pinned.txt"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(f.exists());
    assert_eq!(fs::read(&f).unwrap(), b"payload");
}

/// Volume trash stores relative Path=; --trash-dir pins must infer topdir so
/// list/restore/rm agree with unpinned volume entries (absolute originals).
#[test]
fn volume_trash_pin_resolves_relative_path() {
    let sb = Sandbox::new("volume-pin");
    let uid = unsafe { libc::getuid() };
    let vol = sb.root.join("vol");
    let trash_root = vol.join(format!(".Trash-{uid}"));
    fs::create_dir_all(trash_root.join("files")).unwrap();
    fs::create_dir_all(trash_root.join("info")).unwrap();
    fs::create_dir_all(vol.join("docs")).unwrap();

    let seed = |payload: &[u8]| {
        fs::write(trash_root.join("files/rel.txt"), payload).unwrap();
        fs::write(
            trash_root.join("info/rel.txt.trashinfo"),
            "[Trash Info]\nPath=docs/rel.txt\nDeletionDate=2026-01-02T03:04:05\n",
        )
        .unwrap();
    };
    seed(b"vol-payload");

    let pin = format!("--trash-dir={}", trash_root.display());
    let expect_abs = vol.join("docs/rel.txt");
    let expect_s = expect_abs.display().to_string();

    let out = sb.run(&["list", &pin]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let listed = stdout_of(&out);
    assert!(
        listed.contains(&expect_s),
        "pinned list must resolve relative Path= to absolute under topdir, got: {listed}"
    );
    // Unresolved relative path would appear as a bare "docs/rel.txt" token.
    let only_relative = listed.lines().any(|l| {
        l.ends_with(" docs/rel.txt") || l == "docs/rel.txt" || l.ends_with("\tdocs/rel.txt")
    });
    assert!(
        !only_relative,
        "must not list unresolved relative Path= alone: {listed}"
    );

    let out = sb.run(&["restore", &pin, &expect_s]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        expect_abs.is_file(),
        "restore must land at absolute original"
    );
    assert_eq!(fs::read(&expect_abs).unwrap(), b"vol-payload");
    assert!(!trash_root.join("files/rel.txt").exists());

    // trash-rm by full absolute original must match after topdir resolve.
    seed(b"again");
    let _ = fs::remove_file(&expect_abs);
    let out = sb.run(&["rm", &pin, &expect_s]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(!trash_root.join("files/rel.txt").exists());
    assert!(!trash_root.join("info/rel.txt.trashinfo").exists());
}

/// Multi-file directory payload must be fully removed via the shipped empty
/// path (fastdelete / full wipe), not only a single top-level name.
#[test]
fn empty_removes_deep_directory_payload() {
    let sb = Sandbox::new("empty-deep");
    let root = sb.work().join("deep");
    fs::create_dir_all(root.join("a/b/c")).unwrap();
    for i in 0..50 {
        fs::write(root.join(format!("a/b/c/f{i}")), format!("body{i}")).unwrap();
    }
    fs::write(root.join("a/leaf"), b"leaf").unwrap();
    assert!(sb.run(&["put", "-r", "deep"]).status.success());
    assert!(sb.trash().join("files/deep/a/b/c/f0").is_file());

    let out = sb.empty(&[]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(trash_names(&sb).is_empty());
    assert!(!sb.trash().join("files/deep").exists());
    assert!(fs::read_dir(sb.trash().join("info"))
        .unwrap()
        .next()
        .is_none());
}

#[test]
fn directory_put_writes_directorysizes_empty_prunes() {
    let sb = Sandbox::new("dirsizes");
    fs::create_dir(sb.work().join("tree")).unwrap();
    fs::write(sb.work().join("tree/a"), b"aaaa").unwrap();
    fs::write(sb.work().join("tree/b"), b"bbbbbbbb").unwrap();
    assert!(sb.run(&["put", "-r", "tree"]).status.success());

    let cache = sb.trash().join("directorysizes");
    assert!(cache.is_file(), "directory put must write directorysizes");
    let body = fs::read_to_string(&cache).unwrap();
    // FreeDesktop: "size mtime percent-encoded-name"
    let line = body.lines().next().expect("at least one cache line");
    let parts: Vec<&str> = line.split_whitespace().collect();
    assert!(parts.len() >= 3, "expected size mtime name, got {line:?}");
    let size: u64 = parts[0].parse().expect("size");
    // The spec asks for `du -B1` usage: blocks, the directory's own included.
    use std::os::unix::fs::MetadataExt;
    let payload = sb.trash().join("files/tree");
    let blocks = |p: &std::path::Path| fs::symlink_metadata(p).unwrap().blocks() * 512;
    let want = blocks(&payload) + blocks(&payload.join("a")) + blocks(&payload.join("b"));
    assert_eq!(
        size, want,
        "block usage of the tree, not the 12 payload bytes"
    );
    assert_eq!(parts[parts.len() - 1], "tree");

    // File-only put must not invent a directorysizes line for that file name.
    sb.touch("solo.txt");
    assert!(sb.run(&["put", "solo.txt"]).status.success());
    let body2 = fs::read_to_string(&cache).unwrap();
    assert!(
        !body2.split_whitespace().any(|t| t == "solo.txt"),
        "file put must not add directorysizes for files: {body2}"
    );

    let out = sb.empty(&[]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        !cache.exists(),
        "full empty must prune/remove directorysizes"
    );
}

#[test]
fn completions_stdout_matches_embedded_markers() {
    let out = Command::new(bin())
        .args(["completions", "bash"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let s = stdout_of(&out);
    assert!(s.contains("complete -F _rtrash_main rtrash"));
    assert!(s.contains("--home-only"));

    let out = Command::new(bin())
        .args(["completions", "zsh"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(stdout_of(&out).contains("#compdef"));

    let out = Command::new(bin())
        .args(["completions", "fish"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let fish = stdout_of(&out);
    assert!(fish.contains("complete -c rtrash"));
    assert!(fish.contains("home-only"));
    assert!(fish.contains("dry-run"));
    assert!(fish.contains("status"));
}

#[test]
fn man_subcommand_prints_page() {
    let out = Command::new(bin()).args(["man"]).output().unwrap();
    assert!(out.status.success());
    let s = stdout_of(&out);
    assert!(s.contains(".TH RTRASH 1"));
    assert!(s.contains("setup"));
}

#[test]
fn setup_installs_under_prefix() {
    let prefix = std::env::temp_dir().join(format!(
        "rtrash-setup-cli-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = fs::remove_dir_all(&prefix);
    let prefix_s = prefix.display().to_string();
    let out = Command::new(bin())
        .args(["setup", &format!("--prefix={prefix_s}"), "-f", "-v"])
        .output()
        .unwrap();
    assert!(out.status.success(), "setup failed: {}", stderr_of(&out));
    let bash = prefix.join("share/bash-completion/completions/rtrash");
    let zsh = prefix.join("share/zsh/site-functions/_rtrash");
    let man = prefix.join("share/man/man1/rtrash.1");
    assert!(bash.is_file(), "missing {bash:?}");
    assert!(zsh.is_file(), "missing {zsh:?}");
    assert!(man.is_file(), "missing {man:?}");
    let fish = prefix.join("share/fish/vendor_completions.d/rtrash.fish");
    assert!(fish.is_file(), "missing {fish:?}");
    assert!(prefix.join("bin/trash-put").symlink_metadata().is_ok());
    assert!(fs::read_to_string(&bash).unwrap().contains("setup"));
    assert!(fs::read_to_string(&fish).unwrap().contains("dry-run"));
    // Fish only autoloads <command>.fish — multi-call names need their own files.
    for name in [
        "trash",
        "trash-put",
        "trash-empty",
        "trash-list",
        "trash-restore",
        "trash-rm",
    ] {
        let p = prefix.join(format!("share/fish/vendor_completions.d/{name}.fish"));
        assert!(
            p.symlink_metadata().is_ok() || p.is_file(),
            "setup must install fish multi-call completion {p:?}"
        );
        if p.symlink_metadata()
            .ok()
            .is_some_and(|m| m.file_type().is_symlink())
        {
            assert_eq!(
                fs::read_link(&p).unwrap().as_os_str(),
                "rtrash.fish",
                "fish multi-call link should point at shared rtrash.fish"
            );
        }
    }
    let _ = fs::remove_dir_all(&prefix);
}

#[test]
fn help_mentions_setup() {
    let out = Command::new(bin()).args(["--help"]).output().unwrap();
    assert!(out.status.success());
    let s = stdout_of(&out);
    assert!(s.contains("setup"));
    assert!(s.contains("completions"));
}

#[test]
fn status_uses_directorysizes_cache_when_valid() {
    let sb = Sandbox::new("status-dircache");
    let d = sb.work().join("cachedir");
    fs::create_dir(&d).unwrap();
    fs::write(d.join("inner"), vec![b'q'; 2048]).unwrap();
    assert!(sb.run(&["put", "-r", "cachedir"]).status.success());
    let info = sb.trash().join("info/cachedir.trashinfo");
    assert!(info.is_file());
    // Inject an unmistakable cached size with matching .trashinfo mtime.
    use std::os::unix::fs::MetadataExt;
    let mtime = fs::metadata(&info).unwrap().mtime();
    fs::write(
        sb.trash().join("directorysizes"),
        format!("777000000 {mtime} cachedir\n"),
    )
    .unwrap();
    let out = sb.run(&["status", "--home-only"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let s = stdout_of(&out);
    // 777000000 B ≈ 741.0 MiB via format_bytes — must appear when cache is used.
    assert!(
        s.contains("MiB") || s.contains("GiB") || s.contains("777"),
        "status must reflect huge directorysizes cache line: {s}"
    );
    assert!(!s.contains("Total: 0 items"), "{s}");
    // Corrupt mtime → fallback walk; total should shrink below the forged cache.
    fs::write(
        sb.trash().join("directorysizes"),
        format!("777000000 {} cachedir\n", mtime.saturating_sub(12345)),
    )
    .unwrap();
    let out2 = sb.run(&["status", "--home-only"]);
    assert!(out2.status.success(), "{}", stderr_of(&out2));
    let s2 = stdout_of(&out2);
    // Fallback walk of a 2 KiB file must not still report ~741 MiB.
    assert!(
        !s2.contains("741") && !s2.contains("740.") && !s2.contains("GiB"),
        "stale cache must not dominate status: {s2}"
    );
}

#[test]
fn status_writes_missing_directorysizes_back_and_drops_gone_lines() {
    let sb = Sandbox::new("status-writeback");
    let d = sb.work().join("walked");
    fs::create_dir(&d).unwrap();
    fs::write(d.join("inner"), vec![b'q'; 5000]).unwrap();
    assert!(sb.run(&["put", "-r", "walked"]).status.success());
    let cache = sb.trash().join("directorysizes");
    let first = fs::read_to_string(&cache).unwrap();
    // A trash written by another tool: no line for `walked`, and a line for
    // a directory that a third tool already removed.
    fs::write(&cache, "4096 1 long%20gone\n").unwrap();
    let out = sb.run(&["status", "--home-only"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let body = fs::read_to_string(&cache).unwrap();
    assert_eq!(
        body, first,
        "the walked size returns and the gone line leaves"
    );
}

#[test]
fn empty_dry_run_uses_directorysizes_for_dir_payload() {
    let sb = Sandbox::new("empty-dry-dircache");
    let d = sb.work().join("drytree");
    fs::create_dir(&d).unwrap();
    fs::write(d.join("blob"), vec![b'z'; 1024]).unwrap();
    assert!(sb.run(&["put", "-r", "drytree"]).status.success());
    use std::os::unix::fs::MetadataExt;
    let info = sb.trash().join("info/drytree.trashinfo");
    let mtime = fs::metadata(&info).unwrap().mtime();
    fs::write(
        sb.trash().join("directorysizes"),
        format!("555000000 {mtime} drytree\n"),
    )
    .unwrap();
    let out = sb.empty(&["--dry-run"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let err = stderr_of(&out);
    assert!(err.contains("Would remove"), "{err}");
    assert!(
        err.contains("MiB") || err.contains("GiB") || err.contains("555"),
        "dry-run reclaim must use directorysizes when valid: {err}"
    );
}

#[test]
fn put_trash_dir_pin_creates_layout_and_isolates() {
    let sb = Sandbox::new("put-pin");
    // Fresh pin path under sandbox (not yet a trash root).
    let pin_root = sb.root.join("custom-trash");
    let pin = format!("--trash-dir={}", pin_root.display());
    sb.touch("pinned.txt");
    let out = sb.run(&["put", &pin, "pinned.txt"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(pin_root.join("files/pinned.txt").is_file());
    assert!(pin_root.join("info/pinned.txt.trashinfo").is_file());
    // Home XDG trash in this sandbox must stay empty for this put.
    let home_files = sb.trash().join("files");
    let home_count = fs::read_dir(&home_files).map(|rd| rd.count()).unwrap_or(0);
    assert_eq!(home_count, 0, "home trash should be empty after pin put");
    let listed = sb.run(&["list", &pin]);
    assert!(listed.status.success(), "{}", stderr_of(&listed));
    assert!(
        stdout_of(&listed).contains("pinned.txt"),
        "{}",
        stdout_of(&listed)
    );
}

#[test]
fn empty_json_and_older_than_flag() {
    let sb = Sandbox::new("empty-json");
    let pin = format!("--trash-dir={}", sb.trash().display());
    sb.touch("e.txt");
    assert!(sb.run(&["put", "e.txt"]).status.success());
    let dry = sb.run(&["empty", &pin, "--dry-run", "--json"]);
    assert!(dry.status.success(), "{}", stderr_of(&dry));
    let s = stdout_of(&dry);
    assert!(
        s.contains("\"dry_run\":true") || s.contains("\"dry_run\": true"),
        "{s}"
    );
    assert!(
        s.contains("\"removed\":1") || s.contains("\"removed\": 1"),
        "{s}"
    );
    assert!(s.contains("\"bytes\":"), "{s}");
    // Still present after dry-run.
    assert!(sb.trash().join("files/e.txt").is_file());
    let real = sb.run(&["empty", &pin, "--json", "--older-than=0"]);
    assert!(real.status.success(), "{}", stderr_of(&real));
    let r = stdout_of(&real);
    assert!(
        r.contains("\"dry_run\":false") || r.contains("\"dry_run\": false"),
        "{r}"
    );
    assert!(r.contains("\"days\":0") || r.contains("\"days\": 0"), "{r}");
    assert!(!sb.trash().join("files/e.txt").exists());
}

#[test]
fn rm_json_and_age_filters() {
    let sb = Sandbox::new("rm-json");
    let pin = format!("--trash-dir={}", sb.trash().display());
    sb.touch("keep.c");
    sb.touch("drop.o");
    assert!(sb.run(&["put", "keep.c"]).status.success());
    assert!(sb.run(&["put", "drop.o"]).status.success());
    let j = sb.run(&["rm", &pin, "--json", "-n", "*.o"]);
    assert!(j.status.success(), "{}", stderr_of(&j));
    let s = stdout_of(&j);
    assert!(
        s.contains("\"removed\":1") || s.contains("\"removed\": 1"),
        "{s}"
    );
    assert!(s.contains("drop.o"), "{s}");
    assert!(!s.contains("keep.c"), "{s}");
    // dry-run keeps files
    assert!(sb.trash().join("files/drop.o").is_file());
    let gone = sb.run(&["rm", &pin, "--json", "*.o"]);
    assert!(gone.status.success(), "{}", stderr_of(&gone));
    assert!(!sb.trash().join("files/drop.o").exists());
    assert!(sb.trash().join("files/keep.c").is_file());
}

#[test]
fn list_json_and_age_filters() {
    let sb = Sandbox::new("list-json");
    let pin = format!("--trash-dir={}", sb.trash().display());
    sb.touch("new.txt");
    assert!(sb.run(&["put", "new.txt"]).status.success());
    // Write an old-looking trashinfo by putting then rewriting date.
    sb.touch("old.txt");
    assert!(sb.run(&["put", "old.txt"]).status.success());
    let info = sb.trash().join("info/old.txt.trashinfo");
    let body = fs::read_to_string(&info).unwrap();
    let body = body
        .lines()
        .map(|l| {
            if l.starts_with("DeletionDate=") {
                "DeletionDate=2001-01-01T00:00:00".to_string()
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    fs::write(&info, body).unwrap();

    let j = sb.run(&["list", &pin, "--json"]);
    assert!(j.status.success(), "{}", stderr_of(&j));
    let s = stdout_of(&j);
    assert!(s.trim_start().starts_with('['), "{s}");
    assert!(s.contains("\"original\""), "{s}");
    assert!(s.contains("new.txt") || s.contains("old.txt"), "{s}");

    let old_only = sb.run(&["list", &pin, "--older-than=30"]);
    assert!(old_only.status.success(), "{}", stderr_of(&old_only));
    let o = stdout_of(&old_only);
    assert!(o.contains("old.txt"), "expected old.txt in older-than: {o}");
    assert!(
        !o.contains("new.txt"),
        "new.txt should not match older-than=30: {o}"
    );

    let new_only = sb.run(&["list", &pin, "--newer-than=30"]);
    assert!(new_only.status.success(), "{}", stderr_of(&new_only));
    let n = stdout_of(&new_only);
    assert!(n.contains("new.txt"), "expected new.txt in newer-than: {n}");
    assert!(
        !n.contains("old.txt"),
        "old.txt should not match newer-than=30: {n}"
    );
}

#[test]
fn status_json_and_age_filter() {
    let sb = Sandbox::new("status-json");
    let pin = format!("--trash-dir={}", sb.trash().display());
    sb.touch("a.txt");
    assert!(sb.run(&["put", "a.txt"]).status.success());
    let out = sb.run(&["status", &pin, "--json"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let s = stdout_of(&out);
    assert!(s.contains("\"total_items\""), "{s}");
    assert!(s.contains("\"total_bytes\""), "{s}");
    assert!(s.contains("\"roots\""), "{s}");
    // recent item: older-than large window still counts; older-than=0 with future?
    // older-than=0 means cutoff=now, so items with t<=now are old (all real past items).
    let filtered = sb.run(&["status", &pin, "--json", "--newer-than=0"]);
    assert!(filtered.status.success(), "{}", stderr_of(&filtered));
    // With newer-than=0, cutoff=now, only t>now — typically 0 items for past dates.
    // Use newer-than=36500 (huge) so recent put is included.
    let recent = sb.run(&["status", &pin, "--json", "--newer-than=36500"]);
    assert!(recent.status.success(), "{}", stderr_of(&recent));
    let r = stdout_of(&recent);
    assert!(
        r.contains("\"total_items\": 1") || r.contains("\"total_items\":1"),
        "{r}"
    );
}

fn batch_of(sb: &Sandbox, name: &str) -> String {
    let body = fs::read_to_string(sb.trash().join(format!("info/{name}.trashinfo"))).unwrap();
    body.lines()
        .find_map(|l| l.strip_prefix("X-Rtrash-Batch="))
        .expect("put writes a batch key")
        .to_string()
}

#[test]
fn undo_restores_the_last_put_run_and_only_it() {
    let sb = Sandbox::new("undo");
    sb.touch("first.txt");
    assert!(sb.run(&["put", "first.txt"]).status.success());
    sb.touch("a.txt");
    fs::create_dir(sb.work().join("dir")).unwrap();
    sb.touch("dir/inner");
    assert!(sb.run(&["put", "-r", "a.txt", "dir"]).status.success());
    // An item from another tool carries no batch key and must stay put.
    fs::write(sb.trash().join("files/gio.txt"), b"g").unwrap();
    fs::write(
        sb.trash().join("info/gio.txt.trashinfo"),
        format!(
            "[Trash Info]\nPath={}\nDeletionDate=2099-01-01T00:00:00\n",
            sb.work().join("gio.txt").display()
        ),
    )
    .unwrap();
    let run2 = batch_of(&sb, "a.txt");
    assert_eq!(batch_of(&sb, "dir"), run2, "one run, one id");
    assert_ne!(batch_of(&sb, "first.txt"), run2);

    let listed = stdout_of(&sb.run(&["undo", "--list", "--home-only"]));
    let first_line = listed.lines().next().unwrap_or_default();
    assert!(first_line.starts_with(&run2), "{listed}");
    assert!(first_line.contains("2 items"), "{listed}");
    assert_eq!(listed.lines().count(), 2, "gio item is no run: {listed}");

    let json = stdout_of(&sb.run(&["list", "--json", "--home-only"]));
    assert!(json.contains(&format!("\"batch\":\"{run2}\"")), "{json}");
    assert!(json.contains("\"batch\":null"), "{json}");

    let dry = sb.run(&["undo", "-n", "--home-only"]);
    assert!(dry.status.success(), "{}", stderr_of(&dry));
    assert!(!sb.work().join("a.txt").exists(), "dry run changes nothing");

    let out = sb.run(&["undo", "--home-only", "--json"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let s = stdout_of(&out);
    assert!(s.starts_with(&format!("{{\"batch\":\"{run2}\"")), "{s}");
    assert!(s.contains("\"failed\":0"), "{s}");
    assert!(sb.work().join("a.txt").is_file());
    assert!(sb.work().join("dir/inner").is_file());
    assert!(!sb.work().join("first.txt").exists());
    assert!(sb.trash().join("files/gio.txt").exists());

    // The next undo takes the earlier run, by id here.
    let run1 = batch_of(&sb, "first.txt");
    let out = sb.run(&["undo", "--home-only", &run1]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(sb.work().join("first.txt").is_file());

    // Nothing rtrash trashed is left.
    let out = sb.run(&["undo", "--home-only"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr_of(&out).contains("no put run"),
        "{}",
        stderr_of(&out)
    );
}

#[test]
fn undo_keeps_a_recreated_path_without_force() {
    let sb = Sandbox::new("undo-conflict");
    sb.touch("x.txt");
    sb.touch("y.txt");
    assert!(sb.run(&["put", "x.txt", "y.txt"]).status.success());
    fs::write(sb.work().join("x.txt"), b"new").unwrap();
    let out = sb.run(&["undo", "--home-only"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr_of(&out).contains("1 restored, 1 failed"),
        "{}",
        stderr_of(&out)
    );
    assert_eq!(fs::read(sb.work().join("x.txt")).unwrap(), b"new");
    assert!(sb.work().join("y.txt").is_file());
    // With -f the rest of the run comes back over the new file.
    let out = sb.run(&["undo", "--home-only", "-f"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(fs::read(sb.work().join("x.txt")).unwrap(), b"payload");
}

/// Trash `name` (`len` bytes) dated `days` before 2020-01-28.
fn trash_aged(sb: &Sandbox, name: &str, len: usize, days: u32) {
    fs::write(sb.work().join(name), vec![b'x'; len]).unwrap();
    assert!(sb.run(&["put", name]).status.success());
    let info = sb.trash().join(format!("info/{name}.trashinfo"));
    let body = fs::read_to_string(&info).unwrap();
    assert!(days < 28);
    let day = 28 - days;
    let dated: String = body
        .lines()
        .map(|l| {
            if l.starts_with("DeletionDate=") {
                format!("DeletionDate=2020-01-{day:02}T00:00:00\n")
            } else {
                format!("{l}\n")
            }
        })
        .collect();
    fs::write(&info, dated).unwrap();
}

fn names_in_trash(sb: &Sandbox) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(sb.trash().join("files"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

#[test]
fn empty_max_size_removes_oldest_or_largest_until_it_fits() {
    let sb = Sandbox::new("maxsize");
    trash_aged(&sb, "old", 40_000, 20);
    trash_aged(&sb, "mid", 40_000, 10);
    trash_aged(&sb, "new", 40_000, 1);
    // Three items of about 44 KiB each (blocks plus the .trashinfo).
    let dry = sb.empty(&["--max-size=100K", "-n", "--json"]);
    assert!(dry.status.success(), "{}", stderr_of(&dry));
    let s = stdout_of(&dry);
    assert!(
        s.contains("\"removed\":1") && s.contains("\"max_size\":102400"),
        "{s}"
    );
    assert_eq!(names_in_trash(&sb).len(), 3, "dry run removes nothing");

    let out = sb.empty(&["--max-size=100K"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(names_in_trash(&sb), ["mid", "new"]);
    assert!(!sb.trash().join("info/old.trashinfo").exists());

    trash_aged(&sb, "huge", 200_000, 25);
    let out = sb.empty(&["--max-size=100K", "--prune=largest"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(names_in_trash(&sb), ["mid", "new"], "largest goes first");

    // A budget the trash already fits removes nothing, and never all.
    let out = sb.empty(&["--max-size=1G"]);
    assert!(out.status.success());
    assert_eq!(names_in_trash(&sb).len(), 2);
    let bad = sb.empty(&["--max-size=lots"]);
    assert_eq!(bad.status.code(), Some(2));
}

#[test]
fn put_prunes_to_rtrash_max_size_but_keeps_its_own_items() {
    let sb = Sandbox::new("putprune");
    trash_aged(&sb, "old1", 40_000, 20);
    trash_aged(&sb, "old2", 40_000, 10);
    fs::write(sb.work().join("fresh"), vec![b'y'; 150_000]).unwrap();
    let out = Command::new(bin())
        .args(["put", "fresh"])
        .current_dir(sb.work())
        .env("XDG_DATA_HOME", sb.root.join("xdg"))
        .env("HOME", &sb.root)
        .env("RTRASH_MAX_SIZE", "100K")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr_of(&out));
    // The fresh item alone exceeds the budget; everything older goes, it stays.
    assert_eq!(names_in_trash(&sb), ["fresh"]);
    assert!(
        stderr_of(&out).contains("removed 2 items"),
        "{}",
        stderr_of(&out)
    );
}

#[cfg(unix)]
#[test]
fn finder_list_and_restore_use_put_back_records() {
    use rtrash::ds_store::{write, Record, Value};
    let sb = Sandbox::new("finder");
    let trash = sb.root.join(".Trash");
    fs::create_dir(&trash).unwrap();
    // Finder renames a clashing item inside the trash and records the
    // original folder (volume-relative) and name.
    let folder = sb.work().strip_prefix("/").unwrap().display().to_string() + "/";
    let rec = |name: &str, code: &[u8; 4], s: &str| Record {
        name: name.into(),
        code: *code,
        value: Value::Ustr(s.into()),
    };
    fs::write(trash.join("note 12.00.01.txt"), b"one").unwrap();
    fs::create_dir(trash.join("dir")).unwrap();
    fs::write(trash.join("dir/inner"), b"in").unwrap();
    fs::write(trash.join("orphan"), b"?").unwrap();
    fs::write(
        trash.join(".DS_Store"),
        write(&[
            rec("note 12.00.01.txt", b"ptbL", &folder),
            rec("note 12.00.01.txt", b"ptbN", "note.txt"),
            rec("dir", b"ptbL", &format!("{folder}sub/")),
        ]),
    )
    .unwrap();

    // The default trash is ~/.Trash.
    let out = sb.run(&["finder", "list"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let s = stdout_of(&out);
    let note = sb.work().join("note.txt");
    assert!(
        s.contains(&format!("note 12.00.01.txt -> {}", note.display())),
        "{s}"
    );
    assert!(s.contains("orphan -> ?"), "{s}");
    assert!(!s.contains(".DS_Store"), "{s}");

    let json = stdout_of(&sb.run(&["finder", "list", "--json"]));
    assert!(
        json.contains("{\"name\":\"orphan\",\"original\":null}"),
        "{json}"
    );

    // Restore by original path; the clash policy applies as in restore.
    fs::write(&note, b"live").unwrap();
    let out = sb.run(&["finder", "restore", &note.display().to_string()]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr_of(&out).contains("refusing to overwrite"));
    let out = sb.run(&[
        "finder",
        "restore",
        "--conflict=rename",
        "--json",
        &note.display().to_string(),
    ]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(fs::read(sb.work().join("note.1.txt")).unwrap(), b"one");
    assert_eq!(fs::read(&note).unwrap(), b"live");

    // --all restores what has a record and leaves the orphan.
    let out = sb.run(&["finder", "restore", "--all"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(fs::read(sb.work().join("sub/dir/inner")).unwrap(), b"in");
    assert!(trash.join("orphan").exists());
    let out = sb.run(&["finder", "restore", "orphan"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr_of(&out).contains("no Put Back record"));

    #[cfg(not(target_os = "macos"))]
    {
        let out = sb.run(&["finder", "put", "x"]);
        assert_eq!(out.status.code(), Some(1));
        assert!(stderr_of(&out).contains("macOS only"));
    }
    assert_eq!(sb.run(&["finder"]).status.code(), Some(2));
}

/// Write `body` as the sandbox's config file.
fn write_config(sb: &Sandbox, body: &str) {
    let dir = sb.root.join(".config/rtrash");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("config.toml"), body).unwrap();
}

#[test]
fn config_file_sets_list_defaults_policy_and_protected_paths() {
    let sb = Sandbox::new("config");
    trash_aged(&sb, "zz", 40_000, 20);
    trash_aged(&sb, "aa", 40_000, 10);
    write_config(
        &sb,
        &format!(
            "[list]\nsort = \"name\"\n\n[policy]\nmax_size = \"100K\"\n\n[protect]\npaths = [\"{}/keep*\"]\n",
            sb.work().display()
        ),
    );
    // list: name order from the config; an option still wins.
    let text = stdout_of(&sb.run(&["list"]));
    assert!(
        text.find("/aa").unwrap() < text.find("/zz").unwrap(),
        "{text}"
    );
    let text = stdout_of(&sb.run(&["list", "--sort=date"]));
    assert!(
        text.find("/zz").unwrap() < text.find("/aa").unwrap(),
        "{text}"
    );

    // protect.paths refuses a match and lets others go.
    fs::create_dir(sb.work().join("keepme")).unwrap();
    let out = sb.run(&["put", "-r", "keepme"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr_of(&out).contains("config protect.paths entry"),
        "{}",
        stderr_of(&out)
    );
    assert!(sb.work().join("keepme").exists());

    // policy.max_size prunes after put, naming its source.
    fs::write(sb.work().join("fresh"), vec![b'y'; 150_000]).unwrap();
    let out = sb.run(&["put", "fresh"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(names_in_trash(&sb), ["fresh"]);
    assert!(
        stderr_of(&out).contains("policy.max_size in the config file"),
        "{}",
        stderr_of(&out)
    );
}

#[test]
fn environment_beats_the_config_file() {
    let sb = Sandbox::new("config-env");
    trash_aged(&sb, "old1", 40_000, 20);
    write_config(&sb, "[policy]\nmax_size = \"10K\"\n");
    fs::write(sb.work().join("fresh"), vec![b'y'; 50_000]).unwrap();
    // An empty RTRASH_MAX_SIZE turns the budget off.
    let out = Command::new(bin())
        .args(["put", "fresh"])
        .current_dir(sb.work())
        .env("XDG_DATA_HOME", sb.root.join("xdg"))
        .env("XDG_CONFIG_HOME", sb.root.join(".config"))
        .env("HOME", &sb.root)
        .env("RTRASH_MAX_SIZE", "")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(names_in_trash(&sb), ["fresh", "old1"]);

    // RTRASH_CONFIG names another file; empty turns the file off.
    let other = sb.root.join("other.toml");
    fs::write(&other, "[list]\nsort = \"bogus\"\n").unwrap();
    let run = |cfg: &Path, args: &[&str]| {
        Command::new(bin())
            .args(args)
            .current_dir(sb.work())
            .env("XDG_DATA_HOME", sb.root.join("xdg"))
            .env("HOME", &sb.root)
            .env("RTRASH_CONFIG", cfg)
            .output()
            .unwrap()
    };
    let out = run(&other, &["list"]);
    assert!(out.status.success());
    assert!(
        stderr_of(&out).contains("ignoring list.sort = 'bogus'"),
        "{}",
        stderr_of(&out)
    );
    let out = run(&other, &["config"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "a bad value parses; only lines that cannot be read fail"
    );
    assert!(
        stdout_of(&out).contains("sort = \"bogus\""),
        "{}",
        stdout_of(&out)
    );
    assert_eq!(
        stdout_of(&run(&other, &["config", "--path"])).trim(),
        other.display().to_string()
    );
    fs::write(&other, "[list]\ncolour = 1\n").unwrap();
    let out = run(&other, &["config"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr_of(&out).contains("unknown key 'list.colour'"),
        "{}",
        stderr_of(&out)
    );
    let out = run(Path::new(""), &["config", "--path"]);
    assert!(stdout_of(&out).contains("off"), "{}", stdout_of(&out));
    let out = run(Path::new(""), &["config", "--default"]);
    assert!(
        stdout_of(&out).contains("# max_size = \"50G\""),
        "{}",
        stdout_of(&out)
    );
    assert_eq!(
        run(Path::new(""), &["config", "--bogus"]).status.code(),
        Some(2)
    );
}
