//! Integration tests for multi-select restore / empty / put selection logic.
//! Drive shipped library functions against a real FreeDesktop trash layout
//! (isolated XDG_DATA_HOME). No alternate-screen / ratatui event loop.

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;

/// Serialise tests that mutate process env (put_one reads XDG_DATA_HOME).
static ENV_LOCK: Mutex<()> = Mutex::new(());

use rtrash::empty_tui;
use rtrash::list;
use rtrash::put_tui;
use rtrash::restore_tui;
use rtrash::trashdir;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_rtrash")
}

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
        let mut cmd = Command::new(bin());
        cmd.arg("put").args(names);
        let out = cmd
            .current_dir(self.work())
            .env("XDG_DATA_HOME", self.root.join("xdg"))
            .env("HOME", &self.root)
            .output()
            .expect("put");
        assert!(
            out.status.success(),
            "put failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn collect_entries(&self) -> Vec<list::Entry> {
        let dir = trashdir::TrashDir {
            root: self.trash(),
            topdir: None,
        };
        list::collect(std::slice::from_ref(&dir))
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
    assert_eq!(result.ok_count(), 2, "succeeded={:?}", result.succeeded_idxs);
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

#[test]
fn permanently_remove_entries_subset_leaves_others() {
    let sb = TrashSandbox::new("empty-subset");
    sb.put(&["keep.txt", "drop1.txt", "drop2.txt"]);
    let entries = sb.collect_entries();
    assert_eq!(entries.len(), 3);
    let refs: Vec<&list::Entry> = entries.iter().collect();

    let drop_idxs: Vec<usize> = refs
        .iter()
        .enumerate()
        .filter(|(_, e)| {
            let n = e.original.file_name().and_then(|f| f.to_str()).unwrap_or("");
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
fn put_selection_trashes_two_paths() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let sb = TrashSandbox::new("put-sel");
    let p1 = sb.work().join("p1.txt");
    let p2 = sb.work().join("p2.txt");
    fs::write(&p1, b"1").unwrap();
    fs::write(&p2, b"2").unwrap();

    // put_one / trashdir::select read process env (not the CLI Command sandbox).
    let prev_xdg = std::env::var_os("XDG_DATA_HOME");
    let prev_home = std::env::var_os("HOME");
    // SAFETY: single-threaded under ENV_LOCK; restored below.
    unsafe {
        std::env::set_var("XDG_DATA_HOME", sb.root.join("xdg"));
        std::env::set_var("HOME", &sb.root);
    }
    let code = put_tui::put_selection("rtrash", &[p1.clone(), p2.clone()], false, true);
    unsafe {
        match prev_xdg {
            Some(v) => std::env::set_var("XDG_DATA_HOME", v),
            None => std::env::remove_var("XDG_DATA_HOME"),
        }
        match prev_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
    }
    assert_eq!(code, 0, "put_selection exit");
    assert!(!p1.exists() && !p2.exists());
    let left = sb.collect_entries();
    assert_eq!(left.len(), 2, "both paths in sandbox trash");
}

#[test]
fn put_list_dir_rows_and_filter() {
    let dir = std::env::temp_dir().join(format!(
        "rtrash-put-list-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("alpha.txt"), b"a").unwrap();
    fs::write(dir.join("beta.log"), b"b").unwrap();
    fs::create_dir(dir.join("subdir")).unwrap();
    let rows = put_tui::list_dir_rows(&dir).unwrap();
    assert!(rows.len() >= 3);
    let idxs = put_tui::filter_indices(&rows, "alp");
    assert!(!idxs.is_empty());
    let name = rows[idxs[0]]
        .path
        .file_name()
        .unwrap()
        .to_string_lossy();
    assert!(name.to_lowercase().contains('a'), "{name}");
    let _ = fs::remove_dir_all(&dir);
}
