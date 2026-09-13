use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
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
            .env("XDG_CACHE_HOME", self.root.join("xdg-cache"))
            .env("RTRASH_LIST_TSV", "0")
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
            .env("XDG_CACHE_HOME", self.root.join("xdg-cache"))
            .env("RTRASH_LIST_TSV", "0")
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

fn du_blocks(path: &std::path::Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    let meta = fs::symlink_metadata(path).unwrap();
    let mut total = meta.blocks() as u64 * 512;
    if meta.is_dir() {
        if let Ok(rd) = fs::read_dir(path) {
            for e in rd.flatten() {
                total = total.saturating_add(du_blocks(&e.path()));
            }
        }
    }
    total
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
fn directorysizes_atomic_rewrite_survives_put() {
    let sb = Sandbox::new("dirsizes-atomic");
    fs::create_dir(sb.work().join("d")).unwrap();
    fs::write(sb.work().join("d/x"), b"payload-bytes").unwrap();
    assert!(sb.run(&["put", "-r", "d"]).status.success());
    let pin = format!("--trash-dir={}", sb.trash().display());
    assert!(sb.run(&["metafix", &pin, "--sizes", "-f"]).status.success());
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
    assert!(s.contains("1 items") || s.contains("1 item"), "{s}");
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
fn list_json_marks_orphan_without_invented_path() {
    let sb = Sandbox::new("list-orphan");
    let pin = format!("--trash-dir={}", sb.trash().display());
    let original = sb.touch("gone.txt");
    assert!(sb.run(&["put", "gone.txt"]).status.success());
    fs::remove_file(sb.trash().join("info/gone.txt.trashinfo")).unwrap();
    assert!(sb.trash().join("files/gone.txt").is_file());

    let j = sb.run(&["list", &pin, "--json"]);
    assert!(j.status.success(), "{}", stderr_of(&j));
    let s = stdout_of(&j);
    assert!(s.contains("\"orphan\":true"), "{s}");
    assert!(s.contains("\"original\":null"), "{s}");
    assert!(s.contains("\"name\":\"gone.txt\""), "{s}");
    assert!(
        !s.contains(&original.display().to_string()),
        "must not invent Path=: {s}"
    );

    let text = sb.run(&["list", &pin]);
    assert!(text.status.success(), "{}", stderr_of(&text));
    let t = stdout_of(&text);
    assert!(t.contains("ORPHAN"), "{t}");
    assert!(t.contains("gone.txt"), "{t}");
    assert!(t.contains("????-??-??"), "{t}");
    assert!(
        !t.contains(&original.display().to_string()),
        "must not invent Path=: {t}"
    );

    let only = sb.run(&["list", &pin, "--orphans"]);
    assert!(only.status.success(), "{}", stderr_of(&only));
    let o = stdout_of(&only);
    assert!(o.contains("ORPHAN"), "{o}");
    assert!(o.contains("gone.txt"), "{o}");
}

#[test]
fn restore_refuses_orphan() {
    let sb = Sandbox::new("restore-orphan");
    sb.touch("noinfo.txt");
    assert!(sb.run(&["put", "noinfo.txt"]).status.success());
    fs::remove_file(sb.trash().join("info/noinfo.txt.trashinfo")).unwrap();
    assert!(sb.trash().join("files/noinfo.txt").is_file());

    let out = sb.run(&["restore", "noinfo.txt"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    let err = stderr_of(&out);
    assert!(
        err.contains("no trashed files match") || err.contains("cannot restore orphan"),
        "{err}"
    );
    assert!(
        sb.trash().join("files/noinfo.txt").is_file(),
        "orphan payload must stay"
    );
    assert!(!sb.work().join("noinfo.txt").exists());
}

#[test]
fn empty_age_filter_purges_orphans() {
    let sb = Sandbox::new("empty-days-orphan");
    sb.touch("fresh.txt");
    assert!(sb.run(&["put", "fresh.txt"]).status.success());
    sb.touch("orphan.txt");
    assert!(sb.run(&["put", "orphan.txt"]).status.success());
    fs::remove_file(sb.trash().join("info/orphan.txt.trashinfo")).unwrap();

    let out = sb.empty(&["5"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let names = trash_names(&sb);
    assert_eq!(
        names,
        vec!["fresh.txt"],
        "recent dated item must survive: {names:?}"
    );
    assert!(
        !sb.trash().join("files/orphan.txt").exists(),
        "orphan must be purged on age-filter empty"
    );
}

#[test]
fn restore_if_unique_rejects_colliding_originals() {
    let sb = Sandbox::new("restore-if-unique");
    let unique = sb.touch("one.txt");
    assert!(sb.run(&["put", "one.txt"]).status.success());
    let out = Command::new(bin())
        .args(["restore", "--if-unique", "one.txt"])
        .current_dir(sb.work())
        .env("XDG_DATA_HOME", sb.root.join("xdg"))
        .env("HOME", &sb.root)
        .stdin(Stdio::null())
        .output()
        .expect("restore --if-unique unique");
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(unique.exists());

    let dup = sb.touch("dup.txt");
    assert!(sb.run(&["put", "dup.txt"]).status.success());
    fs::write(&dup, b"second").unwrap();
    assert!(sb.run(&["put", "dup.txt"]).status.success());
    assert_eq!(trash_names(&sb).len(), 2);

    // Closed stdin: must exit 1 without a numbered pick or hang.
    let out = Command::new(bin())
        .args(["restore", "--if-unique", "dup.txt"])
        .current_dir(sb.work())
        .env("XDG_DATA_HOME", sb.root.join("xdg"))
        .env("HOME", &sb.root)
        .stdin(Stdio::null())
        .output()
        .expect("restore --if-unique collide");
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    let err = stderr_of(&out);
    assert!(err.contains("--if-unique requires exactly one"), "{err}");
    assert!(!err.contains("What file to restore"), "{err}");
    assert_eq!(trash_names(&sb).len(), 2);
    assert!(!dup.exists());
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
fn restore_group_after_put_abc() {
    let sb = Sandbox::new("restore-group");
    let a = sb.touch("a.txt");
    let b = sb.touch("b.txt");
    let c = sb.touch("c.txt");
    assert!(sb.run(&["put", "a.txt", "b.txt", "c.txt"]).status.success());
    assert!(!a.exists() && !b.exists() && !c.exists());
    set_deletion_date(&sb, "a.txt", "2020-01-01T00:00:00");
    set_deletion_date(&sb, "b.txt", "2020-01-01T00:00:00");
    set_deletion_date(&sb, "c.txt", "2020-01-01T00:00:00");

    let out = sb.run(&["restore", "--group"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(a.exists() && b.exists() && c.exists());
    assert!(trash_names(&sb).is_empty());

    // A later put (different DeletionDate second) stays when --group
    // restores the newest batch only.
    assert!(sb.run(&["put", "a.txt", "b.txt", "c.txt"]).status.success());
    let later = sb.touch("later.txt");
    assert!(sb.run(&["put", "later.txt"]).status.success());
    set_deletion_date(&sb, "later.txt", "2099-01-01T00:00:00");
    let out = sb.run(&["restore", "--group"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(later.exists(), "newest group (later.txt) restored");
    assert!(!a.exists() && !b.exists() && !c.exists(), "older put stays");

    // One PATH: group is that PATH's second, not the newest in scope.
    set_deletion_date(&sb, "a.txt", "2001-01-01T00:00:00");
    set_deletion_date(&sb, "b.txt", "2001-01-01T00:00:00");
    set_deletion_date(&sb, "c.txt", "2001-01-01T00:00:00");
    let out = sb.run(&["restore", "--group", "a.txt"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(a.exists() && b.exists() && c.exists());
}

#[test]
fn restore_to_relocates_basename_and_overwrite() {
    let sb = Sandbox::new("restore-to");
    sb.touch("dest.txt");
    assert!(sb.run(&["put", "dest.txt"]).status.success());
    let recovered = sb.work().join("recovered");
    fs::create_dir(&recovered).unwrap();

    let dest = recovered.to_str().unwrap();
    let out = sb.run(&["restore", "--restore-to", dest, "dest.txt"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(!sb.work().join("dest.txt").exists());
    assert_eq!(fs::read(recovered.join("dest.txt")).unwrap(), b"payload");
    assert!(trash_names(&sb).is_empty());

    sb.touch("dest.txt");
    assert!(sb.run(&["put", "dest.txt"]).status.success());
    fs::write(recovered.join("dest.txt"), b"newer").unwrap();
    let out = sb.run(&["restore", "--restore-to", dest, "dest.txt"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    assert!(stderr_of(&out).contains("refusing to overwrite"));
    assert_eq!(fs::read(recovered.join("dest.txt")).unwrap(), b"newer");

    let out = sb.run(&["restore", "--overwrite", "--restore-to", dest, "dest.txt"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(fs::read(recovered.join("dest.txt")).unwrap(), b"payload");
}

#[test]
fn restore_plain_range_indexes() {
    let sb = Sandbox::new("restore-plain-range");
    let a = sb.touch("a.txt");
    let b = sb.touch("b.txt");
    let c = sb.touch("c.txt");
    assert!(sb.run(&["put", "a.txt", "b.txt", "c.txt"]).status.success());

    let out = sb.run_with_stdin(&["restore", "--plain"], "0-1\n");
    assert!(out.status.success(), "{}", stderr_of(&out));
    let restored = a.exists() as u8 + b.exists() as u8 + c.exists() as u8;
    assert_eq!(restored, 2, "0-1 restores two of three");
    assert_eq!(trash_names(&sb).len(), 1);
}

#[test]
fn restore_all_and_multi_path() {
    let sb = Sandbox::new("restore-all-multi");
    let a = sb.touch("a.txt");
    let b = sb.touch("b.txt");
    let c = sb.touch("c.txt");
    assert!(sb.run(&["put", "a.txt", "b.txt", "c.txt"]).status.success());

    let out = sb.run(&["restore", "a.txt", "c.txt"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(a.exists() && c.exists());
    assert!(!b.exists());

    let out = sb.run(&["restore", "--all"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(b.exists());
    assert!(trash_names(&sb).is_empty());
}

fn set_deletion_date(sb: &Sandbox, trash_name: &str, date: &str) {
    let info = sb.trash().join(format!("info/{trash_name}.trashinfo"));
    let body = fs::read_to_string(&info).unwrap();
    let rewritten: String = body
        .lines()
        .map(|line| {
            if line.starts_with("DeletionDate=") {
                format!("DeletionDate={date}")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    fs::write(&info, rewritten).unwrap();
    // In-place trashinfo rewrite does not bump info/ mtime; drop the sealed cache.
    let _ = fs::remove_dir_all(sb.root.join("xdg-cache/rtrash"));
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
    let pin = format!("--trash-dir={}", sb.trash().display());
    assert!(sb.run(&["metafix", &pin, "--sizes", "-f"]).status.success());

    let cache = sb.trash().join("directorysizes");
    assert!(cache.is_file(), "metafix --sizes must write directorysizes");
    let body = fs::read_to_string(&cache).unwrap();
    // FreeDesktop: "size mtime percent-encoded-name"
    let line = body.lines().next().expect("at least one cache line");
    let parts: Vec<&str> = line.split_whitespace().collect();
    assert!(parts.len() >= 3, "expected size mtime name, got {line:?}");
    let size: u64 = parts[0].parse().expect("size");
    let payload = sb.trash().join("files/tree");
    let want = du_blocks(&payload);
    assert_eq!(size, want, "directorysizes must be du -B1, not logical 12");
    assert!(size >= 12, "allocation is at least the 12 payload bytes");
    assert_eq!(parts[parts.len() - 1], "tree");

    // File-only put must not invent a directorysizes line for that file name.
    sb.touch("solo.txt");
    assert!(sb.run(&["put", "solo.txt"]).status.success());
    assert!(sb.run(&["metafix", &pin, "--sizes", "-f"]).status.success());
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
fn desktop_help_names_the_hud() {
    let out = Command::new(bin())
        .args(["desktop", "--help"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr_of(&out));
    let s = stdout_of(&out);
    assert!(s.contains("rtrash-hud"), "{s}");
    assert!(s.contains("icedtea"), "{s}");
    assert!(s.contains("--offline"), "{s}");
}

#[test]
fn desktop_hide_without_hud_exits_0() {
    let out = Command::new(bin())
        .args(["desktop", "--hide"])
        .env("RTRASH_HUD_BIN", "/nonexistent/rtrash-hud")
        .env("PATH", "/nonexistent")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr_of(&out));
}

#[test]
fn desktop_missing_hud_exits_127() {
    let out = Command::new(bin())
        .args(["desktop", "--offline"])
        .env("RTRASH_HUD_BIN", "/nonexistent/rtrash-hud")
        .env("PATH", "/nonexistent")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(127), "{}", stderr_of(&out));
    let err = stderr_of(&out);
    assert!(err.contains("rtrash-hud"), "{err}");
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
    // Match the formatted size, not a substring of the sandbox path (PIDs).
    assert!(
        !s2.contains("741.0") && !s2.contains("740.") && !s2.contains(" GiB"),
        "stale cache must not dominate status: {s2}"
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
fn trash_dir_space_form_matches_equals() {
    let sb = Sandbox::new("pin-space");
    let pin_root = sb.root.join("space-trash");
    sb.touch("spaced.txt");
    let out = sb.run(&[
        "put",
        "--trash-dir",
        pin_root.to_str().unwrap(),
        "spaced.txt",
    ]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(pin_root.join("files/spaced.txt").is_file());
    let listed = sb.run(&["list", "--trash-dir", pin_root.to_str().unwrap()]);
    assert!(listed.status.success(), "{}", stderr_of(&listed));
    assert!(
        stdout_of(&listed).contains("spaced.txt"),
        "{}",
        stdout_of(&listed)
    );
    let missing = sb.run(&["list", "--trash-dir"]);
    assert_eq!(missing.status.code(), Some(2), "{}", stderr_of(&missing));
    assert!(
        stderr_of(&missing).contains("--trash-dir requires PATH"),
        "{}",
        stderr_of(&missing)
    );
}

#[test]
fn put_json_and_files_from_nul() {
    let sb = Sandbox::new("put-json");
    let pin = format!("--trash-dir={}", sb.trash().display());
    sb.touch("a.txt");
    sb.touch("b.txt");
    let out = sb.run(&["put", "--json", "--plain", &pin, "a.txt"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let s = stdout_of(&out);
    assert!(s.contains("\"ok\":1") || s.contains("\"ok\": 1"), "{s}");
    assert!(s.contains("a.txt"), "{s}");
    let list = format!("{}\0", sb.work().join("b.txt").display());
    let from = sb.root.join("from0");
    fs::write(&from, list.as_bytes()).unwrap();
    let nul = sb.run(&[
        "put",
        "--json",
        "--plain",
        &pin,
        "--files-from",
        from.to_str().unwrap(),
        "-0",
    ]);
    assert!(nul.status.success(), "{}", stderr_of(&nul));
    let n = stdout_of(&nul);
    assert!(n.contains("\"ok\":1") || n.contains("\"ok\": 1"), "{n}");
    assert!(sb.trash().join("files/b.txt").is_file());
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
    let _ = fs::remove_dir_all(sb.root.join("xdg-cache/rtrash"));

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
fn list_pattern_glob_literal_cwd_only() {
    let sb = Sandbox::new("list-query");
    let pin = format!("--trash-dir={}", sb.trash().display());
    sb.touch("keep.c");
    sb.touch("drop.o");
    let outer = sb.root.join("outside.txt");
    fs::write(&outer, b"outer").unwrap();
    assert!(sb.run(&["put", "keep.c"]).status.success());
    assert!(sb.run(&["put", "drop.o"]).status.success());
    assert!(Command::new(bin())
        .args(["put", outer.to_str().unwrap()])
        .current_dir(sb.work())
        .env("XDG_DATA_HOME", sb.root.join("xdg"))
        .env("HOME", &sb.root)
        .status()
        .unwrap()
        .success());
    let catalog = sb.run(&["list", &pin]);
    assert!(catalog.status.success(), "{}", stderr_of(&catalog));
    let catalog_s = stdout_of(&catalog);
    assert!(
        catalog_s
            .lines()
            .any(|line| line.contains("keep.c") && line.contains(' ') && !line.contains('T')),
        "no-pattern list stays trash-cli date path: {catalog_s}"
    );
    assert!(catalog_s.contains("outside.txt"), "{catalog_s}");
    let glob = sb.run(&["list", &pin, "*.o"]);
    assert!(glob.status.success(), "{}", stderr_of(&glob));
    let g = stdout_of(&glob);
    assert!(g.contains("drop.o"), "{g}");
    assert!(!g.contains("keep.c"), "{g}");
    assert!(!g.contains("outside.txt"), "{g}");
    let lit = sb.run(&["list", &pin, "--match", "literal", "keep.c"]);
    assert!(lit.status.success(), "{}", stderr_of(&lit));
    let l = stdout_of(&lit);
    assert!(l.contains("keep.c"), "{l}");
    assert!(!l.contains("drop.o"), "{l}");
    let lit_sub = sb.run(&["list", &pin, "--match=literal", "keep"]);
    assert!(lit_sub.status.success(), "{}", stderr_of(&lit_sub));
    assert!(
        stdout_of(&lit_sub).trim().is_empty(),
        "literal is exact, not substring: {}",
        stdout_of(&lit_sub)
    );
    let re = sb.run(&["list", &pin, "--match=regex", r"keep\.c$"]);
    assert!(re.status.success(), "{}", stderr_of(&re));
    assert!(stdout_of(&re).contains("keep.c"), "{}", stdout_of(&re));
    let json = sb.run(&["list", &pin, "--json", "*.c"]);
    assert!(json.status.success(), "{}", stderr_of(&json));
    let j = stdout_of(&json);
    assert!(j.contains("keep.c"), "{j}");
    assert!(!j.contains("drop.o"), "{j}");
    assert!(j.contains("\"original\""), "{j}");
    let cwd = sb.run(&["list", &pin, "--cwd-only"]);
    assert!(cwd.status.success(), "{}", stderr_of(&cwd));
    let c = stdout_of(&cwd);
    assert!(c.contains("keep.c"), "{c}");
    assert!(c.contains("drop.o"), "{c}");
    assert!(
        !c.contains("outside.txt"),
        "cwd-only must drop items outside cwd: {c}"
    );
    let bad_mode = sb.run(&["list", &pin, "--match=globstar"]);
    assert_eq!(bad_mode.status.code(), Some(2), "{}", stderr_of(&bad_mode));
    let bad_re = sb.run(&["list", &pin, "--match=regex", "("]);
    assert_eq!(bad_re.status.code(), Some(2), "{}", stderr_of(&bad_re));
}

#[test]
fn list_show_size_sort_last() {
    let sb = Sandbox::new("list-show-size");
    let pin = format!("--trash-dir={}", sb.trash().display());
    fs::write(sb.work().join("tiny.bin"), b"x").unwrap();
    fs::write(sb.work().join("huge.bin"), vec![b'H'; 32 * 1024]).unwrap();
    assert!(sb.run(&["put", "tiny.bin"]).status.success());
    assert!(sb.run(&["put", "huge.bin"]).status.success());

    let catalog = sb.run(&["list", &pin]);
    assert!(catalog.status.success(), "{}", stderr_of(&catalog));
    let catalog_s = stdout_of(&catalog);
    assert!(
        catalog_s.contains("tiny.bin") && catalog_s.contains("huge.bin"),
        "unflagged catalog lists both: {catalog_s}"
    );
    for line in catalog_s.lines().filter(|l| !l.trim().is_empty()) {
        assert!(
            !line.contains(" KiB") && !line.contains(" MiB") && !line.contains(" B "),
            "unflagged line stays DATE PATH: {line}"
        );
    }

    let shown = sb.run(&["list", &pin, "--show-size"]);
    assert!(shown.status.success(), "{}", stderr_of(&shown));
    let shown_s = stdout_of(&shown);
    assert!(
        shown_s.contains("tiny.bin") && shown_s.contains("huge.bin"),
        "show-size lists both: {shown_s}"
    );
    assert!(
        shown_s.contains(" B") || shown_s.contains("KiB") || shown_s.contains("MiB"),
        "show-size prints a size field: {shown_s}"
    );
    assert_ne!(shown_s, catalog_s, "show-size must change the line format");

    let auto = sb.run(&["list", &pin, "--larger-than=0"]);
    assert!(auto.status.success(), "{}", stderr_of(&auto));
    let auto_s = stdout_of(&auto);
    assert!(
        auto_s.contains(" B") || auto_s.contains("KiB") || auto_s.contains("MiB"),
        "size filter auto-enables show-size: {auto_s}"
    );

    let last = sb.run(&["list", &pin, "--sort=size", "--last=1"]);
    assert!(last.status.success(), "{}", stderr_of(&last));
    let last_s = stdout_of(&last);
    assert!(
        last_s.contains("huge.bin"),
        "sort=size --last=1 keeps the larger: {last_s}"
    );
    assert!(
        !last_s.contains("tiny.bin"),
        "sort=size --last=1 drops the smaller: {last_s}"
    );
    assert!(
        !last_s.contains("KiB") && !last_s.contains(" B "),
        "sort/last without --show-size stays DATE PATH: {last_s}"
    );

    let nflag = sb.run(&["list", &pin, "--sort", "size", "-n", "1"]);
    assert!(nflag.status.success(), "{}", stderr_of(&nflag));
    assert!(
        stdout_of(&nflag).contains("huge.bin") && !stdout_of(&nflag).contains("tiny.bin"),
        "-n 1 after size sort keeps larger: {}",
        stdout_of(&nflag)
    );

    let rev = sb.run(&["list", &pin, "--sort=size", "--reverse", "--last=1"]);
    assert!(rev.status.success(), "{}", stderr_of(&rev));
    assert!(
        stdout_of(&rev).contains("tiny.bin") && !stdout_of(&rev).contains("huge.bin"),
        "reverse size last=1 keeps smaller: {}",
        stdout_of(&rev)
    );

    let bad_sort = sb.run(&["list", &pin, "--sort=name"]);
    assert_eq!(bad_sort.status.code(), Some(2), "{}", stderr_of(&bad_sort));
    let bad_last = sb.run(&["list", &pin, "--last=nope"]);
    assert_eq!(bad_last.status.code(), Some(2), "{}", stderr_of(&bad_last));
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

fn json_field<'a>(obj: &'a str, key: &str) -> Option<&'a str> {
    let pat = format!("\"{key}\":");
    let rest = obj.split_once(&pat)?.1.trim_start();
    if let Some(s) = rest.strip_prefix('"') {
        return s.split_once('"').map(|(v, _)| v);
    }
    let end = rest
        .find(|c: char| c == ',' || c == '}' || c.is_whitespace())
        .unwrap_or(rest.len());
    Some(&rest[..end])
}

fn parse_list_json_sizes(s: &str) -> Vec<(String, u64)> {
    let mut out = Vec::new();
    for chunk in s.split('{').skip(1) {
        let Some(name) = json_field(chunk, "name") else {
            continue;
        };
        let Some(b) = json_field(chunk, "bytes") else {
            continue;
        };
        if let Ok(n) = b.parse::<u64>() {
            out.push((name.to_string(), n));
        }
    }
    out
}

#[test]
fn empty_max_bytes_leaves_tiny_set_and_list_size_filters() {
    let sb = Sandbox::new("empty-max-bytes");
    let pin = format!("--trash-dir={}", sb.trash().display());
    fs::write(sb.work().join("big.bin"), vec![b'B'; 20 * 1024]).unwrap();
    assert!(sb.run(&["put", "big.bin"]).status.success());
    for i in 0..8 {
        let name = format!("tiny{i}.txt");
        fs::write(sb.work().join(&name), b"x").unwrap();
        assert!(sb.run(&["put", &name]).status.success());
    }

    let sized = sb.run(&["list", &pin, "--json", "--smaller-than=1G"]);
    assert!(sized.status.success(), "{}", stderr_of(&sized));
    let catalog = stdout_of(&sized);
    assert!(
        catalog.contains("\"bytes\":"),
        "size filter JSON includes bytes: {catalog}"
    );
    let rows = parse_list_json_sizes(&catalog);
    assert_eq!(rows.len(), 9, "expected big + 8 tinies: {catalog}");
    let big_sz = rows
        .iter()
        .find(|(n, _)| n == "big.bin")
        .map(|(_, b)| *b)
        .expect("big.bin in catalog");
    let tinies: Vec<&(String, u64)> = rows.iter().filter(|(n, _)| n.starts_with("tiny")).collect();
    assert_eq!(tinies.len(), 8, "{catalog}");
    let tiny_sum: u64 = tinies.iter().map(|(_, b)| *b).sum();
    let max_tiny = tinies.iter().map(|(_, b)| *b).max().unwrap();
    assert!(
        big_sz > max_tiny,
        "big.bin ({big_sz}) must outrank tinies ({max_tiny}) so largest-first drops it"
    );

    // Size filters imply --show-size; the line is still not JSON.
    let lines = sb.run(&["list", &pin, "--larger-than=0"]);
    assert!(lines.status.success(), "{}", stderr_of(&lines));
    let line_out = stdout_of(&lines);
    assert!(
        line_out.contains("big.bin"),
        "default list line still shows path: {line_out}"
    );
    assert!(
        line_out.contains("KiB") || line_out.contains(" B"),
        "size filter auto-enables the size field: {line_out}"
    );
    assert!(
        !line_out.contains("\"bytes\""),
        "text list is not JSON: {line_out}"
    );

    let split = max_tiny + (big_sz - max_tiny) / 2;
    let split_s = split.to_string();
    let large = sb.run(&["list", &pin, "--json", "--larger-than", &split_s]);
    assert!(large.status.success(), "{}", stderr_of(&large));
    let l = stdout_of(&large);
    assert!(
        l.contains("big.bin"),
        "larger-than should keep big.bin: {l}"
    );
    assert!(!l.contains("tiny"), "larger-than should drop tinies: {l}");
    assert!(l.contains("\"bytes\":"), "{l}");

    let small = sb.run(&["list", &pin, "--json", "--smaller-than", &split_s]);
    assert!(small.status.success(), "{}", stderr_of(&small));
    let sm = stdout_of(&small);
    assert!(
        sm.contains("tiny0.txt"),
        "smaller-than should keep tinies: {sm}"
    );
    assert!(
        !sm.contains("big.bin"),
        "smaller-than should drop big.bin: {sm}"
    );

    let cap = tiny_sum.to_string();
    let dry = sb.run(&[
        "empty",
        &pin,
        "--dry-run",
        "--json",
        &format!("--max-bytes={cap}"),
    ]);
    assert!(dry.status.success(), "{}", stderr_of(&dry));
    let dj = stdout_of(&dry);
    assert!(
        dj.contains("\"removed\":1") || dj.contains("\"removed\": 1"),
        "dry-run should drop only the largest: {dj}"
    );
    assert!(
        dj.contains(&format!("\"max_bytes\":{cap}"))
            || dj.contains(&format!("\"max_bytes\": {cap}")),
        "{dj}"
    );
    assert!(
        sb.trash().join("files/big.bin").is_file(),
        "dry-run keeps payload"
    );

    let real = sb.run(&["empty", &pin, "--json", &format!("--max-bytes={cap}")]);
    assert!(real.status.success(), "{}", stderr_of(&real));
    let rj = stdout_of(&real);
    assert!(
        rj.contains("\"removed\":1") || rj.contains("\"removed\": 1"),
        "json removed count must match one large victim: {rj}"
    );
    assert!(
        !sb.trash().join("files/big.bin").exists(),
        "largest item dropped"
    );
    assert!(!sb.trash().join("info/big.bin.trashinfo").exists());
    for i in 0..8 {
        assert!(
            sb.trash().join(format!("files/tiny{i}.txt")).is_file(),
            "tiny{i}.txt must remain"
        );
    }

    let bad = sb.run(&["empty", &pin, "--max-bytes=nope"]);
    assert_eq!(bad.status.code(), Some(2), "{}", stderr_of(&bad));
    let bad_list = sb.run(&["list", &pin, "--larger-than=nope"]);
    assert_eq!(bad_list.status.code(), Some(2), "{}", stderr_of(&bad_list));
}

#[test]
fn restore_group_all_and_restore_to() {
    let sb = Sandbox::new("restore-group-to");
    let pin = format!("--trash-dir={}", sb.trash().display());
    sb.touch("a.txt");
    sb.touch("b.txt");
    sb.touch("c.txt");
    assert!(sb
        .run(&["put", &pin, "a.txt", "b.txt", "c.txt"])
        .status
        .success());
    let out = sb.run(&["restore", &pin, "--plain", "--group"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(sb.work().join("a.txt").is_file());
    assert!(sb.work().join("b.txt").is_file());
    assert!(sb.work().join("c.txt").is_file());

    sb.touch("d.txt");
    assert!(sb.run(&["put", &pin, "d.txt"]).status.success());
    let dest = sb.root.join("out");
    fs::create_dir_all(&dest).unwrap();
    let to = sb.run(&[
        "restore",
        &pin,
        "--plain",
        "--restore-to",
        dest.to_str().unwrap(),
        "d.txt",
    ]);
    assert!(to.status.success(), "{}", stderr_of(&to));
    assert!(dest.join("d.txt").is_file());
}

#[test]
fn restore_plain_index_range() {
    let sb = Sandbox::new("restore-range");
    let pin = format!("--trash-dir={}", sb.trash().display());
    sb.touch("x1");
    sb.touch("x2");
    sb.touch("x3");
    assert!(sb.run(&["put", &pin, "x1", "x2", "x3"]).status.success());
    let out = sb.run_with_stdin(&["restore", &pin, "--plain"], "0-1\n");
    assert!(out.status.success(), "{}", stderr_of(&out));
    let back = [
        sb.work().join("x1").exists(),
        sb.work().join("x2").exists(),
        sb.work().join("x3").exists(),
    ]
    .iter()
    .filter(|b| **b)
    .count();
    assert_eq!(back, 2, "range 0-1 should restore two of three");
}

#[test]
fn catalog_cache_blob_is_sealed_after_list() {
    let sb = Sandbox::new("catalog-cache-seal");
    sb.touch("hello.txt");
    assert!(sb.run(&["put", "hello.txt"]).status.success());
    let pin = format!("--trash-dir={}", sb.trash().display());
    let first = sb.run(&["list", &pin]);
    assert!(first.status.success(), "{}", stderr_of(&first));
    let out = stdout_of(&first);
    assert!(out.contains("hello.txt"), "{out}");
    let cache_root = sb.root.join("xdg-cache/rtrash");
    let mut blobs = Vec::new();
    if let Ok(rd) = fs::read_dir(&cache_root) {
        for e in rd.flatten() {
            if e.path().extension().and_then(|s| s.to_str()) == Some("bin") {
                blobs.push(e.path());
            }
        }
    }
    assert_eq!(
        blobs.len(),
        1,
        "expected one sealed cache blob under {cache_root:?}"
    );
    let raw = fs::read(&blobs[0]).unwrap();
    assert!(raw.starts_with(b"RTR1"), "magic");
    let text = String::from_utf8_lossy(&raw);
    assert!(!text.contains("hello.txt"), "blob must not be a Path= dump");
    let key = sb.root.join("xdg/rtrash/cache.key");
    assert!(key.is_file(), "cache key under XDG_DATA_HOME");
    let mode = fs::metadata(&key).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let second = sb.run(&["list", &pin]);
    assert!(second.status.success());
    assert_eq!(stdout_of(&second), out);
    let st = sb.run(&["status", &pin]);
    assert!(st.status.success(), "{}", stderr_of(&st));
    assert!(
        stdout_of(&st).contains("1 items") || stdout_of(&st).contains("1 item"),
        "{}",
        stdout_of(&st)
    );
}

#[test]
fn catalog_cache_survives_put_then_list_without_rescan_miss() {
    let sb = Sandbox::new("catalog-cache-put");
    sb.touch("a.txt");
    sb.touch("b.txt");
    let pin = format!("--trash-dir={}", sb.trash().display());
    assert!(sb.run(&["put", "a.txt"]).status.success());
    assert!(sb.run(&["put", "b.txt"]).status.success());
    let listed = stdout_of(&sb.run(&["list", &pin]));
    assert!(
        listed.contains("a.txt") && listed.contains("b.txt"),
        "{listed}"
    );
    // Restore a; cache must drop that row so the next list is still correct.
    assert!(sb
        .run(&["restore", "--plain", "--if-unique", "a.txt"])
        .status
        .success());
    let after = stdout_of(&sb.run(&["list", &pin]));
    assert!(!after.contains("a.txt"), "{after}");
    assert!(after.contains("b.txt"), "{after}");
}

#[test]
fn put_batch_shares_one_deletion_date() {
    let sb = Sandbox::new("put-one-date");
    sb.touch("x");
    sb.touch("y");
    sb.touch("z");
    assert!(sb.run(&["put", "x", "y", "z"]).status.success());
    let mut dates = Vec::new();
    for name in ["x", "y", "z"] {
        let body = fs::read_to_string(sb.trash().join(format!("info/{name}.trashinfo"))).unwrap();
        let line = body
            .lines()
            .find(|l| l.starts_with("DeletionDate="))
            .unwrap();
        dates.push(line.to_string());
        let mode = fs::metadata(sb.trash().join(format!("info/{name}.trashinfo")))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "{name}");
    }
    assert_eq!(dates[0], dates[1]);
    assert_eq!(dates[1], dates[2]);
}

#[test]
fn trash_put_trashes_directory_without_dash_r() {
    let sb = Sandbox::new("trash-put-dir");
    let bindir = sb.root.join("bin");
    fs::create_dir_all(&bindir).unwrap();
    std::os::unix::fs::symlink(bin(), bindir.join("trash-put")).unwrap();
    fs::create_dir_all(sb.work().join("tree")).unwrap();
    fs::write(sb.work().join("tree/f"), b"x").unwrap();
    let out = Command::new(bindir.join("trash-put"))
        .arg("tree")
        .current_dir(sb.work())
        .env("XDG_DATA_HOME", sb.root.join("xdg"))
        .env("HOME", &sb.root)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(!sb.work().join("tree").exists());
    fs::create_dir_all(sb.work().join("tree2")).unwrap();
    let fail = sb.run(&["put", "tree2"]);
    assert!(!fail.status.success(), "rtrash put dir still needs -r");
}

#[test]
fn list_directory_filter_and_tsv() {
    let sb = Sandbox::new("list-dir-tsv");
    fs::create_dir_all(sb.work().join("keep")).unwrap();
    fs::write(sb.work().join("keep/a.txt"), b"a").unwrap();
    fs::write(sb.work().join("skip.txt"), b"s").unwrap();
    assert!(sb.run(&["put", "keep/a.txt", "skip.txt"]).status.success());
    let pin = format!("--trash-dir={}", sb.trash().display());
    let keep = sb.work().join("keep");
    let dirflag = format!("--directory={}", keep.display());
    let out = stdout_of(&sb.run(&["list", &pin, &dirflag]));
    assert!(out.contains("a.txt"), "{out}");
    assert!(!out.contains("skip.txt"), "{out}");
    let tsv = Command::new(bin())
        .args(["list", &pin])
        .current_dir(sb.work())
        .env("XDG_DATA_HOME", sb.root.join("xdg"))
        .env("XDG_CACHE_HOME", sb.root.join("xdg-cache"))
        .env("HOME", &sb.root)
        .env("RTRASH_LIST_TSV", "1")
        .output()
        .unwrap();
    let s = stdout_of(&tsv);
    assert!(s.contains('\t'), "expected TSV: {s}");
}

#[test]
fn put_verbose_and_json_name_dest() {
    let sb = Sandbox::new("put-dest");
    sb.touch("d.txt");
    let pin = format!("--trash-dir={}", sb.trash().display());
    let v = sb.run(&["put", "-v", &pin, "d.txt"]);
    assert!(v.status.success(), "{}", stderr_of(&v));
    let vs = stdout_of(&v);
    assert!(vs.contains("d.txt"), "{vs}");
    assert!(
        vs.contains("Trash") || vs.contains(&sb.trash().display().to_string()),
        "{vs}"
    );
    sb.touch("e.txt");
    let j = stdout_of(&sb.run(&["put", "--json", &pin, "e.txt"]));
    assert!(j.contains("\"dests\""), "{j}");
    assert!(j.contains("Trash") || j.contains("xdg"), "{j}");
}

#[test]
fn dangling_info_is_emergency_and_metafix_removes_it() {
    let sb = Sandbox::new("metafix");
    sb.touch("gone.txt");
    assert!(sb.run(&["put", "gone.txt"]).status.success());
    fs::remove_file(sb.trash().join("files/gone.txt")).unwrap();
    let pin = format!("--trash-dir={}", sb.trash().display());
    let listed = stdout_of(&sb.run(&["list", &pin]));
    assert!(
        listed.contains("DANGLING") || listed.contains("gone.txt"),
        "{listed}"
    );
    let mf = sb.run(&["metafix", &pin, "-f"]);
    assert!(mf.status.success(), "{}", stderr_of(&mf));
    assert!(!sb.trash().join("info/gone.txt.trashinfo").exists());
}

#[test]
fn directory_put_defers_directorysizes_until_metafix_sizes() {
    let sb = Sandbox::new("defer-dirsizes");
    fs::create_dir(sb.work().join("tree")).unwrap();
    fs::write(sb.work().join("tree/a"), b"aaaa").unwrap();
    fs::write(sb.work().join("tree/b"), b"bbbbbbbb").unwrap();
    assert!(sb.run(&["put", "-r", "tree"]).status.success());
    assert!(sb.trash().join("files/tree").is_dir());

    let cache = sb.trash().join("directorysizes");
    if cache.is_file() {
        let body = fs::read_to_string(&cache).unwrap();
        assert!(
            !body.split_whitespace().any(|t| t == "tree"),
            "put must not walk the payload to write directorysizes: {body}"
        );
    }

    let pin = format!("--trash-dir={}", sb.trash().display());
    let mf = sb.run(&["metafix", &pin, "--sizes", "-f"]);
    assert!(mf.status.success(), "{}", stderr_of(&mf));
    assert!(cache.is_file(), "metafix --sizes must write directorysizes");
    let body = fs::read_to_string(&cache).unwrap();
    let line = body
        .lines()
        .find(|l| l.split_whitespace().any(|t| t == "tree"))
        .expect("cache line for tree");
    let size: u64 = line.split_whitespace().next().unwrap().parse().unwrap();
    let want = du_blocks(&sb.trash().join("files/tree"));
    assert_eq!(size, want, "directorysizes must be du -B1");

    let out = sb.empty(&[]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        !cache.exists(),
        "full empty must prune/remove directorysizes"
    );
}

#[test]
fn scan_lists_artifact_dirs_skips_zotero_and_git() {
    let sb = Sandbox::new("scan-art");
    let work = sb.work();
    fs::create_dir_all(work.join("keep")).unwrap();
    fs::write(work.join("keep/file.txt"), b"keep").unwrap();
    fs::create_dir_all(work.join("proj/target/debug")).unwrap();
    fs::write(work.join("proj/Cargo.toml"), b"[package]\nname=\"p\"\n").unwrap();
    fs::write(
        work.join("proj/target/CACHEDIR.TAG"),
        b"Signature: 8a477f597d28d172789f06886806bc55\n",
    )
    .unwrap();
    fs::write(work.join("proj/target/debug/foo"), b"obj").unwrap();
    fs::create_dir_all(work.join("Zotero/storage/AB12")).unwrap();
    fs::write(work.join("Zotero/storage/AB12/x.pdf"), b"%PDF").unwrap();
    fs::create_dir_all(work.join("env/.venv/lib")).unwrap();
    fs::write(work.join("env/.venv/lib/x"), b"py").unwrap();
    fs::create_dir_all(work.join("js/node_modules/pkg")).unwrap();
    fs::write(work.join("js/node_modules/pkg/index.js"), b"1").unwrap();
    fs::create_dir_all(work.join("cache")).unwrap();
    fs::write(
        work.join("cache/CACHEDIR.TAG"),
        b"Signature: 8a477f597d28d172789f06886806bc55\n",
    )
    .unwrap();
    fs::create_dir_all(work.join(".git/objects/ab")).unwrap();
    fs::write(work.join(".git/objects/ab/cd"), b"blob").unwrap();

    let out = sb.run(&["scan", work.to_str().unwrap()]);
    assert!(out.status.success(), "scan: {}", stderr_of(&out));
    let s = stdout_of(&out);
    assert!(s.contains("proj/target"), "cargo target: {s}");
    assert!(s.contains("node_modules"), "node_modules: {s}");
    assert!(s.contains(".venv"), ".venv: {s}");
    assert!(s.contains("cache"), "CACHEDIR.TAG parent: {s}");
    assert!(!s.contains("Zotero"), "must not enter Zotero: {s}");
    assert!(!s.contains("keep/file"), "must not list regular files: {s}");
    assert!(!s.contains(".git/objects"), "must prune .git: {s}");

    let help = sb.run(&["scan", "--help"]);
    assert!(help.status.success(), "{}", stderr_of(&help));
    let h = stdout_of(&help);
    assert!(h.contains("--xdev") || h.contains("xdev"), "{h}");
}
