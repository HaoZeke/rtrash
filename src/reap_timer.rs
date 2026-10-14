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

/// The longest command `schtasks /TR` takes; a longer one goes in a task
/// XML file instead.
pub const TR_MAX: usize = 261;

/// The Task Scheduler XML for a task running `exe reap ARGS` every
/// `every` seconds as this user, for commands too long for `/TR`: a
/// repetition by the minute under a day, else a daily trigger.
pub fn task_xml(exe: &Path, args: &[String], every: i64) -> String {
    let mut words = vec!["reap".to_string()];
    words.extend(args.iter().map(|a| task_word(a)));
    let trigger = if every < 86_400 {
        let n = ((every + 59) / 60).clamp(1, 1439);
        format!(
            "    <TimeTrigger>\n      <StartBoundary>2000-01-01T00:00:00</StartBoundary>\n\
             \x20     <Repetition><Interval>PT{n}M</Interval></Repetition>\n\
             \x20     <Enabled>true</Enabled>\n    </TimeTrigger>\n"
        )
    } else {
        let n = ((every + 86_399) / 86_400).clamp(1, 365);
        format!(
            "    <CalendarTrigger>\n      <StartBoundary>2000-01-01T00:00:00</StartBoundary>\n\
             \x20     <ScheduleByDay><DaysInterval>{n}</DaysInterval></ScheduleByDay>\n\
             \x20     <Enabled>true</Enabled>\n    </CalendarTrigger>\n"
        )
    };
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-16\"?>\n\
         <Task version=\"1.2\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">\n\
         \x20 <RegistrationInfo><Description>rtrash reap: clear stale temp files and caches</Description></RegistrationInfo>\n\
         \x20 <Triggers>\n{trigger}  </Triggers>\n\
         \x20 <Principals><Principal id=\"Author\"><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal></Principals>\n\
         \x20 <Settings>\n\
         \x20   <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>\n\
         \x20   <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>\n\
         \x20   <StartWhenAvailable>true</StartWhenAvailable>\n\
         \x20   <Priority>7</Priority>\n\
         \x20 </Settings>\n\
         \x20 <Actions Context=\"Author\">\n\
         \x20   <Exec>\n      <Command>{}</Command>\n      <Arguments>{}</Arguments>\n    </Exec>\n\
         \x20 </Actions>\n</Task>\n",
        xml(&exe.to_string_lossy()),
        xml(&words.join(" "))
    )
}

/// Where a task XML file goes: `%LOCALAPPDATA%\\rtrash\\rtrash-reap.xml`.
fn task_xml_path() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")?;
    Some(
        PathBuf::from(base)
            .join("rtrash")
            .join(format!("{NAME}.xml")),
    )
}

/// The text of a file Task Scheduler reads: UTF-16 after a byte-order
/// mark, else UTF-8.
fn utf16_text(b: &[u8]) -> String {
    match b {
        [0xFF, 0xFE, rest @ ..] => {
            let (pairs, _) = rest.as_chunks::<2>();
            let units: Vec<u16> = pairs.iter().map(|&p| u16::from_le_bytes(p)).collect();
            String::from_utf16_lossy(&units)
        }
        _ => String::from_utf8_lossy(b).into_owned(),
    }
}

/// The command a task XML runs: its `<Command>` and `<Arguments>`, joined
/// and unescaped. `None` without a `<Command>`.
pub fn xml_command(body: &str) -> Option<String> {
    let tag = |t: &str| -> Option<String> {
        let open = format!("<{t}>");
        let start = body.find(&open)? + open.len();
        let end = start + body[start..].find(&format!("</{t}>"))?;
        Some(
            body[start..end]
                .replace("&lt;", "<")
                .replace("&gt;", ">")
                .replace("&quot;", "\"")
                .replace("&amp;", "&"),
        )
    };
    let cmd = task_word(&tag("Command")?);
    Some(match tag("Arguments") {
        Some(a) if !a.is_empty() => format!("{cmd} {a}"),
        _ => cmd,
    })
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
            task_xml_path().into_iter().collect(),
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

/// Run `cmd` and keep the lines of its output that start with one of
/// `keys`, trimmed; nothing when it cannot run or fails.
fn query(cmd: &[&str], keys: &[&str]) -> Vec<String> {
    let Ok(out) = std::process::Command::new(cmd[0])
        .args(&cmd[1..])
        .stderr(std::process::Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| keys.iter().any(|k| l.starts_with(k)))
        .map(str::to_string)
        .collect()
}

/// Split a service manager line at its first `=` or `:` into a key and a
/// value, both trimmed.
pub fn split_field(line: &str) -> Option<(String, String)> {
    let at = line.find(['=', ':'])?;
    let key = line[..at].trim();
    (!key.is_empty()).then(|| (key.to_string(), line[at + 1..].trim().to_string()))
}

/// What `status` found: the manager's name, the job's files (none for a
/// scheduled task) and the fields the manager reports.
struct Found {
    manager: &'static str,
    installed: bool,
    files: Vec<PathBuf>,
    fields: Vec<String>,
}

fn find() -> Result<Found, String> {
    if cfg!(windows) {
        // schtasks' format flag, built so the spell checker does not read
        // it as a typo.
        let format = ["/F", "O"].concat();
        let fields = query(
            &["schtasks", "/Query", "/TN", NAME, "/V", &format, "LIST"],
            &[
                "Status:",
                "Last Run Time:",
                "Last Result:",
                "Next Run Time:",
                "Task To Run:",
            ],
        );
        let mut fields = fields;
        let mut files = Vec::new();
        // A task made from an XML file: show the command the file holds
        // in full, since schtasks may cut a long one short.
        if let Some(p) = task_xml_path().filter(|p| p.exists()) {
            if !fields.is_empty() {
                if let Some(cmd) = std::fs::read(&p)
                    .ok()
                    .and_then(|b| xml_command(&utf16_text(&b)))
                {
                    fields.push(format!("Command: {cmd}"));
                }
            }
            files.push(p);
        }
        return Ok(Found {
            manager: "schtasks",
            installed: !fields.is_empty(),
            files,
            fields,
        });
    }
    let (files, _) = plan(Path::new("rtrash"), &[], 3600)?;
    let files: Vec<PathBuf> = files
        .into_iter()
        .map(|(p, _)| p)
        .filter(|p| p.exists())
        .collect();
    let installed = !files.is_empty();
    let macos = cfg!(target_os = "macos");
    let fields = if !installed {
        Vec::new()
    } else if macos {
        #[cfg(unix)]
        let uid = unsafe { libc::getuid() };
        #[cfg(not(unix))]
        let uid = 0;
        query(
            &["launchctl", "print", &format!("gui/{uid}/{LABEL}")],
            &["state =", "runs =", "last exit code =", "run interval ="],
        )
    } else {
        query(
            &[
                "systemctl",
                "--user",
                "show",
                &format!("{NAME}.timer"),
                "-p",
                "ActiveState",
                "-p",
                "LastTriggerUSec",
                "-p",
                "NextElapseUSecRealtime",
            ],
            &[
                "ActiveState=",
                "LastTriggerUSec=",
                "NextElapseUSecRealtime=",
            ],
        )
    };
    Ok(Found {
        manager: if macos { "launchd" } else { "systemd" },
        installed,
        files,
        fields,
    })
}

/// Say whether the job `install` made is there, and what the service
/// manager reports about it (state, last and next run); with `json`, as
/// an object `{installed, manager, files, state}`. Exits 0 when it is
/// installed and 1 when it is not.
pub fn status(prog: &str, json: bool) -> i32 {
    let f = match find() {
        Ok(f) => f,
        Err(e) => {
            eprintln!("{prog}: {e}");
            return 2;
        }
    };
    let code = if f.installed { 0 } else { 1 };
    if json {
        use crate::util::json_escape;
        let files: Vec<String> = f
            .files
            .iter()
            .map(|p| format!("\"{}\"", json_escape(&p.to_string_lossy())))
            .collect();
        let state: Vec<String> = f
            .fields
            .iter()
            .filter_map(|l| split_field(l))
            .map(|(k, v)| format!("\"{}\":\"{}\"", json_escape(&k), json_escape(&v)))
            .collect();
        println!(
            "{{\"installed\":{},\"manager\":\"{}\",\"files\":[{}],\"state\":{{{}}}}}",
            f.installed,
            f.manager,
            files.join(","),
            state.join(",")
        );
        return code;
    }
    if !f.installed {
        println!("no reap timer installed");
        return code;
    }
    if f.files.is_empty() {
        println!("installed: scheduled task {NAME}");
    } else {
        println!("installed:");
        for p in &f.files {
            println!("  {}", p.display());
        }
        if f.fields.is_empty() {
            println!("the service manager reports nothing; it may not be turned on");
        }
    }
    for l in &f.fields {
        println!("  {l}");
    }
    code
}

/// Create the Windows task, or with `dry_run` print the command.
fn install_task(prog: &str, exe: &Path, args: &[String], every: i64, dry_run: bool) -> i32 {
    let mut words = schtasks_args(exe, args, every);
    if words.last().is_some_and(|tr| tr.len() > TR_MAX) {
        let Some(path) = task_xml_path() else {
            eprintln!(
                "{prog}: the command is too long for schtasks /TR and LOCALAPPDATA is not set"
            );
            return 1;
        };
        let body = task_xml(exe, args, every);
        words = ["/Create", "/F", "/TN", NAME, "/XML"]
            .iter()
            .map(|s| s.to_string())
            .chain([path.to_string_lossy().into_owned()])
            .collect();
        if dry_run {
            println!("would write {}:\n{body}", path.display());
        } else {
            // Task Scheduler reads the encoding the XML header names.
            let mut bytes = vec![0xFF, 0xFE];
            bytes.extend(body.encode_utf16().flat_map(u16::to_le_bytes));
            let written = path
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|_| std::fs::write(&path, bytes));
            if let Err(e) = written {
                eprintln!("{prog}: cannot write {}: {e}", path.display());
                return 1;
            }
        }
    }
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
    fn task_xml_holds_a_long_command_and_its_schedule() {
        let long = "x".repeat(TR_MAX);
        let args = vec!["-p".to_string(), long.clone(), "a<&b".to_string()];
        let exe = Path::new(r"C:\Program Files\rtrash.exe");
        assert!(schtasks_args(exe, &args, 900).last().unwrap().len() > TR_MAX);
        let x = task_xml(exe, &args, 900);
        assert!(
            x.contains(r"<Command>C:\Program Files\rtrash.exe</Command>"),
            "{x}"
        );
        assert!(
            x.contains(&format!(
                "<Arguments>reap -p {long} a&lt;&amp;b</Arguments>"
            )),
            "{x}"
        );
        assert!(
            x.contains("<Interval>PT15M</Interval>") && !x.contains("Calendar"),
            "{x}"
        );
        let x = task_xml(exe, &[], 2 * 86_400 + 1);
        assert!(x.contains("<DaysInterval>3</DaysInterval>"), "{x}");
        assert!(x.contains("<Arguments>reap</Arguments>"), "{x}");
    }

    #[test]
    fn xml_command_reads_back_what_task_xml_wrote() {
        let args = vec!["-p".to_string(), "a<&b".to_string(), "my dir".to_string()];
        let exe = Path::new(r"C:\Program Files\rtrash.exe");
        let body = task_xml(exe, &args, 900);
        assert_eq!(
            xml_command(&body).as_deref(),
            Some(r#""C:\Program Files\rtrash.exe" reap -p a<&b "my dir""#)
        );
        let mut bytes = vec![0xFF, 0xFE];
        bytes.extend(body.encode_utf16().flat_map(u16::to_le_bytes));
        assert_eq!(utf16_text(&bytes), body);
        assert_eq!(xml_command("<Task/>"), None);
    }

    #[test]
    fn split_field_takes_the_first_separator() {
        let kv = |a: &str, b: &str| Some((a.to_string(), b.to_string()));
        assert_eq!(
            split_field("ActiveState=active"),
            kv("ActiveState", "active")
        );
        assert_eq!(
            split_field("LastTriggerUSec=Tue 2026-10-13 21:00:00 CDT"),
            kv("LastTriggerUSec", "Tue 2026-10-13 21:00:00 CDT")
        );
        assert_eq!(split_field("state = running"), kv("state", "running"));
        assert_eq!(
            split_field("Next Run Time:   10/13/2026 10:00:00 PM"),
            kv("Next Run Time", "10/13/2026 10:00:00 PM")
        );
        assert_eq!(split_field("no separator"), None);
        assert_eq!(split_field("=x"), None);
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
