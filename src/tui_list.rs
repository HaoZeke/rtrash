//! Pure list navigation helpers for TUI browsers (no I/O, no ratatui).

/// Keep `selected` in `0..len` (or `None` when empty).
pub fn clamp_selected(selected: Option<usize>, len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    Some(selected.unwrap_or(0).min(len - 1))
}

/// Move selection by `delta` with wrap-around. Empty list → `None`.
pub fn move_selected(selected: Option<usize>, len: usize, delta: isize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let cur = selected.unwrap_or(0) as isize;
    Some((cur + delta).rem_euclid(len as isize) as usize)
}

/// Page jump by `page_size` rows (at least 1). Does not wrap: clamps to ends.
pub fn page_selected(
    selected: Option<usize>,
    len: usize,
    page_size: usize,
    down: bool,
) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let step = page_size.max(1) as isize;
    let cur = selected.unwrap_or(0) as isize;
    let next = if down {
        (cur + step).min(len as isize - 1)
    } else {
        (cur - step).max(0)
    };
    Some(next as usize)
}

/// First-item offset so `selected` stays visible in a viewport of `height` rows.
///
/// `height` is the number of list rows that fit (inner height). Empty / zero height
/// returns 0.
pub fn scroll_offset(
    selected: Option<usize>,
    len: usize,
    height: usize,
    current_offset: usize,
) -> usize {
    if len == 0 || height == 0 {
        return 0;
    }
    let sel = selected.unwrap_or(0).min(len - 1);
    let max_off = len.saturating_sub(height);
    let mut off = current_offset.min(max_off);
    if sel < off {
        off = sel;
    } else if sel >= off + height {
        off = sel + 1 - height;
    }
    off.min(max_off)
}

/// After a filter rebuild, map the previous *entry* index to a position in
/// `new_filtered` (list of entry indices). Falls back to first match / `None`.
pub fn reselect_after_filter(prev_entry_idx: Option<usize>, new_filtered: &[usize]) -> Option<usize> {
    if new_filtered.is_empty() {
        return None;
    }
    if let Some(ei) = prev_entry_idx {
        if let Some(pos) = new_filtered.iter().position(|&i| i == ei) {
            return Some(pos);
        }
    }
    Some(0)
}

/// Live-filter draft mutation helpers: what query string to rank with.
/// Applied filter is kept until Enter commits or Esc reverts.
pub fn filter_query_for_mode<'a>(applied: &'a str, draft: &'a str, live: bool) -> &'a str {
    if live {
        draft
    } else {
        applied
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_and_move_wrap() {
        assert_eq!(clamp_selected(Some(9), 0), None);
        assert_eq!(clamp_selected(Some(9), 3), Some(2));
        assert_eq!(move_selected(Some(2), 3, 1), Some(0));
        assert_eq!(move_selected(Some(0), 3, -1), Some(2));
        assert_eq!(move_selected(None, 5, 1), Some(1));
    }

    #[test]
    fn page_clamps_no_wrap() {
        assert_eq!(page_selected(Some(0), 100, 10, true), Some(10));
        assert_eq!(page_selected(Some(95), 100, 10, true), Some(99));
        assert_eq!(page_selected(Some(5), 100, 10, false), Some(0));
        assert_eq!(page_selected(None, 0, 10, true), None);
    }

    #[test]
    fn scroll_keeps_selection_visible() {
        // viewport 5, select 7 → offset at least 3
        assert_eq!(scroll_offset(Some(7), 20, 5, 0), 3);
        // scroll up: selected above offset
        assert_eq!(scroll_offset(Some(1), 20, 5, 4), 1);
        // already visible
        assert_eq!(scroll_offset(Some(6), 20, 5, 4), 4);
        // empty
        assert_eq!(scroll_offset(Some(0), 0, 5, 0), 0);
    }

    #[test]
    fn reselect_prefers_same_entry() {
        let f = vec![0, 2, 4];
        assert_eq!(reselect_after_filter(Some(2), &f), Some(1));
        assert_eq!(reselect_after_filter(Some(9), &f), Some(0));
        assert_eq!(reselect_after_filter(Some(1), &[]), None);
    }

    #[test]
    fn live_filter_uses_draft() {
        assert_eq!(filter_query_for_mode("old", "new", true), "new");
        assert_eq!(filter_query_for_mode("old", "new", false), "old");
    }

    #[test]
    fn live_filter_index_sets_shrink_as_query_grows() {
        use crate::tui_fuzzy::rank_indices;
        let hays = ["alpha.txt", "alpine.log", "beta.md", "alphabet"];
        let refs: Vec<&str> = hays.to_vec();
        let all = rank_indices(&refs, "");
        assert_eq!(all.len(), 4);
        let a = rank_indices(&refs, "a");
        assert!(!a.is_empty());
        let al = rank_indices(&refs, "alp");
        assert!(al.len() <= a.len(), "alp={al:?} a={a:?}");
        // Every tighter match must have been in the looser set.
        for &i in &al {
            assert!(a.contains(&i), "index {i} matched alp but not a");
        }
        let alph = rank_indices(&refs, "alph");
        assert!(alph.len() <= al.len(), "alph={alph:?} al={al:?}");
        for &i in &alph {
            assert!(al.contains(&i), "index {i} in alph but not al");
        }
    }
}
