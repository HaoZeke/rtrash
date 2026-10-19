//! Shared alternate-screen setup for TUI modes.

use std::io::{self, stdout, Stdout};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

pub type Term = Terminal<CrosstermBackend<Stdout>>;

pub fn enter() -> io::Result<Term> {
    hold_stderr();
    let term = (|| {
        enable_raw_mode()?;
        let mut out = stdout();
        execute!(out, EnterAlternateScreen)?;
        Terminal::new(CrosstermBackend::new(out))
    })();
    if term.is_err() {
        // The caller reports the error, so stderr must reach the user again.
        let _ = disable_raw_mode();
        let _ = execute!(stdout(), LeaveAlternateScreen);
        release_stderr();
    }
    term
}

pub fn leave(terminal: &mut Term) {
    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let _ = terminal.show_cursor();
    release_stderr();
}

/// Wait up to `tick` for the next key press, for a browser's event loop.
/// `None` when no key came in time, or for an event that is not a press
/// (a release, a resize, a mouse event); the caller redraws either way.
/// Every browser reads keys here and nowhere else, so a key that means
/// the same in all of them is handled once: a [`SharedKey`] is acted on
/// here and never reaches the browser.
pub fn next_key(
    terminal: &mut Term,
    tick: std::time::Duration,
) -> io::Result<Option<(KeyCode, KeyModifiers)>> {
    if !event::poll(tick)? {
        return Ok(None);
    }
    let key = match event::read()? {
        Event::Key(key) if key.kind == KeyEventKind::Press => key,
        _ => return Ok(None),
    };
    match shared_key(key.code, key.modifiers) {
        Some(SharedKey::Redraw) => {
            terminal.clear()?;
            Ok(None)
        }
        Some(SharedKey::Suspend) => {
            suspend(terminal)?;
            Ok(None)
        }
        None => Ok(Some((key.code, key.modifiers))),
    }
}

/// A key every browser treats the same, in every mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SharedKey {
    /// Ctrl-L: draw the whole screen again, as after other output
    /// scribbled over it.
    Redraw,
    /// Ctrl-Z: hand the terminal back to the shell and stop until `fg`.
    /// Windows has no job control, so there the key is taken and does
    /// nothing.
    Suspend,
}

/// The [`SharedKey`] that `code` with `mods` is, if any: Ctrl-L or
/// Ctrl-Z, with or without Shift, and no other modifier.
pub fn shared_key(code: KeyCode, mods: KeyModifiers) -> Option<SharedKey> {
    if mods.difference(KeyModifiers::SHIFT) != KeyModifiers::CONTROL {
        return None;
    }
    match code {
        KeyCode::Char('l' | 'L') => Some(SharedKey::Redraw),
        KeyCode::Char('z' | 'Z') => Some(SharedKey::Suspend),
        _ => None,
    }
}

/// The help lines for the [`SharedKey`]s, under the keymap in every
/// browser's help.
pub fn shared_key_help() -> [&'static str; 2] {
    [
        "  ctrl-l           redraw the screen",
        if cfg!(windows) {
            "  ctrl-z           nothing on Windows, which has no job control"
        } else {
            "  ctrl-z           suspend to the shell; fg comes back"
        },
    ]
}

/// Stop as the terminal's own suspend key would, once the screen is the
/// shell's again, and take the screen back when a SIGCONT resumes this
/// process. Started with SIGTSTP ignored (no job control, as under
/// nohup), it does nothing, since nothing would ever resume it.
#[cfg(unix)]
fn suspend(terminal: &mut Term) -> io::Result<()> {
    if tstp_ignored() {
        return Ok(());
    }
    leave(terminal);
    // SAFETY: plain signal calls on zeroed sigsets this fn owns. SIGTSTP
    // is blocked in this thread while it goes to the process group (the
    // shell's job) and to this thread, so whichever thread takes it, this
    // one stops when the mask comes back, before it resumes the screen.
    unsafe {
        let mut tstp: libc::sigset_t = std::mem::zeroed();
        let mut old: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut tstp);
        libc::sigaddset(&mut tstp, libc::SIGTSTP);
        libc::pthread_sigmask(libc::SIG_BLOCK, &tstp, &mut old);
        libc::kill(0, libc::SIGTSTP);
        libc::pthread_kill(libc::pthread_self(), libc::SIGTSTP);
        libc::pthread_sigmask(libc::SIG_SETMASK, &old, std::ptr::null_mut());
    }
    hold_stderr();
    enable_raw_mode()?;
    execute!(terminal.backend_mut(), EnterAlternateScreen)?;
    terminal.clear()
}

/// Whether SIGTSTP is ignored, so a stop would never come.
#[cfg(unix)]
fn tstp_ignored() -> bool {
    // SAFETY: sigaction with no new action only reads the current one.
    unsafe {
        let mut now: libc::sigaction = std::mem::zeroed();
        libc::sigaction(libc::SIGTSTP, std::ptr::null(), &mut now) == 0
            && now.sa_sigaction == libc::SIG_IGN
    }
}

/// No job control here: the key is taken so no browser sees a stray
/// Ctrl-Z, and the screen stays.
#[cfg(not(unix))]
fn suspend(_terminal: &mut Term) -> io::Result<()> {
    Ok(())
}

/// The shell a browser opens: `$SHELL`, else `/bin/sh`; on Windows
/// `%COMSPEC%`, else `cmd.exe`.
pub fn shell_program() -> std::ffi::OsString {
    let var = if cfg!(windows) { "COMSPEC" } else { "SHELL" };
    shell_or_default(std::env::var_os(var))
}

/// `set`, unless it is missing or empty: then the platform's own shell.
fn shell_or_default(set: Option<std::ffi::OsString>) -> std::ffi::OsString {
    match set {
        Some(s) if !s.is_empty() => s,
        _ if cfg!(windows) => "cmd.exe".into(),
        _ => "/bin/sh".into(),
    }
}

/// The pagers `v` tries, in order, to show `file`: `$PAGER`, split at
/// spaces, else `less` and then `more` (just `more` on Windows).
pub fn pager_commands(file: &std::path::Path) -> Vec<std::process::Command> {
    let fallback: &[&str] = if cfg!(windows) {
        &["more"]
    } else {
        &["less", "more"]
    };
    pager_lines(std::env::var_os("PAGER"), fallback)
        .into_iter()
        .map(|line| {
            let mut cmd = std::process::Command::new(&line[0]);
            cmd.args(&line[1..]).arg(file);
            cmd
        })
        .collect()
}

/// `set` split at spaces, unless it is missing or blank: then each of
/// `fallback`. No line is empty.
fn pager_lines(set: Option<std::ffi::OsString>, fallback: &[&str]) -> Vec<Vec<std::ffi::OsString>> {
    let words: Vec<std::ffi::OsString> = set
        .as_deref()
        .map(|s| {
            s.to_string_lossy()
                .split_whitespace()
                .map(std::ffi::OsString::from)
                .collect()
        })
        .unwrap_or_default();
    if words.is_empty() {
        fallback.iter().map(|p| vec![p.into()]).collect()
    } else {
        vec![words]
    }
}

/// The program that opens a path with its default application:
/// `$RTRASH_OPENER`, split at spaces (gdu #584), else `xdg-open`, `open`
/// on macOS, `explorer.exe` on Windows.
pub fn opener() -> Vec<std::ffi::OsString> {
    opener_words(std::env::var_os("RTRASH_OPENER"))
}

/// The words of [`opener`] for the value `set`: its own, unless it is
/// missing or blank.
fn opener_words(set: Option<std::ffi::OsString>) -> Vec<std::ffi::OsString> {
    let program = if cfg!(windows) {
        "explorer.exe"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    pager_lines(set, &[program])
        .into_iter()
        .next()
        .unwrap_or_else(|| vec![program.into()])
}

/// Hand the terminal to an interactive shell in `dir`, after printing
/// `banner`, and take the screen back once the shell exits, also when it
/// could not start. While the shell runs, Ctrl-C reaches it and the
/// programs it starts, but not this process.
pub fn run_shell(
    terminal: &mut Term,
    dir: &std::path::Path,
    banner: &str,
) -> io::Result<std::process::ExitStatus> {
    run_in_terminal(
        terminal,
        vec![std::process::Command::new(shell_program())],
        dir,
        Some(banner),
    )
}

/// Hand the terminal to the first of `cmds` that exists, run in `dir`
/// after printing `banner` when there is one, as [`run_shell`] does for a
/// shell. With none found, the error is the last one's.
pub fn run_in_terminal(
    terminal: &mut Term,
    cmds: Vec<std::process::Command>,
    dir: &std::path::Path,
    banner: Option<&str>,
) -> io::Result<std::process::ExitStatus> {
    leave(terminal);
    if let Some(b) = banner {
        println!("{b}");
    }
    let mut status = Err(io::Error::from(io::ErrorKind::NotFound));
    for cmd in cmds {
        status = wait_for(cmd, dir);
        if !matches!(&status, Err(e) if e.kind() == io::ErrorKind::NotFound) {
            break;
        }
    }
    hold_stderr();
    enable_raw_mode()?;
    execute!(terminal.backend_mut(), EnterAlternateScreen)?;
    terminal.clear()?;
    status
}

/// Run `cmd` in `dir` to the end, with the terminal's interrupt keys
/// going to it alone.
#[cfg(unix)]
fn wait_for(
    mut cmd: std::process::Command,
    dir: &std::path::Path,
) -> io::Result<std::process::ExitStatus> {
    use std::os::unix::process::CommandExt;
    cmd.current_dir(dir);
    // As system(3) does: this process ignores the terminal's interrupt
    // and quit keys while it waits, and the shell gets the defaults back.
    // SAFETY: signal(2) is async-signal-safe, so it may run between fork
    // and exec.
    unsafe {
        cmd.pre_exec(|| {
            libc::signal(libc::SIGINT, libc::SIG_DFL);
            libc::signal(libc::SIGQUIT, libc::SIG_DFL);
            Ok(())
        });
    }
    // SAFETY: plain signal(2) calls; the old handlers are put back below.
    let old = unsafe {
        (
            libc::signal(libc::SIGINT, libc::SIG_IGN),
            libc::signal(libc::SIGQUIT, libc::SIG_IGN),
        )
    };
    let status = cmd.status();
    // SAFETY: as above.
    unsafe {
        libc::signal(libc::SIGINT, old.0);
        libc::signal(libc::SIGQUIT, old.1);
    }
    status
}

#[cfg(windows)]
fn wait_for(
    mut cmd: std::process::Command,
    dir: &std::path::Path,
) -> io::Result<std::process::ExitStatus> {
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
    // A handler that takes Ctrl-C and Ctrl-Break keeps this process alive
    // while the shell runs. Unlike ignoring them, a handler is not
    // inherited, so programs the shell starts can still be stopped.
    unsafe extern "system" fn swallow(_: u32) -> windows_sys::Win32::Foundation::BOOL {
        1
    }
    // SAFETY: registers and later removes a handler that touches nothing.
    unsafe { SetConsoleCtrlHandler(Some(swallow), 1) };
    let status = cmd.current_dir(dir).status();
    // SAFETY: as above.
    unsafe { SetConsoleCtrlHandler(Some(swallow), 0) };
    status
}

/// Messages written to stderr while a browser owns the terminal land on top
/// of the frame and vanish at the next redraw. While the browser runs, send
/// fd 2 to an unlinked temp file when it is a terminal, then replay the file
/// once the screen is back.
#[cfg(unix)]
static HELD_STDERR: std::sync::Mutex<Option<(libc::c_int, std::fs::File)>> =
    std::sync::Mutex::new(None);

#[cfg(unix)]
fn hold_stderr() {
    // SAFETY: isatty only inspects fd 2.
    if unsafe { libc::isatty(2) } == 1 {
        hold_fd2();
    }
}

#[cfg(unix)]
fn hold_fd2() {
    use std::os::fd::AsRawFd;
    let mut held = HELD_STDERR.lock().unwrap_or_else(|e| e.into_inner());
    if held.is_some() {
        return;
    }
    let Ok(file) = tempfile_unlinked() else {
        return;
    };
    // SAFETY: dup and dup2 on fd 2 and a file we own; on failure fd 2 stays.
    unsafe {
        let saved = libc::dup(2);
        if saved < 0 {
            return;
        }
        if libc::dup2(file.as_raw_fd(), 2) < 0 {
            libc::close(saved);
            return;
        }
        *held = Some((saved, file));
    }
}

#[cfg(unix)]
fn release_stderr() {
    use std::io::Write;
    let text = take_held();
    if !text.is_empty() {
        let _ = io::stderr().write_all(&text);
    }
}

/// Put fd 2 back and return what was written to it while held.
#[cfg(unix)]
fn take_held() -> Vec<u8> {
    use std::io::{Read, Seek};
    let mut held = HELD_STDERR.lock().unwrap_or_else(|e| e.into_inner());
    let Some((saved, mut file)) = held.take() else {
        return Vec::new();
    };
    // SAFETY: put the saved terminal fd back on 2, then drop the copy.
    unsafe {
        libc::dup2(saved, 2);
        libc::close(saved);
    }
    let mut text = Vec::new();
    if file.rewind().is_err() || file.read_to_end(&mut text).is_err() {
        text.clear();
    }
    text
}

#[cfg(unix)]
fn tempfile_unlinked() -> io::Result<std::fs::File> {
    let dir = std::env::temp_dir();
    for n in 0..100u32 {
        let path = dir.join(format!(".rtrash-tui-stderr-{}-{n}", std::process::id()));
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(f) => {
                let _ = std::fs::remove_file(&path);
                return Ok(f);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "no free temp name",
    ))
}

#[cfg(not(unix))]
fn hold_stderr() {}

#[cfg(not(unix))]
fn release_stderr() {}

/// Footer height: the key hints and the status line, inside a border. With
/// three rows only the hints fit, and prompts in the status line vanish.
pub const FOOTER_ROWS: u16 = 4;

/// Draw one frame off screen and return its text, one line per row.
/// The key help overlay every browser shares: the keymap, then `extras`
/// under "This browser".
pub fn draw_help_overlay(f: &mut ratatui::Frame, keys: &crate::tui_binds::Keymap, extras: &[&str]) {
    let area = {
        let r = f.area();
        let v = ratatui::layout::Layout::default()
            .direction(ratatui::layout::Direction::Vertical)
            .constraints([
                ratatui::layout::Constraint::Percentage(5),
                ratatui::layout::Constraint::Percentage(90),
                ratatui::layout::Constraint::Percentage(5),
            ])
            .split(r);
        ratatui::layout::Layout::default()
            .direction(ratatui::layout::Direction::Horizontal)
            .constraints([
                ratatui::layout::Constraint::Percentage(8),
                ratatui::layout::Constraint::Percentage(84),
                ratatui::layout::Constraint::Percentage(8),
            ])
            .split(v[1])[1]
    };
    f.render_widget(ratatui::widgets::Clear, area);
    let mut lines: Vec<ratatui::text::Line> = keys
        .help_lines()
        .into_iter()
        .map(ratatui::text::Line::from)
        .collect();
    if !extras.is_empty() {
        lines.push(ratatui::text::Line::from(""));
        lines.push(ratatui::text::Line::from("This browser"));
        for e in extras {
            lines.push(ratatui::text::Line::from(format!("  {e}")));
        }
    }
    f.render_widget(
        ratatui::widgets::Paragraph::new(lines).block(
            ratatui::widgets::Block::default()
                .borders(ratatui::widgets::Borders::ALL)
                .title(" help "),
        ),
        area,
    );
}

#[cfg(test)]
pub(crate) fn render_text(
    width: u16,
    height: u16,
    draw: impl FnOnce(&mut ratatui::Frame),
) -> String {
    let mut t = Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
    t.draw(draw).unwrap();
    let buf = t.backend().buffer();
    let mut out = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn suspend_stays_put_when_sigtstp_is_ignored() {
        // SAFETY: swaps the SIGTSTP disposition and puts the old one back.
        let old = unsafe { libc::signal(libc::SIGTSTP, libc::SIG_IGN) };
        let ignored = tstp_ignored();
        // SAFETY: as above.
        unsafe { libc::signal(libc::SIGTSTP, libc::SIG_DFL) };
        let default = tstp_ignored();
        // SAFETY: as above.
        unsafe { libc::signal(libc::SIGTSTP, old) };
        assert!(ignored, "SIG_IGN reads as ignored");
        assert!(!default, "SIG_DFL does not");
    }

    #[test]
    fn held_stderr_comes_back_after_release() {
        hold_fd2();
        let msg = b"rtrash: written while a browser ran\n";
        // SAFETY: plain write to fd 2, which now points at the temp file.
        let n = unsafe { libc::write(2, msg.as_ptr().cast(), msg.len()) };
        assert_eq!(n, msg.len() as isize);
        assert_eq!(take_held(), msg);
        // Released: nothing is held any more.
        assert!(take_held().is_empty());
    }
}

#[cfg(test)]
mod shell_tests {
    use super::*;

    #[test]
    fn ctrl_l_and_ctrl_z_are_shared_keys_and_nothing_else_is() {
        let ctrl = KeyModifiers::CONTROL;
        let shift = KeyModifiers::SHIFT;
        for (code, mods, want) in [
            (KeyCode::Char('l'), ctrl, Some(SharedKey::Redraw)),
            (KeyCode::Char('L'), ctrl | shift, Some(SharedKey::Redraw)),
            (KeyCode::Char('z'), ctrl, Some(SharedKey::Suspend)),
            (KeyCode::Char('Z'), ctrl | shift, Some(SharedKey::Suspend)),
            // Plain l opens a directory in the explorer; it stays a key.
            (KeyCode::Char('l'), KeyModifiers::NONE, None),
            (KeyCode::Char('z'), KeyModifiers::NONE, None),
            (KeyCode::Char('l'), ctrl | KeyModifiers::ALT, None),
            (KeyCode::Char('c'), ctrl, None),
        ] {
            assert_eq!(shared_key(code, mods), want, "{code:?} {mods:?}");
        }
        let help = shared_key_help();
        assert!(help[0].contains("ctrl-l") && help[0].contains("redraw"));
        assert!(help[1].contains("ctrl-z"));
        assert_eq!(help[1].contains("Windows"), cfg!(windows), "{}", help[1]);
        // Every browser's help shows them under the keymap.
        let lines = crate::tui_binds::Keymap::builtin().help_lines();
        assert!(lines.iter().any(|l| l == "Every browser"), "{lines:?}");
        assert!(lines.iter().any(|l| l == help[1]), "{lines:?}");
    }

    #[test]
    fn the_pager_comes_from_the_environment_split_at_spaces() {
        let fallback = &["less", "more"];
        assert_eq!(
            pager_lines(Some("most -s".into()), fallback),
            [["most", "-s"]]
        );
        assert_eq!(
            pager_lines(Some("  ".into()), fallback),
            [["less"], ["more"]]
        );
        assert_eq!(pager_lines(None, &["more"]), [["more"]]);
        for cmd in pager_commands(std::path::Path::new("f.txt")) {
            assert_eq!(cmd.get_args().last().unwrap(), "f.txt");
        }
        assert_eq!(opener_words(None).len(), 1);
    }

    #[test]
    fn rtrash_opener_names_the_program_and_its_arguments() {
        assert_eq!(opener_words(Some("code -r".into())), ["code", "-r"]);
        assert_eq!(opener_words(Some("  ".into())), opener_words(None));
    }

    #[test]
    fn the_shell_comes_from_the_environment_or_the_platform() {
        assert_eq!(shell_or_default(Some("/bin/zsh".into())), "/bin/zsh");
        let own = if cfg!(windows) { "cmd.exe" } else { "/bin/sh" };
        assert_eq!(shell_or_default(Some("".into())), own);
        assert_eq!(shell_or_default(None), own);
    }

    #[cfg(unix)]
    #[test]
    fn the_shell_runs_in_the_directory_with_the_interrupt_key_live() {
        use std::os::unix::process::ExitStatusExt;
        let dir = std::env::temp_dir().canonicalize().unwrap();
        let mut pwd = std::process::Command::new("/bin/sh");
        pwd.args(["-c", "test \"$(pwd -P)\" = \"$1\"", "sh"])
            .arg(&dir);
        assert!(wait_for(pwd, &dir).unwrap().success());
        // This process ignores SIGINT while it waits; the child must not,
        // or Ctrl-C could not stop what runs in the shell.
        let mut int = std::process::Command::new("/bin/sh");
        int.args(["-c", "kill -INT $$; sleep 5"]);
        assert_eq!(wait_for(int, &dir).unwrap().signal(), Some(libc::SIGINT));
    }
}
