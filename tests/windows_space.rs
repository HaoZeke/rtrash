//! Windows: `status` shows each drive's free space from GetDiskFreeSpaceExW,
//! `restore` takes `--no-space-check`, `trash-stats` reads the Recycle
//! Bins, `reap --install-timer` makes a Task Scheduler task, and
//! `reap --purge-trash` lists a drive's Recycle Bin oldest first.
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
                .unwrap_or_else(|| panic!("{n} not listed: {stdout}"))
        })
        .collect();
    assert!(at[0] < at[1] && at[1] < at[2], "not oldest first: {stdout}");
    assert!(old.exists() && base.join(&names[0]).symlink_metadata().is_err());
    let _ = fs::remove_dir_all(&base);
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
        text.starts_with("schtasks /Delete /F /TN rtrash-reap"),
        "{text}"
    );
}
