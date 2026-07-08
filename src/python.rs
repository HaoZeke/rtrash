//! PyO3 bindings: FreeDesktop trash as a safe alternative to permanent delete.
//!
//! Primary entry points replace `os.remove` / `pathlib.Path.unlink` /
//! `shutil.rmtree` of *user data you may want back* by moving paths into the
//! XDG trash instead of unlinking them.
//!
//! **GIL:** put / empty / restore / list scan release the GIL around FreeDesktop
//! I/O (`Python::detach`) so other Python threads can run during slow
//! trash moves and bulk empty. List materialization back into a `PyList` re-takes
//! the GIL only for the Python object build.

use std::path::{Path, PathBuf};

use pyo3::exceptions::{PyOSError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyList;

use crate::{empty, list, put as put_mod, restore, trashdir};

fn map_code(code: i32, what: &str) -> PyResult<()> {
    match code {
        0 => Ok(()),
        2 => Err(PyValueError::new_err(format!("{what}: invalid arguments"))),
        _ => Err(PyOSError::new_err(format!("{what} failed (exit {code})"))),
    }
}

/// Move one or more paths into the FreeDesktop trash.
///
/// Safe alternative to permanent deletion via `os.remove`, `os.unlink`,
/// `pathlib.Path.unlink`, or `shutil.rmtree` when you want recoverability.
///
/// Releases the GIL for the duration of the trash I/O.
///
/// Parameters
/// ----------
/// paths : sequence of str or path-like
///     Paths to trash.
/// recursive : bool, default False
///     Allow directories (like ``rm -r`` / trash-put of trees).
/// force : bool, default False
///     Ignore missing paths (like ``rm -f``).
///
/// Raises
/// ------
/// OSError
///     When put fails for a reason other than usage.
/// ValueError
///     When arguments are invalid (e.g. empty paths without force).
#[pyfunction]
#[pyo3(signature = (paths, *, recursive=false, force=false))]
fn put_paths(py: Python<'_>, paths: Vec<PathBuf>, recursive: bool, force: bool) -> PyResult<()> {
    if paths.is_empty() && !force {
        return Err(PyValueError::new_err("put: missing path operand"));
    }
    let mut args: Vec<String> = Vec::new();
    if recursive {
        args.push("-r".into());
    }
    if force {
        args.push("-f".into());
    }
    // Always non-interactive for the in-process API (no put TUI).
    args.push("--plain".into());
    for p in &paths {
        args.push(p.as_os_str().to_string_lossy().into_owned());
    }
    let code = py.detach(|| put_mod::run("rtrash", &args));
    map_code(code, "put")
}

/// Convenience: trash a single path (keyword flags same as [`put_paths`]).
#[pyfunction]
#[pyo3(name = "put", signature = (path, *, recursive=false, force=false))]
fn py_put(py: Python<'_>, path: PathBuf, recursive: bool, force: bool) -> PyResult<()> {
    put_paths(py, vec![path], recursive, force)
}

/// Alias intended as a drop-in mindset for `os.remove` / `Path.unlink`.
///
/// Always moves to trash (never permanent unlink). Directories require
/// ``recursive=True``. Releases the GIL during the trash I/O.
#[pyfunction]
#[pyo3(signature = (path, *, recursive=false, force=false))]
fn unlink(py: Python<'_>, path: PathBuf, recursive: bool, force: bool) -> PyResult<()> {
    put_paths(py, vec![path], recursive, force)
}

/// Alias for permanent-delete replacement of `shutil.rmtree` via trash.
///
/// Equivalent to ``put(path, recursive=True)``. Releases the GIL during I/O.
#[pyfunction]
#[pyo3(signature = (path, *, force=false))]
fn rmtree(py: Python<'_>, path: PathBuf, force: bool) -> PyResult<()> {
    put_paths(py, vec![path], true, force)
}

/// List trashed items as ``(deletion_date, original_path)`` strings.
///
/// ``deletion_date`` uses trash-list form ``YYYY-MM-DD HH:MM:SS``.
/// Scans trash dirs with the GIL released; builds the Python list under the GIL.
#[pyfunction]
#[pyo3(signature = (*, trash_dir=None))]
fn list_trash(py: Python<'_>, trash_dir: Option<PathBuf>) -> PyResult<Py<PyList>> {
    let rows: Vec<(String, String)> = py.detach(|| {
        let dirs = match trash_dir {
            Some(p) => trashdir::resolve_dirs(&[p]),
            None => trashdir::all(),
        };
        list::collect(&dirs)
            .into_iter()
            .map(|e| {
                let date = e
                    .date
                    .as_deref()
                    .unwrap_or("????-??-??T??:??:??")
                    .replacen('T', " ", 1);
                let path = e.original.display().to_string();
                (date, path)
            })
            .collect()
    });
    let list = PyList::empty(py);
    for row in rows {
        list.append(row)?;
    }
    Ok(list.unbind())
}

/// Empty the trash (optionally only items older than ``days``).
///
/// When ``trash_dir`` is set, only that trash root is emptied (repeat pin
/// semantics via a single path; pass the FreeDesktop trash root containing
/// ``files/`` and ``info/``).
///
/// Always non-interactive (``--plain``); never opens the empty TUI.
/// Releases the GIL for the duration of the purge.
#[pyfunction]
#[pyo3(signature = (days=None, *, trash_dir=None, dry_run=false))]
fn empty_trash(
    py: Python<'_>,
    days: Option<i64>,
    trash_dir: Option<PathBuf>,
    dry_run: bool,
) -> PyResult<()> {
    let mut args: Vec<String> = Vec::new();
    // Never enter the ratatui empty browser from Python.
    args.push("--plain".into());
    if dry_run {
        args.push("--dry-run".into());
    }
    if let Some(p) = trash_dir {
        args.push(format!("--trash-dir={}", p.display()));
    }
    if let Some(d) = days {
        if d < 0 {
            return Err(PyValueError::new_err("days must be >= 0"));
        }
        args.push(d.to_string());
    }
    let code = py.detach(|| empty::run("rtrash", &args));
    map_code(code, "empty")
}

/// Restore a trashed item originally at ``path``.
///
/// Releases the GIL during restore I/O. Does not open the restore TUI
/// (a PATH operand selects the item directly).
#[pyfunction]
#[pyo3(signature = (path, *, force=false, trash_dir=None))]
fn restore_path(
    py: Python<'_>,
    path: PathBuf,
    force: bool,
    trash_dir: Option<PathBuf>,
) -> PyResult<()> {
    let mut args: Vec<String> = Vec::new();
    args.push("--plain".into());
    if force {
        args.push("-f".into());
    }
    if let Some(p) = trash_dir {
        args.push(format!("--trash-dir={}", p.display()));
    }
    args.push(path.as_os_str().to_string_lossy().into_owned());
    let code = py.detach(|| restore::run("rtrash", &args));
    map_code(code, "restore")
}

/// Return the home trash directory path (``$XDG_DATA_HOME/Trash``).
#[pyfunction]
fn home_trash() -> PathBuf {
    trashdir::home_trash()
}

/// Package version string.
#[pyfunction]
fn version() -> &'static str {
    crate::version()
}

/// Whether ``path`` currently exists (thin helper for examples/tests).
#[pyfunction]
fn path_exists(path: &str) -> bool {
    Path::new(path).exists()
}

#[pymodule]
fn rtrash(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("__version__", crate::version())?;
    m.add_function(wrap_pyfunction!(py_put, m)?)?;
    m.add_function(wrap_pyfunction!(put_paths, m)?)?;
    m.add_function(wrap_pyfunction!(unlink, m)?)?;
    m.add_function(wrap_pyfunction!(rmtree, m)?)?;
    m.add_function(wrap_pyfunction!(list_trash, m)?)?;
    m.add_function(wrap_pyfunction!(empty_trash, m)?)?;
    m.add_function(wrap_pyfunction!(restore_path, m)?)?;
    m.add_function(wrap_pyfunction!(home_trash, m)?)?;
    m.add_function(wrap_pyfunction!(version, m)?)?;
    m.add_function(wrap_pyfunction!(path_exists, m)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    /// Structural: every I/O entry point must release the GIL.
    #[test]
    fn source_releases_gil_on_io_paths() {
        let src = include_str!("python.rs");
        assert!(
            src.matches("detach").count() >= 3,
            "put/empty/restore/list must use Python::detach"
        );
        assert!(
            src.contains("args.push(\"--plain\""),
            "Python empty/put/restore must force --plain (no TUI)"
        );
    }
}
