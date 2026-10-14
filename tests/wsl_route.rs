//! The WSL Recycle Bin route, with stand-ins for `wslpath` and
//! `powershell.exe` on PATH (one test, so PATH changes do not race).
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;

#[test]
fn recycle_hands_the_windows_path_to_powershell_and_checks_the_result() {
    let root = std::env::temp_dir().join(format!("rtrash-wsl-{}", std::process::id()));
    let bin = root.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let script = |name: &str, body: &str| {
        let p = bin.join(name);
        fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    };
    script("wslpath", r#"[ "$1" = -w ] && printf '%s\n' "$2""#);
    // Records what it got, then removes the target unless told to fail.
    script(
        "powershell.exe",
        r#"printf '%s|%s\n' "$RTRASH_WSL_TARGET" "$WSLENV" >> "$(dirname "$0")/calls"
[ -n "$FAKE_FAIL" ] && { echo boom >&2; exit 1; }
[ -n "$FAKE_KEEP" ] && exit 0
rm -f -- "$RTRASH_WSL_TARGET""#,
    );
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    std::env::set_var("PATH", path);
    std::env::set_var("WSLENV", "OTHER/p");

    let f = root.join("a b's \"x\".txt");
    fs::write(&f, b"x").unwrap();
    rtrash::wsl::recycle(&f, false).unwrap();
    assert!(!f.exists());
    let calls = fs::read_to_string(bin.join("calls")).unwrap();
    assert_eq!(
        calls.lines().last().unwrap(),
        format!("{}|OTHER/p:RTRASH_WSL_TARGET", f.display())
    );

    fs::write(&f, b"x").unwrap();
    std::env::set_var("FAKE_FAIL", "1");
    let e = rtrash::wsl::recycle(&f, false).unwrap_err();
    assert!(e.contains("boom"), "{e}");
    std::env::remove_var("FAKE_FAIL");
    std::env::set_var("FAKE_KEEP", "1");
    let e = rtrash::wsl::recycle(&f, false).unwrap_err();
    assert!(e.contains("still there"), "{e}");
    assert!(f.exists());

    // Detection reads the kernel release.
    let rel = root.join("osrelease");
    fs::write(&rel, "5.15.153.1-microsoft-standard-WSL2\n").unwrap();
    std::env::set_var("RTRASH_WSL_OSRELEASE", &rel);
    assert!(rtrash::wsl::is_wsl());
    fs::write(&rel, "6.8.0-generic\n").unwrap();
    assert!(!rtrash::wsl::is_wsl());
    let _ = fs::remove_dir_all(&root);
}
