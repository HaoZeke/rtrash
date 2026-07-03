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
    unsafe { libc::isatty(libc::STDIN_FILENO) == 1 }
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
}
