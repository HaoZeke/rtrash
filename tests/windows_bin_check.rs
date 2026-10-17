//! `rtrash status --check [--fix [-n]]` on the Windows Recycle Bin, run
//! against a fake bin folder through the debug-build `RTRASH_TEST_BIN_DIR`
//! seam, so the real bin is never read or changed.
#![cfg(windows)]

mod common;

use std::fs;
use std::path::Path;
use std::process::Output;

/// The stdout lines of `out` that start with `prefix`.
fn lines_starting(out: &Output, prefix: &str) -> usize {
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.starts_with(prefix))
        .count()
}

/// The names in `dir`, sorted.
fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

/// `status --check` finds a dangling record, a record that cannot be
/// parsed and an orphaned payload once each; `--fix -n` and every guard
/// leave the bin as it was just before; `--fix` removes the two records
/// only, keeping the bad record's payload, which a second check reports
/// as an orphan.
#[test]
fn status_check_reports_and_fix_removes_only_broken_bin_records() {
    let home = common::scratch_dir("bincheck", "status");
    let (bin, work) = (home.join("bin"), home.join("work"));
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(&work).unwrap();
    let record = |name: &str, origin: &str| {
        let body = rtrash::win_recycle::encode_recycle_i_v2(
            &work.join(origin),
            1,
            rtrash::win_recycle::FILETIME_UNIX_EPOCH,
        );
        fs::write(bin.join(name), body).unwrap();
    };
    record("$IBCKGOOD.txt", "ck-good.txt");
    fs::write(bin.join("$RBCKGOOD.txt"), b"good").unwrap();
    record("$IBCKGONE.txt", "ck-gone.txt");
    fs::write(bin.join("$IBCKBAD.txt"), b"junk").unwrap();
    fs::write(bin.join("$RBCKBAD.txt"), b"bad").unwrap();
    fs::write(bin.join("$RBCKORPH.txt"), b"orph").unwrap();
    fs::write(bin.join("desktop.ini"), b"[.ShellClassInfo]").unwrap();
    let run = |args: &[&str], envs: &[(&str, &str)]| {
        let mut c = common::rtrash(&home);
        c.env("RTRASH_TEST_BIN_DIR", &bin).args(args);
        for (k, v) in envs {
            c.env(k, v);
        }
        c.output().unwrap()
    };
    let err = common::stderr_text;
    let out_text = |o: &Output| String::from_utf8_lossy(&o.stdout).into_owned();

    let check = run(&["status", "--check"], &[]);
    assert_eq!(check.status.code(), Some(1), "{}", err(&check));
    for prefix in ["dangling: ", "bad record: ", "orphan: "] {
        assert_eq!(
            lines_starting(&check, prefix),
            1,
            "{prefix}: {}",
            out_text(&check)
        );
    }
    assert_eq!(lines_starting(&check, "stray: "), 0, "{}", out_text(&check));
    assert!(
        !out_text(&check).contains("BCKGOOD"),
        "{}",
        out_text(&check)
    );
    assert!(
        err(&check).contains("3 problems found, 2 records --fix would remove"),
        "{}",
        err(&check)
    );

    // Each of these leaves the bin as it was just before it.
    let watch = [bin.as_path()];
    let unchanged = |args: &[&str], envs: &[(&str, &str)], code: i32| {
        common::assert_refused_unchanged(&(args, envs), &watch, code, &[], || run(args, envs))
    };
    let fix = ["status", "--check", "--fix"];
    let out = unchanged(&["status", "--check", "--fix", "-n"], &[], 1);
    assert_eq!(
        lines_starting(&out, "would remove "),
        2,
        "{}",
        out_text(&out)
    );
    let out = unchanged(&fix, &[("RTRASH_NO_PERMANENT", "1")], 2);
    assert!(err(&out).contains("RTRASH_NO_PERMANENT"), "{}", err(&out));
    let out = unchanged(&fix, &[("RTRASH_MAX_PERMANENT", "1")], 2);
    assert!(
        err(&out).contains("more than the cap of 1"),
        "{}",
        err(&out)
    );
    // The bad record names no origin, so a scope refuses the whole fix.
    let scope = work.display().to_string();
    let out = unchanged(&fix, &[("RTRASH_ALLOW_ROOTS", scope.as_str())], 2);
    assert_eq!(common::outside_lines(&out), 1, "{}", err(&out));
    unchanged(&["status", "--fix"], &[], 2);
    unchanged(&["status", "--check", "--older-than=1"], &[], 2);

    let out = run(&fix, &[]);
    assert_eq!(out.status.code(), Some(0), "{}", err(&out));
    assert_eq!(lines_starting(&out, "removed "), 2, "{}", out_text(&out));
    assert_eq!(
        names(&bin),
        [
            "$IBCKGOOD.txt",
            "$RBCKBAD.txt",
            "$RBCKGOOD.txt",
            "$RBCKORPH.txt",
            "desktop.ini"
        ]
    );

    // What is left is for the user: two payloads with no record.
    let again = run(&["status", "--check"], &[]);
    assert_eq!(again.status.code(), Some(1), "{}", err(&again));
    assert_eq!(
        lines_starting(&again, "orphan: "),
        2,
        "{}",
        out_text(&again)
    );
    for prefix in ["dangling: ", "bad record: "] {
        assert_eq!(lines_starting(&again, prefix), 0, "{}", out_text(&again));
    }
    assert!(
        err(&again).contains("2 problems found, 0 records --fix would remove"),
        "{}",
        err(&again)
    );
    let _ = fs::remove_dir_all(&home);
}

/// `list` shows the items it can read and, after its rows, one stderr
/// line counting the entries it left out (a dangling record, a record
/// that cannot be parsed and a payload with no record), naming
/// `rtrash status --check`. stdout is the same as for a bin holding only
/// the readable item, and with nothing left out there is no hint.
#[test]
fn list_counts_the_bin_entries_it_skips_and_points_at_status_check() {
    let home = common::scratch_dir("bincheck", "list");
    let (bin, work) = (home.join("bin"), home.join("work"));
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(&work).unwrap();
    let record = |name: &str, origin: &str| {
        let body = rtrash::win_recycle::encode_recycle_i_v2(
            &work.join(origin),
            1,
            rtrash::win_recycle::FILETIME_UNIX_EPOCH,
        );
        fs::write(bin.join(name), body).unwrap();
    };
    record("$ILSTGOOD.txt", "ls-good.txt");
    fs::write(bin.join("$RLSTGOOD.txt"), b"good").unwrap();
    record("$ILSTGONE.txt", "ls-gone.txt");
    fs::write(bin.join("$ILSTBAD.txt"), b"junk").unwrap();
    fs::write(bin.join("$RLSTBAD.txt"), b"bad").unwrap();
    fs::write(bin.join("$RLSTORPH.txt"), b"orph").unwrap();
    fs::write(bin.join("desktop.ini"), b"[.ShellClassInfo]").unwrap();
    let list = || {
        common::rtrash(&home)
            .env("RTRASH_TEST_BIN_DIR", &bin)
            .arg("list")
            .output()
            .unwrap()
    };
    let hint = "`rtrash status --check` shows them";
    let out = common::assert_refused_unchanged(&"list", &[bin.as_path()], 0, &[(hint, 1)], list);
    let err = common::stderr_text(&out);
    let rows = String::from_utf8_lossy(&out.stdout).into_owned();
    assert_eq!(rows.lines().count(), 1, "{rows}");
    assert!(rows.trim_end().ends_with("ls-good.txt"), "{rows}");
    let counted = err
        .lines()
        .filter(|l| l.contains("3 Recycle Bin entries could not be listed"))
        .count();
    assert_eq!(counted, 1, "{err}");

    // With only the readable item left, the rows are the same and the
    // hint is gone.
    for name in [
        "$ILSTGONE.txt",
        "$ILSTBAD.txt",
        "$RLSTBAD.txt",
        "$RLSTORPH.txt",
    ] {
        fs::remove_file(bin.join(name)).unwrap();
    }
    let clean = common::assert_refused_unchanged(&"clean list", &[bin.as_path()], 0, &[], list);
    let _ = fs::remove_dir_all(&home);
    assert_eq!(clean.stdout, out.stdout);
    let err = common::stderr_text(&clean);
    let hints = err
        .lines()
        .filter(|l| l.contains("could not be listed"))
        .count();
    assert_eq!(hints, 0, "{err}");
}
