//! The trash budget for the prune after put: a size from `RTRASH_MAX_SIZE`
//! or `policy.max_size`, and which items go first. Shared by the Unix
//! trash and the Windows Recycle Bin.

use std::path::{Path, PathBuf};

use crate::filters::parse_size;

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

/// Indices of `items` (deletion time, bytes) to purge, in order, so the
/// rest totals at most `max` bytes: oldest first, or largest first when
/// `largest` is set (ties go to the older item).
pub fn pick_budget(items: &[(i64, u64)], max: u64, largest: bool) -> Vec<usize> {
    let mut total: u64 = items.iter().map(|i| i.1).sum();
    let mut order: Vec<usize> = (0..items.len()).collect();
    if largest {
        order.sort_by(|&a, &b| {
            items[b]
                .1
                .cmp(&items[a].1)
                .then(items[a].0.cmp(&items[b].0))
        });
    } else {
        order.sort_by(|&a, &b| {
            items[a]
                .0
                .cmp(&items[b].0)
                .then(items[b].1.cmp(&items[a].1))
        });
    }
    let mut out = Vec::new();
    for i in order {
        if total <= max {
            break;
        }
        total -= items[i].1;
        out.push(i);
    }
    out
}

/// Indices of `rows` (path, deletion time, bytes) a put's trim removes to
/// bring them under `max`. Rows whose path is in `moved` (what this put
/// trashed: the original path on Windows, the path in the trash for the
/// Finder Trash) count toward the total but never go.
pub fn trim_after_put(
    rows: &[(PathBuf, i64, u64)],
    moved: &[PathBuf],
    max: u64,
    policy: Policy,
) -> Vec<usize> {
    let mine = |p: &Path| moved.iter().any(|m| m == p);
    let kept: u64 = rows.iter().filter(|r| mine(&r.0)).map(|r| r.2).sum();
    let rest: Vec<usize> = (0..rows.len()).filter(|&i| !mine(&rows[i].0)).collect();
    let sized: Vec<(i64, u64)> = rest.iter().map(|&i| (rows[i].1, rows[i].2)).collect();
    pick_budget(&sized, max.saturating_sub(kept), policy == Policy::Largest)
        .into_iter()
        .map(|i| rest[i])
        .collect()
}

/// The guard on an unscoped empty: it goes ahead when every one of the
/// `total` items came from the latest put run; otherwise it asks on a
/// terminal and refuses elsewhere, since the trash may be shared (other
/// agents, other sessions) and an empty cannot be undone.
pub fn guard_ok(prog: &str, total: usize, foreign: usize) -> bool {
    if foreign == 0 {
        return true;
    }
    if crate::util::stdin_is_tty() {
        use std::io::Write;
        eprint!(
            "Permanently delete all {total} items, {foreign} of them not from the latest put? (y/N) "
        );
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
        return matches!(line.trim(), "y" | "Y" | "yes" | "Yes");
    }
    eprintln!(
        "{prog}: refusing to empty the whole trash: {foreign} of {total} items are not from the latest put"
    );
    eprintln!(
        "{prog}: pass --all to remove everything, or scope it (DAYS, --max-size, --until-free, --trash-dir, or rm PATTERN)"
    );
    if cfg!(unix) {
        eprintln!("{prog}: to free only what your latest put moved: {prog} rm --batch=last");
    }
    false
}

/// How many of `items` (original path, deletion time) the last put did
/// not move: that put moved `last`, starting at epoch `since`.
pub fn not_from_last_put(items: &[(PathBuf, i64)], last: &[PathBuf], since: i64) -> usize {
    items.len() - from_last_put(items, last, since).len()
}

/// The indices of `items` (original path, deletion time) that the last
/// put moved: that put moved `last`, starting at epoch `since`.
pub fn from_last_put(items: &[(PathBuf, i64)], last: &[PathBuf], since: i64) -> Vec<usize> {
    (0..items.len())
        .filter(|&i| items[i].1 >= since && last.iter().any(|l| *l == items[i].0))
        .collect()
}

/// The last-put record: its start epoch on the first line, then one
/// moved path per line.
pub fn last_put_text(since: i64, moved: &[PathBuf]) -> String {
    let mut s = format!("{since}\n");
    for m in moved {
        s.push_str(&m.to_string_lossy());
        s.push('\n');
    }
    s
}

/// The start epoch and paths of a last-put record; none for a damaged one.
pub fn parse_last_put(text: &str) -> Option<(i64, Vec<PathBuf>)> {
    let mut lines = text.lines();
    let since = lines.next()?.trim().parse().ok()?;
    Some((
        since,
        lines.filter(|l| !l.is_empty()).map(PathBuf::from).collect(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_empty_guard_counts_items_the_last_put_did_not_move() {
        let p = |s: &str| PathBuf::from(s);
        let text = last_put_text(100, &[p("C:\\w\\a"), p("C:\\w\\b")]);
        let (since, last) = parse_last_put(&text).unwrap();
        assert_eq!(since, 100);
        let items = [
            (p("C:\\w\\a"), 101),
            (p("C:\\w\\b"), 100),
            // Same path, but put before the last run began.
            (p("C:\\w\\a"), 50),
            (p("C:\\w\\c"), 102),
        ];
        assert_eq!(not_from_last_put(&items, &last, since), 2);
        assert_eq!(not_from_last_put(&items[..2], &last, since), 0);
        assert_eq!(from_last_put(&items, &last, since), vec![0, 1]);
        assert_eq!(parse_last_put("garbled"), None);
    }

    #[test]
    fn a_put_trim_spares_what_the_put_moved_but_counts_it() {
        let p = |s: &str| PathBuf::from(s);
        let rows = vec![
            (p("/a/old"), 10, 50),
            (p("/a/new"), 30, 50),
            (p("/a/mine"), 5, 60),
            (p("/a/mid"), 20, 10),
        ];
        let moved = vec![p("/a/mine")];
        // 170 in all, budget 100: mine (60, oldest) stays, so the other
        // three must come down to 40: old goes, then mid, then new.
        assert_eq!(
            trim_after_put(&rows, &moved, 100, Policy::Oldest),
            vec![0, 3, 1]
        );
        // Budget 120 leaves room for 60 besides mine: old alone is enough.
        assert_eq!(trim_after_put(&rows, &moved, 120, Policy::Oldest), vec![0]);
        // Largest first takes one 50 (the older), never the 60 of mine.
        assert_eq!(trim_after_put(&rows, &moved, 120, Policy::Largest), vec![0]);
        // Mine alone over budget: everything else goes, mine stays.
        assert_eq!(
            trim_after_put(&rows, &moved, 10, Policy::Oldest),
            vec![0, 3, 1]
        );
        assert!(trim_after_put(&rows, &moved, 1000, Policy::Oldest).is_empty());
        assert!(trim_after_put(&rows, &[], 1000, Policy::Oldest).is_empty());
    }

    #[test]
    fn pick_budget_trims_to_the_size_oldest_or_largest_first() {
        let items = [(30, 5), (10, 4), (20, 100), (40, 1)];
        assert_eq!(pick_budget(&items, 200, false), Vec::<usize>::new());
        assert_eq!(pick_budget(&items, 10, false), vec![1, 2]);
        assert_eq!(pick_budget(&items, 10, true), vec![2]);
        assert_eq!(pick_budget(&items, 0, true), vec![2, 0, 1, 3]);
    }
}
