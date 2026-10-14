//! `rtrash reap --install-timer`: a per-user job that runs reap on a
//! schedule, as a systemd user service and timer on Linux, a launchd
//! agent on macOS and a Task Scheduler task on Windows. Nothing needs root
//! or an administrator, and `--dry-run` prints the files (or the schtasks
//! command) instead of writing them. `--remove-timer` stops the job and
//! removes it again.

use std::path::{Path, PathBuf};

/// The unit and agent name.
pub const NAME: &str = "rtrash-reap";
/// The launchd label.
pub const LABEL: &str = "io.github.haozeke.rtrash-reap";

/// Quote one word for a systemd `ExecStart=` line.
fn systemd_word(w: &str) -> String {
    let body = w
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%")
        .replace('$', "$$");
    format!("\"{body}\"")
}

/// The service and timer that run `exe reap ARGS` every `every` seconds.
pub fn systemd_units(exe: &Path, args: &[String], every: i64) -> (String, String) {
    let mut cmd = vec![systemd_word(&exe.to_string_lossy()), systemd_word("reap")];
    cmd.extend(args.iter().map(|a| systemd_word(a)));
    let service = format!(
        "[Unit]\nDescription=rtrash reap: clear stale temp files and caches\nDocumentation=man:rtrash(1)\n\n\
         [Service]\nType=oneshot\nExecStart={}\nNice=19\nIOSchedulingClass=idle\n",
        cmd.join(" ")
    );
    let timer = format!(
        "[Unit]\nDescription=Run rtrash reap every {every}s\nDocumentation=man:rtrash(1)\n\n\
         [Timer]\nOnBootSec=5min\nOnUnitActiveSec={every}s\nRandomizedDelaySec=60s\n\n\
         [Install]\nWantedBy=timers.target\n"
    );
    (service, timer)
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The launchd agent that runs `exe reap ARGS` every `every` seconds.
pub fn launchd_plist(exe: &Path, args: &[String], every: i64) -> String {
    let mut words = vec![exe.to_string_lossy().into_owned(), "reap".to_string()];
    words.extend(args.iter().cloned());
    let argv: String = words
        .iter()
        .map(|w| format!("    <string>{}</string>\n", xml(w)))
        .collect();
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n<dict>\n  <key>Label</key>\n  <string>{LABEL}</string>\n\
         \x20 <key>ProgramArguments</key>\n  <array>\n{argv}  </array>\n\
         \x20 <key>StartInterval</key>\n  <integer>{every}</integer>\n\
         \x20 <key>RunAtLoad</key>\n  <false/>\n\
         \x20 <key>LowPriorityIO</key>\n  <true/>\n\
         \x20 <key>Nice</key>\n  <integer>19</integer>\n\
         \x20 <key>ProcessType</key>\n  <string>Background</string>\n</dict>\n</plist>\n"
    )
}

/// Quote one word of the command a scheduled task runs (`schtasks /TR`).
fn task_word(w: &str) -> String {
    if !w.is_empty() && !w.contains([' ', '\t', '"']) {
        return w.to_string();
    }
    format!("\"{}\"", w.replace('"', "\\\""))
}

/// The `schtasks` arguments that create (or replace) a task running
/// `exe reap ARGS` every `every` seconds as this user: by the minute under
/// a day, else by the day, rounded up.
pub fn schtasks_args(exe: &Path, args: &[String], every: i64) -> Vec<String> {
    let mut cmd = vec![task_word(&exe.to_string_lossy()), "reap".to_string()];
    cmd.extend(args.iter().map(|a| task_word(a)));
    let (unit, n) = if every < 86_400 {
        ("MINUTE", ((every + 59) / 60).clamp(1, 1439))
    } else {
        ("DAILY", ((every + 86_399) / 86_400).clamp(1, 365))
    };
    [
        "/Create",
        "/F",
        "/TN",
        NAME,
        "/SC",
        unit,
        "/MO",
        &n.to_string(),
        "/TR",
        &cmd.join(" "),
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Files to write and the command that turns the job on.
fn plan(
    exe: &Path,
    args: &[String],
    every: i64,
) -> Result<(Vec<(PathBuf, String)>, String), String> {
    let home = crate::util::home_dir().ok_or("no home directory")?;
    if cfg!(target_os = "macos") {
        let p = home
            .join("Library/LaunchAgents")
            .join(format!("{LABEL}.plist"));
        let on = format!("launchctl bootstrap gui/$(id -u) {}", p.display());
        return Ok((vec![(p, launchd_plist(exe, args, every))], on));
    }
    if cfg!(target_os = "linux") {
        let base = match std::env::var_os("XDG_CONFIG_HOME") {
            Some(v) if Path::new(&v).is_absolute() => PathBuf::from(v),
            _ => home.join(".config"),
        };
        let dir = base.join("systemd/user");
        let (service, timer) = systemd_units(exe, args, every);
        let on =
            format!("systemctl --user daemon-reload && systemctl --user enable --now {NAME}.timer");
        return Ok((
            vec![
                (dir.join(format!("{NAME}.service")), service),
                (dir.join(format!("{NAME}.timer")), timer),
            ],
            on,
        ));
    }
    Err("timers are installed only on Linux (systemd), macOS (launchd) and Windows (Task Scheduler)".into())
}

/// The job's files, the commands that stop it before they go, and the
/// commands to run after.
type Removal = (Vec<PathBuf>, Vec<Vec<String>>, Vec<Vec<String>>);

fn removal() -> Result<Removal, String> {
    let words = |w: &[&str]| w.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    if cfg!(windows) {
        return Ok((
            Vec::new(),
            vec![words(&["schtasks", "/Delete", "/F", "/TN", NAME])],
            Vec::new(),
        ));
    }
    let (files, _) = plan(Path::new("rtrash"), &[], 3600)?;
    let files: Vec<PathBuf> = files.into_iter().map(|(p, _)| p).collect();
    if cfg!(target_os = "macos") {
        #[cfg(unix)]
        let uid = unsafe { libc::getuid() };
        #[cfg(not(unix))]
        let uid = 0;
        let target = format!("gui/{uid}/{LABEL}");
        return Ok((
            files,
            vec![words(&["launchctl", "bootout", &target])],
            Vec::new(),
        ));
    }
    let timer = format!("{NAME}.timer");
    Ok((
        files,
        vec![words(&["systemctl", "--user", "disable", "--now", &timer])],
        vec![words(&["systemctl", "--user", "daemon-reload"])],
    ))
}

/// Stop and remove the job `install` made, or with `dry_run` print what
/// that would run and remove. The service manager's commands are best
/// effort: a job that was never turned on still loses its files.
pub fn remove(prog: &str, dry_run: bool) -> i32 {
    let (files, before, after) = match removal() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{prog}: {e}");
            return 2;
        }
    };
    let present: Vec<&PathBuf> = files
        .iter()
        .filter(|f| f.symlink_metadata().is_ok())
        .collect();
    if !cfg!(windows) && present.is_empty() {
        println!("no reap timer installed");
        return 0;
    }
    let run = |cmd: &Vec<String>| -> bool {
        if dry_run {
            println!("{}", cmd.join(" "));
            return true;
        }
        std::process::Command::new(&cmd[0])
            .args(&cmd[1..])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    };
    let stopped = before.iter().all(run);
    if cfg!(windows) && !dry_run {
        if !stopped {
            eprintln!("{prog}: schtasks could not delete the task {NAME} (is it installed?)");
            return 1;
        }
        println!("deleted the scheduled task {NAME}");
        return 0;
    }
    let mut status = 0;
    for f in &present {
        if dry_run {
            println!("would remove {}", f.display());
        } else if let Err(e) = std::fs::remove_file(f) {
            eprintln!("{prog}: cannot remove '{}': {e}", f.display());
            status = 1;
        } else {
            println!("removed {}", f.display());
        }
    }
    for cmd in &after {
        run(cmd);
    }
    status
}

/// Create the Windows task, or with `dry_run` print the command.
fn install_task(prog: &str, exe: &Path, args: &[String], every: i64, dry_run: bool) -> i32 {
    let words = schtasks_args(exe, args, every);
    if dry_run {
        let shown: Vec<String> = words.iter().map(|w| task_word(w)).collect();
        println!("schtasks {}", shown.join(" "));
        return 0;
    }
    match std::process::Command::new("schtasks").args(&words).status() {
        Ok(s) if s.success() => {
            println!(
                "created the scheduled task {NAME}; remove it with: schtasks /Delete /TN {NAME}"
            );
            0
        }
        Ok(s) => {
            eprintln!("{prog}: schtasks failed ({s})");
            1
        }
        Err(e) => {
            eprintln!("{prog}: cannot run schtasks: {e}");
            1
        }
    }
}

/// Write (or with `dry_run` print) the job running `reap ARGS` every
/// `every` seconds.
pub fn install(prog: &str, args: &[String], every: i64, dry_run: bool) -> i32 {
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("{prog}: cannot find the rtrash binary: {e}");
            return 1;
        }
    };
    if cfg!(windows) {
        return install_task(prog, &exe, args, every, dry_run);
    }
    let (files, on) = match plan(&exe, args, every) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{prog}: {e}");
            return 2;
        }
    };
    for (path, body) in &files {
        if dry_run {
            println!("# {}\n{body}", path.display());
            continue;
        }
        let write = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::write(path, body));
        if let Err(e) = write {
            eprintln!("{prog}: cannot write '{}': {e}", path.display());
            return 1;
        }
        println!("wrote {}", path.display());
    }
    if dry_run {
        println!("# then: {on}");
    } else {
        println!("turn it on with: {on}");
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn systemd_units_quote_the_command_and_set_the_interval() {
        let args = vec![
            "-p".to_string(),
            "ai".to_string(),
            "--when-below=10%".to_string(),
        ];
        let (svc, timer) = systemd_units(Path::new("/opt/my bin/rtrash"), &args, 3600);
        assert!(
            svc.contains(r#"ExecStart="/opt/my bin/rtrash" "reap" "-p" "ai" "--when-below=10%%""#),
            "{svc}"
        );
        assert!(svc.contains("Type=oneshot"));
        assert!(
            timer.contains("OnUnitActiveSec=3600s") && timer.contains("WantedBy=timers.target")
        );
    }

    #[test]
    fn schtasks_args_quote_the_command_and_pick_a_schedule() {
        let args = vec!["-p".to_string(), "my dir".to_string()];
        let w = schtasks_args(Path::new(r"C:\Program Files\rtrash.exe"), &args, 3600);
        let at = |k: &str| w[w.iter().position(|x| x == k).unwrap() + 1].clone();
        assert_eq!(at("/TN"), NAME);
        assert_eq!((at("/SC"), at("/MO")), ("MINUTE".into(), "60".into()));
        assert_eq!(
            at("/TR"),
            r#""C:\Program Files\rtrash.exe" reap -p "my dir""#
        );
        assert!(w.contains(&"/F".to_string()));
        let w = schtasks_args(Path::new("rtrash"), &[], 90);
        assert_eq!(w[w.iter().position(|x| x == "/MO").unwrap() + 1], "2");
        let w = schtasks_args(Path::new("rtrash"), &[], 2 * 86_400 + 1);
        let at = |k: &str| w[w.iter().position(|x| x == k).unwrap() + 1].clone();
        assert_eq!((at("/SC"), at("/MO")), ("DAILY".into(), "3".into()));
    }

    #[test]
    fn launchd_plist_escapes_arguments() {
        let p = launchd_plist(
            Path::new("/usr/local/bin/rtrash"),
            &["a<&b".to_string()],
            600,
        );
        assert!(p.contains("<string>a&lt;&amp;b</string>"), "{p}");
        assert!(p.contains("<integer>600</integer>") && p.contains(LABEL));
    }
}
