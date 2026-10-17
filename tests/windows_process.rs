//! Process-aware commands on Windows: `who`, `put --in-use` and `reap`
//! ask the Restart Manager which processes have a file open.
#![cfg(windows)]

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::{Duration, SystemTime};

fn sandbox(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("rtrash-win-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    root
}

fn run(home: &Path, args: &[&str]) -> Output {
    common::rtrash_real(common::Real::RecycleBin)
        .args(args)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("APPDATA", home.join("AppData").join("Roaming"))
        .env("LOCALAPPDATA", home.join("AppData").join("Local"))
        .env("TEMP", home.join("tmp"))
        .env("TMP", home.join("tmp"))
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("RTRASH_CONFIG")
        .env_remove("RTRASH_IN_USE")
        .output()
        .unwrap()
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn stale(path: &Path) {
    let day_ago = SystemTime::now() - Duration::from_secs(86_400);
    let f = fs::File::options().write(true).open(path).unwrap();
    f.set_times(
        fs::FileTimes::new()
            .set_accessed(day_ago)
            .set_modified(day_ago),
    )
    .unwrap();
}

#[test]
fn who_names_the_process_holding_a_file_open() {
    let root = sandbox("who");
    let held = root.join("held.txt");
    fs::write(&held, b"x").unwrap();
    let _open = fs::File::open(&held).unwrap();
    let out = run(&root, &["who", "--json", held.to_str().unwrap()]);
    assert!(out.status.success(), "{}", text(&out));
    let pid = std::process::id().to_string();
    assert!(
        text(&out).contains(&pid),
        "pid {pid} missing: {}",
        text(&out)
    );

    let free = root.join("free.txt");
    fs::write(&free, b"y").unwrap();
    let out = run(&root, &["who", free.to_str().unwrap()]);
    assert!(!text(&out).contains(&pid), "{}", text(&out));
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn put_in_use_refuse_keeps_an_open_file() {
    let root = sandbox("put");
    let held = root.join("held.txt");
    fs::write(&held, b"x").unwrap();
    let open = fs::File::open(&held).unwrap();
    let out = run(&root, &["put", "--in-use=refuse", held.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(text(&out).contains("in use by"), "{}", text(&out));
    assert!(held.exists());
    drop(open);
    let out = run(&root, &["put", "--in-use=bogus", held.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn reap_skips_open_files_and_takes_stale_ones() {
    let root = sandbox("reap");
    let dir = root.join("scratch");
    fs::create_dir_all(&dir).unwrap();
    let old = dir.join("old.txt");
    let held = dir.join("held.txt");
    fs::write(&old, b"o").unwrap();
    fs::write(&held, b"h").unwrap();
    stale(&old);
    stale(&held);
    let open = fs::File::open(&held).unwrap();
    let d = dir.to_str().unwrap();

    let out = run(
        &root,
        &[
            "reap",
            "--older-than=1h",
            "--age-by=m",
            "--dry-run",
            "--json",
            d,
        ],
    );
    assert!(out.status.success(), "{}", text(&out));
    let t = text(&out);
    assert!(!t.contains("unsupported"), "{t}");
    let reaped = t.split("\"skipped\"").next().unwrap();
    assert!(
        reaped.contains("old.txt") && !reaped.contains("held.txt"),
        "{t}"
    );
    assert!(
        t.contains(&format!("in use by pid {}", std::process::id())),
        "{t}"
    );
    assert!(old.exists(), "the dry run removed a file");

    let out = run(
        &root,
        &["reap", "--older-than=1h", "--age-by=m", "--permanent", d],
    );
    assert!(out.status.success(), "{}", text(&out));
    assert!(!old.exists() && held.exists());
    drop(open);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn reap_profiles_work_with_windows_paths() {
    let root = sandbox("profiles");
    let cfg = root.join("AppData").join("Roaming").join("rtrash");
    fs::create_dir_all(&cfg).unwrap();
    fs::write(
        cfg.join("config.toml"),
        "[reap.profiles.mine]\nabout = \"my scratch\"\npaths = [\"windows:C/scratch/*\", \"unix:/nowhere/*\"]\nage = \"1h\"\naction = \"permanent\"\n",
    )
    .unwrap();
    let scratch = root.join("AppData").join("Local").join("scratch");
    fs::create_dir_all(&scratch).unwrap();
    let old = scratch.join("old.txt");
    fs::write(&old, b"o").unwrap();
    stale(&old);

    let out = run(&root, &["reap", "--list-profiles"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("my scratch (config)"), "{}", text(&out));
    let out = run(&root, &["reap", "-p", "mine", "--age-by=m"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        text(&out).contains("mine: reaped 1 entry"),
        "{}",
        text(&out)
    );
    assert!(!old.exists());
    let _ = fs::remove_dir_all(&root);
}
