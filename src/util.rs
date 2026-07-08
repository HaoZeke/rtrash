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

/// Decode `%XX` sequences; malformed escapes pass through verbatim.
pub fn url_decode(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
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

pub fn stdin_is_tty() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal()
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
