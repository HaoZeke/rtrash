//! Python bindings must release the GIL around FreeDesktop I/O.

#[test]
fn python_rs_uses_detach_for_io() {
    let src = include_str!("../src/python.rs");
    let n = src.matches("detach").count();
    assert!(
        n >= 4,
        "expected detach on put/empty/restore/list (found {n})"
    );
    assert!(
        src.contains("--plain"),
        "Python empty/put/restore must force --plain to avoid TUI"
    );
}
