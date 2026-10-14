//! Plain put, list, restore and empty on the Finder Trash, the macOS
//! default. `RTRASH_MACOS_TRASH=finder` picks it on any Unix, and a stand-in
//! for osascript moves each file into `~/.Trash` as Finder would; the test
//! then writes the Put Back record Finder keeps in `.DS_Store`.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use rtrash::ds_store::{self, Record, Value};

struct Box_ {
    root: PathBuf,
}

impl Box_ {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!("rtrash-fd-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("work")).unwrap();
        let fake = root.join("osascript");
        fs::write(
            &fake,
            "#!/bin/sh\nshift 2\nmkdir -p \"$HOME/.Trash\"\nfor p in \"$@\"; do mv \"$p\" \"$HOME/.Trash/\" || exit 1; done\n",
        )
        .unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
        Box_ { root }
    }

    fn work(&self) -> PathBuf {
        self.root.join("work")
    }

    fn finder(&self) -> PathBuf {
        self.root.join(".Trash")
    }

    fn run(&self, args: &[&str], stdin: &str) -> Output {
        use std::io::Write;
        let mut child = Command::new(env!("CARGO_BIN_EXE_rtrash"))
            .args(args)
            .current_dir(self.work())
            .env("HOME", &self.root)
            .env("XDG_DATA_HOME", self.root.join("xdg"))
            .env("XDG_CONFIG_HOME", self.root.join(".config"))
            .env("RTRASH_MACOS_TRASH", "finder")
            .env("RTRASH_OSASCRIPT", self.root.join("osascript"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    /// Record Put Back for `name` in the Finder Trash, as Finder does.
    fn put_back(&self, name: &str, original: &Path) {
        let folder = original.parent().unwrap().to_string_lossy();
        let rec = Record {
            name: name.into(),
            code: *b"ptbL",
            value: Value::Ustr(format!("{}/", folder.trim_start_matches('/'))),
        };
        fs::write(self.finder().join(".DS_Store"), ds_store::write(&[rec])).unwrap();
    }
}

impl Drop for Box_ {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn plain_commands_use_the_finder_trash() {
    let b = Box_::new("plain");
    let f = b.work().join("a b.txt");
    fs::write(&f, b"payload").unwrap();

    let o = b.run(&["put", "a b.txt"], "");
    assert!(o.status.success(), "{}", err(&o));
    assert!(!f.exists());
    assert!(b.finder().join("a b.txt").exists());
    // Nothing reaches the FreeDesktop trash.
    assert!(!b.root.join("xdg/Trash/files/a b.txt").exists());

    b.put_back("a b.txt", &f);
    let o = b.run(&["list"], "");
    assert!(
        out(&o).trim_end().ends_with(&*f.to_string_lossy()),
        "{}",
        out(&o)
    );
    let o = b.run(&["list", "--json"], "");
    assert!(out(&o).contains("\"name\":\"a b.txt\""), "{}", out(&o));
    // Ages count from the move into the trash, sizes from disk usage.
    let o = b.run(&["list", "--older-than=1d"], "");
    assert!(o.status.success() && out(&o).is_empty(), "{}", out(&o));
    let o = b.run(&["list", "--newer-than=1d"], "");
    assert!(out(&o).contains("a b.txt"), "{}", out(&o));
    let o = b.run(&["list", "--larger-than=1G"], "");
    assert!(out(&o).is_empty(), "{}", out(&o));

    let o = b.run(&["restore", "a b.txt"], "");
    assert!(o.status.success(), "{}", err(&o));
    assert_eq!(fs::read(&f).unwrap(), b"payload");

    // A bare restore offers a numbered table and reads the pick.
    assert!(b.run(&["put", "a b.txt"], "").status.success());
    b.put_back("a b.txt", &f);
    let o = b.run(&["restore", "--plain"], "0\n");
    assert!(o.status.success(), "{}{}", out(&o), err(&o));
    assert!(f.exists());

    // Empty removes the items and keeps .DS_Store.
    assert!(b.run(&["put", "a b.txt"], "").status.success());
    let o = b.run(&["empty", "--plain"], "");
    assert!(o.status.success(), "{}", err(&o));
    assert!(!b.finder().join("a b.txt").exists());
    assert!(b.finder().join(".DS_Store").exists());
}

#[test]
fn a_pin_or_the_setting_keeps_the_freedesktop_trash() {
    let b = Box_::new("pin");
    fs::write(b.work().join("x"), b"x").unwrap();
    let pin = format!("--trash-dir={}", b.root.join("xdg/Trash").display());
    let o = b.run(&["put", &pin, "x"], "");
    assert!(o.status.success(), "{}", err(&o));
    assert!(b.root.join("xdg/Trash/files/x").exists());
    assert!(!b.finder().join("x").exists());

    // macos.trash in the config file switches every command back.
    fs::create_dir_all(b.root.join(".config/rtrash")).unwrap();
    fs::write(
        b.root.join(".config/rtrash/config.toml"),
        "[macos]\ntrash = \"freedesktop\"\n",
    )
    .unwrap();
    let mut c = Command::new(env!("CARGO_BIN_EXE_rtrash"));
    let o = c
        .args(["list"])
        .env("HOME", &b.root)
        .env("XDG_DATA_HOME", b.root.join("xdg"))
        .env("XDG_CONFIG_HOME", b.root.join(".config"))
        .env_remove("RTRASH_MACOS_TRASH")
        .output()
        .unwrap();
    assert!(out(&o).contains("/x"), "{}{}", out(&o), err(&o));
}

#[test]
fn a_symlink_moves_in_as_itself() {
    let b = Box_::new("link");
    fs::write(b.work().join("target"), b"t").unwrap();
    std::os::unix::fs::symlink("target", b.work().join("ln")).unwrap();
    let o = b.run(&["put", "ln"], "");
    assert!(o.status.success(), "{}", err(&o));
    assert!(b
        .finder()
        .join("ln")
        .symlink_metadata()
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(b.work().join("target").exists());
}

#[test]
fn undo_status_and_trash_stats_cover_the_finder_trash() {
    let b = Box_::new("undo");
    for n in ["one", "two"] {
        fs::write(b.work().join(n), vec![b'x'; 5000]).unwrap();
    }
    fs::write(b.work().join("later"), b"l").unwrap();
    assert!(b.run(&["put", "one", "two"], "").status.success());
    assert!(b.run(&["put", "later"], "").status.success());

    let o = b.run(&["status", "--json"], "");
    assert!(o.status.success(), "{}", err(&o));
    assert!(out(&o).contains("\"total_items\": 3"), "{}", out(&o));
    assert!(out(&o).contains("\"finder\":true"), "{}", out(&o));
    let o = b.run(&["status"], "");
    assert!(out(&o).contains("Total: 3 items"), "{}", out(&o));
    let o = b.run(&["status", "--json", "--older-than=1d"], "");
    assert!(out(&o).contains("\"total_items\": 0"), "{}", out(&o));

    let o = b.run(&["trash-stats", "--json", "--top=1"], "");
    assert!(o.status.success(), "{}", err(&o));
    assert!(out(&o).contains(".Trash"), "{}", out(&o));
    assert!(out(&o).contains("\"items\":3"), "{}", out(&o));
    assert_eq!(
        b.run(&["trash-stats", "--total"], "").status.code(),
        Some(2)
    );

    // Two runs: undo --list shows both, undo brings back the newest.
    let o = b.run(&["undo", "--list"], "");
    assert_eq!(out(&o).lines().count(), 2, "{}", out(&o));
    let o = b.run(&["undo"], "");
    assert!(o.status.success(), "{}", err(&o));
    assert!(b.work().join("later").exists());
    assert!(!b.work().join("one").exists());
    let o = b.run(&["undo", "--json"], "");
    assert!(o.status.success(), "{}", err(&o));
    assert!(out(&o).contains("restored_to"), "{}", out(&o));
    assert!(b.work().join("one").exists() && b.work().join("two").exists());
    assert_eq!(b.run(&["undo"], "").status.code(), Some(1));
}

#[test]
fn volume_trashes_join_list_restore_rm_and_status() {
    use std::os::unix::fs::MetadataExt;
    let b = Box_::new("vol");
    let vol = b.root.join("vol");
    let uid = fs::metadata(&b.root).unwrap().uid();
    let vt = vol.join(".Trashes").join(uid.to_string());
    fs::create_dir_all(&vt).unwrap();
    fs::create_dir_all(vol.join("data")).unwrap();
    fs::write(vt.join("v.bin"), b"vol").unwrap();
    let rec = Record {
        name: "v.bin".into(),
        code: *b"ptbL",
        value: Value::Ustr("data/".into()),
    };
    fs::write(vt.join(".DS_Store"), ds_store::write(&[rec])).unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_rtrash"))
            .args(args)
            .current_dir(b.work())
            .env("HOME", &b.root)
            .env("XDG_CONFIG_HOME", b.root.join(".config"))
            .env("RTRASH_MACOS_TRASH", "finder")
            .env("RTRASH_FINDER_VOLUMES", &vol)
            .output()
            .unwrap()
    };
    let orig = vol.join("data/v.bin");
    let o = run(&["list"]);
    assert!(
        out(&o).contains(&*orig.to_string_lossy()),
        "{}{}",
        out(&o),
        err(&o)
    );
    let o = run(&["status", "--json"]);
    assert!(out(&o).contains(&*vt.to_string_lossy()), "{}", out(&o));
    let o = run(&["restore", &orig.to_string_lossy()]);
    assert!(o.status.success(), "{}", err(&o));
    assert_eq!(fs::read(&orig).unwrap(), b"vol");

    // Back into the volume trash, then a Finder rm takes it for good.
    fs::rename(&orig, vt.join("v.bin")).unwrap();
    let o = run(&["rm", "-n", "v.bin"]);
    assert!(out(&o).contains("v.bin") && vt.join("v.bin").exists());
    let o = run(&["rm", "v.bin"]);
    assert!(o.status.success(), "{}", err(&o));
    assert!(!vt.join("v.bin").exists());
    assert!(vt.join(".DS_Store").exists());
    let o = run(&["rm", "--older-than=1d", "*", "--force"]);
    assert!(o.status.success(), "{}", err(&o));
}

#[test]
fn restore_matchers_work_on_the_finder_trash() {
    let b = Box_::new("match");
    for n in ["r1.log", "r2.log", "keep.txt"] {
        fs::write(b.work().join(n), n).unwrap();
        assert!(b.run(&["put", n], "").status.success());
        std::thread::sleep(std::time::Duration::from_millis(1100));
    }
    // Finder writes one .DS_Store with every Put Back record.
    let recs: Vec<Record> = ["r1.log", "r2.log", "keep.txt"]
        .iter()
        .map(|n| Record {
            name: n.to_string(),
            code: *b"ptbL",
            value: Value::Ustr(format!(
                "{}/",
                b.work().to_string_lossy().trim_start_matches('/')
            )),
        })
        .collect();
    fs::write(b.finder().join(".DS_Store"), ds_store::write(&recs)).unwrap();

    // Two matches need --latest or --all.
    let o = b.run(&["restore", "--glob=*.log"], "");
    assert_eq!(o.status.code(), Some(1), "{}", err(&o));
    let o = b.run(&["restore", "--glob=*.log", "--latest"], "");
    assert!(o.status.success(), "{}", err(&o));
    assert!(b.work().join("r2.log").exists() && !b.work().join("r1.log").exists());

    let o = b.run(&["restore", "--regex=r1\\.log$"], "");
    assert!(o.status.success(), "{}", err(&o));
    assert!(b.work().join("r1.log").exists());

    let dest = b.root.join("elsewhere");
    fs::create_dir_all(&dest).unwrap();
    let to = format!("--to={}", dest.display());
    let o = b.run(&["restore", "--name=keep.txt", &to], "");
    assert!(o.status.success(), "{}", err(&o));
    assert_eq!(fs::read(dest.join("keep.txt")).unwrap(), b"keep.txt");
}
