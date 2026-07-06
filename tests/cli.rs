use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

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
            .env("HOME", &self.root)
            .output()
            .expect("failed to run rtrash")
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
    assert!(sb.run(&["put", "keep.dat", "drop.o", "also.o"]).status.success());
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
    assert!(expect_abs.is_file(), "restore must land at absolute original");
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
    assert!(
        parts.len() >= 3,
        "expected size mtime name, got {line:?}"
    );
    let size: u64 = parts[0].parse().expect("size");
    assert_eq!(size, 12, "4+8 payload bytes");
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
