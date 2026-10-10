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
    enable_raw_mode()?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen)?;
    Terminal::new(CrosstermBackend::new(out))
}

pub fn leave(terminal: &mut Term) {
    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let _ = terminal.show_cursor();
}

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
