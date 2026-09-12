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

/// `--trash-dir=PATH` or `--trash-dir` (PATH is the next argv), matching trash-cli.
pub fn is_trash_dir_flag(arg: &str) -> bool {
    arg == "--trash-dir" || arg.starts_with("--trash-dir=")
}

/// Parse a byte size: `N`, `Nk`/`NK`, `Nm`/`NM`, `Ng`/`NG` (1024-based).
/// The suffix is a single letter; `KiB`/`MB` forms are rejected.
pub fn parse_bytes(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty size".into());
    }
    let (num, mul) = match s.as_bytes().last().copied() {
        Some(b'k' | b'K') => (&s[..s.len() - 1], 1024u64),
        Some(b'm' | b'M') => (&s[..s.len() - 1], 1024 * 1024),
        Some(b'g' | b'G') => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        Some(b) if b.is_ascii_digit() => (s, 1u64),
        _ => return Err(format!("invalid size '{s}'")),
    };
    let num = num.trim();
    if num.is_empty() {
        return Err(format!("invalid size '{s}'"));
    }
    let n: u64 = num
        .parse()
        .map_err(|_| format!("invalid size '{s}'"))?;
    n.checked_mul(mul)
        .ok_or_else(|| format!("size overflow '{s}'"))
}

/// `--name=VALUE` or `--name VALUE`. Advances `i` on the space form.
pub fn take_eq_or_next<'a>(
    args: &'a [String],
    i: &mut usize,
    name: &str,
) -> Result<&'a str, String> {
    let arg = args.get(*i).map(String::as_str).unwrap_or("");
    if let Some(rest) = arg.strip_prefix(&format!("{name}=")) {
        if rest.is_empty() {
            return Err(format!("{name} requires a value"));
        }
        return Ok(rest);
    }
    if arg == name {
        *i += 1;
        return args
            .get(*i)
            .map(String::as_str)
            .ok_or_else(|| format!("{name} requires a value"));
    }
    Err(format!("expected {name}"))
}

/// Consume a `--trash-dir` flag at `args[*i]`. On the space form, advances `i`
/// to the PATH slot. Missing PATH is an error for the caller to print as usage.
pub fn take_trash_dir_value(
    args: &[String],
    i: &mut usize,
) -> Result<std::path::PathBuf, &'static str> {
    let arg = args
        .get(*i)
        .map(String::as_str)
        .ok_or("--trash-dir requires PATH")?;
    if let Some(rest) = arg.strip_prefix("--trash-dir=") {
        return Ok(std::path::PathBuf::from(rest));
    }
    if arg == "--trash-dir" {
        *i += 1;
        return args
            .get(*i)
            .map(|s| std::path::PathBuf::from(s))
            .ok_or("--trash-dir requires PATH");
    }
    Err("--trash-dir requires PATH")
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

    #[test]
    fn trash_dir_equals_and_space_forms() {
        let eq = vec!["--trash-dir=/tmp/t".to_string()];
        let mut i = 0usize;
        assert!(is_trash_dir_flag(&eq[0]));
        assert_eq!(
            take_trash_dir_value(&eq, &mut i).unwrap(),
            std::path::PathBuf::from("/tmp/t")
        );
        assert_eq!(i, 0);

        let sp = vec!["--trash-dir".to_string(), "/tmp/u".to_string(), "x".into()];
        let mut j = 0usize;
        assert!(is_trash_dir_flag(&sp[0]));
        assert_eq!(
            take_trash_dir_value(&sp, &mut j).unwrap(),
            std::path::PathBuf::from("/tmp/u")
        );
        assert_eq!(j, 1);

        let missing = vec!["--trash-dir".to_string()];
        let mut k = 0usize;
        assert!(take_trash_dir_value(&missing, &mut k).is_err());
    }

    #[test]
    fn parse_bytes_plain_and_iec_suffixes() {
        assert_eq!(parse_bytes("0").unwrap(), 0);
        assert_eq!(parse_bytes("1234").unwrap(), 1234);
        assert_eq!(parse_bytes("20k").unwrap(), 20 * 1024);
        assert_eq!(parse_bytes("20K").unwrap(), 20 * 1024);
        assert_eq!(parse_bytes("2m").unwrap(), 2 * 1024 * 1024);
        assert_eq!(parse_bytes("1G").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_bytes(" 8k ").unwrap(), 8 * 1024);
        assert!(parse_bytes("").is_err());
        assert!(parse_bytes("k").is_err());
        assert!(parse_bytes("20KiB").is_err());
        assert!(parse_bytes("5GB").is_err());
        assert!(parse_bytes("-1").is_err());
        assert!(parse_bytes("1.5M").is_err());
    }

    #[test]
    fn take_eq_or_next_equals_and_space() {
        let eq = vec!["--max-bytes=10k".to_string()];
        let mut i = 0usize;
        assert_eq!(take_eq_or_next(&eq, &mut i, "--max-bytes").unwrap(), "10k");
        assert_eq!(i, 0);

        let sp = vec!["--larger-than".to_string(), "1M".into(), "x".into()];
        let mut j = 0usize;
        assert_eq!(take_eq_or_next(&sp, &mut j, "--larger-than").unwrap(), "1M");
        assert_eq!(j, 1);

        let missing = vec!["--smaller-than".to_string()];
        let mut k = 0usize;
        assert!(take_eq_or_next(&missing, &mut k, "--smaller-than").is_err());
    }
}
