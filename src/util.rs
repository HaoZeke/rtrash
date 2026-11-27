use std::io;

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
/// Windows strings are UTF-16 units that may hold an unpaired surrogate;
/// they pass through WTF-8 (see [`wtf8_encode`]), so such a unit becomes
/// stray bytes and round-trips the same way.
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

/// `s` in Unicode NFC, so equivalent composed and decomposed paths match.
pub fn nfc(s: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    s.nfc().collect()
}

#[cfg(unix)]
fn os_bytes(s: &std::ffi::OsStr) -> std::borrow::Cow<'_, [u8]> {
    use std::os::unix::ffi::OsStrExt;
    std::borrow::Cow::Borrowed(s.as_bytes())
}

#[cfg(windows)]
fn os_bytes(s: &std::ffi::OsStr) -> std::borrow::Cow<'_, [u8]> {
    use std::os::windows::ffi::OsStrExt;
    let units: Vec<u16> = s.encode_wide().collect();
    std::borrow::Cow::Owned(wtf8_encode(&units))
}

#[cfg(not(any(unix, windows)))]
fn os_bytes(s: &std::ffi::OsStr) -> std::borrow::Cow<'_, [u8]> {
    std::borrow::Cow::Owned(s.to_string_lossy().into_owned().into_bytes())
}

/// UTF-16 units as WTF-8: UTF-8, except that an unpaired surrogate is
/// written as its own three-byte sequence (`ED A0..BF xx`) instead of
/// failing. Valid UTF-16 gives plain UTF-8.
pub fn wtf8_encode(units: &[u16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(units.len());
    let mut push = |cp: u32| {
        if cp < 0x80 {
            out.push(cp as u8);
        } else if cp < 0x800 {
            out.extend([0xC0 | (cp >> 6) as u8, 0x80 | (cp & 0x3F) as u8]);
        } else if cp < 0x1_0000 {
            out.extend([
                0xE0 | (cp >> 12) as u8,
                0x80 | ((cp >> 6) & 0x3F) as u8,
                0x80 | (cp & 0x3F) as u8,
            ]);
        } else {
            out.extend([
                0xF0 | (cp >> 18) as u8,
                0x80 | ((cp >> 12) & 0x3F) as u8,
                0x80 | ((cp >> 6) & 0x3F) as u8,
                0x80 | (cp & 0x3F) as u8,
            ]);
        }
    };
    let mut i = 0;
    while i < units.len() {
        let u = u32::from(units[i]);
        let next = units.get(i + 1).map(|&n| u32::from(n));
        match (u, next) {
            (0xD800..=0xDBFF, Some(lo @ 0xDC00..=0xDFFF)) => {
                push(0x1_0000 + ((u - 0xD800) << 10) + (lo - 0xDC00));
                i += 2;
            }
            _ => {
                push(u);
                i += 1;
            }
        }
    }
    out
}

/// Inverse of [`wtf8_encode`]. A byte that starts no well-formed sequence
/// becomes U+FFFD; that only happens for bytes no Windows name produced.
pub fn wtf8_decode(b: &[u8]) -> Vec<u16> {
    let mut out = Vec::with_capacity(b.len());
    let cont = |i: usize| b.get(i).is_some_and(|&c| c & 0xC0 == 0x80);
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        let (cp, len) = match c {
            0x00..=0x7F => (u32::from(c), 1),
            0xC2..=0xDF if cont(i + 1) => {
                ((u32::from(c & 0x1F) << 6) | u32::from(b[i + 1] & 0x3F), 2)
            }
            0xE0..=0xEF if cont(i + 1) && cont(i + 2) => {
                let cp = (u32::from(c & 0x0F) << 12)
                    | (u32::from(b[i + 1] & 0x3F) << 6)
                    | u32::from(b[i + 2] & 0x3F);
                if cp < 0x800 {
                    (0xFFFD, 1)
                } else {
                    (cp, 3)
                }
            }
            0xF0..=0xF4 if cont(i + 1) && cont(i + 2) && cont(i + 3) => {
                let cp = (u32::from(c & 0x07) << 18)
                    | (u32::from(b[i + 1] & 0x3F) << 12)
                    | (u32::from(b[i + 2] & 0x3F) << 6)
                    | u32::from(b[i + 3] & 0x3F);
                if (0x1_0000..=0x10_FFFF).contains(&cp) {
                    (cp, 4)
                } else {
                    (0xFFFD, 1)
                }
            }
            _ => (0xFFFD, 1),
        };
        if cp >= 0x1_0000 {
            let v = cp - 0x1_0000;
            out.push(0xD800 | (v >> 10) as u16);
            out.push(0xDC00 | (v & 0x3FF) as u16);
        } else {
            out.push(cp as u16);
        }
        i += len;
    }
    out
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
    os_from_bytes(bytes_from_str(s))
}

/// The bytes behind a string kept by [`str_from_os`]: raw-byte characters
/// become their bytes, everything else its UTF-8.
fn bytes_from_str(s: &str) -> Vec<u8> {
    let raw = |c: char| (RAW_BYTE_BASE + 0x80..=RAW_BYTE_BASE + 0xFF).contains(&u32::from(c));
    let mut out = Vec::with_capacity(s.len());
    for c in s.chars() {
        if raw(c) {
            out.push((u32::from(c) - RAW_BYTE_BASE) as u8);
        } else {
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        }
    }
    out
}

#[cfg(unix)]
fn os_from_bytes(b: Vec<u8>) -> std::ffi::OsString {
    use std::os::unix::ffi::OsStringExt;
    std::ffi::OsString::from_vec(b)
}

#[cfg(windows)]
fn os_from_bytes(b: Vec<u8>) -> std::ffi::OsString {
    use std::os::windows::ffi::OsStringExt;
    std::ffi::OsString::from_wide(&wtf8_decode(&b))
}

#[cfg(not(any(unix, windows)))]
fn os_from_bytes(b: Vec<u8>) -> std::ffi::OsString {
    String::from_utf8_lossy(&b).into_owned().into()
}

/// A path argument as the OS spells it: [`os_from_str`] as a `PathBuf`.
pub fn arg_path(s: &str) -> std::path::PathBuf {
    os_from_str(s).into()
}

/// The plain form of the verbatim Windows path `s`: `\\?\C:\x` reads
/// `C:\x` and `\\?\UNC\srv\share\x` reads `\\srv\share\x`, the forms
/// users type and the Recycle Bin records. None when `s` is not verbatim,
/// or has no plain form, as a volume GUID path (`\\?\Volume{..}\`).
pub fn verbatim_to_plain(s: &str) -> Option<String> {
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return Some(format!(r"\\{rest}"));
    }
    let rest = s.strip_prefix(r"\\?\")?;
    let b = rest.as_bytes();
    let drive = b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':';
    drive.then(|| rest.to_string())
}

/// `std::path::absolute`, keeping a trailing dot or space on the last
/// component. On Windows `absolute` drops those, and NTFS keeps them
/// when the name is not made only of dots.
pub fn absolute_keep_tail(path: &std::path::Path) -> std::path::PathBuf {
    let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return abs;
    };
    let stem = name.trim_end_matches(['.', ' ']);
    if stem.is_empty() || stem.len() == name.len() {
        return abs;
    }
    let tail = &name[stem.len()..];
    let Some(abs_name) = abs.file_name().and_then(|n| n.to_str()) else {
        return abs;
    };
    if abs_name.ends_with(tail) || abs_name != stem {
        return abs;
    }
    let mut fixed = std::ffi::OsString::from(abs_name);
    fixed.push(tail);
    abs.with_file_name(fixed)
}

/// `p` without the verbatim prefix that `fs::canonicalize` gives and a
/// user may type on Windows ([`verbatim_to_plain`]), so paths print, match
/// and are recorded in one form. Elsewhere `p` comes back as it is: there
/// `\\?\` is an ordinary name.
pub fn plain_path(p: &std::path::Path) -> std::path::PathBuf {
    if cfg!(windows) {
        if let Some(s) = p.to_str().and_then(verbatim_to_plain) {
            return s.into();
        }
    }
    p.to_path_buf()
}

/// Windows: paths from this many UTF-16 units on get the verbatim prefix
/// in [`win_path`]. CreateDirectoryW stops at 248, short of MAX_PATH.
#[cfg(windows)]
pub const WIN_LONG: usize = 248;

/// Windows: `path` as the NUL-terminated wide string a Win32 call takes
/// (dua #410 and #406). From [`WIN_LONG`] units on, the path gets the
/// verbatim prefix (`\\?\C:\..`, `\\?\UNC\srv\..`), which lifts the
/// 260-character limit, as std does for its own calls. It is made absolute
/// first: a verbatim path skips the handling of `..`, `.` and `/`. A path
/// that is verbatim already, or a `\\.\` device path, stays as it is.
/// Every Win32 call that takes a path converts it here (precheck
/// check_win_paths).
#[cfg(windows)]
pub fn win_path(path: &std::path::Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    let units = |p: &std::path::Path| -> Vec<u16> { p.as_os_str().encode_wide().collect() };
    let (bs, colon) = (u16::from(b'\\'), u16::from(b':'));
    let mut s = units(path);
    let verbatim = s.starts_with(&[bs, bs, u16::from(b'?'), bs])
        || s.starts_with(&[bs, bs, u16::from(b'.'), bs]);
    let mut out = Vec::with_capacity(s.len() + 9);
    if s.len() >= WIN_LONG && !verbatim {
        if let Ok(abs) = std::path::absolute(path) {
            s = units(&abs);
        }
        if s.starts_with(&[bs, bs]) {
            out.extend(r"\\?\UNC".encode_utf16());
            out.extend_from_slice(&s[1..]);
        } else if s.get(1) == Some(&colon) {
            out.extend(r"\\?\".encode_utf16());
            out.extend_from_slice(&s);
        } else {
            out.extend_from_slice(&s);
        }
    } else {
        out.extend_from_slice(&s);
    }
    out.push(0);
    out
}

/// Windows: `s` as a NUL-terminated wide string, for a Win32 call that
/// takes a name that is not a path (a registry key or value name).
#[cfg(windows)]
pub fn win_text(s: &std::ffi::OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    s.encode_wide().chain(Some(0)).collect()
}

/// Why an item that a scan of the Finder Trash or a Recycle Bin named was
/// left alone when the time came to act on it: the check made just before
/// the action found it gone, or found another item in its place (round
/// 246). The FreeDesktop trash has `list::Relock` for this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stale {
    /// The item left the trash.
    Gone,
    /// Another item, or a changed one, sits where the scan saw it.
    Changed,
}

impl Stale {
    /// Why nothing was done to the item shown as `shown`, for an error
    /// line; the words of `list::Relock::reason`.
    pub fn reason(self, shown: &str) -> String {
        match self {
            Stale::Gone => format!("'{shown}' is no longer in the trash"),
            Stale::Changed => format!(
                "'{shown}' changed in the trash after it was read (another item now has \
                 its name); left as it is"
            ),
        }
    }
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

/// The item name behind the info file name `fname`, or `None` when it is no
/// `.trashinfo` file or names no payload. `files/` joined with "", "." or
/// ".." points at `files/` itself or the trash root, so a stray `.trashinfo`,
/// `..trashinfo` or `...trashinfo` must never become an item that `rm`,
/// `empty` or `restore` acts on.
pub fn trash_item_name(fname: &str) -> Option<&str> {
    let name = fname.strip_suffix(".trashinfo")?;
    let payload = !matches!(name, "" | "." | "..") && !name.contains('/');
    payload.then_some(name)
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

/// `1 item`, `2 items`: a count with its noun in the right number.
/// The stderr line `list` prints after its rows when `skipped` entries,
/// called `one` or `many`, could not be listed; None when there are
/// none. `rtrash status --check` names each one, on Unix and on Windows.
pub fn unlisted_hint(prog: &str, skipped: u64, one: &str, many: &str) -> Option<String> {
    (skipped > 0).then(|| {
        format!(
            "{prog}: {} could not be listed; `rtrash status --check` shows {}",
            plural(skipped, one, many),
            if skipped == 1 { "it" } else { "them" }
        )
    })
}

pub fn plural(n: u64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// `du --si`: [`format_bytes`] uses powers of 1000 for the rest of the run.
static SI_UNITS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Switch [`format_bytes`] to decimal units (kB/MB/GB), as `du --si` does.
pub fn set_si_units(on: bool) {
    SI_UNITS.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// `du --no-prefix`: [`format_bytes`] prints plain byte counts for the
/// rest of the run.
static RAW_BYTES: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Switch [`format_bytes`] to plain byte counts (`123456`), as
/// `du --no-prefix` does.
pub fn set_raw_bytes(on: bool) {
    RAW_BYTES.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// A block size from `BLOCK_SIZE` or the like: [`format_bytes`] then
/// prints a count of `size`-byte blocks for the rest of the run.
static BLOCK_UNITS: std::sync::Mutex<Option<(u64, String)>> = std::sync::Mutex::new(None);

/// Switch [`format_bytes`] to whole blocks of `size` bytes, rounded up
/// as GNU du rounds, with `suffix` after each count; `None` switches back.
pub fn set_block_units(units: Option<(u64, String)>) {
    *BLOCK_UNITS.lock().unwrap_or_else(|e| e.into_inner()) = units;
}

/// Human-readable sizes for CLI summaries: binary units (KiB/MiB/GiB), or
/// decimal ones after [`set_si_units`], plain bytes after
/// [`set_raw_bytes`], or blocks after [`set_block_units`].
pub fn format_bytes(n: u64) -> String {
    if RAW_BYTES.load(std::sync::atomic::Ordering::Relaxed) {
        return n.to_string();
    }
    if let Some((size, suffix)) = &*BLOCK_UNITS.lock().unwrap_or_else(|e| e.into_inner()) {
        return format_blocks(n, *size, suffix);
    }
    format_bytes_in(n, SI_UNITS.load(std::sync::atomic::Ordering::Relaxed))
}

/// `n` bytes as whole `size`-byte blocks, rounded up as GNU du rounds,
/// with `suffix` after the count.
pub fn format_blocks(n: u64, size: u64, suffix: &str) -> String {
    format!("{}{suffix}", n.div_ceil(size.max(1)))
}

/// A size display from a GNU block size value (`--block-size`,
/// `BLOCK_SIZE`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlockSize {
    /// `human-readable`: rtrash's own units (KiB, MiB, GiB).
    Human,
    /// `si`: powers of 1000, as `--si`.
    Si,
    /// A count of `size`-byte blocks with `suffix` after it.
    Blocks { size: u64, suffix: String },
}

/// Parse a block size as GNU coreutils reads one: `human-readable` or
/// `si` (or a part of either from the start), else an optional count
/// and an optional unit: K, M, G, T, P or E (any case) for powers of
/// 1024, the same with `iB`, or with `B` for powers of 1000. The unit
/// is printed after each size only when the value starts with it, so
/// `M` prints `12M` and `1M` prints `12`. A leading `'` asks GNU tools
/// to group digits; rtrash takes the value and does not group. Zero, an
/// unknown unit or a size past 2^64 gives `None`.
pub fn parse_block_size(spec: &str) -> Option<BlockSize> {
    let spec = spec.strip_prefix('\'').unwrap_or(spec);
    if spec.is_empty() {
        return None;
    }
    if "human-readable".starts_with(spec) {
        return Some(BlockSize::Human);
    }
    if "si".starts_with(spec) {
        return Some(BlockSize::Si);
    }
    let digits = spec.bytes().take_while(u8::is_ascii_digit).count();
    let count: u64 = if digits == 0 {
        1
    } else {
        spec[..digits].parse().ok()?
    };
    let unit = &spec[digits..];
    let (size, suffix) = if unit.is_empty() {
        (1, String::new())
    } else {
        let mut chars = unit.chars();
        let letter = chars.next()?.to_ascii_uppercase();
        let power = "KMGTPE".find(letter)? as u32 + 1;
        let (base, tail) = match chars.as_str() {
            "" => (1024u64, ""),
            "iB" => (1024, "iB"),
            "B" => (1000, "B"),
            _ => return None,
        };
        // A decimal kilo is a small k, as GNU prints it (kB).
        let shown = if base == 1000 && power == 1 {
            'k'
        } else {
            letter
        };
        (base.checked_pow(power)?, format!("{shown}{tail}"))
    };
    let size = count.checked_mul(size).filter(|&n| n > 0)?;
    let suffix = if digits == 0 { suffix } else { String::new() };
    Some(BlockSize::Blocks { size, suffix })
}

/// The variables du takes a block size from, first one set first:
/// rtrash's own, then GNU du's order.
pub const BLOCK_SIZE_VARS: [&str; 4] = [
    "RTRASH_BLOCK_SIZE",
    "DU_BLOCK_SIZE",
    "BLOCK_SIZE",
    "BLOCKSIZE",
];

/// The block size from the first of [`BLOCK_SIZE_VARS`] that is set and
/// not empty. A value that does not parse gives `None`, as GNU du then
/// uses its default and does not look further; only a bad
/// `RTRASH_BLOCK_SIZE` gets a warning, since the others are shared with
/// tools that may take values rtrash does not.
pub fn block_size_from_env(prog: &str) -> Option<BlockSize> {
    let (var, value) = BLOCK_SIZE_VARS.iter().find_map(|v| {
        std::env::var_os(v)
            .filter(|x| !x.is_empty())
            .map(|x| (*v, x.to_string_lossy().into_owned()))
    })?;
    let parsed = parse_block_size(&value);
    if parsed.is_none() && var == BLOCK_SIZE_VARS[0] {
        let want = "a size such as 1K, M or MB, or human-readable or si";
        eprintln!("{prog}: ignoring {var}='{value}' ({want})");
    }
    parsed
}

/// [`format_bytes`] with the unit base given: decimal when `si` is set.
pub fn format_bytes_in(n: u64, si: bool) -> String {
    if si {
        const K: f64 = 1000.0;
        let x = n as f64;
        return if n < 1000 {
            format!("{n} B")
        } else if x < K * K {
            format!("{:.1} kB", x / K)
        } else if x < K * K * K {
            format!("{:.1} MB", x / (K * K))
        } else if x < K * K * K * K {
            format!("{:.2} GB", x / (K * K * K))
        } else {
            format!("{:.2} TB", x / (K * K * K * K))
        };
    }
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

#[cfg(test)]
thread_local! {
    /// `Some(home)` while [`override_home`] is in force on this thread.
    /// `Some(None)` is an unknown home. `None` means read the environment.
    static TEST_HOME: std::cell::RefCell<Option<Option<std::path::PathBuf>>> =
        const { std::cell::RefCell::new(None) };
}

/// Restores the previous [`home_dir`] override when dropped.
/// [`Drop`] writes a thread-local, so this guard is `!Send` and `!Sync`.
#[cfg(test)]
#[must_use = "the home override ends when this guard drops"]
pub struct HomeOverride {
    prev: Option<Option<std::path::PathBuf>>,
    /// A raw pointer is `!Send` and `!Sync`, and so is this guard.
    _thread: std::marker::PhantomData<*const ()>,
}

#[cfg(test)]
impl Drop for HomeOverride {
    fn drop(&mut self) {
        let prev = self.prev.take();
        TEST_HOME.with(|slot| *slot.borrow_mut() = prev);
    }
}

/// Make [`home_dir`] return `home` on this thread until the guard drops.
/// `None` is an unknown home. Other threads keep reading the environment.
#[cfg(test)]
pub fn override_home(home: Option<std::path::PathBuf>) -> HomeOverride {
    let prev = TEST_HOME.with(|slot| slot.replace(Some(home)));
    HomeOverride {
        prev,
        _thread: std::marker::PhantomData,
    }
}

/// The home directory from `HOME` (`USERPROFILE` on Windows), if set
/// to an absolute path.
pub fn home_dir() -> Option<std::path::PathBuf> {
    #[cfg(test)]
    if let Some(over) = TEST_HOME.with(|slot| slot.borrow().clone()) {
        return over.filter(|path| path.is_absolute());
    }
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

/// A path for human output on stdout (see [`term_text`]). Anything that
/// names a path will do: a `Path`, `PathBuf`, `OsStr` or a reference to one.
pub fn term_path<P: AsRef<std::path::Path> + ?Sized>(p: &P) -> String {
    let shown = p.as_ref().display().to_string();
    match term_text(&shown) {
        std::borrow::Cow::Borrowed(_) => shown,
        std::borrow::Cow::Owned(o) => o,
    }
}

/// `p` ended by `end`, as a list of paths on stdout writes it: the raw
/// bytes when `tty` is false, so `xargs` and a pipe get each name
/// exactly, and with `?` for each control character on a terminal
/// (dua #365), where a crafted name would act on the screen.
pub fn path_line(p: &std::path::Path, end: u8, tty: bool) -> Vec<u8> {
    let mut line = if tty {
        scrub(&p.display().to_string()).into_owned().into_bytes()
    } else {
        p.as_os_str().as_encoded_bytes().to_vec()
    };
    line.push(end);
    line
}

/// Text for a message on stderr: scrubbed when stderr is a terminal.
pub fn err_text(s: &str) -> std::borrow::Cow<'_, str> {
    if stderr_is_tty() {
        scrub(s)
    } else {
        std::borrow::Cow::Borrowed(s)
    }
}

/// A path for messages on stderr: scrubbed when stderr is a terminal.
pub fn err_path<P: AsRef<std::path::Path> + ?Sized>(p: &P) -> String {
    let shown = p.as_ref().display().to_string();
    if stderr_is_tty() {
        scrub(&shown).into_owned()
    } else {
        shown
    }
}

/// One human-readable output stream beside an optional JSON document.
///
/// Human mode writes enabled lines to stdout. JSON mode keeps stdout for
/// the document and moves the same lines to stderr.
#[derive(Clone, Copy, Debug, Default)]
pub struct HumanOutput {
    json: bool,
    enabled: bool,
}

impl HumanOutput {
    pub fn new(json: bool, enabled: bool) -> Self {
        Self { json, enabled }
    }

    pub fn line(&self, text: &str) {
        if !self.enabled {
            return;
        }
        if self.json {
            eprintln!("{}", err_text(text));
        } else {
            println!("{}", term_text(text));
        }
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

/// Parse a pick like `0-2,5 7` or `all` against `len` rows: indices in
/// the order given, each once.
pub fn parse_selection(line: &str, len: usize) -> Result<Vec<usize>, String> {
    let line = line.trim();
    if line.eq_ignore_ascii_case("all") || line == "*" {
        return Ok((0..len).collect());
    }
    let mut out: Vec<usize> = Vec::new();
    let mut seen = vec![false; len];
    for tok in line.split([',', ' ', '\t']).filter(|t| !t.is_empty()) {
        let (lo, hi) = match tok.split_once('-') {
            Some((a, b)) => (a.trim(), b.trim()),
            None => (tok, tok),
        };
        let num = |s: &str| {
            s.parse::<usize>()
                .map_err(|_| format!("'{tok}' is no index or range"))
        };
        let (lo, hi) = (num(lo)?, num(hi)?);
        if lo > hi {
            return Err(format!("range '{tok}' runs backwards"));
        }
        if hi >= len {
            return Err(format!("{hi} lies outside 0..{}", len - 1));
        }
        for (i, was) in seen.iter_mut().enumerate().take(hi + 1).skip(lo) {
            if !*was {
                *was = true;
                out.push(i);
            }
        }
    }
    if out.is_empty() {
        return Err("nothing selected".into());
    }
    Ok(out)
}

/// What a look at a path finds, without following a final link.
pub enum Look {
    /// Boxed: a `Metadata` is 224 bytes on FreeBSD, and the other
    /// variants are a word (clippy::large_enum_variant).
    There(Box<std::fs::Metadata>),
    /// The system says nothing is there: [`is_missing`].
    Gone,
    /// Something may be there, but the look failed: a parent that
    /// denies search, a name too long, an I/O error.
    Unreachable(io::Error),
}

/// Look at `path` without following a final link. A path that cannot be
/// reached is never taken for a missing one, so a denied file is not
/// reported as "No such file or directory" (trash-cli #377), and a
/// record whose payload sits under a denied folder is not taken for
/// dangling. The precheck refuses `metadata(..).is_err()` and a
/// `metadata(..).is_ok()` presence test elsewhere.
pub fn look(path: &std::path::Path) -> Look {
    // Debug-only: one path is unreachable, so a test can put that name
    // beside one that stats normally. The message is fixed. glibc, musl
    // and Windows do not agree on what raw os error 5 says.
    if cfg!(debug_assertions)
        && std::env::var_os("RTRASH_TEST_LOOK_IO").as_deref() == Some(path.as_os_str())
    {
        return Look::Unreachable(io::Error::other("rtrash test: stat failed"));
    }
    match std::fs::symlink_metadata(path) {
        Ok(m) => Look::There(Box::new(m)),
        Err(e) if is_missing(&e) => Look::Gone,
        Err(e) => Look::Unreachable(e),
    }
}

/// Whether nothing is at `path`, as [`look`] tells it: false for a path
/// that is there and for one that cannot be reached.
pub fn is_gone(path: &std::path::Path) -> bool {
    matches!(look(path), Look::Gone)
}

/// Whether `path` is there, without following a final link. A path that
/// is gone and one that cannot be reached are both not there. A decision
/// that the path is gone uses [`is_gone`], which stays false when the
/// path cannot be reached.
pub fn is_there(path: &std::path::Path) -> bool {
    matches!(look(path), Look::There(_))
}

/// Which file a payload was when a scan or a match read it: its device and
/// inode (Unix) or volume and file index (Windows), and for a file its
/// length and modification time. A payload that another item replaced
/// under the same name, with its record unchanged, reads as another file.
/// A folder keeps only its identity: its own modification time moves when
/// something is added inside it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fingerprint {
    id: (u64, u64),
    len: u64,
    mtime: Option<std::time::SystemTime>,
}

/// The [`Fingerprint`] of `path` without following a final link, or `None`
/// when it cannot be read.
pub fn fingerprint(path: &std::path::Path) -> Option<Fingerprint> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    #[cfg(unix)]
    let id = {
        use std::os::unix::fs::MetadataExt;
        (meta.dev(), meta.ino())
    };
    #[cfg(windows)]
    let id = crate::win_recycle::file_identity(path)?;
    #[cfg(not(any(unix, windows)))]
    let id = (0, 0);
    let file = !meta.is_dir();
    Some(Fingerprint {
        id,
        len: if file { meta.len() } else { 0 },
        mtime: if file { meta.modified().ok() } else { None },
    })
}

impl Fingerprint {
    /// Whether a staged payload is the same one after a move. A rename on
    /// one volume keeps its identity; a cross-volume copy keeps the file
    /// attributes the fingerprint records but necessarily gets a new id.
    pub fn matches_move(self, staged: Self) -> bool {
        if self.id.0 == staged.id.0 {
            self == staged
        } else {
            self.len == staged.len && self.mtime == staged.mtime
        }
    }
}

/// Debug-only seam: replace the payload named by `RTRASH_TEST_REPLACE_PAYLOAD`
/// with another file, as a swap between a check and the act that follows it.
pub fn replace_payload_seam(path: &std::path::Path) {
    if cfg!(debug_assertions)
        && std::env::var_os("RTRASH_TEST_REPLACE_PAYLOAD").as_deref() == Some(path.as_os_str())
    {
        let _ = crate::permanent_guard::remove(path);
        let _ = crate::file_replace::write_own(path, b"replacement");
    }
}

/// Debug-only seam: replace a Recycle Bin payload as an overwrite begins,
/// so staging must reject it before touching the destination.
pub fn replace_payload_after_overwrite_seam(path: &std::path::Path) {
    if cfg!(debug_assertions)
        && std::env::var_os("RTRASH_TEST_REPLACE_PAYLOAD_AFTER_OVERWRITE").as_deref()
            == Some(path.as_os_str())
    {
        let _ = crate::permanent_guard::remove(path);
        let _ = crate::file_replace::write_own(path, b"replacement");
    }
}

/// Whether `e` says that a path is not there: ENOENT, or ENOTDIR when a
/// part of the path is a file (Windows: file or path not found).
pub fn is_missing(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    )
}

/// The parenthetical a status row adds when a trash has orphans or
/// records whose payload is gone. Empty when it has neither.
/// `rtrash-hud` prints the same words. This crate's status module is
/// Unix-only, so on Windows the HUD is the caller.
#[cfg_attr(windows, allow(dead_code))]
pub fn status_notes(orphans: u64, dangling: u64) -> String {
    let mut parts = Vec::new();
    if orphans > 0 {
        parts.push(format!("{orphans} without .trashinfo"));
    }
    if dangling > 0 {
        parts.push(format!("{dangling} without a payload"));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(" ({})", parts.join(", "))
    }
}

/// Debug-only seam: when `RTRASH_TEST_NOFOLLOW_SWAP` names `path`, replace
/// that file with a symlink to `RTRASH_TEST_NOFOLLOW_TARGET` after the
/// lstat and before the open. The open's own nofollow is then what
/// refuses the link; the lstat already passed.
fn nofollow_swap_seam(path: &std::path::Path) {
    if cfg!(debug_assertions)
        && std::env::var_os("RTRASH_TEST_NOFOLLOW_SWAP").as_deref() == Some(path.as_os_str())
    {
        let Some(target) = std::env::var_os("RTRASH_TEST_NOFOLLOW_TARGET") else {
            return;
        };
        let _ = std::fs::remove_file(path);
        #[cfg(unix)]
        let _ = std::os::unix::fs::symlink(&target, path);
        #[cfg(windows)]
        let _ = std::os::windows::fs::symlink_file(&target, path);
    }
}

/// Open `path` for reading only when it is a regular file. A symlink is
/// refused from an lstat, and the open itself does not follow one that
/// appears in its place (`O_NOFOLLOW` on Unix,
/// `FILE_FLAG_OPEN_REPARSE_POINT` on Windows), including one planted
/// after the lstat. A Windows data reparse point, such as a Compact
/// file, is reopened from the same handle without that flag so the
/// filter supplies the file bytes. A tag that cannot be confirmed is an
/// error, not file data. Cache and record reads use this so a planted
/// link cannot be read as `directorysizes` or a `.trashinfo`.
pub(crate) fn open_regular_nofollow(path: &std::path::Path) -> io::Result<std::fs::File> {
    let not_regular = || io::Error::new(io::ErrorKind::InvalidData, "not a regular file");
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.file_type().is_file() {
        return Err(not_regular());
    }
    nofollow_swap_seam(path);
    let file = open_nofollow_read(path)?;
    if !file.metadata()?.file_type().is_file() {
        return Err(not_regular());
    }
    Ok(file)
}

/// `winnt.h` `IsReparseTagNameSurrogate`: the tag names another file
/// (a symlink, a junction, a mount point) rather than holding this
/// file's data (Compact / WOF, a cloud placeholder).
#[cfg(any(windows, test))]
pub(crate) fn reparse_tag_names_another_file(tag: u32) -> bool {
    const NAME_SURROGATE: u32 = 0x2000_0000;
    tag & NAME_SURROGATE != 0
}

/// What a reparse-tag query says about a handle opened with
/// `FILE_FLAG_OPEN_REPARSE_POINT`.
#[cfg(any(windows, test))]
#[derive(Clone, Copy, Debug)]
enum ReparseLook {
    /// The query succeeded and the file is not a reparse point.
    Plain,
    /// A data tag (Compact / WOF and the like), not a name surrogate.
    Data,
    /// A tag that names another file.
    Surrogate,
    /// The query failed, or the attribute and the tag disagree.
    Unconfirmed,
}

/// `query_ok` is a successful `GetFileInformationByHandleEx`. A clear
/// reparse attribute together with tag 0 is a plain file. A set attribute
/// with tag 0, a clear attribute with a tag, or a failed query is not
/// confirmed: a Compact file in that state must not be read as its
/// reparse buffer.
#[cfg(any(windows, test))]
fn classify_reparse(query_ok: bool, reparse_attribute: bool, tag: u32) -> ReparseLook {
    if !query_ok {
        return ReparseLook::Unconfirmed;
    }
    if reparse_attribute && tag != 0 {
        if reparse_tag_names_another_file(tag) {
            ReparseLook::Surrogate
        } else {
            ReparseLook::Data
        }
    } else if !reparse_attribute && tag == 0 {
        ReparseLook::Plain
    } else {
        ReparseLook::Unconfirmed
    }
}

/// What to do with a handle after [`classify_reparse`]. `reopened` is
/// true once the handle has been opened again without the reparse flag.
/// A data tag is reopened once; after that, only a plain file is kept.
#[cfg(any(windows, test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReparseOpen {
    /// Keep this handle.
    Plain,
    /// Open again from this handle, without the reparse flag.
    Reopen,
    /// Do not read this handle. The text is the error.
    Refuse(&'static str),
}

/// The open decision for `look`. A name surrogate is refused. A data tag
/// is reopened once. A tag that cannot be confirmed is refused, and a
/// reopen that is still a reparse point is refused too.
#[cfg(any(windows, test))]
fn reparse_open(look: ReparseLook, reopened: bool) -> ReparseOpen {
    match (look, reopened) {
        (ReparseLook::Plain, _) => ReparseOpen::Plain,
        (ReparseLook::Surrogate, _) => ReparseOpen::Refuse("not a regular file"),
        (ReparseLook::Data, false) => ReparseOpen::Reopen,
        (ReparseLook::Unconfirmed, false) => {
            ReparseOpen::Refuse("reparse tag could not be confirmed")
        }
        (ReparseLook::Data | ReparseLook::Unconfirmed, true) => {
            ReparseOpen::Refuse("reparse data was not read through its filter")
        }
    }
}

/// What to do with a handle once [`reparse_open`] has classified it.
/// `reopen` runs only for a first reopen. A second one is the filter
/// error, so the reparse buffer is not read. Both Windows matches call
/// this, and the unit test runs on every platform.
#[cfg(any(windows, test))]
fn apply_reparse_open<T>(
    decision: ReparseOpen,
    reopened: bool,
    file: T,
    reopen: impl FnOnce(T) -> io::Result<T>,
) -> io::Result<T> {
    match decision {
        ReparseOpen::Plain => Ok(file),
        ReparseOpen::Refuse(why) => Err(io::Error::new(io::ErrorKind::InvalidData, why)),
        ReparseOpen::Reopen if !reopened => reopen(file),
        ReparseOpen::Reopen => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "reparse data was not read through its filter",
        )),
    }
}

fn open_nofollow_read(path: &std::path::Path) -> io::Result<std::fs::File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY)
            .open(path)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        // The flag above skips filters. [`apply_reparse_open`] keeps a
        // plain file, reopens a data tag from this handle, and refuses
        // the rest. Reading a refused handle would return the reparse
        // buffer, or follow a symlink raced in after the lstat.
        apply_reparse_open(
            reparse_open(reparse_look(&file), false),
            false,
            file,
            reopen_without_reparse_flag,
        )
    }
}

/// The reparse tag of a handle opened with `FILE_FLAG_OPEN_REPARSE_POINT`.
#[cfg(windows)]
fn reparse_look(file: &std::fs::File) -> ReparseLook {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FileAttributeTagInfo, GetFileInformationByHandleEx, FILE_ATTRIBUTE_REPARSE_POINT,
        FILE_ATTRIBUTE_TAG_INFO,
    };
    let mut info = std::mem::MaybeUninit::<FILE_ATTRIBUTE_TAG_INFO>::uninit();
    // SAFETY: the buffer holds exactly one FILE_ATTRIBUTE_TAG_INFO.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle() as _,
            FileAttributeTagInfo,
            info.as_mut_ptr().cast(),
            std::mem::size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    };
    if ok == 0 {
        return ReparseLook::Unconfirmed;
    }
    // SAFETY: the call succeeded, so it filled `info`.
    let info = unsafe { info.assume_init() };
    classify_reparse(
        true,
        info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0,
        info.ReparseTag,
    )
}

/// Volume serial and file index of `file`.
#[cfg(windows)]
fn file_id(file: &std::fs::File) -> io::Result<(u32, u32, u32)> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };
    let mut info = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
    // SAFETY: the buffer holds exactly one BY_HANDLE_FILE_INFORMATION.
    let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, info.as_mut_ptr()) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the call succeeded, so it filled `info`.
    let info = unsafe { info.assume_init() };
    Ok((
        info.dwVolumeSerialNumber,
        info.nFileIndexHigh,
        info.nFileIndexLow,
    ))
}

/// A second handle on `file` without `FILE_FLAG_OPEN_REPARSE_POINT`, so a
/// data reparse point's filter can supply the file bytes. Not a path open:
/// a symlink that took the name is not followed.
#[cfg(windows)]
fn reopen_file(file: &std::fs::File) -> io::Result<std::fs::File> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{
        ReOpenFile, FILE_GENERIC_READ, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    // SAFETY: `file` is open. A null or invalid handle is not owned.
    let handle = unsafe {
        ReOpenFile(
            file.as_raw_handle() as _,
            FILE_GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            0,
        )
    };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: ReOpenFile returned a new handle, and this owns it. The
    // original `file` stays open until the caller drops it.
    let owned = unsafe { OwnedHandle::from_raw_handle(handle as _) };
    Ok(std::fs::File::from(owned))
}

/// Reopen a data reparse point from its handle and keep the result only
/// when it is the same file and the filter supplied plain bytes.
#[cfg(windows)]
fn reopen_without_reparse_flag(file: std::fs::File) -> io::Result<std::fs::File> {
    let before = file_id(&file)?;
    let again = reopen_file(&file)?;
    let after = file_id(&again)?;
    if before != after {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "reopened file is not the same file",
        ));
    }
    // A second reopen is refused inside [`apply_reparse_open`]. The
    // closure is the same refusal if that function asks for one anyway.
    apply_reparse_open(
        reparse_open(reparse_look(&again), true),
        true,
        again,
        |_| {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "reparse data was not read through its filter",
            ))
        },
    )
}

#[cfg(test)]
mod tests {
    /// One blanket impl, plus one that exists only when `T: Send`.
    /// Inference of `_` then fails exactly when the guard is `Send`.
    trait NotSend<A> {
        fn item() {}
    }
    impl<T: ?Sized> NotSend<()> for T {}
    impl<T: ?Sized + Send> NotSend<u8> for T {}

    /// Same check for `Sync`.
    trait NotSync<A> {
        fn item() {}
    }
    impl<T: ?Sized> NotSync<()> for T {}
    impl<T: ?Sized + Sync> NotSync<u8> for T {}

    #[test]
    fn an_overridden_home_stays_on_this_thread() {
        use super::{home_dir, override_home};
        let pinned = std::env::temp_dir().join("rtrash-pattern-home-absent");
        let nested = std::env::temp_dir().join("rtrash-pattern-home-nested");
        let real = home_dir();
        assert_ne!(real.as_deref(), Some(pinned.as_path()));
        assert_ne!(real.as_deref(), Some(nested.as_path()));
        let outer = override_home(Some(pinned.clone()));
        assert_eq!(home_dir().as_deref(), Some(pinned.as_path()));
        let seen = std::thread::spawn(home_dir).join().expect("home thread");
        assert_eq!(seen, real, "another thread must keep the real home");
        {
            let _inner = override_home(Some(nested.clone()));
            assert_eq!(home_dir().as_deref(), Some(nested.as_path()));
        }
        assert_eq!(
            home_dir().as_deref(),
            Some(pinned.as_path()),
            "inner drop must restore the outer override"
        );
        drop(outer);
        assert_eq!(home_dir(), real);
    }

    /// `Drop` writes this thread's local, so the guard cannot move.
    #[test]
    fn the_home_override_cannot_leave_its_thread() {
        let _ = <super::HomeOverride as NotSend<_>>::item;
        let _ = <super::HomeOverride as NotSync<_>>::item;
    }

    /// dua #410 and #406: a long path gets the verbatim prefix in its
    /// absolute form, with its `/` and `..` resolved; a short one, a
    /// verbatim one and a device path stay as they are.
    #[cfg(windows)]
    #[test]
    fn win_paths_take_the_verbatim_form_from_win_long_units() {
        use super::{win_path, win_text, WIN_LONG};
        use std::path::Path;
        let text = |w: Vec<u16>| String::from_utf16(&w[..w.len() - 1]).unwrap();
        assert_eq!(text(win_path(Path::new(r"C:\short"))), r"C:\short");
        let deep = "d".repeat(WIN_LONG);
        let long = format!(r"C:\top/{deep}\x\..\leaf");
        assert_eq!(
            text(win_path(Path::new(&long))),
            format!(r"\\?\C:\top\{deep}\leaf")
        );
        let share = format!(r"\\srv\share\{deep}");
        assert_eq!(
            text(win_path(Path::new(&share))),
            format!(r"\\?\UNC\srv\share\{deep}")
        );
        for kept in [format!(r"\\?\C:\{deep}"), format!(r"\\.\C:\{deep}")] {
            assert_eq!(text(win_path(Path::new(&kept))), kept);
        }
        assert_eq!(text(win_text("Key".as_ref())), "Key");
    }

    /// A missing path and a path below a file are gone; a path that is
    /// there is not, and (on Unix, not as root) neither is one below a
    /// folder that denies search.
    #[test]
    fn look_tells_gone_from_unreachable() {
        use super::{is_gone, look, Look};
        let top = std::env::temp_dir().join(format!("rtrash-look-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&top);
        std::fs::create_dir_all(top.join("shut")).unwrap();
        std::fs::write(top.join("file"), b"x").unwrap();
        std::fs::write(top.join("shut/inner"), b"x").unwrap();
        assert!(is_gone(&top.join("none")));
        assert!(!is_there(&top.join("none")));
        assert!(is_gone(&top.join("file/below")));
        assert!(is_there(&top.join("file")));
        assert!(matches!(look(&top.join("file")), Look::There(_)));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let shut = top.join("shut");
            std::fs::set_permissions(&shut, std::fs::Permissions::from_mode(0o600)).unwrap();
            // SAFETY: geteuid has no preconditions.
            if unsafe { libc::geteuid() } != 0 {
                assert!(matches!(look(&shut.join("inner")), Look::Unreachable(_)));
                assert!(!is_gone(&shut.join("inner")));
                assert!(!is_there(&shut.join("inner")));
            }
            std::fs::set_permissions(&shut, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::remove_dir_all(&top).unwrap();
    }

    #[test]
    fn unlisted_hint_counts_and_names_status_check() {
        assert_eq!(
            super::unlisted_hint("rtrash list", 0, "trash record", "trash records"),
            None
        );
        assert_eq!(
            super::unlisted_hint("rtrash list", 1, "trash record", "trash records").as_deref(),
            Some(
                "rtrash list: 1 trash record could not be listed; \
                 `rtrash status --check` shows it"
            )
        );
        assert_eq!(
            super::unlisted_hint("rtrash list", 3, "trash record", "trash records").as_deref(),
            Some(
                "rtrash list: 3 trash records could not be listed; \
                 `rtrash status --check` shows them"
            )
        );
    }

    use super::*;

    #[test]
    fn verbatim_paths_have_a_plain_form_only_on_a_drive_or_share() {
        let p = verbatim_to_plain;
        assert_eq!(p(r"\\?\C:\Users\me").as_deref(), Some(r"C:\Users\me"));
        assert_eq!(p(r"\\?\UNC\srv\share\x").as_deref(), Some(r"\\srv\share\x"));
        assert_eq!(p(r"\\?\Volume{1}\x"), None);
        assert_eq!(p(r"C:\plain"), None);
        assert_eq!(p("/home/me"), None);
        let typed = std::path::Path::new(r"\\?\C:\Users\me");
        let want = if cfg!(windows) {
            r"C:\Users\me"
        } else {
            r"\\?\C:\Users\me"
        };
        assert_eq!(plain_path(typed), std::path::PathBuf::from(want));
    }

    #[test]
    fn absolute_keeps_a_trailing_dot_or_space_on_the_last_name() {
        let top = std::env::temp_dir().join(format!("rtrash-tail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&top);
        std::fs::create_dir_all(&top).unwrap();
        let dotted = top.join("notes.txt.");
        let spaced = top.join("notes.txt ");
        let plain = top.join("notes.txt");
        std::fs::write(&plain, b"x").unwrap();
        let kept_dot = super::absolute_keep_tail(&dotted);
        let kept_space = super::absolute_keep_tail(&spaced);
        assert!(
            kept_dot.file_name().and_then(|n| n.to_str()) == Some("notes.txt."),
            "{}",
            kept_dot.display()
        );
        assert!(
            kept_space.file_name().and_then(|n| n.to_str()) == Some("notes.txt "),
            "{}",
            kept_space.display()
        );
        assert_eq!(
            super::absolute_keep_tail(&plain)
                .file_name()
                .and_then(|n| n.to_str()),
            Some("notes.txt")
        );
        std::fs::remove_dir_all(&top).unwrap();
    }

    #[test]
    fn si_units_use_powers_of_1000() {
        assert_eq!(format_bytes_in(999, true), "999 B");
        assert_eq!(format_bytes_in(5000, true), "5.0 kB");
        assert_eq!(format_bytes_in(5000, false), "4.9 KiB");
        assert_eq!(format_bytes_in(2_500_000, true), "2.5 MB");
        assert_eq!(format_bytes_in(3_000_000_000, true), "3.00 GB");
        assert_eq!(format_bytes_in(4_000_000_000_000, true), "4.00 TB");
    }

    #[test]
    fn block_sizes_parse_as_gnu_coreutils_reads_them() {
        let blocks = |size: u64, suffix: &str| {
            Some(BlockSize::Blocks {
                size,
                suffix: suffix.to_string(),
            })
        };
        assert_eq!(parse_block_size("human-readable"), Some(BlockSize::Human));
        assert_eq!(parse_block_size("human"), Some(BlockSize::Human));
        assert_eq!(parse_block_size("si"), Some(BlockSize::Si));
        assert_eq!(parse_block_size("1"), blocks(1, ""));
        assert_eq!(parse_block_size("512"), blocks(512, ""));
        assert_eq!(parse_block_size("1K"), blocks(1024, ""));
        assert_eq!(parse_block_size("1k"), blocks(1024, ""));
        assert_eq!(parse_block_size("K"), blocks(1024, "K"));
        assert_eq!(parse_block_size("KiB"), blocks(1024, "KiB"));
        assert_eq!(parse_block_size("kB"), blocks(1000, "kB"));
        assert_eq!(parse_block_size("KB"), blocks(1000, "kB"));
        assert_eq!(parse_block_size("M"), blocks(1 << 20, "M"));
        assert_eq!(parse_block_size("MB"), blocks(1_000_000, "MB"));
        assert_eq!(parse_block_size("4M"), blocks(4 << 20, ""));
        assert_eq!(parse_block_size("'1K"), blocks(1024, ""));
        assert_eq!(parse_block_size("G"), blocks(1 << 30, "G"));
        for bad in [
            "",
            "0",
            "0K",
            "1X",
            "Mb",
            "1KiBx",
            "Z",
            "99999999999999999999",
            "16E",
            "-1",
        ] {
            assert_eq!(parse_block_size(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn block_units_print_whole_blocks_rounded_up() {
        let k = |n| format_blocks(n, 1024, "K");
        assert_eq!([k(0), k(1), k(1024), k(1025)], ["0K", "1K", "1K", "2K"]);
        assert_eq!(format_blocks(2_500_000, 1_000_000, ""), "3");
        assert_eq!(format_blocks(u64::MAX, 1, ""), u64::MAX.to_string());
    }

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

    /// dua #365: a pipe gets a listed path as it is, a terminal sees
    /// each control character as `?`.
    #[test]
    fn path_lines_go_raw_down_a_pipe_and_scrubbed_to_a_terminal() {
        let p = std::path::Path::new("d/a\x1b[2Jb\nc");
        assert_eq!(path_line(p, b'\n', false), b"d/a\x1b[2Jb\nc\n");
        assert_eq!(path_line(p, 0, false), b"d/a\x1b[2Jb\nc\0");
        assert_eq!(path_line(p, b'\n', true), b"d/a?[2Jb?c\n");
        assert_eq!(path_line(p, 0, true), b"d/a?[2Jb?c\0");
        let plain = std::path::Path::new("plain/ünïcode name");
        assert_eq!(
            path_line(plain, b'\n', true),
            path_line(plain, b'\n', false)
        );
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

    #[test]
    fn info_names_that_point_at_the_trash_are_no_items() {
        assert_eq!(trash_item_name("a.txt.trashinfo"), Some("a.txt"));
        assert_eq!(trash_item_name(".hidden.trashinfo"), Some(".hidden"));
        assert_eq!(trash_item_name("....trashinfo"), Some("..."));
        for bad in [
            ".trashinfo",
            "..trashinfo",
            "...trashinfo",
            "a/b.trashinfo",
            "a.txt",
        ] {
            assert_eq!(trash_item_name(bad), None, "{bad}");
        }
    }

    #[test]
    fn wtf8_round_trips_lone_surrogates_and_pairs() {
        let cases: [&[u16]; 6] = [
            &[0x61, 0xD800, 0x62],
            &[0xDC00],
            &[0xD800, 0xD800, 0xDC00],
            &[0xD83D, 0xDDD1],
            &[0x65, 0x301, 0x2E, 0x74],
            &[0xDBFF],
        ];
        for units in cases {
            let b = wtf8_encode(units);
            assert_eq!(wtf8_decode(&b), units, "{b:x?}");
            // Through the kept-string form and back, byte for byte.
            let kept = escape_bytes(&b);
            assert_eq!(bytes_from_str(&kept), b, "{kept:?}");
        }
    }

    #[test]
    fn wtf8_of_valid_utf16_is_plain_utf8() {
        let s = "café \u{1f5d1}.txt";
        let units: Vec<u16> = s.encode_utf16().collect();
        assert_eq!(wtf8_encode(&units), s.as_bytes());
        assert_eq!(escape_bytes(&wtf8_encode(&units)), s);
    }

    #[test]
    fn wtf8_lone_surrogate_is_three_escaped_bytes_shown_as_replacement() {
        let b = wtf8_encode(&[0x61, 0xD800]);
        assert_eq!(b, [0x61, 0xED, 0xA0, 0x80]);
        let kept = escape_bytes(&b);
        assert_eq!(kept.chars().count(), 4);
        assert!(!show_name(&kept).is_empty());
        assert!(!show_name(&kept).contains(char::from_u32(RAW_BYTE_BASE + 0xED).unwrap()));
    }

    #[test]
    fn wtf8_decode_replaces_bytes_no_name_made() {
        assert_eq!(wtf8_decode(&[0xFF, 0x61]), [0xFFFD, 0x61]);
        assert_eq!(wtf8_decode(&[0xE0, 0x80, 0x80]), [0xFFFD, 0xFFFD, 0xFFFD]);
    }

    /// The open refuses a name-surrogate tag and rereads a data tag.
    /// A symlink (`0xA000000C`) and a mount point (`0xA0000003`) set the
    /// name-surrogate bit; Compact / WOF (`0x80000017`) does not.
    #[test]
    fn a_name_surrogate_tag_is_not_file_data() {
        assert!(super::reparse_tag_names_another_file(0xA000_000C));
        assert!(super::reparse_tag_names_another_file(0xA000_0003));
        assert!(!super::reparse_tag_names_another_file(0x8000_0017));
        assert!(!super::reparse_tag_names_another_file(0));
    }

    /// A failed query, a set attribute with tag 0, or a clear attribute
    /// with a tag is not file data. A clear attribute and tag 0 is a
    /// plain file. WOF is data; a symlink tag is a surrogate.
    #[test]
    fn an_unconfirmed_reparse_tag_is_not_file_data() {
        use super::{classify_reparse, ReparseLook};
        assert!(matches!(
            classify_reparse(false, true, 0x8000_0017),
            ReparseLook::Unconfirmed
        ));
        assert!(matches!(
            classify_reparse(true, true, 0),
            ReparseLook::Unconfirmed
        ));
        assert!(matches!(
            classify_reparse(true, false, 0x8000_0017),
            ReparseLook::Unconfirmed
        ));
        assert!(matches!(
            classify_reparse(true, false, 0),
            ReparseLook::Plain
        ));
        assert!(matches!(
            classify_reparse(true, true, 0x8000_0017),
            ReparseLook::Data
        ));
        assert!(matches!(
            classify_reparse(true, true, 0xA000_000C),
            ReparseLook::Surrogate
        ));
    }

    /// The Windows open keeps a plain file, reopens a data tag once, and
    /// refuses a surrogate or a tag it cannot confirm.
    #[test]
    fn a_reparse_open_refuses_reopens_or_keeps_the_handle() {
        use super::{reparse_open, ReparseLook, ReparseOpen};
        let refuse = |look, again, why| {
            assert_eq!(
                reparse_open(look, again),
                ReparseOpen::Refuse(why),
                "{look:?} reopened={again}"
            );
        };
        assert_eq!(reparse_open(ReparseLook::Plain, false), ReparseOpen::Plain);
        assert_eq!(reparse_open(ReparseLook::Plain, true), ReparseOpen::Plain);
        assert_eq!(reparse_open(ReparseLook::Data, false), ReparseOpen::Reopen);
        refuse(ReparseLook::Surrogate, false, "not a regular file");
        refuse(ReparseLook::Surrogate, true, "not a regular file");
        refuse(
            ReparseLook::Unconfirmed,
            false,
            "reparse tag could not be confirmed",
        );
        refuse(
            ReparseLook::Data,
            true,
            "reparse data was not read through its filter",
        );
        refuse(
            ReparseLook::Unconfirmed,
            true,
            "reparse data was not read through its filter",
        );
    }

    /// Applying the decision keeps a plain handle, reopens once, and
    /// refuses a second reopen. Replacing that with `Ok(file)` fails
    /// here, on Linux as well as on Windows.
    #[test]
    fn apply_reparse_open_keeps_reopens_or_refuses() {
        use super::{apply_reparse_open, ReparseOpen};
        let kept = apply_reparse_open(ReparseOpen::Plain, false, 7, |_| panic!("reopen")).unwrap();
        assert_eq!(kept, 7);
        let kept = apply_reparse_open(ReparseOpen::Plain, true, 7, |_| panic!("reopen")).unwrap();
        assert_eq!(kept, 7);
        let reopened = apply_reparse_open(ReparseOpen::Reopen, false, 7, |n| Ok(n + 1)).unwrap();
        assert_eq!(reopened, 8);
        let err = apply_reparse_open(ReparseOpen::Reopen, true, 7, |_| panic!("second reopen"))
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(
            err.to_string(),
            "reparse data was not read through its filter"
        );
        let err = apply_reparse_open(ReparseOpen::Refuse("not a regular file"), false, 7, |_| {
            panic!("reopen")
        })
        .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(err.to_string(), "not a regular file");
    }

    /// Set `key` for this test and put the previous value back, including
    /// when the open panics.
    #[cfg(debug_assertions)]
    struct RestoreEnv(&'static str, Option<std::ffi::OsString>);

    #[cfg(debug_assertions)]
    impl Drop for RestoreEnv {
        fn drop(&mut self) {
            match self.1.take() {
                Some(v) => std::env::set_var(self.0, v),
                None => std::env::remove_var(self.0),
            }
        }
    }

    #[cfg(debug_assertions)]
    impl RestoreEnv {
        fn set(key: &'static str, value: &std::ffi::OsStr) -> Self {
            let prev = std::env::var_os(key);
            std::env::set_var(key, value);
            Self(key, prev)
        }
    }

    /// The lstat sees a regular file. The seam then plants a symlink, so
    /// only the open's own nofollow can refuse it. Dropping `O_NOFOLLOW`
    /// (or `FILE_FLAG_OPEN_REPARSE_POINT`) would read the target.
    #[cfg(debug_assertions)]
    #[test]
    fn open_refuses_a_symlink_planted_after_the_stat() {
        let dir = std::env::temp_dir().join(format!("rtrash-nofollow-swap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let secret = dir.join("secret");
        std::fs::write(&secret, b"secret-target").unwrap();
        let file = dir.join("plain");
        std::fs::write(&file, b"plain-bytes").unwrap();
        let _swap = RestoreEnv::set("RTRASH_TEST_NOFOLLOW_SWAP", file.as_os_str());
        let _target = RestoreEnv::set("RTRASH_TEST_NOFOLLOW_TARGET", secret.as_os_str());
        let opened = super::open_regular_nofollow(&file);
        let meta = std::fs::symlink_metadata(&file).ok();
        let planted = meta.is_some_and(|m| m.file_type().is_symlink());
        assert!(
            planted,
            "the seam did not plant a symlink (privilege or Developer Mode?)"
        );
        assert!(opened.is_err(), "the open followed the planted symlink");
        assert_eq!(std::fs::read(&secret).unwrap(), b"secret-target");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A symlink to a regular file is not opened. The target stays as it was.
    #[cfg(unix)]
    #[test]
    fn open_regular_nofollow_refuses_a_symlink_to_a_file() {
        let dir = std::env::temp_dir().join(format!("rtrash-nofollow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("target");
        std::fs::write(&target, b"secret-target").unwrap();
        let link = dir.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let err = super::open_regular_nofollow(&link).unwrap_err();
        // The lstat refuses the link. Without that check the open's
        // O_NOFOLLOW fails instead, and the error is not this one.
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(err.to_string(), "not a regular file");
        assert_eq!(std::fs::read(&target).unwrap(), b"secret-target");
        let file = dir.join("plain");
        std::fs::write(&file, b"plain-bytes").unwrap();
        let mut got = Vec::new();
        use std::io::Read;
        super::open_regular_nofollow(&file)
            .unwrap()
            .read_to_end(&mut got)
            .unwrap();
        assert_eq!(got, b"plain-bytes");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The Windows open refuses a file symlink the same way. Creating the
    /// link needs the symlink privilege or Developer Mode.
    #[cfg(windows)]
    #[test]
    fn open_regular_nofollow_refuses_a_symlink_to_a_file() {
        let dir = std::env::temp_dir().join(format!("rtrash-nofollow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("target");
        std::fs::write(&target, b"secret-target").unwrap();
        let link = dir.join("link");
        if let Err(e) = std::os::windows::fs::symlink_file(&target, &link) {
            panic!(
                "cannot create a file link ({e}); this test needs the symlink \
                 privilege or Developer Mode"
            );
        }
        let err = super::open_regular_nofollow(&link).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(err.to_string(), "not a regular file");
        assert_eq!(std::fs::read(&target).unwrap(), b"secret-target");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
