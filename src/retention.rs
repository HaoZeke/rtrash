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

pub use crate::filters::parse_size;

/// One trashed item considered for removal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub root: usize,
    pub name: String,
    pub bytes: u64,
    pub epoch: i64,
    /// Inodes the item takes (its `.trashinfo` and payload tree), counted
    /// only when a prune must free inodes; 0 otherwise.
    pub inodes: u64,
    /// Never removed (items of the put run that triggered the prune).
    pub protected: bool,
}

/// Indices of `items` to remove, in removal order, so the rest totals at
/// most `max` bytes. Protected items count toward the total but are never
/// picked, so the budget can stay exceeded when they alone exceed it.
pub fn plan(items: &[Candidate], max: u64, policy: Policy) -> Vec<usize> {
    plan_freeing(items, max, policy, 0)
}

/// [`plan`] that also keeps picking until the picked items free at least
/// `free_inodes` inodes.
pub fn plan_freeing(items: &[Candidate], max: u64, policy: Policy, free_inodes: u64) -> Vec<usize> {
    let mut total: u64 = items.iter().map(|c| c.bytes).sum();
    let mut freed_inodes = 0u64;
    if total <= max && free_inodes == 0 {
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
        if total <= max && freed_inodes >= free_inodes {
            break;
        }
        total = total.saturating_sub(items[i].bytes);
        freed_inodes = freed_inodes.saturating_add(items[i].inodes);
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
    /// Inodes the removed items took, when the prune counted them.
    pub inodes: u64,
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
    /// Inodes the removed items must free besides the byte budget (0 for
    /// none); each item's inodes are then counted.
    pub free_inodes: u64,
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
            inodes: if p.free_inodes > 0 {
                1 + tree_inodes(&crate::util::join_name(&dir.files(), &e.name))
            } else {
                0
            },
            protected: p.protect_batch.is_some() && e.batch.as_deref() == p.protect_batch,
            name: e.name,
        }));
        sizes.write_back_locked();
    }
    let total: u64 = items.iter().map(|c| c.bytes).sum();
    out.left = total;
    // Per root (bytes, items) still in the trash, for its running total.
    let mut kept = vec![(0u64, 0u64); dirs.len()];
    for c in &items {
        kept[c.root].0 += c.bytes;
        kept[c.root].1 += 1;
    }
    for i in plan_freeing(&items, p.max, p.policy, p.free_inodes) {
        let c = &items[i];
        let dir = &dirs[c.root];
        let shown = crate::util::join_name(&dir.files(), &c.name);
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
                eprintln!(
                    "{prog}: cannot remove '{}': {e}",
                    crate::util::err_path(&shown)
                );
                out.errors += 1;
                continue;
            }
            if p.verbose {
                println!("removed {}", crate::util::term_path(&shown));
            }
        }
        kept[c.root].0 -= c.bytes;
        kept[c.root].1 -= 1;
        out.removed += 1;
        out.bytes += c.bytes;
        out.inodes += c.inodes;
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
    // A real prune leaves exactly what it counted; so does a dry run that
    // set nothing aside as already gone.
    if !p.dry_run || p.gone_before.is_none() {
        for (ri, dir) in dirs.iter().enumerate() {
            if usable[ri] {
                crate::trash_total::store(dir, kept[ri].0, kept[ri].1);
            }
        }
    }
    drop(locks);
    out
}

/// The inode of `path` and of everything below it, links not followed.
fn tree_inodes(path: &std::path::Path) -> u64 {
    let Ok(m) = fs::symlink_metadata(path) else {
        return 0;
    };
    let mut n = 1u64;
    if !m.is_dir() {
        return n;
    }
    let mut stack = vec![path.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).into_iter().flatten().flatten() {
            n += 1;
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                stack.push(e.path());
            }
        }
    }
    n
}

fn remove_entry(dir: &TrashDir, name: &str) -> io::Result<()> {
    trashdir::remove_any_path(&crate::util::join_name(&dir.files(), name))?;
    match fs::remove_file(crate::util::join_info(&dir.info(), name)) {
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
    Some((max, env_policy(prog).unwrap_or(Policy::Oldest)))
}

/// `RTRASH_PRUNE`, when set to something.
fn env_policy(prog: &str) -> Option<Policy> {
    match std::env::var("RTRASH_PRUNE") {
        Ok(v) if !v.is_empty() => match Policy::parse(&v) {
            Some(p) => Some(p),
            None => {
                eprintln!("{prog}: warning: ignoring RTRASH_PRUNE='{v}' (want oldest or largest)");
                None
            }
        },
        _ => None,
    }
}

/// The budget for the prune after put, and where it came from: the
/// environment when `RTRASH_MAX_SIZE` is set (empty turns it off), else
/// `[policy]` in the config file. `RTRASH_PRUNE` beats `policy.prune`.
pub fn budget(prog: &str) -> Option<(u64, Policy, &'static str)> {
    if std::env::var_os("RTRASH_MAX_SIZE").is_some() {
        return from_env(prog).map(|(m, p)| (m, p, "RTRASH_MAX_SIZE"));
    }
    let cfg = crate::config::get();
    let raw = cfg.str("policy", "max_size")?;
    let Some(max) = parse_size(raw) else {
        eprintln!(
            "{prog}: warning: {}",
            cfg.bad_value("policy", "max_size", "a size such as 10G")
        );
        return None;
    };
    let policy = env_policy(prog)
        .or_else(|| {
            let v = cfg.str("policy", "prune")?;
            let p = Policy::parse(v);
            if p.is_none() {
                eprintln!(
                    "{prog}: warning: {}",
                    cfg.bad_value("policy", "prune", "oldest or largest")
                );
            }
            p
        })
        .unwrap_or(Policy::Oldest);
    Some((max, policy, "policy.max_size in the config file"))
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
            inodes: 0,
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
    fn plan_freeing_keeps_picking_until_enough_inodes_go() {
        let mut items = vec![c("a", 10, 1), c("b", 10, 2), c("c", 10, 3)];
        for (it, n) in items.iter_mut().zip([2, 30, 2]) {
            it.inodes = n;
        }
        assert!(plan_freeing(&items, 1000, Policy::Oldest, 0).is_empty());
        assert_eq!(plan_freeing(&items, 1000, Policy::Oldest, 2), vec![0]);
        assert_eq!(plan_freeing(&items, 1000, Policy::Oldest, 3), vec![0, 1]);
        assert_eq!(plan_freeing(&items, 5, Policy::Oldest, 3), vec![0, 1, 2]);
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
