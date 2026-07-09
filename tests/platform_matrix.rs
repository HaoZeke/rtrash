//! Structural + pure tests for cross-platform backends (host-free).
//! Does not require macOS or Windows runtimes.

use std::path::{Path, PathBuf};

use rtrash::platform::{
    format_deletion_list_line, home_trash_from, trash_backend, trash_backend_label, TrashBackend,
};
use rtrash::win_recycle::{
    encode_recycle_i_v2, filetime_to_unix_epoch, parse_recycle_i, FILETIME_UNIX_EPOCH,
    HUNDRED_NS_PER_SEC,
};

#[test]
fn backend_matches_target_os() {
    let b = trash_backend();
    if cfg!(target_os = "macos") {
        assert_eq!(b, TrashBackend::MacosFdoExperimental);
        assert!(trash_backend_label().contains("not Finder"));
    } else if cfg!(windows) {
        assert_eq!(b, TrashBackend::WindowsRecycleBin);
        assert!(trash_backend_label().contains("Recycle"));
    } else if cfg!(target_os = "linux") {
        assert_eq!(b, TrashBackend::FreeDesktop);
    }
}

#[test]
fn home_trash_macos_style_path() {
    use std::ffi::OsStr;
    let p = home_trash_from(None, Some(OsStr::new("/Users/alice")));
    assert_eq!(p, PathBuf::from("/Users/alice/.local/share/Trash"));
    let p2 = home_trash_from(Some(OsStr::new("/Users/alice/Library/xdg")), None);
    assert_eq!(p2, PathBuf::from("/Users/alice/Library/xdg/Trash"));
}

#[test]
fn windows_i_file_roundtrip_is_not_freedesktop() {
    // $I format is Recycle Bin metadata, not .trashinfo.
    let ft = FILETIME_UNIX_EPOCH + 3_600 * HUNDRED_NS_PER_SEC;
    let buf = encode_recycle_i_v2(Path::new(r"D:\work\report.pdf"), 99, ft);
    let meta = parse_recycle_i(&buf).expect("$I parse");
    assert_eq!(meta.size, 99);
    assert_eq!(filetime_to_unix_epoch(ft), 3_600);
    assert!(meta.original.to_string_lossy().contains("report.pdf"));
    // Must not look like a FreeDesktop trashinfo body.
    let as_str = String::from_utf8_lossy(&buf);
    assert!(!as_str.contains("[Trash Info]"));
}

#[test]
fn list_line_helper_stable() {
    let line = format_deletion_list_line(Some("2020-06-01T12:00:00"), Path::new("/tmp/x"));
    assert_eq!(line, "2020-06-01T12:00:00 /tmp/x");
}

#[test]
fn readme_documents_platform_matrix() {
    let readme = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/README.md"));
    assert!(
        readme.contains("experimental") && readme.contains("Finder"),
        "README must state macOS FDO is experimental / not Finder"
    );
    assert!(
        readme.contains("Recycle Bin")
            && (readme.contains("not FreeDesktop")
                || readme.contains("Not** FreeDesktop")
                || readme.contains("**Not** FreeDesktop")),
        "README must refuse FreeDesktop-as-Recycle-Bin fiction"
    );
    assert!(
        readme.contains("Linux"),
        "README platform matrix mentions Linux"
    );
}
