//! The answers rtrash reads at a prompt, parsed in one place.
//!
//! A yes is `y` or `yes` in any case, with blanks around it ignored.
//! Anything else is a no: `n`, `yolo`, `y please`, an empty line, the end
//! of input and a read error. A pipe answers like a terminal, so
//! `echo n | rtrash empty -i` keeps the trash (trash-cli #333), and an
//! answer that only starts with `y` no longer counts, as it did for put
//! `-i`. A prompt that picks items (restore) reads its line here too and
//! parses it with `util::parse_selection`. The precheck refuses a prompt
//! that reads stdin or matches a yes by itself.

use std::io::{self, BufRead, Write};

/// Whether `reply` is a yes: `y` or `yes` in any case, blanks trimmed.
pub fn is_yes(reply: &str) -> bool {
    let r = reply.trim();
    r.eq_ignore_ascii_case("y") || r.eq_ignore_ascii_case("yes")
}

/// One line of stdin with its newline; empty at the end of input.
pub fn read_reply() -> io::Result<String> {
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    Ok(line)
}

/// Write `prompt` to stderr, read the answer from stdin: true for a yes.
pub fn ask(prompt: &str) -> bool {
    eprint!("{prompt}");
    let _ = io::stderr().flush();
    read_reply().is_ok_and(|l| is_yes(&l))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_y_or_yes_in_any_case_is_a_yes() {
        for yes in ["y\n", "Y", " yes\n", "YES", "Yes\r\n", "\ty \n"] {
            assert!(is_yes(yes), "{yes:?}");
        }
        for no in ["", "\n", "n", "no", "yess", "y please", "yolo", "ja", "1"] {
            assert!(!is_yes(no), "{no:?}");
        }
    }
}
