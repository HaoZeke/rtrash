//! The `status --json` object, shared by the FreeDesktop status and the
//! Windows Recycle Bin status so both keep one schema.
//!
//! ```text
//! {
//!   "roots": [
//!     {"path":P,"items":N,"bytes":N,"orphans":N|null,"dangling":N|null,
//!      "fs_bytes":N,"fs_free_bytes":N,"fs_inodes":N|null,
//!      "fs_free_inodes":N|null,"fs_low":B, ...extras}
//!   ],
//!   "total_items": N,
//!   "total_bytes": N
//! }
//! ```
//!
//! A count the source cannot give is null, never a guess: the Recycle Bin
//! reports no orphans or dangling records, and a file system without an
//! inode limit has no inode counts. The `fs_*` fields are left out when
//! the free space cannot be read.

use crate::space::{self, Space};
use crate::util::json_escape;

/// One root of the object.
pub struct Root<'a> {
    pub path: &'a str,
    pub items: u64,
    pub bytes: u64,
    /// Payloads with no record; None where the source cannot tell.
    pub orphans: Option<u64>,
    /// Records whose payload is gone; None where the source cannot tell.
    pub dangling: Option<u64>,
    /// The root's file system; None leaves the `fs_*` fields out.
    pub space: Option<Space>,
    /// Fields after the shared ones, each written as `,"key":value`.
    pub extra: String,
}

/// A count, or `null` when unknown.
pub fn count(v: Option<u64>) -> String {
    v.map_or_else(|| "null".to_string(), |n| n.to_string())
}

/// The `fs_*` fields of a root, each with its leading comma; empty when
/// the free space is unknown.
pub fn free_fields(s: Option<&Space>) -> String {
    let Some(s) = s else {
        return String::new();
    };
    format!(
        ",\"fs_bytes\":{},\"fs_free_bytes\":{},\"fs_inodes\":{},\
         \"fs_free_inodes\":{},\"fs_low\":{}",
        s.total,
        s.avail,
        count(s.inodes),
        count(s.inodes_avail),
        !s.lows(space::WARN_FREE_PERCENT).is_empty()
    )
}

/// One root as its line of the object, indented, without a comma.
pub fn root_line(r: &Root) -> String {
    format!(
        "    {{\"path\":\"{}\",\"items\":{},\"bytes\":{},\"orphans\":{},\"dangling\":{}{}{}}}",
        json_escape(r.path),
        r.items,
        r.bytes,
        count(r.orphans),
        count(r.dangling),
        free_fields(r.space.as_ref()),
        r.extra
    )
}

/// The whole object, ending in a newline.
pub fn document(roots: &[Root], total_items: u64, total_bytes: u64) -> String {
    let mut out = String::from("{\n  \"roots\": [\n");
    let lines: Vec<String> = roots.iter().map(root_line).collect();
    out.push_str(&lines.join(",\n"));
    if !lines.is_empty() {
        out.push('\n');
    }
    out.push_str(&format!(
        "  ],\n  \"total_items\": {total_items},\n  \"total_bytes\": {total_bytes}\n}}\n"
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(path: &str) -> Root<'_> {
        Root {
            path,
            items: 2,
            bytes: 10,
            orphans: Some(1),
            dangling: Some(0),
            space: None,
            extra: String::new(),
        }
    }

    #[test]
    fn a_freedesktop_root_keeps_its_fields_in_order() {
        let mut r = root("/t/Trash");
        r.space = Some(Space {
            total: 100,
            avail: 5,
            inodes: Some(10),
            inodes_avail: Some(9),
        });
        assert_eq!(
            root_line(&r),
            "    {\"path\":\"/t/Trash\",\"items\":2,\"bytes\":10,\"orphans\":1,\"dangling\":0,\
             \"fs_bytes\":100,\"fs_free_bytes\":5,\"fs_inodes\":10,\
             \"fs_free_inodes\":9,\"fs_low\":true}"
        );
    }

    #[test]
    fn unknown_counts_are_null_and_unknown_space_is_left_out() {
        let mut r = root("C:\\");
        r.orphans = None;
        r.dangling = None;
        r.extra = ",\"x\":1".into();
        assert_eq!(
            root_line(&r),
            "    {\"path\":\"C:\\\\\",\"items\":2,\"bytes\":10,\
             \"orphans\":null,\"dangling\":null,\"x\":1}"
        );
        r.space = Some(Space {
            total: 100,
            avail: 50,
            inodes: None,
            inodes_avail: None,
        });
        assert!(root_line(&r).contains(
            ",\"fs_bytes\":100,\"fs_free_bytes\":50,\"fs_inodes\":null,\
             \"fs_free_inodes\":null,\"fs_low\":false,\"x\":1}"
        ));
    }

    #[test]
    fn the_document_lists_roots_then_totals() {
        assert_eq!(
            document(&[], 0, 0),
            "{\n  \"roots\": [\n  ],\n  \"total_items\": 0,\n  \"total_bytes\": 0\n}\n"
        );
        let two = document(&[root("/a"), root("/b")], 4, 20);
        let lines: Vec<&str> = two.lines().collect();
        assert_eq!(lines.len(), 8, "{two}");
        assert!(lines[2].starts_with("    {\"path\":\"/a\"") && lines[2].ends_with("},"));
        assert!(lines[3].starts_with("    {\"path\":\"/b\"") && lines[3].ends_with('}'));
        assert_eq!(lines[5], "  \"total_items\": 4,");
        assert_eq!(lines[6], "  \"total_bytes\": 20");
        assert_eq!(lines[7], "}");
    }
}
