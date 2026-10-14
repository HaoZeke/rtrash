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
    // Each put stored a total and a history line, read without a walk.
    let o = b.run(&["trash-stats", "--total", "--json"], "");
    assert!(o.status.success(), "{}", err(&o));
    assert!(
        out(&o).contains("\"items\":3,\"stale\":false"),
        "{}",
        out(&o)
    );
    let o = b.run(&["trash-stats", "--history", "--json"], "");
    assert!(o.status.success(), "{}", err(&o));
    assert_eq!(
        out(&o).matches("\"event\":\"put\"").count(),
        2,
        "{}",
        out(&o)
    );
    // Finder rewriting its .DS_Store leaves the total current.
    fs::write(b.finder().join(".DS_Store"), b"Bud1").unwrap();
    let o = b.run(&["trash-stats", "--total", "--json"], "");
    assert!(
        out(&o).contains("\"items\":3,\"stale\":false"),
        "{}",
        out(&o)
    );
    fs::remove_file(b.finder().join(".DS_Store")).unwrap();
    // A change rtrash did not make leaves the total stale until --measure.
    std::thread::sleep(std::time::Duration::from_millis(20));
    fs::write(b.finder().join("by-finder"), b"f").unwrap();
    let o = b.run(&["trash-stats", "--total", "--json"], "");
    assert!(out(&o).contains("\"stale\":true"), "{}", out(&o));
    let o = b.run(&["trash-stats", "--total", "--measure", "--json"], "");
    assert!(
        out(&o).contains("\"items\":4,\"stale\":false"),
        "{}",
        out(&o)
    );
    fs::remove_file(b.finder().join("by-finder")).unwrap();

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
    let o = b.run(&["trash-stats", "--history", "--json"], "");
    assert!(out(&o).contains("\"event\":\"undo\""), "{}", out(&o));
    let o = b.run(&["trash-stats", "--total", "--json"], "");
    assert!(
        out(&o).contains("\"items\":0,\"stale\":false"),
        "{}",
        out(&o)
    );
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

#[test]
fn empty_max_size_trims_the_finder_trash() {
    let b = Box_::new("maxsize");
    for (n, len) in [("big", 400_000), ("mid", 60_000), ("small", 10)] {
        fs::write(b.work().join(n), vec![b'x'; len]).unwrap();
    }
    assert!(b.run(&["put", "big", "mid", "small"], "").status.success());
    let o = b.run(
        &[
            "empty",
            "--max-size=200K",
            "--prune=largest",
            "-n",
            "--json",
        ],
        "",
    );
    assert!(o.status.success(), "{}", err(&o));
    assert!(out(&o).contains("\"removed\":1"), "{}", out(&o));
    assert!(b.finder().join("big").exists());
    let o = b.run(
        &["empty", "--max-size=200K", "--prune=largest", "--json"],
        "",
    );
    assert!(o.status.success(), "{}", err(&o));
    assert!(!b.finder().join("big").exists());
    assert!(b.finder().join("mid").exists() && b.finder().join("small").exists());
    // A budget the trash already fits removes nothing.
    let o = b.run(&["empty", "--max-size=1G", "--json"], "");
    assert!(out(&o).contains("\"removed\":0"), "{}", out(&o));
    assert_eq!(b.run(&["empty", "--dangling"], "").status.code(), Some(2));
}

/// Put and rm carry a current total forward instead of walking the trash:
/// a total doctored before them stays off by the same amount.
#[test]
fn put_and_rm_carry_the_finder_total() {
    let b = Box_::new("carry");
    fs::write(b.work().join("first"), b"1").unwrap();
    assert!(b.run(&["put", "first"], "").status.success());
    let key = b
        .finder()
        .to_string_lossy()
        .replace('%', "%25")
        .replace('/', "%2F");
    let total = b
        .root
        .join(format!("state/rtrash/finder-totals/{key}.total"));
    let body = fs::read_to_string(&total).unwrap();
    let f: Vec<&str> = body.split_whitespace().collect();
    assert_eq!((f[0], f[2], f[3]), ("v2", "1", "1"), "{body}");
    fs::write(&total, format!("v2 1000000000 1 1 {}\n", f[4])).unwrap();
    // A directory of three files: four inodes.
    fs::create_dir(b.work().join("second")).unwrap();
    for n in ["a", "b", "c"] {
        fs::write(b.work().join("second").join(n), vec![b'x'; 3000]).unwrap();
    }
    assert!(b.run(&["put", "-r", "second"], "").status.success());
    let o = b.run(&["trash-stats", "--total", "--json"], "");
    let j = out(&o);
    assert!(j.contains("\"items\":2,\"stale\":false"), "{j}");
    let bytes = |j: &str| -> u64 {
        let i = j.find("\"bytes\":").unwrap() + 8;
        j[i..].split(',').next().unwrap().parse().unwrap()
    };
    assert!(bytes(&j) > 1_000_000_000, "{j}");
    assert!(j.contains("\"inodes\":5}"), "{j}");
    assert!(b.run(&["rm", "second", "--force"], "").status.success());
    let j = out(&b.run(&["trash-stats", "--total", "--json"], ""));
    assert!(j.contains("\"items\":1,\"stale\":false"), "{j}");
    assert_eq!(bytes(&j), 1_000_000_000, "{j}");
    assert!(j.contains("\"inodes\":1}"), "{j}");
    // A restore carries the item's size out as well.
    let to = format!("--to={}", b.work().display());
    let o = b.run(&["restore", "--name=first", &to], "");
    assert!(o.status.success(), "{}", err(&o));
    let j = out(&b.run(&["trash-stats", "--total", "--json"], ""));
    assert!(j.contains("\"items\":0,\"stale\":false"), "{j}");
    assert!(j.contains("\"inodes\":0}"), "{j}");
    assert!(bytes(&j) > 999_000_000, "{j}");
    // --measure finds the trash current and keeps the carried figure;
    // a change from outside makes it stale, and --measure then fixes it.
    fs::write(b.finder().join("outside"), b"o").unwrap();
    let j = out(&b.run(&["trash-stats", "--total", "--measure", "--json"], ""));
    assert!(j.contains("\"items\":1,\"stale\":false"), "{j}");
    assert!(bytes(&j) < 1_000_000, "{j}");
}

/// The files of a trash gone for over 90 days go at the next logged
/// change; recent ones and live trashes stay.
#[test]
fn stale_finder_totals_of_gone_volumes_are_pruned() {
    let b = Box_::new("prune");
    let dir = b.root.join("state/rtrash/finder-totals");
    fs::create_dir_all(&dir).unwrap();
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(100 * 86_400);
    for (n, t) in [
        ("uuid-gone-.Trashes%2F501.total", old),
        ("uuid-gone-.Trashes%2F501.history", old),
        ("uuid-recent-x.total", std::time::SystemTime::now()),
        ("notes.txt", old),
    ] {
        let p = dir.join(n);
        fs::write(&p, b"v1 1 1 1\n").unwrap();
        fs::File::options()
            .write(true)
            .open(&p)
            .unwrap()
            .set_modified(t)
            .unwrap();
    }
    fs::write(b.work().join("f"), b"1").unwrap();
    assert!(b.run(&["put", "f"], "").status.success());
    assert!(!dir.join("uuid-gone-.Trashes%2F501.total").exists());
    assert!(!dir.join("uuid-gone-.Trashes%2F501.history").exists());
    assert!(dir.join("uuid-recent-x.total").exists());
    assert!(dir.join("notes.txt").exists());
    let home = fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .any(|e| e.file_name().to_string_lossy().ends_with(".Trash.total"));
    assert!(home, "the live home total stays");
}

/// `reap --purge-trash` empties the Finder Trash on a short file system,
/// since a put there frees nothing, and carries the Finder total.
#[test]
fn reap_purge_trash_empties_the_finder_trash() {
    let b = Box_::new("reap-purge");
    let tmp = b.root.join("tmp");
    fs::create_dir_all(&tmp).unwrap();
    let gone = b.work().join("gone.txt");
    fs::write(&gone, "g").unwrap();
    let o = b.run(&["put", gone.to_str().unwrap()], "");
    assert!(o.status.success(), "{}", err(&o));
    assert!(b.finder().join("gone.txt").exists());
    let tmp_arg = tmp.to_str().unwrap();
    let args = [
        "reap",
        "--when-below=100%",
        "--until=100%",
        "--purge-trash",
        "--older-than=1h",
        "--age-by=m",
    ];
    let mut dry: Vec<&str> = args.to_vec();
    dry.extend(["-n", tmp_arg]);
    let o = b.run(&dry, "");
    assert!(o.status.success(), "{}", err(&o));
    assert!(
        err(&o).contains("would empty 1 item") && err(&o).contains("Finder Trash"),
        "{}",
        err(&o)
    );
    assert!(
        b.finder().join("gone.txt").exists(),
        "the dry run emptied it"
    );
    let mut real: Vec<&str> = args.to_vec();
    real.push(tmp_arg);
    let o = b.run(&real, "");
    assert!(o.status.success(), "{}", err(&o));
    assert!(err(&o).contains("emptied 1 item"), "{}", err(&o));
    assert!(!b.finder().join("gone.txt").exists());
    let o = b.run(&["trash-stats", "--total", "--json"], "");
    assert!(out(&o).contains(r#""items":0"#), "{}", out(&o));
}

/// `empty --until-free` takes the oldest Finder items on a short file
/// system first.
#[test]
fn empty_until_free_takes_finder_items() {
    let b = Box_::new("until-free");
    let p = b.work().join("gone.txt");
    fs::write(&p, "g").unwrap();
    assert!(b.run(&["put", p.to_str().unwrap()], "").status.success());
    let fs_spec = format!("{}:{}", 1u64 << 40, b.root.display());
    let run = |args: &[&str], spec: &str| {
        Command::new(env!("CARGO_BIN_EXE_rtrash"))
            .args(args)
            .env("HOME", &b.root)
            .env("XDG_DATA_HOME", b.root.join("xdg"))
            .env("XDG_CONFIG_HOME", b.root.join(".config"))
            .env("XDG_STATE_HOME", b.root.join("state"))
            .env("RTRASH_MACOS_TRASH", "finder")
            .env("RTRASH_TEST_FS", spec)
            .output()
            .unwrap()
    };
    // A 1 TiB fake disk asked for all but one byte free: short.
    let want = format!("--until-free={}", (1u64 << 40) - 1);
    let o = run(&["empty", "--home-only", &want], &fs_spec);
    assert!(
        err(&o).contains("emptied 1 item") && err(&o).contains("Finder Trash"),
        "{}",
        err(&o)
    );
    assert!(!b.finder().join("gone.txt").exists());
}

/// A trash budget trims the Finder Trash after a put, oldest first, and
/// never takes what that put moved; a put on a short disk warns.
#[test]
fn a_budget_trims_the_finder_trash_after_put_and_low_space_warns() {
    let b = Box_::new("budget");
    let fs_total = std::cell::Cell::new(1u64 << 40);
    let put = |name: &str, budget: Option<&str>| {
        let fs_total = fs_total.get();
        let p = b.work().join(name);
        fs::write(&p, "z".repeat(20_000)).unwrap();
        let mut c = Command::new(env!("CARGO_BIN_EXE_rtrash"));
        c.arg("put")
            .arg(&p)
            .current_dir(b.work())
            .env("HOME", &b.root)
            .env("XDG_DATA_HOME", b.root.join("xdg"))
            .env("XDG_CONFIG_HOME", b.root.join(".config"))
            .env("XDG_STATE_HOME", b.root.join("state"))
            .env("RTRASH_MACOS_TRASH", "finder")
            .env("RTRASH_OSASCRIPT", b.root.join("osascript"))
            .env(
                "RTRASH_TEST_FS",
                format!("{}:{}", fs_total, b.root.display()),
            )
            .env_remove("RTRASH_PRUNE");
        match budget {
            Some(v) => c.env("RTRASH_MAX_SIZE", v),
            None => c.env_remove("RTRASH_MAX_SIZE"),
        };
        c.output().unwrap()
    };
    assert!(put("old.bin", None).status.success());
    assert!(put("mid.bin", Some("1G")).status.success());
    assert!(
        b.finder().join("old.bin").exists(),
        "a roomy budget trimmed"
    );
    // A 1-byte budget: everything but this put's item goes.
    let o = put("new.bin", Some("1"));
    assert!(o.status.success(), "{}", err(&o));
    assert!(err(&o).contains("removed 2 items from"), "{}", err(&o));
    assert!(b.finder().join("new.bin").exists());
    assert!(!b.finder().join("old.bin").exists() && !b.finder().join("mid.bin").exists());
    let o = b.run(&["trash-stats", "--total", "--json"], "");
    assert!(out(&o).contains(r#""items":1"#), "{}", out(&o));
    assert!(!err(&o).contains("is low"));
    // On a nearly full disk the put warns that it freed nothing.
    fs_total.set(45_000);
    let o = put("last.bin", None);
    assert!(o.status.success(), "{}", err(&o));
    assert!(err(&o).contains("is low ("), "{}", err(&o));
}
