//! Windows: `status` shows each drive's free space from GetDiskFreeSpaceExW,
//! `restore` takes `--no-space-check`, `trash-stats` reads the Recycle
//! Bins, `reap --install-timer` makes a Task Scheduler task,
//! `reap --purge-trash` lists a drive's Recycle Bin oldest first, and `put`
//! warns when the volume of the Recycle Bin it used is low.
#![cfg(windows)]

use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_rtrash")
}

#[test]
fn status_shows_free_space_on_each_drive() {
    let out = Command::new(bin()).arg("status").output().unwrap();
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
    let out = Command::new(bin())
        .args(["restore", "--no-space-check", "--help"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("--no-space-check"));
    // The floor only matters to a copy; status still reports.
    let out = Command::new(bin())
        .arg("status")
        .env("RTRASH_MIN_FREE", "lots")
        .output()
        .unwrap();
    assert!(out.status.success());
}

#[test]
fn trash_stats_summarizes_the_recycle_bins_as_json() {
    let out = Command::new(bin())
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
    let out = Command::new(bin())
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
            std::thread::sleep(std::time::Duration::from_millis(1100));
        }
        let p = base.join(name);
        fs::write(&p, name).unwrap();
        let out = Command::new(bin()).arg("put").arg(&p).output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let out = Command::new(bin())
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
    let out = Command::new(bin())
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
    let out = Command::new(bin())
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
    let out = Command::new(bin())
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
    let out = Command::new(bin())
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
    let out = Command::new(bin())
        .args(["reap", "--timer-status"])
        .env("RTRASH_TEST_SERVICE_TOOL", &fake)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(1), "{text}");
    assert!(text.contains("no reap timer installed"), "{text}");
    let out = Command::new(bin())
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
    let out = Command::new(bin()).arg("put").arg(&p).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = Command::new(bin())
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
    let out = Command::new(bin())
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
        let mut c = Command::new(bin());
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
            && err.contains("trashing frees nothing"),
        "{err}"
    );
    let err = put("fine.txt", false);
    assert!(!err.contains("is low"), "{err}");
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
    let out = Command::new(bin()).arg("put").arg(&p).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = Command::new(bin())
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
    assert!(stderr.contains("from the Recycle Bin of"), "{stderr}");
    // The dry run left the item; take it out by name.
    let _ = Command::new(bin()).args(["rm", &name]).output();
    assert_eq!(
        Command::new(bin())
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
    let out = Command::new(bin()).arg("put").arg(&p).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let dry = |args: &[&str]| {
        let out = Command::new(bin())
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
    let roomy = dry(&["--max-size=999999G"]);
    assert!(roomy.contains("0 item(s)"), "{roomy}");
    let tight = dry(&["--max-size=0", "--prune=largest"]);
    assert!(tight.contains(&name), "{tight}");
    let json = dry(&["--max-size=0", "--json"]);
    assert!(
        json.starts_with(r#"{"dry_run":true,"removed":"#) && json.contains(r#""max_size":0}"#),
        "{json}"
    );
    let _ = Command::new(bin()).args(["rm", &name]).output();
    assert_eq!(
        Command::new(bin())
            .args(["empty", "--prune=newest", "-n"])
            .output()
            .unwrap()
            .status
            .code(),
        Some(2)
    );
    let _ = fs::remove_dir_all(&base);
}
