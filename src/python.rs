//! PyO3 bindings: FreeDesktop trash as a safe alternative to permanent delete.
//!
//! Primary entry points replace `os.remove` / `pathlib.Path.unlink` /
//! `shutil.rmtree` of *user data you may want back* by moving paths into the
//! XDG trash instead of unlinking them.

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
fn put_paths(paths: Vec<PathBuf>, recursive: bool, force: bool) -> PyResult<()> {
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
    for p in &paths {
        args.push(p.as_os_str().to_string_lossy().into_owned());
    }
    map_code(put_mod::run("rtrash", &args), "put")
}

/// Convenience: trash a single path (keyword flags same as [`put_paths`]).
#[pyfunction]
#[pyo3(name = "put", signature = (path, *, recursive=false, force=false))]
fn py_put(path: PathBuf, recursive: bool, force: bool) -> PyResult<()> {
    put_paths(vec![path], recursive, force)
}

/// Alias intended as a drop-in mindset for `os.remove` / `Path.unlink`.
///
/// Always moves to trash (never permanent unlink). Directories require
/// ``recursive=True``.
#[pyfunction]
#[pyo3(signature = (path, *, recursive=false, force=false))]
fn unlink(path: PathBuf, recursive: bool, force: bool) -> PyResult<()> {
    put_paths(vec![path], recursive, force)
}

/// Alias for permanent-delete replacement of `shutil.rmtree` via trash.
///
/// Equivalent to ``put(path, recursive=True)``.
#[pyfunction]
#[pyo3(signature = (path, *, force=false))]
fn rmtree(path: PathBuf, force: bool) -> PyResult<()> {
    put_paths(vec![path], true, force)
}

/// List trashed items as ``(deletion_date, original_path)`` strings.
///
/// ``deletion_date`` uses trash-list form ``YYYY-MM-DD HH:MM:SS``.
#[pyfunction]
#[pyo3(signature = (*, trash_dir=None))]
fn list_trash(py: Python<'_>, trash_dir: Option<PathBuf>) -> PyResult<Py<PyList>> {
    let dirs = match trash_dir {
        Some(p) => trashdir::resolve_dirs(&[p]),
        None => trashdir::all(),
    };
    let entries = list::collect(&dirs);
    let list = PyList::empty(py);
    for e in entries {
        let date = e
            .date
            .as_deref()
            .unwrap_or("????-??-??T??:??:??")
            .replacen('T', " ", 1);
        let path = e.original.display().to_string();
        list.append((date, path))?;
    }
    Ok(list.unbind())
}

/// Empty the trash (optionally only items older than ``days``).
///
/// When ``trash_dir`` is set, only that trash root is emptied (repeat pin
/// semantics via a single path; pass the FreeDesktop trash root containing
/// ``files/`` and ``info/``).
#[pyfunction]
#[pyo3(signature = (days=None, *, trash_dir=None, dry_run=false))]
fn empty_trash(days: Option<i64>, trash_dir: Option<PathBuf>, dry_run: bool) -> PyResult<()> {
    let mut args: Vec<String> = Vec::new();
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
    map_code(empty::run("rtrash", &args), "empty")
}

/// Restore a trashed item originally at ``path``.
#[pyfunction]
#[pyo3(signature = (path, *, force=false, trash_dir=None))]
fn restore_path(path: PathBuf, force: bool, trash_dir: Option<PathBuf>) -> PyResult<()> {
    let mut args: Vec<String> = Vec::new();
    if force {
        args.push("-f".into());
    }
    if let Some(p) = trash_dir {
        args.push(format!("--trash-dir={}", p.display()));
    }
    args.push(path.as_os_str().to_string_lossy().into_owned());
    map_code(restore::run("rtrash", &args), "restore")
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
