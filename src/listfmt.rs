//! `list --format`: the template parser and renderer, shared by the
//! FreeDesktop, Finder and Windows listings.

/// A path's bytes: raw on Unix, UTF-8 (lossy) on Windows.
fn os_bytes(s: &std::ffi::OsStr) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        s.as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        s.to_string_lossy().into_owned().into_bytes()
    }
}

#[cfg(unix)]
fn owner_of(p: &std::path::Path) -> String {
    crate::trashdir::owner_name(p)
}

#[cfg(not(unix))]
fn owner_of(_: &std::path::Path) -> String {
    String::new()
}

/// One field `list --format` can print.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    Date,
    Epoch,
    Path,
    Name,
    Trash,
    Batch,
    Size,
    Owner,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FmtPart {
    Lit(Vec<u8>),
    Field(Field),
}

/// Parse a `--format` template; an unknown or unclosed field is an error.
pub fn parse_format(t: &str) -> Result<Vec<FmtPart>, String> {
    let mut parts = Vec::new();
    let mut lit: Vec<u8> = Vec::new();
    let mut it = t.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '{' if it.peek() == Some(&'{') => {
                it.next();
                lit.push(b'{');
            }
            '}' if it.peek() == Some(&'}') => {
                it.next();
                lit.push(b'}');
            }
            '{' => {
                let key: String = it.by_ref().take_while(|&c| c != '}').collect();
                let f = match key.as_str() {
                    "date" => Field::Date,
                    "epoch" => Field::Epoch,
                    "path" => Field::Path,
                    "name" => Field::Name,
                    "trash" => Field::Trash,
                    "batch" => Field::Batch,
                    "size" => Field::Size,
                    "owner" => Field::Owner,
                    _ => return Err(format!("--format: unknown field '{{{key}}}'")),
                };
                if !lit.is_empty() {
                    parts.push(FmtPart::Lit(std::mem::take(&mut lit)));
                }
                parts.push(FmtPart::Field(f));
            }
            '}' => return Err("--format: a lone '}' (write '}}')".into()),
            '\\' => match it.next() {
                Some('t') => lit.push(b'\t'),
                Some('n') => lit.push(b'\n'),
                Some('0') => lit.push(0),
                Some('\\') => lit.push(b'\\'),
                Some(o) => {
                    lit.push(b'\\');
                    let mut b = [0; 4];
                    lit.extend_from_slice(o.encode_utf8(&mut b).as_bytes());
                }
                None => lit.push(b'\\'),
            },
            c => {
                let mut b = [0; 4];
                lit.extend_from_slice(c.encode_utf8(&mut b).as_bytes());
            }
        }
    }
    if !lit.is_empty() {
        parts.push(FmtPart::Lit(lit));
    }
    Ok(parts)
}

/// The values one `--format` row can show.
pub struct FmtRow<'a> {
    pub date: &'a str,
    pub epoch: i64,
    pub path: &'a std::path::Path,
    pub name: &'a str,
    pub trash: &'a std::path::Path,
    pub batch: &'a str,
    pub size: Option<u64>,
    pub owner: Option<&'a str>,
}

/// One item through a parsed template; paths go out as raw bytes.
pub fn render_format(parts: &[FmtPart], r: &FmtRow) -> Vec<u8> {
    let mut out = Vec::new();
    for p in parts {
        match p {
            FmtPart::Lit(b) => out.extend_from_slice(b),
            FmtPart::Field(f) => match f {
                Field::Date => out.extend_from_slice(r.date.as_bytes()),
                Field::Epoch => out.extend_from_slice(r.epoch.to_string().as_bytes()),
                Field::Path => out.extend_from_slice(&os_bytes(r.path.as_os_str())),
                Field::Name => out.extend_from_slice(r.name.as_bytes()),
                Field::Trash => out.extend_from_slice(&os_bytes(r.trash.as_os_str())),
                Field::Batch => out.extend_from_slice(r.batch.as_bytes()),
                Field::Size => {
                    if let Some(n) = r.size {
                        out.extend_from_slice(n.to_string().as_bytes());
                    }
                }
                Field::Owner => {
                    let o = r
                        .owner
                        .map(str::to_string)
                        .unwrap_or_else(|| owner_of(r.trash));
                    out.extend_from_slice(o.as_bytes());
                }
            },
        }
    }
    out
}
