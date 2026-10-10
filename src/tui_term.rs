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
