//! `rtrash reap --install-timer`: a per-user job that runs reap on a
//! schedule, as a systemd user service and timer on Linux and a launchd
//! agent on macOS. Nothing needs root, and `--dry-run` prints the files
//! instead of writing them.

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
    Err("timers are installed only on Linux (systemd) and macOS (launchd)".into())
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
