//! Catalog load and library-API actions. No CLI spawn on the happy path.

use std::path::{Path, PathBuf};

use rtrash::empty_tui;
use rtrash::list::{self, Entry};
use rtrash::put::{self, Interactive, Opts as PutOpts};
use rtrash::restore;
use rtrash::status;
use rtrash::trashdir::{self, TrashDir};

/// One HUD pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    /// Catalog browser.
    List,
    /// Restore marked / selected entries.
    Restore,
    /// Permanently delete marked / selected entries.
    Empty,
    /// Put a path into the pinned trash.
    Put,
    /// Per-root count and reclaimable size.
    Status,
}

impl Pane {
    /// Left-to-right tab order.
    pub const ALL: &'static [Pane] = &[
        Pane::List,
        Pane::Restore,
        Pane::Empty,
        Pane::Put,
        Pane::Status,
    ];

    /// Tab label.
    pub fn label(self) -> &'static str {
        match self {
            Pane::List => "List",
            Pane::Restore => "Restore",
            Pane::Empty => "Empty",
            Pane::Put => "Put",
            Pane::Status => "Status",
        }
    }

    /// Digit 1..=5 → pane.
    pub fn from_digit(d: u8) -> Option<Pane> {
        match d {
            1 => Some(Pane::List),
            2 => Some(Pane::Restore),
            3 => Some(Pane::Empty),
            4 => Some(Pane::Put),
            5 => Some(Pane::Status),
            _ => None,
        }
    }
}

/// Loaded trash catalog plus pin options.
#[derive(Debug, Clone)]
pub struct Catalog {
    /// Resolved trash roots.
    pub dirs: Vec<TrashDir>,
    /// `list::collect` rows, oldest first.
    pub entries: Vec<Entry>,
}

impl Catalog {
    /// Resolve pins and collect. Invalid pins yield an empty dir list.
    pub fn load(pins: &[PathBuf], home_only: bool) -> Self {
        let dirs = trashdir::resolve_dirs_opts(pins, home_only);
        let entries = if dirs.is_empty() {
            Vec::new()
        } else {
            list::collect(&dirs)
        };
        Catalog { dirs, entries }
    }

    /// Restore the chosen entries via [`restore::restore_entries`].
    pub fn restore(&self, indices: &[usize], force: bool) -> i32 {
        let chosen: Vec<&Entry> = indices.iter().filter_map(|&i| self.entries.get(i)).collect();
        if chosen.is_empty() {
            return 1;
        }
        restore::restore_entries("rtrash-hud", &chosen, force, None)
    }

    /// Permanently remove the chosen entries via the empty library path.
    pub fn empty(&self, indices: &[usize], dry_run: bool) -> empty_tui::RemoveBatchResult {
        let chosen: Vec<&Entry> = indices.iter().filter_map(|&i| self.entries.get(i)).collect();
        empty_tui::permanently_remove_entries("rtrash-hud", &chosen, dry_run)
    }

    /// Put one path via [`put::put_one`].
    pub fn put(&self, path: &Path, force: bool, recursive: bool) -> Result<(), i32> {
        let pin = self.dirs.first().map(|d| d.root.clone());
        let opts = PutOpts {
            force,
            recursive,
            empty_dirs: false,
            verbose: false,
            interactive: Interactive::Never,
            preserve_root: true,
            trash_pin: pin.clone(),
            home_only: pin.is_none(),
        };
        put::put_one("rtrash-hud", path, &opts)
    }

    /// Status rows from [`status::summarize`].
    pub fn status_rows(&self) -> (Vec<status::RootSummary>, u64, u64) {
        status::summarize(&self.dirs, None, None)
    }
}

/// Headless paint: catalog lines on stdout, no TTY, no compositor.
pub fn paint_offline(pins: &[PathBuf], home_only: bool) -> i32 {
    let cat = Catalog::load(pins, home_only);
    println!("rtrash-hud {}  pane:list  offline", rtrash::version());
    for pane in Pane::ALL {
        println!("pane:{}", pane.label().to_ascii_lowercase());
    }
    if cat.dirs.is_empty() {
        println!("(no trash directories)");
    }
    if cat.entries.is_empty() {
        println!("(empty catalog)");
    }
    for e in &cat.entries {
        let date = e.date.as_deref().unwrap_or("????-??-??T??:??:??");
        println!("{date}  {}", e.path_label());
    }
    let (roots, items, bytes) = cat.status_rows();
    println!(
        "status: {} roots, {items} items, {}",
        roots.len(),
        rtrash::fastdelete::format_bytes(bytes)
    );
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pane_digits_cover_all() {
        assert_eq!(Pane::ALL.len(), 5);
        assert_eq!(Pane::from_digit(1), Some(Pane::List));
        assert_eq!(Pane::from_digit(5), Some(Pane::Status));
        assert_eq!(Pane::from_digit(9), None);
    }
}
