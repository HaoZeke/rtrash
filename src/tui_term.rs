//! Shared alternate-screen setup for TUI modes.

use std::io::{self, stdout, Stdout};

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

/// Hand the terminal to an interactive shell in `dir`, after printing
/// `banner`, and take the screen back once the shell exits, also when it
/// could not start. While the shell runs, Ctrl-C reaches it and the
/// programs it starts, but not this process.
pub fn run_shell(
    terminal: &mut Term,
    dir: &std::path::Path,
    banner: &str,
) -> io::Result<std::process::ExitStatus> {
    leave(terminal);
    println!("{banner}");
    let status = wait_for(std::process::Command::new(shell_program()), dir);
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
