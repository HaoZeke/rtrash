//! The question `rm` asks before it deletes trashed items for good.
//!
//! On a terminal it names the matches and asks once, as `rm -I` does and
//! as trash-cli #383 asks of `trash-rm`. A script, whose stdin or stderr
//! is not a terminal, gets no question and works as before. `-i` asks
//! even then (reading the answer from stdin); `-f` and `-y` never ask.
//! The last of the three on the command line wins, as with rm(1).

use std::sync::atomic::{AtomicU8, Ordering};

/// Matches named before the question; the rest are counted.
pub const SHOWN: usize = 10;

/// When `rm` asks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// On a terminal only (the default).
    Auto = 0,
    /// Always (`-i`).
    Always = 1,
    /// Never (`-f`, `-y`).
    Never = 2,
}

static MODE: AtomicU8 = AtomicU8::new(Mode::Auto as u8);

/// Set the mode from `-i`, `-f` or `-y`; a later one replaces it.
pub fn set(mode: Mode) {
    MODE.store(mode as u8, Ordering::Relaxed);
}

fn mode() -> Mode {
    match MODE.load(Ordering::Relaxed) {
        1 => Mode::Always,
        2 => Mode::Never,
        _ => Mode::Auto,
    }
}

/// Whether `rm` asks under `mode`. A dry run deletes nothing and never
/// asks; [`Mode::Auto`] asks only on a terminal and not for `--json`.
pub fn asks(mode: Mode, dry_run: bool, json: bool, tty: bool) -> bool {
    !dry_run
        && match mode {
            Mode::Always => true,
            Mode::Never => false,
            Mode::Auto => tty && !json,
        }
}

/// Whether this run asks: stdin and stderr must both be terminals for
/// the default, so a pipe or a captured stderr never waits on a reply.
pub fn would_ask(dry_run: bool, json: bool) -> bool {
    let tty = crate::util::stdin_is_tty() && crate::util::stderr_is_tty();
    asks(mode(), dry_run, json, tty)
}

/// What is shown before the reply: up to [`SHOWN`] matches by path with
/// their sizes, a count of the rest, then the question.
pub fn prompt(matches: &[(String, u64)]) -> String {
    let mut text = String::new();
    for (path, size) in matches.iter().take(SHOWN) {
        text += &format!("  {path} ({})\n", crate::util::format_bytes(*size));
    }
    if matches.len() > SHOWN {
        text += &format!("  ... and {} more\n", matches.len() - SHOWN);
    }
    let total: u64 = matches.iter().map(|m| m.1).sum();
    text += &format!(
        "Permanently remove {} ({}) from the trash? This cannot be undone. (y/N) ",
        crate::util::plural(matches.len() as u64, "item", "items"),
        crate::util::format_bytes(total)
    );
    text
}

/// Ask about `matches` on stderr and read the reply from stdin. True to
/// go on. With no match there is nothing to ask: stderr says so. A no
/// says that nothing was removed.
pub fn confirm(prog: &str, matches: &[(String, u64)]) -> bool {
    if matches.is_empty() {
        eprintln!("{prog}: no trashed item matches");
        return true;
    }
    let yes = crate::answer::ask(&prompt(matches));
    if !yes {
        eprintln!("{prog}: nothing removed");
    }
    yes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_terminal_asks_by_default() {
        assert!(asks(Mode::Auto, false, false, true));
        assert!(!asks(Mode::Auto, false, false, false));
        assert!(!asks(Mode::Auto, false, true, true));
        assert!(asks(Mode::Always, false, true, false));
        assert!(!asks(Mode::Never, false, false, true));
        // A dry run deletes nothing, so it never asks.
        assert!(!asks(Mode::Always, true, false, true));
    }

    #[test]
    fn the_prompt_names_ten_matches_and_counts_the_rest() {
        let matches: Vec<(String, u64)> = (0..12).map(|i| (format!("/w/f{i}"), 0)).collect();
        let text = prompt(&matches);
        assert_eq!(
            text.lines().filter(|l| l.starts_with("  /w/f")).count(),
            SHOWN
        );
        assert!(text.contains("  ... and 2 more\n"), "{text}");
        assert!(text.ends_with("(y/N) "), "{text}");
        assert!(text.contains("Permanently remove 12 items ("), "{text}");
    }
}
