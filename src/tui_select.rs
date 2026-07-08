//! Multi-select set over entry indices (no I/O).

use std::collections::BTreeSet;

#[derive(Debug, Default, Clone)]
pub struct Selection {
    marked: BTreeSet<usize>,
}

impl Selection {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_marked(&self, idx: usize) -> bool {
        self.marked.contains(&idx)
    }

    pub fn toggle(&mut self, idx: usize) {
        if !self.marked.remove(&idx) {
            self.marked.insert(idx);
        }
    }

    pub fn mark(&mut self, idx: usize) {
        self.marked.insert(idx);
    }

    pub fn clear(&mut self) {
        self.marked.clear();
    }

    pub fn mark_all(&mut self, indices: impl IntoIterator<Item = usize>) {
        self.marked.extend(indices);
    }

    pub fn len(&self) -> usize {
        self.marked.len()
    }

    pub fn is_empty(&self) -> bool {
        self.marked.is_empty()
    }

    /// Marked indices that appear in `visible`, preserving `visible` order.
    pub fn marked_visible(&self, visible: &[usize]) -> Vec<usize> {
        visible
            .iter()
            .copied()
            .filter(|i| self.marked.contains(i))
            .collect()
    }

    /// All marked indices ascending.
    pub fn marked_sorted(&self) -> Vec<usize> {
        self.marked.iter().copied().collect()
    }

    /// Drop marks for indices not in `alive` (after removals).
    pub fn retain(&mut self, alive: &BTreeSet<usize>) {
        self.marked.retain(|i| alive.contains(i));
    }

    /// After removing `removed` entry indices from the list, remap marks.
    pub fn remap_after_removals(&mut self, removed: &[usize]) {
        if removed.is_empty() {
            return;
        }
        let rem: BTreeSet<usize> = removed.iter().copied().collect();
        let mut next = BTreeSet::new();
        for &m in &self.marked {
            if rem.contains(&m) {
                continue;
            }
            let shift = rem.iter().filter(|&&r| r < m).count();
            next.insert(m - shift);
        }
        self.marked = next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toggle_add_clear() {
        let mut s = Selection::new();
        assert!(s.is_empty());
        s.toggle(2);
        s.toggle(5);
        assert!(s.is_marked(2) && s.is_marked(5));
        assert_eq!(s.len(), 2);
        s.toggle(2);
        assert!(!s.is_marked(2) && s.is_marked(5));
        s.clear();
        assert!(s.is_empty());
    }

    #[test]
    fn mark_all_and_visible_order() {
        let mut s = Selection::new();
        s.mark_all([0, 1, 2, 3]);
        let vis = vec![3, 1, 0];
        assert_eq!(s.marked_visible(&vis), vec![3, 1, 0]);
    }

    #[test]
    fn remap_after_removals_shifts_indices() {
        let mut s = Selection::new();
        s.mark_all([0, 2, 4]);
        s.remap_after_removals(&[2]); // remove index 2
                                      // 0 stays 0; 4 becomes 3; 2 gone
        assert_eq!(s.marked_sorted(), vec![0, 3]);
    }
}
