//! Integration tests for multi-select restore / empty / put selection logic.
//! Drive shipped library functions against a real FreeDesktop trash layout
//! (isolated XDG_DATA_HOME). No alternate-screen / ratatui event loop.

// The browsers and their helpers exist only with the `tui` feature.
#![cfg(all(unix, feature = "tui"))]

mod common;

use std::cell::Cell;
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

/// Serialise tests that mutate process env (put and the catalog read it).
static ENV_LOCK: Mutex<()> = Mutex::new(());

thread_local! {
    static CATALOG_DEPTH: Cell<u32> = const { Cell::new(0) };
}

use rtrash::empty_tui;
use rtrash::list;
use rtrash::put_tui;
use rtrash::restore_tui;
use rtrash::trashdir;

struct TrashSandbox {
    root: PathBuf,
}

impl TrashSandbox {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "rtrash-tui-logic-{}-{}-{}",
            std::process::id(),
            tag,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("work")).unwrap();
        fs::create_dir_all(root.join("xdg")).unwrap();
        // Originals are recorded with symlinked parents resolved (macOS
        // /var is /private/var), so compare against the resolved root.
        let root = fs::canonicalize(&root).unwrap();
        TrashSandbox { root }
    }

    fn work(&self) -> PathBuf {
        self.root.join("work")
    }

    fn trash(&self) -> PathBuf {
        self.root.join("xdg/Trash")
    }

    fn put(&self, names: &[&str]) {
        for n in names {
            let p = self.work().join(n);
            fs::write(&p, format!("payload-{n}").as_bytes()).unwrap();
        }
        let mut cmd = common::rtrash(&self.root);
        cmd.arg("put").args(names);
        let out = cmd.current_dir(self.work()).output().expect("put");
        assert!(
            out.status.success(),
            "put failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// `HOME`, `XDG_DATA_HOME` and `XDG_CACHE_HOME` for this sandbox.
    /// In-process `list::collect` uses the test process, and `cfg(test)`
    /// is not set on the library it links, so an unpinned call writes the
    /// real cache and key. A nested call keeps the outer pin.
    fn catalog_env(&self) -> CatalogEnv {
        common::install_catalog_guard();
        if CATALOG_DEPTH.with(|d| d.get()) > 0 {
            CATALOG_DEPTH.with(|d| d.set(d.get() + 1));
            return CatalogEnv {
                lock: None,
                home: None,
                data: None,
                cache: None,
                outer: false,
            };
        }
        let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = std::env::var_os("HOME");
        let data = std::env::var_os("XDG_DATA_HOME");
        let cache = std::env::var_os("XDG_CACHE_HOME");
        // SAFETY: ENV_LOCK serializes these writes, and Drop puts them back.
        unsafe {
            std::env::set_var("HOME", &self.root);
            std::env::set_var("XDG_DATA_HOME", self.root.join("xdg"));
            std::env::set_var("XDG_CACHE_HOME", self.root.join(".cache"));
        }
        CATALOG_DEPTH.with(|d| d.set(1));
        CatalogEnv {
            lock: Some(lock),
            home,
            data,
            cache,
            outer: true,
        }
    }

    fn collect_entries(&self) -> Vec<list::Entry> {
        let _env = self.catalog_env();
        let dir = trashdir::TrashDir {
            root: self.trash(),
            topdir: None,
        };
        list::collect(std::slice::from_ref(&dir))
    }
}

struct CatalogEnv {
    lock: Option<MutexGuard<'static, ()>>,
    home: Option<OsString>,
    data: Option<OsString>,
    cache: Option<OsString>,
    outer: bool,
}

impl Drop for CatalogEnv {
    fn drop(&mut self) {
        if !self.outer {
            CATALOG_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
            return;
        }
        restore_var("HOME", self.home.take());
        restore_var("XDG_DATA_HOME", self.data.take());
        restore_var("XDG_CACHE_HOME", self.cache.take());
        CATALOG_DEPTH.with(|d| d.set(0));
        // Keep the lock until the variables are back, then release it.
        drop(self.lock.take());
    }
}

fn restore_var(key: &str, val: Option<OsString>) {
    // SAFETY: the outer guard still holds ENV_LOCK.
    unsafe {
        match val {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
    }
}

impl Drop for TrashSandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn restore_selection_restores_two_of_three() {
    let sb = TrashSandbox::new("restore-2of3");
    let _env = sb.catalog_env();
    sb.put(&["a.txt", "b.txt", "c.txt"]);
    assert!(!sb.work().join("a.txt").exists());
    assert!(!sb.work().join("b.txt").exists());
    assert!(!sb.work().join("c.txt").exists());

    let entries = sb.collect_entries();
    assert_eq!(entries.len(), 3, "expected 3 trash entries");
    let refs: Vec<&list::Entry> = entries.iter().collect();

    // Map names to indices (order is by deletion epoch).
    let idx_of = |name: &str| -> usize {
        refs.iter()
            .position(|e| e.original.file_name().and_then(|f| f.to_str()) == Some(name))
            .unwrap_or_else(|| panic!("missing {name}"))
    };
    let ia = idx_of("a.txt");
    let ic = idx_of("c.txt");
    let ib = idx_of("b.txt");

    let result = restore_tui::restore_selection("rtrash", &refs, &[ia, ic], true);
    assert_eq!(
        result.ok_count(),
        2,
        "succeeded={:?}",
        result.succeeded_idxs
    );
    assert_eq!(result.fail_count, 0);
    assert!(result.succeeded_idxs.contains(&ia));
    assert!(result.succeeded_idxs.contains(&ic));
    assert!(!result.succeeded_idxs.contains(&ib));

    assert!(sb.work().join("a.txt").exists(), "a restored");
    assert!(sb.work().join("c.txt").exists(), "c restored");
    assert!(!sb.work().join("b.txt").exists(), "b still trashed");
    assert_eq!(
        fs::read_to_string(sb.work().join("a.txt")).unwrap(),
        "payload-a.txt"
    );

    // Trash should still hold b only.
    let left = sb.collect_entries();
    assert_eq!(left.len(), 1);
    assert_eq!(
        left[0].original.file_name().and_then(|f| f.to_str()),
        Some("b.txt")
    );
}

/// The browser owns the screen while it restores, so outcome lines must
/// come back as data rather than land on stdout or stderr mid-draw.
#[test]
fn restore_selection_returns_outcome_lines() {
    let sb = TrashSandbox::new("restore-log");
    let _env = sb.catalog_env();
    sb.put(&["ok.txt", "busy.txt"]);
    fs::write(sb.work().join("busy.txt"), "blocker").unwrap();
    let entries = sb.collect_entries();
    let refs: Vec<&list::Entry> = entries.iter().collect();
    let all: Vec<usize> = (0..refs.len()).collect();

    let result = restore_tui::restore_selection("rtrash", &refs, &all, false);
    assert_eq!((result.ok_count(), result.fail_count), (1, 1));
    let ok = sb.work().join("ok.txt");
    let busy = sb.work().join("busy.txt");
    let restored = format!("restored '{}'", ok.display());
    let refused = format!(
        "rtrash: refusing to overwrite existing '{}' (use -f to force)",
        busy.display()
    );
    let lines: Vec<(bool, &str)> = result
        .log
        .iter()
        .map(|l| (l.is_error, l.text.as_str()))
        .collect();
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(lines.contains(&(false, restored.as_str())), "{lines:?}");
    assert!(lines.contains(&(true, refused.as_str())), "{lines:?}");
    assert_eq!(fs::read_to_string(&busy).unwrap(), "blocker");
}

#[test]
fn permanently_remove_entries_subset_leaves_others() {
    let sb = TrashSandbox::new("empty-subset");
    let _env = sb.catalog_env();
    sb.put(&["keep.txt", "drop1.txt", "drop2.txt"]);
    let entries = sb.collect_entries();
    assert_eq!(entries.len(), 3);
    let refs: Vec<&list::Entry> = entries.iter().collect();

    let drop_idxs: Vec<usize> = refs
        .iter()
        .enumerate()
        .filter(|(_, e)| {
            let n = e
                .original
                .file_name()
                .and_then(|f| f.to_str())
                .unwrap_or("");
            n.starts_with("drop")
        })
        .map(|(i, _)| i)
        .collect();
    assert_eq!(drop_idxs.len(), 2);

    let batch: Vec<&list::Entry> = drop_idxs.iter().map(|&i| refs[i]).collect();
    let result = empty_tui::permanently_remove_entries("rtrash", &batch, false);
    assert_eq!(result.ok_count(), 2, "succeeded={:?}", result.succeeded);
    assert_eq!(result.fail_count(), 0);
    assert!(result.succeeded.iter().all(|&s| s));

    let left = sb.collect_entries();
    assert_eq!(left.len(), 1);
    assert_eq!(
        left[0].original.file_name().and_then(|f| f.to_str()),
        Some("keep.txt")
    );
    // keep still only in trash, not work
    assert!(!sb.work().join("keep.txt").exists());
    assert!(sb.trash().join("files").read_dir().unwrap().count() >= 1);
}

#[test]
fn permanently_remove_entries_keeps_paths_in_the_keep_file() {
    let sb = TrashSandbox::new("empty-keep");
    let _env = sb.catalog_env();
    sb.put(&["kept.txt", "drop.txt"]);
    let kept = sb.work().join("kept.txt");
    fs::create_dir_all(sb.root.join(".config/rtrash")).unwrap();
    fs::write(
        sb.root.join(".config/rtrash/keep"),
        format!("{}\n", kept.display()),
    )
    .unwrap();
    let old_config = std::env::var_os("XDG_CONFIG_HOME");
    let old_override = std::env::var_os("RTRASH_CONFIG");
    // SAFETY: catalog_env holds ENV_LOCK, and these are restored below.
    unsafe {
        std::env::set_var("XDG_CONFIG_HOME", sb.root.join(".config"));
        std::env::remove_var("RTRASH_CONFIG");
    }
    let entries = sb.collect_entries();
    let refs: Vec<&list::Entry> = entries.iter().collect();
    let result = empty_tui::permanently_remove_entries("rtrash", &refs, false);
    unsafe {
        match old_config {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
        match old_override {
            Some(v) => std::env::set_var("RTRASH_CONFIG", v),
            None => std::env::remove_var("RTRASH_CONFIG"),
        }
    }
    assert_eq!((result.ok_count(), result.fail_count()), (1, 1));
    assert!(
        result
            .reasons
            .iter()
            .any(|line| line.contains("listed in the keep file")),
        "{:?}",
        result.reasons
    );
    let left = sb.collect_entries();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].original, kept);
}

#[test]
fn put_selection_trashes_two_paths() {
    let sb = TrashSandbox::new("put-sel");
    let _env = sb.catalog_env();
    let p1 = sb.work().join("p1.txt");
    let p2 = sb.work().join("p2.txt");
    fs::write(&p1, b"1").unwrap();
    fs::write(&p2, b"2").unwrap();

    // put_one reads the process env, which catalog_env pointed at this sandbox.
    let code = put_tui::put_selection("rtrash", &[p1.clone(), p2.clone()], false, true, false);
    assert_eq!(code, 0, "put_selection exit");
    assert!(!p1.exists() && !p2.exists());
    let left = sb.collect_entries();
    assert_eq!(left.len(), 2, "both paths in sandbox trash");
}

#[test]
fn put_list_dir_rows_and_filter() {
    let dir = std::env::temp_dir().join(format!("rtrash-put-list-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("alpha.txt"), b"a").unwrap();
    fs::write(dir.join("beta.log"), b"b").unwrap();
    fs::create_dir(dir.join("subdir")).unwrap();
    let rows = put_tui::list_dir_rows(&dir).unwrap();
    assert!(rows.len() >= 3);
    let owned: Vec<String> = rows
        .iter()
        .map(|r| {
            r.path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    let refs: Vec<&str> = owned.iter().map(String::as_str).collect();
    let idxs = put_tui::filter_indices(&refs, "alp");
    assert!(!idxs.is_empty());
    let name = rows[idxs[0]].path.file_name().unwrap().to_string_lossy();
    assert!(name.to_lowercase().contains('a'), "{name}");
    let _ = fs::remove_dir_all(&dir);
}
