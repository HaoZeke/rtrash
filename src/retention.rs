//! Size-based retention: keep the trash under a byte budget by removing
//! the oldest (or the largest) items first. Used by `empty --max-size` and,
//! when `RTRASH_MAX_SIZE` is set, after every put.

use std::fs;
use std::io;

use rayon::prelude::*;

use crate::list;
use crate::trashdir::{self, TrashDir, TrashLock};
use crate::util::format_bytes;

/// Which items go first when the trash is over budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    Oldest,
    Largest,
}

impl Policy {
    pub fn parse(s: &str) -> Option<Policy> {
        match s {
            "oldest" => Some(Policy::Oldest),
            "largest" => Some(Policy::Largest),
            _ => None,
        }
    }
}

/// Parse a size such as `500M`, `10G`, `1.5GiB`, `64k` or `123` (bytes).
/// Units are powers of 1024; a trailing `B` or `iB` is optional.
pub fn parse_size(s: &str) -> Option<u64> {
    let t = s.trim();
    let t = t
        .strip_suffix("iB")
        .or_else(|| t.strip_suffix('B'))
        .unwrap_or(t);
    let (num, mult) = match t.char_indices().last() {
        Some((i, c)) if c.is_ascii_alphabetic() => {
            let m: u64 = match c.to_ascii_uppercase() {
                'K' => 1 << 10,
                'M' => 1 << 20,
                'G' => 1 << 30,
                'T' => 1 << 40,
                _ => return None,
            };
            (&t[..i], m)
        }
        _ => (t, 1),
    };
    let v: f64 = num.trim().parse().ok()?;
    if !v.is_finite() || v < 0.0 {
        return None;
    }
    let bytes = v * mult as f64;
    (bytes <= u64::MAX as f64).then_some(bytes as u64)
}

/// One trashed item considered for removal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub root: usize,
    pub name: String,
    pub bytes: u64,
    pub epoch: i64,
    /// Never removed (items of the put run that triggered the prune).
    pub protected: bool,
}

/// Indices of `items` to remove, in removal order, so the rest totals at
/// most `max` bytes. Protected items count toward the total but are never
/// picked, so the budget can stay exceeded when they alone exceed it.
pub fn plan(items: &[Candidate], max: u64, policy: Policy) -> Vec<usize> {
    let mut total: u64 = items.iter().map(|c| c.bytes).sum();
    if total <= max {
        return Vec::new();
    }
    let mut order: Vec<usize> = (0..items.len()).filter(|&i| !items[i].protected).collect();
    order.sort_by(|&a, &b| {
        let (x, y) = (&items[a], &items[b]);
        match policy {
            Policy::Oldest => x.epoch.cmp(&y.epoch).then_with(|| y.bytes.cmp(&x.bytes)),
            Policy::Largest => y.bytes.cmp(&x.bytes).then_with(|| x.epoch.cmp(&y.epoch)),
        }
        .then_with(|| x.name.cmp(&y.name))
    });
    let mut out = Vec::new();
    for i in order {
        if total <= max {
            break;
        }
        total = total.saturating_sub(items[i].bytes);
        out.push(i);
    }
    out
}

/// What a prune did (or would do, on a dry run).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub removed: u64,
    pub bytes: u64,
    pub errors: u64,
    /// Bytes left in the trash afterwards.
    pub left: u64,
}

/// Settings for [`prune`].
pub struct Prune<'a> {
    pub max: u64,
    pub policy: Policy,
    pub dry_run: bool,
    pub verbose: bool,
    /// Items with this batch id are never removed.
    pub protect_batch: Option<&'a str>,
    /// Items trashed at or before this epoch count as already gone (an age
    /// pass of the same `empty` run takes them).
    pub gone_before: Option<i64>,
}

/// Bring the trash roots in `dirs`, taken together, under `p.max` bytes.
pub fn prune(prog: &str, dirs: &[TrashDir], p: &Prune) -> Outcome {
    let mut out = Outcome::default();
    // Lock every root, in path order so two prunes cannot deadlock.
    let mut order: Vec<usize> = (0..dirs.len()).collect();
    order.sort_by(|&a, &b| dirs[a].root.cmp(&dirs[b].root));
    let mut locks = Vec::new();
    let mut usable = vec![false; dirs.len()];
    for &i in &order {
        match TrashLock::acquire(&dirs[i].root) {
            Ok(l) => {
                locks.push(l);
                usable[i] = true;
            }
            Err(e) => {
                eprintln!(
                    "{prog}: cannot lock trash '{}': {e}",
                    dirs[i].root.display()
                );
                out.errors += 1;
            }
        }
    }
    let mut items = Vec::new();
    for (ri, dir) in dirs.iter().enumerate() {
        if !usable[ri] {
            continue;
        }
        let sizes = trashdir::DirSizes::load(dir);
        let entries: Vec<list::Entry> = list::collect(std::slice::from_ref(dir))
            .into_iter()
            .filter(|e| !p.gone_before.is_some_and(|cut| e.epoch <= cut))
            .collect();
        // Sizing stats every payload; spread it over the rayon pool.
        items.par_extend(entries.into_par_iter().map(|e| Candidate {
            root: ri,
            bytes: sizes.entry_bytes(&e.name),
            epoch: e.epoch,
            protected: p.protect_batch.is_some() && e.batch.as_deref() == p.protect_batch,
            name: e.name,
        }));
        sizes.write_back_locked();
    }
    let total: u64 = items.iter().map(|c| c.bytes).sum();
    out.left = total;
    for i in plan(&items, p.max, p.policy) {
        let c = &items[i];
        let dir = &dirs[c.root];
        let shown = dir.files().join(&c.name);
        if p.dry_run {
            if p.verbose {
                println!(
                    "would remove {} ({})",
                    shown.display(),
                    format_bytes(c.bytes)
                );
            }
        } else {
            if let Err(e) = remove_entry(dir, &c.name) {
                eprintln!("{prog}: cannot remove '{}': {e}", shown.display());
                out.errors += 1;
                continue;
            }
            if p.verbose {
                println!("removed {}", shown.display());
            }
        }
        out.removed += 1;
        out.bytes += c.bytes;
        out.left = out.left.saturating_sub(c.bytes);
    }
    if !p.dry_run && out.removed > 0 {
        for (ri, dir) in dirs.iter().enumerate() {
            if usable[ri] {
                // Drops the cache lines of removed directories.
                trashdir::DirSizes::load(dir).write_back_locked();
            }
        }
    }
    drop(locks);
    out
}

fn remove_entry(dir: &TrashDir, name: &str) -> io::Result<()> {
    trashdir::remove_any_path(&dir.files().join(name))?;
    match fs::remove_file(dir.info().join(format!("{name}.trashinfo"))) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// The budget from `RTRASH_MAX_SIZE` and the policy from `RTRASH_PRUNE`
/// (`oldest` unless set), for the prune after put. A bad value warns once
/// and turns the prune off.
pub fn from_env(prog: &str) -> Option<(u64, Policy)> {
    let raw = std::env::var("RTRASH_MAX_SIZE").ok()?;
    if raw.trim().is_empty() {
        return None;
    }
    let Some(max) = parse_size(&raw) else {
        eprintln!("{prog}: warning: ignoring RTRASH_MAX_SIZE='{raw}' (want a size such as 10G)");
        return None;
    };
    let policy = match std::env::var("RTRASH_PRUNE") {
        Ok(v) if !v.is_empty() => match Policy::parse(&v) {
            Some(p) => p,
            None => {
                eprintln!("{prog}: warning: ignoring RTRASH_PRUNE='{v}' (want oldest or largest)");
                Policy::Oldest
            }
        },
        _ => Policy::Oldest,
    };
    Some((max, policy))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(name: &str, bytes: u64, epoch: i64) -> Candidate {
        Candidate {
            root: 0,
            name: name.into(),
            bytes,
            epoch,
            protected: false,
        }
    }

    #[test]
    fn sizes_parse_with_binary_units() {
        assert_eq!(parse_size("123"), Some(123));
        assert_eq!(parse_size("64k"), Some(65_536));
        assert_eq!(parse_size("500M"), Some(500 << 20));
        assert_eq!(parse_size("1.5GiB"), Some(3 << 29));
        assert_eq!(parse_size("2TB"), Some(2 << 40));
        assert_eq!(parse_size("10X"), None);
        assert_eq!(parse_size("-1G"), None);
        assert_eq!(parse_size(""), None);
    }

    #[test]
    fn oldest_first_stops_once_under_budget() {
        let items = vec![c("new", 50, 300), c("old", 30, 100), c("mid", 40, 200)];
        assert_eq!(plan(&items, 100, Policy::Oldest), vec![1]);
        assert!(plan(&items, 120, Policy::Oldest).is_empty());
        assert_eq!(plan(&items, 60, Policy::Oldest), vec![1, 2]);
        assert!(plan(&items, 1000, Policy::Oldest).is_empty());
    }

    #[test]
    fn largest_first_and_protected_items_stay() {
        let mut items = vec![c("a", 10, 1), c("big", 90, 3), c("b", 20, 2)];
        assert_eq!(plan(&items, 50, Policy::Largest), vec![1]);
        items[1].protected = true;
        // Even with everything else gone the budget stays exceeded.
        assert_eq!(plan(&items, 50, Policy::Largest), vec![2, 0]);
    }
}
