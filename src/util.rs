use std::io::{self, BufRead, Write};

/// Percent-encode a byte string for a `.trashinfo` `Path=` value.
/// Unreserved characters (RFC 3986) and `/` pass through; everything else,
/// including spaces and non-UTF8 bytes, becomes `%XX`.
pub fn url_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for &b in bytes {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => {
                out.push('%');
                out.push(HEX[(b >> 4) as usize] as char);
                out.push(HEX[(b & 0xf) as usize] as char);
            }
        }
    }
    out
}

const HEX: &[u8; 16] = b"0123456789ABCDEF";

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// First code point of the range that carries raw bytes through a `String`.
/// A byte `b` that is not part of valid UTF-8 becomes `U+10FF00 + b`, a
/// private-use character. Byte values below 0x80 are always valid, so
/// only `U+10FF80..=U+10FFFF` ever shows up.
const RAW_BYTE_BASE: u32 = 0x10_FF00;

/// Turn an OS string (an argument or a file name) into a `String` without
/// losing bytes. Valid UTF-8 passes through; each stray byte becomes one
/// private-use character that [`os_from_str`] turns back into that byte.
/// Windows strings are UTF-16 and almost always valid, so they go through
/// lossy conversion there.
pub fn str_from_os(s: std::ffi::OsString) -> String {
    match s.into_string() {
        Ok(s) => s,
        Err(s) => escape_bytes(&os_bytes(&s)),
    }
}

/// Bytes read from a file as a string that keeps stray bytes, the way
/// [`str_from_os`] keeps them in names; [`os_from_str`] turns it back.
pub fn str_from_bytes(b: Vec<u8>) -> String {
    match String::from_utf8(b) {
        Ok(s) => s,
        Err(e) => escape_bytes(e.as_bytes()),
    }
}

/// [`str_from_os`] for a borrowed name.
pub fn str_from_os_ref(s: &std::ffi::OsStr) -> String {
    match s.to_str() {
        Some(s) => s.to_string(),
        None => escape_bytes(&os_bytes(s)),
    }
}

#[cfg(unix)]
fn os_bytes(s: &std::ffi::OsStr) -> std::borrow::Cow<'_, [u8]> {
    use std::os::unix::ffi::OsStrExt;
    std::borrow::Cow::Borrowed(s.as_bytes())
}

#[cfg(not(unix))]
fn os_bytes(s: &std::ffi::OsStr) -> std::borrow::Cow<'_, [u8]> {
    std::borrow::Cow::Owned(s.to_string_lossy().into_owned().into_bytes())
}

fn escape_bytes(mut b: &[u8]) -> String {
    let mut out = String::with_capacity(b.len() + 8);
    loop {
        match std::str::from_utf8(b) {
            Ok(s) => {
                out.push_str(s);
                return out;
            }
            Err(e) => {
                let (good, rest) = b.split_at(e.valid_up_to());
                // from_utf8 already checked this prefix.
                out.push_str(std::str::from_utf8(good).unwrap_or_default());
                let bad = e.error_len().unwrap_or(rest.len());
                for &x in &rest[..bad] {
                    out.extend(char::from_u32(RAW_BYTE_BASE + u32::from(x)));
                }
                b = &rest[bad..];
            }
        }
    }
}

/// Inverse of [`str_from_os`]: private-use characters in the raw-byte
/// range become their bytes again.
pub fn os_from_str(s: &str) -> std::ffi::OsString {
    let raw = |c: char| (RAW_BYTE_BASE + 0x80..=RAW_BYTE_BASE + 0xFF).contains(&u32::from(c));
    if !s.chars().any(raw) {
        return s.into();
    }
    let mut out = Vec::with_capacity(s.len());
    for c in s.chars() {
        if raw(c) {
            out.push((u32::from(c) - RAW_BYTE_BASE) as u8);
        } else {
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        }
    }
    os_from_bytes(out)
}

#[cfg(unix)]
fn os_from_bytes(b: Vec<u8>) -> std::ffi::OsString {
    use std::os::unix::ffi::OsStringExt;
    std::ffi::OsString::from_vec(b)
}

#[cfg(not(unix))]
fn os_from_bytes(b: Vec<u8>) -> std::ffi::OsString {
    String::from_utf8_lossy(&b).into_owned().into()
}

/// A path argument as the OS spells it: [`os_from_str`] as a `PathBuf`.
pub fn arg_path(s: &str) -> std::path::PathBuf {
    os_from_str(s).into()
}

/// A name kept by [`str_from_os`] as text for people and JSON: stray bytes
/// show as U+FFFD, like a lossy path.
pub fn show_name(name: &str) -> String {
    os_from_str(name).to_string_lossy().into_owned()
}

/// `dir/NAME` for a name kept by [`str_from_os`].
pub fn join_name(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    dir.join(os_from_str(name))
}

/// `dir/NAME.trashinfo` for a name kept by [`str_from_os`].
pub fn join_info(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    dir.join(os_from_str(&format!("{name}.trashinfo")))
}

/// Decode `%XX` sequences; malformed escapes pass through verbatim.
pub fn url_decode(s: &str) -> Vec<u8> {
    url_decode_bytes(s.as_bytes())
}

/// [`url_decode`] for a raw byte value.
pub fn url_decode_bytes(b: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex_val(b[i + 1]), hex_val(b[i + 2])) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

/// Print `prompt` to stderr and read one line from stdin; true on y/Y.
pub fn confirm(prompt: &str) -> bool {
    eprint!("{prompt}");
    io::stderr().flush().ok();
    let mut line = String::new();
    if io::stdin().lock().read_line(&mut line).is_err() {
        return false;
    }
    matches!(
        line.trim_start().as_bytes().first(),
        Some(b'y') | Some(b'Y')
    )
}

/// `1 item`, `2 items`: a count with its noun in the right number.
pub fn plural(n: u64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// Human-readable binary units (KiB/MiB/GiB) for CLI summaries.
pub fn format_bytes(n: u64) -> String {
    const K: f64 = 1024.0;
    if n < 1024 {
        format!("{n} B")
    } else if (n as f64) < K * K {
        format!("{:.1} KiB", n as f64 / K)
    } else if (n as f64) < K * K * K {
        format!("{:.1} MiB", n as f64 / (K * K))
    } else {
        format!("{:.2} GiB", n as f64 / (K * K * K))
    }
}

/// Escape a string for JSON (UTF-8, control chars).
pub fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

/// The `put --json` summary: each moved item as (original path, name in
/// the trash, trash root), plus the count of failures. Shared by the
/// FreeDesktop and Recycle Bin backends so both print the same shape.
pub fn put_summary_json(
    items: &[(&std::path::Path, &str, &std::path::Path)],
    failed: usize,
) -> String {
    items_json("trashed", items, failed)
}

/// `{"<key>": [{original, name, trash_dir}...], "failed": N}`, the shape
/// of the `put --json` and `restore --json` summaries.
pub fn items_json(
    key: &str,
    items: &[(&std::path::Path, &str, &std::path::Path)],
    failed: usize,
) -> String {
    let items: Vec<String> = items
        .iter()
        .map(|(original, name, dir)| {
            format!(
                "{{\"original\":\"{}\",\"name\":\"{}\",\"trash_dir\":\"{}\"}}",
                json_escape(&original.to_string_lossy()),
                json_escape(&show_name(name)),
                json_escape(&dir.to_string_lossy()),
            )
        })
        .collect();
    format!(
        "{{\"{}\":[{}],\"failed\":{failed}}}",
        json_escape(key),
        items.join(",")
    )
}

/// The home directory from `HOME` (`USERPROFILE` on Windows), if set
/// to an absolute path.
pub fn home_dir() -> Option<std::path::PathBuf> {
    let v = std::env::var_os("HOME").filter(|v| !v.is_empty());
    #[cfg(windows)]
    let v = v.or_else(|| std::env::var_os("USERPROFILE").filter(|v| !v.is_empty()));
    v.map(std::path::PathBuf::from).filter(|p| p.is_absolute())
}

/// Seconds since the Unix epoch, now.
pub fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn stdin_is_tty() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal()
}

/// Whether stdout is a terminal, checked once per process.
pub fn stdout_is_tty() -> bool {
    use std::io::IsTerminal;
    static TTY: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *TTY.get_or_init(|| io::stdout().is_terminal())
}

/// Whether stderr is a terminal, checked once per process.
pub fn stderr_is_tty() -> bool {
    use std::io::IsTerminal;
    static TTY: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *TTY.get_or_init(|| io::stderr().is_terminal())
}

/// Characters a terminal would act on instead of print: C0 and C1 controls,
/// DEL and the bidirectional overrides that reorder the rest of a line.
fn is_unsafe_char(c: char) -> bool {
    c.is_control() || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

/// Replace each unsafe character with `?`, as `ls` does on a terminal.
///
/// A trashed name (or the `Path=` line of a `.trashinfo` in a shared trash)
/// can carry escape sequences or newlines that would rewrite the screen or
/// fake extra rows.
pub fn scrub(s: &str) -> std::borrow::Cow<'_, str> {
    if !s.chars().any(is_unsafe_char) {
        return std::borrow::Cow::Borrowed(s);
    }
    std::borrow::Cow::Owned(
        s.chars()
            .map(|c| if is_unsafe_char(c) { '?' } else { c })
            .collect(),
    )
}

/// `s` made safe for stdout: scrubbed on a terminal, raw in a pipe.
pub fn term_text(s: &str) -> std::borrow::Cow<'_, str> {
    if stdout_is_tty() {
        scrub(s)
    } else {
        std::borrow::Cow::Borrowed(s)
    }
}

/// A path for human output on stdout (see [`term_text`]).
pub fn term_path(p: &std::path::Path) -> String {
    let shown = p.display().to_string();
    match term_text(&shown) {
        std::borrow::Cow::Borrowed(_) => shown,
        std::borrow::Cow::Owned(o) => o,
    }
}

/// A path for messages on stderr: scrubbed when stderr is a terminal.
pub fn err_path(p: &std::path::Path) -> String {
    let shown = p.display().to_string();
    if stderr_is_tty() {
        scrub(&shown).into_owned()
    } else {
        shown
    }
}

/// Shell-style glob match (`*`, `?`, `[…]` / `[!…]`), as used by trash-cli `trash-rm`.
/// Matching is on the full string (callers also try the basename separately).
pub fn fnmatch(pat: &str, text: &str) -> bool {
    fnmatch_bytes(pat.as_bytes(), text.as_bytes())
}

fn fnmatch_bytes(pat: &[u8], text: &[u8]) -> bool {
    let mut pi = 0;
    let mut ti = 0;
    let mut star_p: Option<usize> = None;
    let mut star_t: usize = 0;
    while ti < text.len() {
        if pi < pat.len() {
            match pat[pi] {
                b'*' => {
                    star_p = Some(pi);
                    star_t = ti;
                    pi += 1;
                    continue;
                }
                b'?' => {
                    pi += 1;
                    ti += 1;
                    continue;
                }
                b'[' => {
                    if let Some((next_p, ok)) = class_match(&pat[pi..], text[ti]) {
                        if ok {
                            pi += next_p;
                            ti += 1;
                            continue;
                        }
                    }
                }
                c if c == text[ti] => {
                    pi += 1;
                    ti += 1;
                    continue;
                }
                _ => {}
            }
        }
        if let Some(sp) = star_p {
            pi = sp + 1;
            star_t += 1;
            ti = star_t;
            continue;
        }
        return false;
    }
    while pi < pat.len() && pat[pi] == b'*' {
        pi += 1;
    }
    pi == pat.len()
}

/// Parse a `[…]` class starting at `pat[0] == b'['`. Returns (bytes consumed, matched).
fn class_match(pat: &[u8], ch: u8) -> Option<(usize, bool)> {
    if pat.first() != Some(&b'[') || pat.len() < 3 {
        return None;
    }
    let mut i = 1;
    let mut negated = false;
    if pat[i] == b'!' || pat[i] == b'^' {
        negated = true;
        i += 1;
    }
    let mut matched = false;
    let mut first = true;
    while i < pat.len() {
        if pat[i] == b']' && !first {
            let ok = if negated { !matched } else { matched };
            return Some((i + 1, ok));
        }
        first = false;
        if i + 2 < pat.len() && pat[i + 1] == b'-' && pat[i + 2] != b']' {
            let lo = pat[i];
            let hi = pat[i + 2];
            if (lo..=hi).contains(&ch) {
                matched = true;
            }
            i += 3;
        } else {
            if pat[i] == ch {
                matched = true;
            }
            i += 1;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrub_replaces_controls_and_keeps_text() {
        assert!(matches!(
            scrub("plain/ünïcode name"),
            std::borrow::Cow::Borrowed(_)
        ));
        assert_eq!(scrub("a\x1b[2Jb"), "a?[2Jb");
        assert_eq!(scrub("one\ntwo\rthree\tfour"), "one?two?three?four");
        assert_eq!(scrub("del\x7f c1\u{9b}"), "del? c1?");
        assert_eq!(scrub("txt.\u{202e}exe\u{2066}"), "txt.?exe?");
    }

    #[test]
    fn plural_matches_the_count() {
        assert_eq!(plural(1, "item", "items"), "1 item");
        assert_eq!(plural(0, "item", "items"), "0 items");
        assert_eq!(plural(2, "directory", "directories"), "2 directories");
    }

    #[test]
    fn encode_plain_path() {
        assert_eq!(url_encode(b"/home/user/file.txt"), "/home/user/file.txt");
    }

    #[test]
    fn encode_space_and_percent() {
        assert_eq!(url_encode(b"/a b/c%d"), "/a%20b/c%25d");
    }

    #[test]
    fn roundtrip_non_utf8() {
        let raw: &[u8] = &[b'/', 0xff, b' ', 0x01, b'x'];
        assert_eq!(url_decode(&url_encode(raw)), raw);
    }

    #[test]
    fn decode_malformed_passthrough() {
        assert_eq!(url_decode("%zz%4"), b"%zz%4");
    }

    #[test]
    fn fnmatch_star_question_class() {
        assert!(fnmatch("*.o", "a.o"));
        assert!(fnmatch("foo?", "food"));
        assert!(!fnmatch("foo?", "foo"));
        assert!(fnmatch("file[0-9].txt", "file3.txt"));
        assert!(!fnmatch("file[0-9].txt", "filea.txt"));
        assert!(fnmatch("[!a]*", "bcd"));
        assert!(!fnmatch("[!a]*", "abc"));
        assert!(fnmatch("*", "anything"));
        assert!(fnmatch("exact", "exact"));
        assert!(!fnmatch("exact", "exactx"));
    }
}
