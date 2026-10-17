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

/// `status --check --fix` appends one `fix` line to the history of the
/// volume whose Recycle Bin folder it removed records from. A check, a
/// dry run and a fix with nothing left to remove append none, and undo,
/// which needs the FreeDesktop trash, refuses and leaves the history
/// alone. The fake bin is laid out as `VOL\$Recycle.Bin\SID`, so its
/// history is the one for VOL, under a private `%LOCALAPPDATA%`.
#[test]
fn status_fix_appends_one_fix_line_to_the_bin_history() {
    let home = common::scratch_dir("bincheck", "history");
    let bin = home.join("vol").join("$Recycle.Bin").join("S-1-5-21-0");
    let (work, local) = (home.join("work"), home.join("local"));
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(&work).unwrap();
    let body = rtrash::win_recycle::encode_recycle_i_v2(
        &work.join("fx-gone.txt"),
        1,
        rtrash::win_recycle::FILETIME_UNIX_EPOCH,
    );
    fs::write(bin.join("$IFXGONE.txt"), body).unwrap();
    fs::write(bin.join("$IFXBAD.txt"), b"junk").unwrap();
    fs::write(bin.join("$RFXBAD.txt"), b"bad").unwrap();
    let history = local.join("rtrash").join("history");
    let lines = || -> Vec<String> {
        let Ok(rd) = fs::read_dir(&history) else {
            return Vec::new();
        };
        let mut files: Vec<_> = rd.map(|e| e.unwrap().path()).collect();
        files.sort();
        files
            .iter()
            .flat_map(|f| {
                let body = fs::read_to_string(f).unwrap();
                body.lines().map(String::from).collect::<Vec<_>>()
            })
            .collect()
    };
    let run = |args: &[&str]| {
        common::rtrash(&home)
            .env("RTRASH_TEST_BIN_DIR", &bin)
            .env("LOCALAPPDATA", &local)
            .args(args)
            .output()
            .unwrap()
    };
    let watch = [bin.as_path(), history.as_path()];
    common::assert_refused_unchanged(&"check", &watch, 1, &[], || run(&["status", "--check"]));
    let dry = common::assert_refused_unchanged(&"dry run", &watch, 1, &[], || {
        run(&["status", "--check", "--fix", "-n"])
    });
    assert_eq!(lines_starting(&dry, "would remove "), 2);
    assert!(lines().is_empty(), "{:?}", lines());

    let out = run(&["status", "--check", "--fix"]);
    let err = common::stderr_text(&out);
    assert_eq!(out.status.code(), Some(0), "{err}");
    assert_eq!(lines_starting(&out, "removed "), 2, "{err}");
    let got = lines();
    assert_eq!(got.len(), 1, "{got:?}");
    let fields: Vec<&str> = got[0].split(' ').collect();
    assert_eq!(fields.len(), 4, "{got:?}");
    assert_eq!(fields[1], "fix", "{got:?}");
    for n in [fields[0], fields[2], fields[3]] {
        assert!(n.parse::<u64>().is_ok(), "{got:?}");
    }

    // Nothing is left to remove, so this fix logs nothing; undo refuses.
    common::assert_refused_unchanged(&"second fix", &watch, 0, &[], || {
        run(&["status", "--check", "--fix"])
    });
    let undo = "undo needs the FreeDesktop trash; on Windows, use restore";
    common::assert_refused_unchanged(&"undo", &watch, 1, &[(undo, 1)], || run(&["undo"]));
    assert_eq!(lines(), got);
    let _ = fs::remove_dir_all(&home);
}

/// A fake bin with one good item, `$IRJGOOD.txt` for `WORK\rj-good.txt`,
/// read through `RTRASH_TEST_BIN_DIR` under a private `%LOCALAPPDATA%`.
struct FlaggedBin {
    home: std::path::PathBuf,
    bin: std::path::PathBuf,
    work: std::path::PathBuf,
    local: std::path::PathBuf,
}

impl FlaggedBin {
    fn new(tag: &str) -> Self {
        let home = common::scratch_dir("bincheck", tag);
        let (bin, work, local) = (home.join("bin"), home.join("work"), home.join("local"));
        for d in [&bin, &work, &local] {
            fs::create_dir_all(d).unwrap();
        }
        let b = FlaggedBin {
            home,
            bin,
            work,
            local,
        };
        fs::write(b.bin.join("$IRJGOOD.txt"), b.record("rj-good.txt")).unwrap();
        fs::write(b.bin.join("$RRJGOOD.txt"), b"good").unwrap();
        b
    }

    /// A good `$I` record for `WORK\origin`.
    fn record(&self, origin: &str) -> Vec<u8> {
        rtrash::win_recycle::encode_recycle_i_v2(
            &self.work.join(origin),
            1,
            rtrash::win_recycle::FILETIME_UNIX_EPOCH,
        )
    }

    fn run(&self, args: &[&str]) -> Output {
        common::rtrash(&self.home)
            .env("RTRASH_TEST_BIN_DIR", &self.bin)
            .env("LOCALAPPDATA", &self.local)
            .args(args)
            .output()
            .unwrap()
    }

    /// With one flagged `$I` in the bin, for `WORK\origin`, whose problem
    /// line starts with `prefix`: the check reports it once, list leaves it
    /// out with a hint counting 1, rm and restore do not act on it, and the
    /// good item still restores.
    fn assert_one_flagged_is_skipped(&self, origin: &str, prefix: &str, id: &str) {
        let err = common::stderr_text;
        let out_text = |o: &Output| String::from_utf8_lossy(&o.stdout).into_owned();
        let watch = [self.bin.as_path(), self.work.as_path()];

        let check = self.run(&["status", "--check"]);
        assert_eq!(check.status.code(), Some(1), "{}", err(&check));
        let problems: usize = ["dangling: ", "bad record: ", "orphan: ", "stray: "]
            .iter()
            .map(|p| lines_starting(&check, p))
            .sum();
        assert_eq!(problems, 1, "{}", out_text(&check));
        assert_eq!(lines_starting(&check, prefix), 1, "{}", out_text(&check));
        assert!(out_text(&check).contains(id), "{}", out_text(&check));

        let tail = rtrash::win_recycle::unlisted_hint("", 1).unwrap();
        let list =
            common::assert_refused_unchanged(&"list", &watch, 0, &[(tail.as_str(), 1)], || {
                self.run(&["list"])
            });
        let rows = out_text(&list);
        assert_eq!(rows.lines().count(), 1, "{rows}");
        assert!(rows.trim_end().ends_with("rj-good.txt"), "{rows}");

        let dry = common::assert_refused_unchanged(&"rm -n", &watch, 0, &[], || {
            self.run(&["rm", "-n", "-f", "*"])
        });
        let matched = out_text(&dry);
        assert_eq!(matched.lines().count(), 1, "{matched}");
        assert!(matched.trim_end().ends_with("rj-good.txt"), "{matched}");
        let before = names(&self.bin);
        let out = self.run(&["rm", "-v", origin]);
        assert!(!out_text(&out).contains(origin), "{}", out_text(&out));
        assert_eq!(names(&self.bin), before, "rm {origin}: {}", err(&out));

        let path = self.work.join(origin).display().to_string();
        let none = "no trashed files match";
        common::assert_refused_unchanged(&origin, &watch, 1, &[(none, 1)], || {
            self.run(&["restore", &path])
        });
        let good = self.work.join("rj-good.txt");
        let out = self.run(&["restore", &good.display().to_string()]);
        assert_eq!(out.status.code(), Some(0), "{}", err(&out));
        assert_eq!(fs::read(&good).unwrap(), b"good");
        assert!(!self.bin.join("$IRJGOOD.txt").exists());
    }
}

impl Drop for FlaggedBin {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.home);
    }
}

/// A `$I` padded past the record cap still parses, but `status --check`
/// calls it a bad record; list, rm and restore leave it alone.
#[test]
fn list_rm_and_restore_skip_the_index_records_status_check_flags() {
    let b = FlaggedBin::new("rejected");
    // Padding after the path still parses; only the size flags it.
    let mut big = b.record("rj-big.txt");
    big.resize(rtrash::win_recycle::I_RECORD_CAP as usize + 1, 0);
    fs::write(b.bin.join("$IRJBIG.txt"), big).unwrap();
    fs::write(b.bin.join("$RRJBIG.txt"), b"big").unwrap();
    b.assert_one_flagged_is_skipped("rj-big.txt", "bad record: ", "RJBIG");
    assert!(b.bin.join("$IRJBIG.txt").exists());
}

/// A `$I` that is a file link to a good record: `status --check` calls
/// it stray, and list, rm and restore leave it alone. Creating the link
/// needs the symlink privilege or Developer Mode, which the Windows CI
/// runner has; without it this test fails, so a skip can never pass
/// unseen.
#[test]
fn list_rm_and_restore_skip_a_linked_index_record() {
    let b = FlaggedBin::new("linked");
    let target = b.home.join("rj-link-record");
    fs::write(&target, b.record("rj-link.txt")).unwrap();
    if let Err(e) = std::os::windows::fs::symlink_file(&target, b.bin.join("$IRJLINK.txt")) {
        panic!(
            "cannot create a file link ({e}); this test needs the symlink \
             privilege or Developer Mode"
        );
    }
    fs::write(b.bin.join("$RRJLINK.txt"), b"link").unwrap();
    b.assert_one_flagged_is_skipped("rj-link.txt", "stray: ", "RJLINK");
    assert!(b.bin.join("$IRJLINK.txt").symlink_metadata().is_ok());
}
