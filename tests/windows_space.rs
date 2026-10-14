//! Windows: `status` shows each drive's free space from GetDiskFreeSpaceExW,
//! `restore` takes `--no-space-check`, `trash-stats` reads the Recycle
//! Bins, and `reap --install-timer` makes a Task Scheduler task.
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
