//! Fuzzy path matching for TUI filters (no I/O).
//!
//! Scoring is a lightweight sequential character match (fzf-style), not
//! substring-only: characters must appear in order; consecutive runs and
//! matches after `/` score higher.

/// Score how well `needle` fuzzy-matches `hay` (case-insensitive).
/// Returns `None` if not all needle characters appear in order.
/// Higher scores rank first. Empty needle matches everything with score 0.
///
/// Consecutive runs dominate sparse matches so `Notes.md` ranks above
/// `n_x_o_t_e_s` for needle `notes`.
pub fn fuzzy_score(hay: &str, needle: &str) -> Option<i64> {
    let needle = needle.trim();
    if needle.is_empty() {
        return Some(0);
    }
    let hay_chars: Vec<char> = hay.chars().map(|c| c.to_ascii_lowercase()).collect();
    let needle_chars: Vec<char> = needle.chars().map(|c| c.to_ascii_lowercase()).collect();
    let mut score: i64 = 0;
    let mut hi = 0usize;
    let mut consec = 0i64;
    let mut gaps: i64 = 0;
    for &nc in &needle_chars {
        let mut found = false;
        let start = hi;
        while hi < hay_chars.len() {
            let hc = hay_chars[hi];
            hi += 1;
            if hc == nc {
                found = true;
                let skipped = (hi - 1 - start) as i64;
                gaps += skipped;
                let prev = if hi >= 2 { hay_chars[hi - 2] } else { '/' };
                if prev == '/' {
                    score += 4;
                }
                if skipped == 0 && consec > 0 {
                    // Adjacent to previous needle match in hay.
                    consec += 1;
                    score += 20 * consec;
                } else {
                    consec = 1;
                    score += 2;
                }
                break;
            }
        }
        if !found {
            return None;
        }
    }
    // Heavy penalty for sparse matches vs tight runs.
    score -= gaps * 15;
    score -= (hay_chars.len().saturating_sub(hi) as i64) / 16;
    Some(score)
}

/// Indices of `hays` that fuzzy-match `query`, sorted by score descending then index.
/// Empty query returns all indices in order.
pub fn rank_indices(hays: &[&str], query: &str) -> Vec<usize> {
    let q = query.trim();
    if q.is_empty() {
        return (0..hays.len()).collect();
    }
    let mut scored: Vec<(i64, usize)> = hays
        .iter()
        .enumerate()
        .filter_map(|(i, h)| fuzzy_score(h, q).map(|s| (s, i)))
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().map(|(_, i)| i).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_query_matches_all_in_order() {
        let h = ["a/foo", "b/bar"];
        let refs: Vec<&str> = h.to_vec();
        assert_eq!(rank_indices(&refs, ""), vec![0, 1]);
        assert_eq!(rank_indices(&refs, "  "), vec![0, 1]);
    }

    #[test]
    fn fuzzy_not_substring_only_skips_wrong_order() {
        // 'on' appears in order in "notes" but reversed needle 'no' vs 'on' for "xo"
        assert!(fuzzy_score("notes.md", "nts").is_some());
        assert!(fuzzy_score("notes.md", "xzz").is_none());
        // Substring-only would reject "n1otes" for "notes"; fuzzy accepts with gaps.
        assert!(fuzzy_score("/tmp/n1o2t3e4s.md", "notes").is_some());
    }

    #[test]
    fn closer_path_ranks_above_distant_when_both_match() {
        // Both match "notes"; path with consecutive "notes" should beat sparse gaps.
        let h = ["/var/cache/n_x_o_t_e_s_backup.bin", "/home/user/Notes.md"];
        let refs: Vec<&str> = h.to_vec();
        let ranked = rank_indices(&refs, "notes");
        assert_eq!(ranked.len(), 2, "{ranked:?}");
        // Notes.md should rank first (consecutive + path component start).
        assert_eq!(ranked[0], 1, "expected Notes.md first, got {ranked:?}");
    }

    #[test]
    fn case_insensitive() {
        assert!(fuzzy_score("/Home/User/Notes.MD", "notes").is_some());
    }
}
