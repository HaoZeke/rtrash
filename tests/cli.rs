use std::fs;
use std::path::{Path, PathBuf};
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
        let root = std::env::temp_dir().join(format!(
            "rtrash-test-{}-{tag}",
            std::process::id()
        ));
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
    assert!(stderr_of(&out).contains("Is a directory"), "{}", stderr_of(&out));
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
    assert!(sb.trash().join("files/link").symlink_metadata().unwrap().is_symlink());
}

#[test]
fn list_shows_original_path() {
    let sb = Sandbox::new("list");
    let f = sb.touch("seen.txt");
    sb.run(&["put", "seen.txt"]);
    let out = sb.run(&["list"]);
    assert!(out.status.success());
    assert!(stdout_of(&out).contains(&f.display().to_string()), "{}", stdout_of(&out));
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

    let out = sb.run(&["empty"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(trash_names(&sb).is_empty());
    assert!(fs::read_dir(sb.trash().join("info")).unwrap().next().is_none());
}

#[test]
fn empty_days_keeps_recent_items() {
    let sb = Sandbox::new("empty-days");
    sb.touch("fresh.txt");
    sb.run(&["put", "fresh.txt"]);

    let out = sb.run(&["empty", "5"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(trash_names(&sb), vec!["fresh.txt"], "recent item must survive");

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

    let out = sb.run(&["empty", "5"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(trash_names(&sb).is_empty());
}

#[test]
fn empty_dry_run_removes_nothing() {
    let sb = Sandbox::new("empty-dry");
    sb.touch("keep.txt");
    sb.run(&["put", "keep.txt"]);
    let out = sb.run(&["empty", "--dry-run"]);
    assert!(out.status.success());
    assert_eq!(trash_names(&sb), vec!["keep.txt"]);
    assert!(stderr_of(&out).contains("Would remove 1 item"));
}

#[test]
fn empty_purges_orphaned_files() {
    let sb = Sandbox::new("empty-orphan");
    sb.touch("x");
    sb.run(&["put", "x"]);
    fs::remove_file(sb.trash().join("info/x.trashinfo")).unwrap();
    let out = sb.run(&["empty"]);
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
fn multicall_names_dispatch() {
    let sb = Sandbox::new("multicall");
    let bindir = sb.root.join("bin");
    fs::create_dir_all(&bindir).unwrap();
    for name in ["rm", "trash-put", "trash-empty", "trash-list", "trash-restore"] {
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

    assert!(run_as("trash-empty", &[]).status.success());
    assert!(trash_names(&sb).is_empty());
}
