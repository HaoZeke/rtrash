//! Windows: `status` shows each drive's free space from GetDiskFreeSpaceExW,
//! `restore` takes `--no-space-check`, `trash-stats` reads the Recycle
//! Bins, `reap --install-timer` makes a Task Scheduler task,
//! `reap --purge-trash` lists a drive's Recycle Bin oldest first, and `put`
//! warns when the volume of the Recycle Bin it used is low and refuses an
//! item over the bin's cap.
#![cfg(windows)]

mod common;

use std::path::{Path, PathBuf};

#[cfg(debug_assertions)]
fn canonical_plain(path: &Path) -> PathBuf {
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    rtrash::util::plain_path(&canonical)
}

#[test]
fn status_shows_free_space_on_each_drive() {
    let out = common::rtrash_real(common::Real::RecycleBin)
        .arg("status")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    let drive = std::env::temp_dir()
        .to_str()
        .unwrap()
        .chars()
        .next()
        .unwrap()
        .to_ascii_uppercase();
    let line = text
        .lines()
        .find(|l| l.starts_with(&format!("Free on {drive}:\\")))
        .unwrap_or_else(|| panic!("no free line for {drive}: {text}"));
    assert!(
        line.contains('%') && line.ends_with("no inode limit"),
        "{line}"
    );
}

#[test]
fn restore_takes_no_space_check_and_status_rejects_a_bad_floor_quietly() {
    let out = common::rtrash_real(common::Real::RecycleBin)
        .args(["restore", "--no-space-check", "--help"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("--no-space-check"));
    // The floor only matters to a copy; status still reports.
    let out = common::rtrash_real(common::Real::RecycleBin)
        .arg("status")
        .env("RTRASH_MIN_FREE", "lots")
        .output()
        .unwrap();
    assert!(out.status.success());
}

#[test]
fn trash_stats_summarizes_the_recycle_bins_as_json() {
    let out = common::rtrash_real(common::Real::RecycleBin)
        .args(["trash-stats", "--json", "--top=2"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.starts_with(r#"{"trashes":["#) && text.contains(r#""total":{"bytes":"#),
        "{text}"
    );
}

#[test]
fn reap_install_timer_dry_run_prints_a_schtasks_command() {
    let out = common::rtrash_real(common::Real::RecycleBin)
        .args([
            "reap",
            "--install-timer=30m",
            "-n",
            "--when-below=10%",
            "--purge-trash",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.starts_with("schtasks /Create /F /TN rtrash-reap /SC MINUTE /MO 30 /TR ")
            && text.contains("reap --when-below=10% --purge-trash"),
        "{text}"
    );
}

/// Debug builds only: the RTRASH_TEST_FS seam makes the reaped directory
/// a tiny file system, so --when-below asks for more than any Recycle Bin
/// holds and the dry run lists the whole bin of that drive, oldest first.
#[cfg(debug_assertions)]
#[test]
fn reap_purge_trash_dry_run_lists_the_drive_recycle_bin_oldest_first() {
    use std::fs;
    let base = std::env::temp_dir().join(format!("rtrash-purge-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let dir = base.join("tmp");
    fs::create_dir_all(&dir).unwrap();
    let old = dir.join("old.txt");
    fs::write(&old, "old").unwrap();
    let day_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 86_400);
    fs::File::options()
        .write(true)
        .open(&old)
        .unwrap()
        .set_times(
            fs::FileTimes::new()
                .set_accessed(day_ago)
                .set_modified(day_ago),
        )
        .unwrap();
    // Three items in the Recycle Bin, a second apart so their deletion
    // times order them.
    let names: Vec<String> = (1..=3)
        .map(|n| format!("rtrash-purge-{}-{n}.txt", std::process::id()))
        .collect();
    for (i, name) in names.iter().enumerate() {
        if i > 0 {
            common::next_trash_second();
        }
        let p = base.join(name);
        fs::write(&p, name).unwrap();
        let out = common::rtrash_real(common::Real::RecycleBin)
            .arg("put")
            .arg(&p)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let out = common::rtrash_real(common::Real::RecycleBin)
        .args([
            "reap",
            "--older-than=1h",
            "--age-by=m",
            "--when-below=999999G",
            "--purge-trash",
            "--dry-run",
        ])
        .arg(&dir)
        .env("RTRASH_TEST_FS", format!("1000000:{}", dir.display()))
        .output()
        .unwrap();
    let (stdout, stderr) = (
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(out.status.success(), "{stderr}");
    assert!(stderr.contains("from the Recycle Bin of"), "{stderr}");
    let at: Vec<usize> = names
        .iter()
        .map(|n| {
            stdout
                .lines()
                .position(|l| l.starts_with("would remove ") && l.contains(n.as_str()))
                .unwrap_or_else(|| panic!("{n} not listed: {stdout:?}"))
        })
        .collect();
    assert!(
        at[0] < at[1] && at[1] < at[2],
        "not oldest first: {stdout:?}"
    );
    assert!(old.exists() && base.join(&names[0]).symlink_metadata().is_err());
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn reap_install_timer_dry_run_prints_task_xml_for_a_long_command() {
    let local = std::env::temp_dir().join(format!("rtrash-xml-{}", std::process::id()));
    let mut args = vec!["reap", "--install-timer=30m", "-n", "--when-below=10%"];
    // Past the 261 characters /TR takes.
    args.extend(std::iter::repeat_n("--older-than=1h", 20));
    let out = common::rtrash_real(common::Real::RecycleBin)
        .args(&args)
        .env("LOCALAPPDATA", &local)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let xml = local.join("rtrash").join("rtrash-reap.xml");
    assert!(
        text.contains(&format!("would write {}:", xml.display())),
        "{text:?}"
    );
    assert!(text.contains("<Interval>PT30M</Interval>"), "{text:?}");
    assert!(
        text.contains("<Arguments>reap --when-below=10% --older-than=1h"),
        "{text:?}"
    );
    assert!(
        text.contains(&format!(
            "schtasks /Create /F /TN rtrash-reap /XML {}",
            xml.display()
        )),
        "{text:?}"
    );
    assert!(!xml.exists(), "a dry run wrote the XML");
}

#[test]
fn reap_remove_timer_dry_run_prints_the_schtasks_delete() {
    let out = common::rtrash_real(common::Real::RecycleBin)
        .args(["reap", "--remove-timer", "-n"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.starts_with("schtasks /End /TN rtrash-reap\nschtasks /Delete /F /TN rtrash-reap"),
        "{text}"
    );
}

/// `--install-timer --watch` writes a logon task with no time limit,
/// creates it from the XML and starts it, all through the stand-in.
#[cfg(debug_assertions)]
#[test]
fn reap_install_timer_with_watch_makes_a_logon_task_and_starts_it() {
    let (fake, log) = fake_schtasks("watch-task");
    let local = log.parent().unwrap().join("local");
    let out = common::rtrash_real(common::Real::RecycleBin)
        .args(["reap", "--install-timer", "--watch=5m", "--when-below=10%"])
        .env("RTRASH_TEST_SERVICE_TOOL", &fake)
        .env("LOCALAPPDATA", &local)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let xml = local.join("rtrash").join("rtrash-reap.xml");
    let bytes = std::fs::read(&xml).unwrap();
    let units: Vec<u16> = bytes[2..]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&p| u16::from_le_bytes(p))
        .collect();
    let body = String::from_utf16_lossy(&units);
    assert!(body.contains("<LogonTrigger>"), "{body}");
    assert!(
        body.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"),
        "{body}"
    );
    assert!(body.contains("--watch=5m"), "{body}");
    let calls = std::fs::read_to_string(&log).unwrap();
    assert!(calls.contains("/Create /F /TN rtrash-reap /XML"), "{calls}");
    assert!(calls.contains("/Run /TN rtrash-reap"), "{calls}");
    let out = common::rtrash_real(common::Real::RecycleBin)
        .args(["reap", "--remove-timer"])
        .env("RTRASH_TEST_SERVICE_TOOL", &fake)
        .env("LOCALAPPDATA", &local)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let calls = std::fs::read_to_string(&log).unwrap();
    assert!(calls.contains("/End /TN rtrash-reap"), "{calls}");
    assert!(calls.contains("/Delete /F /TN rtrash-reap"), "{calls}");
    let _ = std::fs::remove_dir_all(log.parent().unwrap());
}

/// A stand-in for schtasks that logs each call and prints nothing, so the
/// test never asks the runner's Task Scheduler.
#[cfg(debug_assertions)]
fn fake_schtasks(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("rtrash-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("calls.log");
    let cmd = dir.join("fake-schtasks.cmd");
    std::fs::write(
        &cmd,
        format!("@echo %* >> \"{}\"\r\n@exit /b 0\r\n", log.display()),
    )
    .unwrap();
    (cmd, log)
}

#[cfg(debug_assertions)]
#[test]
fn reap_timer_status_says_when_no_task_is_installed() {
    let (fake, log) = fake_schtasks("status");
    let out = common::rtrash_real(common::Real::RecycleBin)
        .args(["reap", "--timer-status"])
        .env("RTRASH_TEST_SERVICE_TOOL", &fake)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(1), "{text}");
    assert!(text.contains("no reap timer installed"), "{text}");
    let out = common::rtrash_real(common::Real::RecycleBin)
        .args(["reap", "--timer-status", "--json"])
        .env("RTRASH_TEST_SERVICE_TOOL", &fake)
        .output()
        .unwrap();
    let json = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(1), "{json}");
    assert!(
        json.starts_with(r#"{"installed":false,"manager":"schtasks","files":[],"state":{}}"#),
        "{json}"
    );
    // Both queries went to the stand-in.
    let calls = std::fs::read_to_string(&log).unwrap();
    assert_eq!(calls.matches("schtasks /Query").count(), 2, "{calls}");
    let _ = std::fs::remove_dir_all(log.parent().unwrap());
}

#[test]
fn put_adds_a_line_to_the_recycle_bin_history() {
    use std::fs;
    let p = std::env::temp_dir().join(format!("rtrash-history-{}.txt", std::process::id()));
    fs::write(&p, "history").unwrap();
    let out = common::rtrash_real(common::Real::RecycleBin)
        .arg("put")
        .arg(&p)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = common::rtrash_real(common::Real::RecycleBin)
        .args(["trash-stats", "--history", "--json", "--top=0"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains(r#""event":"put""#), "{text}");
    let drive = p
        .to_str()
        .unwrap()
        .chars()
        .next()
        .unwrap()
        .to_ascii_uppercase();
    let file = std::path::PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap())
        .join(format!("rtrash/history/{drive}.txt"));
    assert!(
        fs::read_to_string(&file).unwrap().contains(" put "),
        "{}",
        file.display()
    );
}

#[test]
fn trash_stats_history_lists_a_drive_whose_bin_is_empty() {
    use std::fs;
    // The runner's bin may or may not hold items; either way, a drive
    // with a history file must be listed with that history.
    let drive = std::env::temp_dir()
        .to_str()
        .unwrap()
        .chars()
        .next()
        .unwrap()
        .to_ascii_uppercase();
    let file = std::path::PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap())
        .join(format!("rtrash/history/{drive}.txt"));
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    let mut body = fs::read_to_string(&file).unwrap_or_default();
    body.push_str("1700000000 empty 0 0\n");
    fs::write(&file, body).unwrap();
    let out = common::rtrash_real(common::Real::RecycleBin)
        .args(["trash-stats", "--history", "--json", "--top=0"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    let bin_root = format!("{drive}:\\\\$Recycle.Bin");
    assert!(text.contains(&bin_root), "{bin_root} missing: {text}");
    assert!(
        text.contains(r#""epoch":1700000000,"event":"empty""#),
        "{text}"
    );
}

/// A put whose Recycle Bin volume is short says that it freed nothing.
/// The volume is faked: a directory named as the bin root, sized by
/// `RTRASH_TEST_FS` and nearly full of ballast.
#[cfg(debug_assertions)]
#[test]
fn put_warns_when_the_recycle_bin_volume_is_low() {
    let base = std::env::temp_dir().join(format!("rtrash-winlow-{}", std::process::id()));
    let vol = base.join("vol");
    std::fs::create_dir_all(&vol).unwrap();
    std::fs::write(vol.join("ballast.bin"), vec![7u8; 95_000]).unwrap();
    let put = |name: &str, fake: bool| {
        let f = base.join(name);
        std::fs::write(&f, b"x").unwrap();
        let mut c = common::rtrash_real(common::Real::RecycleBin);
        c.args(["put", f.to_str().unwrap()])
            .env("RTRASH_TEST_BIN_ROOT", &vol)
            .env("RTRASH_IN_USE", "ignore");
        if fake {
            c.env("RTRASH_TEST_FS", format!("100000:{}", vol.display()));
        }
        let out = c.output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stderr).into_owned()
    };
    let err = put("low.txt", true);
    assert!(
        err.contains("Recycle Bin of")
            && err.contains("is low (")
            && err.contains("trashing frees nothing")
            && err.contains("rtrash list --sort=size")
            && err.contains("rtrash rm -i --max-count=1 --match=id ID")
            && !err.contains("rtrash empty")
            && !err.contains("would lose the work"),
        "{err}"
    );
    let err = put("fine.txt", false);
    assert!(!err.contains("is low"), "{err}");
    let _ = std::fs::remove_dir_all(&base);
}

/// A put of a dirty or unpushed repository on a short volume says that
/// permanently removing it would lose the work, and does not suggest
/// `rtrash rm`. A plain file put beside the repository is not named.
/// The same check as the Unix put test.
#[cfg(debug_assertions)]
#[test]
fn put_space_hint_keeps_flagged_git_work() {
    let git = |repo: &std::path::Path, home: &std::path::Path, args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(repo)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("HOME", home)
            .env("GIT_AUTHOR_NAME", "Trash Test")
            .env("GIT_AUTHOR_EMAIL", "trash-test@example.com")
            .env("GIT_COMMITTER_NAME", "Trash Test")
            .env("GIT_COMMITTER_EMAIL", "trash-test@example.com")
            .output()
    };
    for (tag, commit, case) in [
        ("unpushed", true, "put-git-unpushed"),
        ("dirty", false, "put-git-dirty"),
    ] {
        let base = common::scratch_dir("wingit", case);
        let vol = base.join("vol");
        std::fs::create_dir_all(&vol).unwrap();
        std::fs::write(vol.join("ballast.bin"), vec![7u8; 95_000]).unwrap();
        let repo = base.join(tag);
        let _ = std::fs::remove_dir_all(&repo);
        std::fs::create_dir(&repo).unwrap();
        let init = match git(&repo, &base, &["init", "-q"]) {
            Ok(out) => out,
            Err(_) => {
                common::skip("git unavailable; skipping");
                let _ = std::fs::remove_dir_all(&base);
                return;
            }
        };
        if !init.status.success() {
            common::skip("git unavailable; skipping");
            let _ = std::fs::remove_dir_all(&base);
            return;
        }
        std::fs::write(repo.join("f"), vec![b'x'; 90_000]).unwrap();
        let plain = base.join("plain.txt");
        std::fs::write(&plain, b"ok").unwrap();
        if commit {
            let add = git(&repo, &base, &["add", "f"]).unwrap();
            let committed = git(&repo, &base, &["commit", "-qm", "work"]).unwrap();
            assert!(
                add.status.success() && committed.status.success(),
                "add: {}\ncommit: {}",
                String::from_utf8_lossy(&add.stderr),
                String::from_utf8_lossy(&committed.stderr)
            );
        }
        let out = common::rtrash(&base)
            .args(["put", repo.to_str().unwrap(), plain.to_str().unwrap()])
            .env("RTRASH_TEST_BIN_ROOT", &vol)
            .env("RTRASH_IN_USE", "ignore")
            .env("RTRASH_TEST_FS", format!("100000:{}", vol.display()))
            .output()
            .unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{tag}: {err}");
        let lose = err
            .split("permanently removing it would lose the work")
            .nth(1)
            .unwrap_or("");
        let remedy = if tag == "dirty" {
            "uncommitted work is committed"
        } else {
            "until the work is pushed"
        };
        assert!(
            err.contains(&format!("with {tag} work"))
                && err.contains(remedy)
                && err.contains("`rtrash restore --batch=last` brings it back")
                && err.contains("is low (")
                && lose.contains(tag)
                && !err.contains("plain.txt")
                && !err.contains("unrecoverable")
                && !err.contains("rtrash empty")
                && !err.contains("rtrash list")
                && !err.contains("rtrash rm"),
            "{tag}: {err}"
        );
        if tag == "dirty" {
            assert!(!err.contains("until the work is pushed"), "{tag}: {err}");
        }
        let _ = std::fs::remove_dir_all(&base);
    }
}

/// An unpushed repository and a dirty one in one bin: a low-space put names
/// both, in the plural advice, so neither is dropped when the other is
/// flagged too.
#[cfg(debug_assertions)]
#[test]
fn put_space_hint_names_every_flagged_repository_in_the_bin() {
    let git = |repo: &std::path::Path, home: &std::path::Path, args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(repo)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("HOME", home)
            .env("GIT_AUTHOR_NAME", "Trash Test")
            .env("GIT_AUTHOR_EMAIL", "trash-test@example.com")
            .env("GIT_COMMITTER_NAME", "Trash Test")
            .env("GIT_COMMITTER_EMAIL", "trash-test@example.com")
            .output()
    };
    let base = common::scratch_dir("wingit", "put-git-both");
    let vol = base.join("vol");
    std::fs::create_dir_all(&vol).unwrap();
    std::fs::write(vol.join("ballast.bin"), vec![7u8; 95_000]).unwrap();
    for (tag, commit) in [("unpushed", true), ("dirty", false)] {
        let repo = base.join(tag);
        std::fs::create_dir(&repo).unwrap();
        if !git(&repo, &base, &["init", "-q"]).is_ok_and(|o| o.status.success()) {
            common::skip("git unavailable; skipping");
            let _ = std::fs::remove_dir_all(&base);
            return;
        }
        std::fs::write(repo.join("f"), b"x").unwrap();
        if commit {
            let add = git(&repo, &base, &["add", "f"]).unwrap();
            let done = git(&repo, &base, &["commit", "-qm", "work"]).unwrap();
            assert!(add.status.success() && done.status.success());
        }
    }
    let put = |path: &std::path::Path, low: bool| {
        let mut c = common::rtrash(&base);
        c.args(["put", path.to_str().unwrap()])
            .env("RTRASH_TEST_BIN_ROOT", &vol)
            .env("RTRASH_IN_USE", "ignore");
        if low {
            c.env("RTRASH_TEST_FS", format!("100000:{}", vol.display()));
        }
        let out = c.output().unwrap();
        assert!(out.status.success(), "{}", common::stderr_text(&out));
        common::stderr_text(&out)
    };
    put(&base.join("unpushed"), false);
    let err = put(&base.join("dirty"), true);
    assert!(
        err.contains("is low (")
            && err.contains("with dirty work")
            && err.contains("would lose the work")
            && err.contains("'")
            && err.contains("unpushed")
            && err.contains("dirty")
            && err.contains("uncommitted work is committed")
            && !err.contains("rtrash rm"),
        "{err}"
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// A put of a plain file on a short volume still names git work that an
/// earlier put left in that bin, and does not point at removing it.
#[cfg(debug_assertions)]
#[test]
fn a_low_space_put_names_risky_work_already_in_the_bin() {
    let base = common::scratch_dir("wingit", "put-old-risk");
    let vol = base.join("vol");
    let repo = base.join("oldrepo");
    std::fs::create_dir_all(&vol).unwrap();
    std::fs::create_dir(&repo).unwrap();
    std::fs::write(vol.join("ballast.bin"), vec![7u8; 95_000]).unwrap();
    let git = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&repo)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", &base)
        .output();
    if !git.is_ok_and(|o| o.status.success()) {
        common::skip("git unavailable; skipping");
        let _ = std::fs::remove_dir_all(&base);
        return;
    }
    std::fs::write(repo.join("f"), b"dirty").unwrap();
    let put = |path: &std::path::Path, low: bool| {
        let mut c = common::rtrash(&base);
        c.args(["put", path.to_str().unwrap()])
            .env("RTRASH_TEST_BIN_ROOT", &vol)
            .env("RTRASH_IN_USE", "ignore");
        if low {
            c.env("RTRASH_TEST_FS", format!("100000:{}", vol.display()));
        }
        let out = c.output().unwrap();
        assert!(out.status.success(), "{}", common::stderr_text(&out));
        common::stderr_text(&out)
    };
    put(&repo, false);
    let plain = base.join("plain.txt");
    std::fs::write(&plain, b"ok").unwrap();
    let err = put(&plain, true);
    assert!(
        err.contains("is low (")
            && err.contains("would lose the work")
            && err.contains("oldrepo")
            && !err.contains("rtrash rm"),
        "{err}"
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// Undo is named only after the item is in the bin. A dry run does not
/// promise it.
#[cfg(debug_assertions)]
#[test]
fn a_dry_run_does_not_promise_undo_before_the_move() {
    let base = common::scratch_dir("wingit", "put-git-dry");
    let repo = base.join("repo");
    let _ = std::fs::remove_dir_all(&repo);
    std::fs::create_dir(&repo).unwrap();
    let init = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&repo)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", &base)
        .output();
    let Ok(init) = init else {
        common::skip("git unavailable; skipping");
        let _ = std::fs::remove_dir_all(&base);
        return;
    };
    if !init.status.success() {
        common::skip("git unavailable; skipping");
        let _ = std::fs::remove_dir_all(&base);
        return;
    }
    std::fs::write(repo.join("f"), b"x").unwrap();
    let dry = common::rtrash(&base)
        .args(["put", "--dry-run", repo.to_str().unwrap()])
        .env("RTRASH_IN_USE", "ignore")
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&dry.stderr);
    let out = String::from_utf8_lossy(&dry.stdout);
    assert!(dry.status.success(), "{err}");
    assert!(out.contains("would trash"), "{out}");
    assert!(!err.contains("rtrash undo"), "{err}");
    assert!(repo.join("f").is_file());
    let moved = common::rtrash(&base)
        .args(["put", repo.to_str().unwrap()])
        .env("RTRASH_IN_USE", "ignore")
        .output()
        .unwrap();
    let moved_err = String::from_utf8_lossy(&moved.stderr);
    assert!(moved.status.success(), "{moved_err}");
    assert!(
        moved_err.contains("`rtrash restore --batch=last` brings it back"),
        "{moved_err}"
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// A junction operand is not reported as the repository it points at.
/// A real put, so the warning that follows a move is what is checked.
/// `mklink /J` needs no Developer Mode.
#[test]
fn put_does_not_attribute_a_junction_to_the_repository_it_points_at() {
    let base = common::scratch_dir("wingit", "put-git-link");
    let vol = base.join("vol");
    std::fs::create_dir_all(&vol).unwrap();
    std::fs::write(vol.join("ballast.bin"), vec![7u8; 95_000]).unwrap();
    let repo = base.join("real");
    let _ = std::fs::remove_dir_all(&repo);
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::fs::write(repo.join("f"), b"kept").unwrap();
    let link = base.join("shortcut");
    let _ = std::fs::remove_dir_all(&link);
    common::mklink_junction(&link, &repo).expect("make a junction to the repository");
    let out = common::rtrash(&base)
        .args(["put", link.to_str().unwrap()])
        .env("RTRASH_TEST_BIN_ROOT", &vol)
        .env("RTRASH_IN_USE", "ignore")
        .env("RTRASH_TEST_FS", format!("100000:{}", vol.display()))
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{err}");
    assert!(!err.contains("git repository"), "{err}");
    assert!(!err.contains("would lose the work"), "{err}");
    assert!(!err.contains("rtrash undo"), "{err}");
    assert!(
        err.contains("rtrash rm -i --max-count=1 --match=id ID"),
        "{err}"
    );
    assert!(
        repo.join("f").is_file(),
        "the repository behind the junction stays"
    );
    assert_eq!(std::fs::read(repo.join("f")).unwrap(), b"kept");
    let _ = std::fs::remove_dir_all(&base);
}

/// The last-put record is what `restore --batch=last` reads. When it
/// cannot be written, the warning does not name that command or `undo`.
#[test]
fn a_failed_last_put_record_does_not_promise_restore() {
    let base = common::scratch_dir("wingit", "last-put-fail");
    let lad = common::local_app_data(&base);
    std::fs::write(&lad, b"not a directory").unwrap();
    let f = base.join("a.txt");
    std::fs::write(&f, b"x").unwrap();
    let out = common::rtrash(&base)
        .args(["put", f.to_str().unwrap()])
        .env("RTRASH_IN_USE", "ignore")
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{err}");
    assert!(err.contains("could not be written"), "{err}");
    assert!(err.contains("Recycle Bin"), "{err}");
    assert!(err.contains("restore --batch=last"), "{err}");
    assert!(err.contains("cannot see them yet"), "{err}");
    assert!(!err.contains("rtrash undo"), "{err}");
    let _ = std::fs::remove_dir_all(&base);
}

/// `put --json` of a dirty repository keeps stderr to errors: no
/// repository note, and no low-space warning from the faked volume.
#[cfg(debug_assertions)]
#[test]
fn put_json_leaves_out_the_repository_note() {
    let base = common::scratch_dir("wingit", "put-json");
    let vol = base.join("vol");
    std::fs::create_dir_all(&vol).unwrap();
    std::fs::write(vol.join("ballast.bin"), vec![7u8; 95_000]).unwrap();
    let repo = base.join("repo");
    let _ = std::fs::remove_dir_all(&repo);
    std::fs::create_dir(&repo).unwrap();
    let init = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&repo)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", &base)
        .env("GIT_AUTHOR_NAME", "Trash Test")
        .env("GIT_AUTHOR_EMAIL", "trash-test@example.com")
        .env("GIT_COMMITTER_NAME", "Trash Test")
        .env("GIT_COMMITTER_EMAIL", "trash-test@example.com")
        .output();
    let Ok(init) = init else {
        common::skip("git unavailable; skipping");
        let _ = std::fs::remove_dir_all(&base);
        return;
    };
    if !init.status.success() {
        common::skip("git unavailable; skipping");
        let _ = std::fs::remove_dir_all(&base);
        return;
    }
    std::fs::write(repo.join("f"), b"x").unwrap();
    let out = common::rtrash(&base)
        .args(["put", "--json", repo.to_str().unwrap()])
        .env("RTRASH_TEST_BIN_ROOT", &vol)
        .env("RTRASH_IN_USE", "ignore")
        .env("RTRASH_TEST_FS", format!("100000:{}", vol.display()))
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    let json = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{err}");
    assert!(!repo.join("f").exists(), "{err}");
    assert!(!err.contains("contains a git repository"), "{err}");
    assert!(!err.contains("brings it back"), "{err}");
    assert!(!err.contains("is low"), "{err}");
    assert!(
        json.contains("\"trashed\":[")
            && json.contains("\"name\":\"$R")
            && json.trim_end().ends_with("\"failed\":0}"),
        "{json}"
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// A last-put record that cannot be written does not hide the git work: the
/// put names the repository, and does not say `restore --batch=last` brings it back.
#[test]
fn a_failed_last_put_record_still_names_the_git_work() {
    let base = common::scratch_dir("wingit", "last-put-fail-risk");
    let repo = base.join("repo");
    let _ = std::fs::remove_dir_all(&repo);
    std::fs::create_dir(&repo).unwrap();
    let init = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&repo)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", &base)
        .output();
    if !init.is_ok_and(|o| o.status.success()) {
        common::skip("git unavailable; skipping");
        let _ = std::fs::remove_dir_all(&base);
        return;
    }
    std::fs::write(repo.join("f"), b"x").unwrap();
    let lad = common::local_app_data(&base);
    std::fs::write(&lad, b"not a directory").unwrap();
    let out = common::rtrash(&base)
        .args(["put", repo.to_str().unwrap()])
        .env("RTRASH_IN_USE", "ignore")
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{err}");
    assert!(err.contains("could not be written"), "{err}");
    assert!(err.contains("contains a git repository with"), "{err}");
    assert!(err.contains("cannot see them yet"), "{err}");
    assert!(!err.contains("brings it back"), "{err}");
    let _ = std::fs::remove_dir_all(&base);
}

/// Two puts of the same path in one deletion second still report the payload
/// from the second move, not the older payload that sorts after it by name.
#[cfg(debug_assertions)]
#[test]
fn two_quick_puts_of_one_path_find_the_second_payload() {
    let base = common::scratch_dir("wingit", "put-same-second");
    let fixed = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 86_400)
        .to_string();
    let home = base.join("file");
    std::fs::create_dir_all(&home).unwrap();
    let file = home.join("same");
    std::fs::write(&file, b"old").unwrap();
    let first = common::rtrash(&home)
        .args(["put", file.to_str().unwrap()])
        .env("RTRASH_IN_USE", "ignore")
        .env("RTRASH_TEST_BIN_TIME", &fixed)
        .output()
        .unwrap();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );

    let bin = common::test_bin(&home);
    let first_payload = std::fs::read_dir(&bin)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| p.file_name().unwrap().to_string_lossy().starts_with("$R"))
        .unwrap();
    let first_index = rtrash::win_recycle::index_file_for(&first_payload).unwrap();
    let old_payload = bin.join("$RZOLD");
    let old_index = bin.join("$IZOLD");
    std::fs::rename(first_payload, &old_payload).unwrap();
    std::fs::rename(first_index, &old_index).unwrap();

    std::fs::write(&file, b"new").unwrap();
    let second = common::rtrash(&home)
        .args(["put", "--json", file.to_str().unwrap()])
        .env("RTRASH_IN_USE", "ignore")
        .env("RTRASH_TEST_BIN_TIME", &fixed)
        .output()
        .unwrap();
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    let json = String::from_utf8_lossy(&second.stdout);
    assert!(json.contains(r#""name":"$RT00000""#), "{json}");
    let new_index = bin.join("$IT00000");
    let deleted = |index: &std::path::Path| match rtrash::win_recycle::read_bin_index(index) {
        rtrash::win_recycle::IndexRead::Record(meta) => meta.delete_filetime,
        other => panic!("cannot read {}: {other:?}", index.display()),
    };
    assert_eq!(deleted(&old_index), deleted(&new_index));

    let home = base.join("git");
    let repo = home.join("repo");
    let git = |repo: &std::path::Path, args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(repo)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("HOME", &home)
            .env("GIT_AUTHOR_NAME", "Trash Test")
            .env("GIT_AUTHOR_EMAIL", "trash-test@example.com")
            .env("GIT_COMMITTER_NAME", "Trash Test")
            .env("GIT_COMMITTER_EMAIL", "trash-test@example.com")
            .output()
    };
    let make_repo = || {
        std::fs::create_dir_all(&repo).unwrap();
        let Ok(init) = git(&repo, &["init", "-q"]) else {
            return false;
        };
        if !init.status.success() {
            return false;
        }
        std::fs::write(repo.join("f"), b"x").unwrap();
        git(&repo, &["add", "f"]).unwrap().status.success()
            && git(&repo, &["commit", "-qm", "work"])
                .unwrap()
                .status
                .success()
    };
    if !make_repo() {
        common::skip("git unavailable; skipping directory case");
        let _ = std::fs::remove_dir_all(&base);
        return;
    }
    let first = common::rtrash(&home)
        .args(["put", repo.to_str().unwrap()])
        .env("RTRASH_IN_USE", "ignore")
        .env("RTRASH_TEST_BIN_TIME", &fixed)
        .output()
        .unwrap();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let bin = common::test_bin(&home);
    std::fs::rename(bin.join("$RT00000"), bin.join("$RZOLD")).unwrap();
    std::fs::rename(bin.join("$IT00000"), bin.join("$IZOLD")).unwrap();
    assert!(make_repo());
    let second = common::rtrash(&home)
        .args(["put", repo.to_str().unwrap()])
        .env("RTRASH_IN_USE", "ignore")
        .env("RTRASH_TEST_BIN_TIME", &fixed)
        .env("RTRASH_TEST_AFTER_PROBE", canonical_plain(&repo))
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&second.stderr);
    assert!(second.status.success(), "{err}");
    assert!(err.contains("dirty and unpushed work"), "{err}");
    assert!(bin.join("$RT00000/.rtrash-probe-seam").is_file(), "{err}");
    let _ = std::fs::remove_dir_all(&base);
}

/// Putting a plain directory by a differently cased spelling still finds its
/// `$I` record: no false unknown git-work warning, the low-space rm hint
/// stays, `put --json` keeps the `$R` payload name, and post-put budget
/// trimming keeps the just-trashed item.
#[cfg(debug_assertions)]
#[test]
fn put_plain_dir_with_mismatched_case_keeps_rm_hint_and_json_name() {
    let base = common::scratch_dir("wingit", "put-case");
    let vol = base.join("vol");
    std::fs::create_dir_all(&vol).unwrap();
    std::fs::write(vol.join("ballast.bin"), vec![7u8; 95_000]).unwrap();

    let real = base.join("PlainDir");
    std::fs::create_dir(&real).unwrap();
    std::fs::write(real.join("note.txt"), vec![b'x'; 20_000]).unwrap();
    let typed = base.join("plaindir");
    let out = common::rtrash(&base)
        .args(["put", typed.to_str().unwrap()])
        .env("RTRASH_TEST_BIN_ROOT", &vol)
        .env("RTRASH_IN_USE", "ignore")
        .env("RTRASH_TEST_FS", format!("100000:{}", vol.display()))
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{err}");
    assert!(!real.exists(), "plain dir should be trashed");
    assert!(!err.contains("contains a git repository"), "{err}");
    assert!(!err.contains("would lose the work"), "{err}");
    assert!(
        err.contains("is low (") && err.contains("rtrash rm -i --max-count=1 --match=id ID"),
        "{err}"
    );

    let real_json = base.join("PlainJsonDir");
    std::fs::create_dir(&real_json).unwrap();
    std::fs::write(real_json.join("note.txt"), b"ok").unwrap();
    let typed_json = base.join("plainjsondir");
    let out = common::rtrash(&base)
        .args(["put", "--json", typed_json.to_str().unwrap()])
        .env("RTRASH_IN_USE", "ignore")
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    let json = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{err}");
    assert!(
        json.contains("\"trashed\":[")
            && json.contains("\"name\":\"$R")
            && json.trim_end().ends_with("\"failed\":0}"),
        "{json}"
    );

    let real_budget = base.join("PlainBudgetDir");
    std::fs::create_dir(&real_budget).unwrap();
    std::fs::write(real_budget.join("note.txt"), vec![b'x'; 20_000]).unwrap();
    let typed_budget = base.join("plainbudgetdir");
    let out = common::rtrash(&base)
        .args(["put", typed_budget.to_str().unwrap()])
        .env("RTRASH_IN_USE", "ignore")
        .env("RTRASH_MAX_SIZE", "1")
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{err}");
    let listed = common::rtrash(&base).args(["list"]).output().unwrap();
    let list_out = String::from_utf8_lossy(&listed.stdout);
    assert!(
        listed.status.success(),
        "{}",
        String::from_utf8_lossy(&listed.stderr)
    );
    assert!(list_out.contains("PlainBudgetDir"), "{list_out}");
    assert!(!list_out.contains("PlainDir"), "{list_out}");
    let _ = std::fs::remove_dir_all(&base);
}

/// The command the Windows warning names restores the repository.
#[test]
fn restore_batch_last_brings_back_the_repository_the_warning_names() {
    let base = common::scratch_dir("wingit", "restore-last");
    let repo = base.join("repo");
    let _ = std::fs::remove_dir_all(&repo);
    std::fs::create_dir(&repo).unwrap();
    std::fs::write(repo.join("f"), b"back").unwrap();
    let put = common::rtrash(&base)
        .args(["put", repo.to_str().unwrap()])
        .env("RTRASH_IN_USE", "ignore")
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&put.stderr);
    assert!(put.status.success(), "{err}");
    assert!(!repo.join("f").exists());
    let back = common::rtrash(&base)
        .args(["restore", "--batch=last"])
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&back.stderr);
    assert!(back.status.success(), "{err}");
    assert_eq!(std::fs::read(repo.join("f")).unwrap(), b"back");
    let _ = std::fs::remove_dir_all(&base);
}

/// `empty --until-free` on Windows: asked for far more free room than any
/// drive has, the dry run counts the Recycle Bin of the drive it put to,
/// reports it per file system in JSON, removes nothing and exits 1.
#[test]
fn empty_until_free_dry_run_reports_each_short_drive() {
    use std::fs;
    let base = std::env::temp_dir().join(format!("rtrash-untilfree-{}", std::process::id()));
    fs::create_dir_all(&base).unwrap();
    let name = format!("rtrash-untilfree-{}.txt", std::process::id());
    let p = base.join(&name);
    fs::write(&p, "until free").unwrap();
    let out = common::rtrash_real(common::Real::RecycleBin)
        .arg("put")
        .arg(&p)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = common::rtrash_real(common::Real::RecycleBin)
        .args(["empty", "--until-free=999999G", "--json", "-n"])
        .output()
        .unwrap();
    let (stdout, stderr) = (
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert_eq!(out.status.code(), Some(1), "{stdout} {stderr}");
    assert!(
        stdout.contains(r#""dry_run":true"#)
            && stdout.contains(r#""file_systems":[{"path":"#)
            && stdout.contains("$Recycle.Bin"),
        "{stdout}"
    );
    serde_json::from_slice::<serde_json::Value>(&out.stdout)
        .unwrap_or_else(|e| panic!("{e}: {stdout}"));
    assert!(stderr.contains("from the Recycle Bin of"), "{stderr}");
    // The dry run left the item; take it out by exact original path.
    let _ = common::rtrash_real(common::Real::RecycleBin)
        .arg("rm")
        .arg(&p)
        .output();
    assert_eq!(
        common::rtrash_real(common::Real::RecycleBin)
            .args(["empty", "--until-free=10%", "3"])
            .output()
            .unwrap()
            .status
            .code(),
        Some(2)
    );
    let _ = fs::remove_dir_all(&base);
}

/// `empty --max-size` on Windows: a budget larger than any bin takes
/// nothing, and a zero budget lists the item just put (dry runs only).
#[test]
fn empty_max_size_dry_run_trims_the_recycle_bins() {
    use std::fs;
    let base = std::env::temp_dir().join(format!("rtrash-maxsize-{}", std::process::id()));
    fs::create_dir_all(&base).unwrap();
    let name = format!("rtrash-maxsize-{}.txt", std::process::id());
    let p = base.join(&name);
    fs::write(&p, "budget").unwrap();
    let out = common::rtrash_real(common::Real::RecycleBin)
        .arg("put")
        .arg(&p)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let dry = |args: &[&str]| {
        let out = common::rtrash_real(common::Real::RecycleBin)
            .arg("empty")
            .args(args)
            .arg("-n")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let roomy = common::rtrash_real(common::Real::RecycleBin)
        .args(["empty", "--max-size=999999G", "-n"])
        .output()
        .unwrap();
    assert_eq!(
        roomy.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&roomy.stderr)
    );
    let roomy_out = String::from_utf8_lossy(&roomy.stdout);
    assert!(roomy_out.contains("0 item(s)"), "{roomy_out}");
    assert!(
        String::from_utf8_lossy(&roomy.stderr).contains("nothing would be permanently removed"),
        "{}",
        String::from_utf8_lossy(&roomy.stderr)
    );
    let tight = dry(&["--max-size=0", "--prune=largest"]);
    assert!(tight.contains(&name), "{tight}");
    let json = dry(&["--max-size=0", "--json"]);
    assert!(
        json.starts_with(r#"{"dry_run":true,"removed":"#) && json.contains(r#""max_size":0}"#),
        "{json}"
    );
    let _ = common::rtrash_real(common::Real::RecycleBin)
        .arg("rm")
        .arg(&p)
        .output();
    assert_eq!(
        common::rtrash_real(common::Real::RecycleBin)
            .args(["empty", "--prune=newest", "-n"])
            .output()
            .unwrap()
            .status
            .code(),
        Some(2)
    );
    let _ = fs::remove_dir_all(&base);
}

/// A put under a trash budget the Recycle Bins already fit in trims
/// nothing and says nothing. (A tight budget would purge the shared bin
/// that parallel tests use, so the trim itself is unit tested.)
#[test]
fn put_under_a_roomy_budget_trims_nothing() {
    use std::fs;
    let name = format!("rtrash-budget-{}.txt", std::process::id());
    let p = std::env::temp_dir().join(&name);
    fs::write(&p, "budget").unwrap();
    let out = common::rtrash_real(common::Real::RecycleBin)
        .arg("put")
        .arg(&p)
        .env("RTRASH_MAX_SIZE", "999999G")
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{err}");
    assert!(!err.contains("to keep them under"), "{err}");
    let _ = common::rtrash_real(common::Real::RecycleBin)
        .arg("rm")
        .arg(&p)
        .output();
}

/// `rm --json` on the Recycle Bin prints the FreeDesktop summary: a dry
/// run lists the match, the real run removes it and says so.
#[test]
fn rm_json_summarizes_the_recycle_bin_removal() {
    use std::fs;
    let name = format!("rtrash-rm-json-{}.txt", std::process::id());
    let p = std::env::temp_dir().join(&name);
    fs::write(&p, "rm json").unwrap();
    let put = common::rtrash_real(common::Real::RecycleBin)
        .arg("put")
        .arg(&p)
        .output()
        .unwrap();
    assert!(
        put.status.success(),
        "{}",
        String::from_utf8_lossy(&put.stderr)
    );
    let rm = |args: &[&str]| {
        let out = common::rtrash_real(common::Real::RecycleBin)
            .arg("rm")
            .args(args)
            .arg(&p)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let dry = rm(&["--json", "-n"]);
    assert!(dry.contains("\"dry_run\": true"), "{dry}");
    assert!(dry.contains("\"removed\": 1"), "{dry}");
    assert!(dry.contains(&name), "{dry}");
    let done = rm(&["--json"]);
    assert!(done.contains("\"dry_run\": false"), "{done}");
    assert!(done.contains("\"removed\": 1"), "{done}");
    for args in [&[][..], &["-n"][..], &["--json"][..]] {
        let again = common::rtrash_real(common::Real::RecycleBin)
            .arg("rm")
            .args(args)
            .arg(&p)
            .output()
            .unwrap();
        assert_eq!(
            again.status.code(),
            Some(1),
            "{args:?}: {}",
            String::from_utf8_lossy(&again.stderr)
        );
        assert!(
            !String::from_utf8_lossy(&again.stderr).contains("no exact match"),
            "{args:?}: {}",
            String::from_utf8_lossy(&again.stderr)
        );
        assert!(
            String::from_utf8_lossy(&again.stderr).contains("no trash items match"),
            "{args:?}: {}",
            String::from_utf8_lossy(&again.stderr)
        );
    }
}

/// `reap --permanent` on Windows deletes through permanent_guard::remove,
/// as on Unix: a read-only file past its age goes like any other.
/// RTRASH_NO_PERMANENT refuses the run before it starts (exit 2), and
/// RTRASH_MAX_PERMANENT=0 refuses each removal at the guard (exit 1);
/// both leave the folder as it was.
#[test]
fn reap_permanent_removes_a_read_only_entry_and_keeps_the_guards() {
    use std::fs;
    let home = common::scratch_dir("reap", "readonly");
    let (dir, local) = (home.join("scratch"), home.join("local"));
    fs::create_dir_all(&dir).unwrap();
    let day_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 86_400);
    let stale = |name: &str| {
        let p = dir.join(name);
        fs::write(&p, name).unwrap();
        let times = fs::FileTimes::new()
            .set_accessed(day_ago)
            .set_modified(day_ago);
        fs::File::options()
            .write(true)
            .open(&p)
            .unwrap()
            .set_times(times)
            .unwrap();
        p
    };
    let plain = stale("rp-plain.txt");
    let locked = stale("rp-locked.txt");
    let mut perms = fs::metadata(&locked).unwrap().permissions();
    perms.set_readonly(true);
    fs::set_permissions(&locked, perms).unwrap();
    let d = dir.to_str().unwrap().to_string();
    let run = |envs: &[(&str, &str)]| {
        let mut c = common::rtrash(&home);
        c.env("LOCALAPPDATA", &local).args([
            "reap",
            "--older-than=1h",
            "--age-by=m",
            "--permanent",
            d.as_str(),
        ]);
        for (k, v) in envs {
            c.env(k, v);
        }
        c.output().unwrap()
    };
    let watch = [dir.as_path()];
    let out = common::assert_refused_unchanged(&"no permanent", &watch, 2, &[], || {
        run(&[("RTRASH_NO_PERMANENT", "1")])
    });
    assert!(
        common::stderr_text(&out).contains("RTRASH_NO_PERMANENT"),
        "{}",
        common::stderr_text(&out)
    );
    let out = common::assert_refused_unchanged(&"cap 0", &watch, 1, &[], || {
        run(&[("RTRASH_MAX_PERMANENT", "0")])
    });
    let err = common::stderr_text(&out);
    let capped = err
        .lines()
        .filter(|l| l.contains("caps this run at 0 permanent deletions"))
        .count();
    assert_eq!(capped, 2, "{err}");

    let out = run(&[]);
    let err = common::stderr_text(&out);
    assert_eq!(out.status.code(), Some(0), "{err}");
    assert!(plain.symlink_metadata().is_err(), "{err}");
    assert!(
        locked.symlink_metadata().is_err(),
        "the read-only file stayed: {err}"
    );
    let _ = fs::remove_dir_all(&home);
}

/// A put refuses an item over the Recycle Bin's cap, which the shell would
/// delete for good without a word, and leaves it in place.
/// `--allow-permanent` lets it go, unless `RTRASH_NO_PERMANENT` is on. The
/// cap is the debug-only `RTRASH_TEST_BIN_CAP`, checked under the scratch
/// bin of `common::rtrash`, so the real bin is not read or changed.
#[cfg(debug_assertions)]
#[test]
fn put_refuses_an_item_over_the_recycle_bin_cap() {
    let home = common::scratch_dir("bincap", "put");
    let big = home.join("big.bin");
    std::fs::write(&big, vec![1u8; 2048]).unwrap();
    let small = home.join("small.txt");
    std::fs::write(&small, b"x").unwrap();
    let put = |extra: &[&str], f: &std::path::Path, guard: bool| {
        let mut c = common::rtrash(&home);
        c.arg("put")
            .args(extra)
            .arg(f)
            .env("RTRASH_TEST_BIN_CAP", "1024")
            .env("RTRASH_IN_USE", "ignore");
        if guard {
            c.env("RTRASH_NO_PERMANENT", "1");
        }
        c.output().unwrap()
    };
    let out = put(&[], &big, false);
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(err.contains("so Windows would delete it for good"), "{err}");
    assert!(err.contains("--allow-permanent"), "{err}");
    assert!(big.exists());
    let out = put(&["--dry-run", "--allow-permanent"], &big, false);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.starts_with("would delete for good '"), "{text}");
    let out = put(&["--allow-permanent"], &big, true);
    assert_eq!(out.status.code(), Some(1));
    assert!(big.exists());
    let out = put(&["--allow-permanent"], &big, false);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{err}");
    assert!(err.contains("for good"), "{err}");
    assert!(!big.exists());
    let out = put(&[], &small, false);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!small.exists());
    std::fs::remove_dir_all(&home).unwrap();
}
