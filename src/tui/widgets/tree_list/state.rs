//! Tree list navigation state.

use ratatui::widgets::ListState;

/// Tree list state
#[derive(Debug, Default)]
pub struct TreeListState {
    /// Inner list state
    pub list_state: ListState,
    /// Total number of items
    pub item_count: usize,
    /// Per-index selectability (empty = all selectable).
    selectable: Vec<bool>,
}

impl TreeListState {
    /// Create a new state
    pub fn new() -> Self {
        Self::default()
    }

    /// Get the selected index
    pub fn selected(&self) -> Option<usize> {
        self.list_state.selected()
    }

    /// Select an item
    pub fn select(&mut self, index: Option<usize>) {
        self.list_state.select(index);
    }

    fn is_selectable(&self, idx: usize) -> bool {
        self.selectable.get(idx).copied().unwrap_or(true)
    }

    fn any_selectable(&self) -> bool {
        if self.selectable.is_empty() {
            return self.item_count > 0;
        }
        self.selectable.iter().any(|s| *s)
    }

    /// Select the next item, skipping unselectable rows.
    pub fn next(&mut self) {
        if !self.any_selectable() {
            return;
        }
        let count = self.item_count;
        let start = self
            .list_state
            .selected()
            .map(|i| (i + 1) % count)
            .unwrap_or(0);
        for offset in 0..count {
            let i = (start + offset) % count;
            if self.is_selectable(i) {
                self.list_state.select(Some(i));
                return;
            }
        }
    }

    /// Select the previous item, skipping unselectable rows.
    pub fn previous(&mut self) {
        if !self.any_selectable() {
            return;
        }
        let count = self.item_count;
        let start = match self.list_state.selected() {
            Some(i) => {
                if i == 0 {
                    count - 1
                } else {
                    i - 1
                }
            }
            None => 0,
        };
        for offset in 0..count {
            let i = (start + count - offset) % count;
            if self.is_selectable(i) {
                self.list_state.select(Some(i));
                return;
            }
        }
    }

    /// Update item count and ensure selection is valid.
    ///
    /// Also clears any per-index `selectable` mask installed by a prior
    /// `set_selectable` call — `set_item_count` is the "no mask, every row
    /// is selectable" entry point, and a stale mask from another view
    /// would otherwise make rows at the same indices unreachable with
    /// up/down navigation.
    pub fn set_item_count(&mut self, count: usize) {
        self.item_count = count;
        self.selectable.clear();

        // Ensure selection is still valid
        if let Some(selected) = self.list_state.selected() {
            if selected >= count && count > 0 {
                self.list_state.select(Some(count - 1));
            } else if count == 0 {
                self.list_state.select(None);
            }
        }
    }

    /// Set a per-index selectable mask. The mask length should equal the
    /// current item count; shorter masks default unknown indices to selectable.
    /// Also updates item count to match mask length.
    pub fn set_selectable(&mut self, mask: Vec<bool>) {
        self.item_count = mask.len();
        self.selectable = mask;
        if let Some(sel) = self.list_state.selected()
            && (sel >= self.item_count || !self.is_selectable(sel))
        {
            self.list_state.select(None);
        }
    }
}
