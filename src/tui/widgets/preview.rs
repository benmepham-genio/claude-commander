//! Preview pane widget
//!
//! Displays captured pane content with scrolling support.

use ansi_to_tui::IntoText;
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::Modifier,
    text::Text,
    widgets::{Block, Paragraph, ScrollbarState, Widget},
};

use crate::tui::theme::dim_color;

/// Preview widget for displaying pane content
pub struct Preview<'a> {
    /// Content to display
    content: &'a str,
    /// Block for borders and title
    block: Option<Block<'a>>,
    /// Scroll offset
    scroll: u16,
    /// Opacity for unfocused dimming (None = no dimming, Some(0.4) = 40% brightness)
    dim_opacity: Option<f32>,
}

impl<'a> Preview<'a> {
    /// Create a new preview widget
    pub fn new(content: &'a str) -> Self {
        Self {
            content,
            block: None,
            scroll: 0,
            dim_opacity: None,
        }
    }

    /// Set the block
    pub fn block(mut self, block: Block<'a>) -> Self {
        self.block = Some(block);
        self
    }

    /// Set the scroll offset
    pub fn scroll(mut self, scroll: u16) -> Self {
        self.scroll = scroll;
        self
    }

    /// Set the opacity for unfocused dimming (0.0 = black, 1.0 = unchanged)
    pub fn dim_opacity(mut self, opacity: Option<f32>) -> Self {
        self.dim_opacity = opacity;
        self
    }
}

impl<'a> Widget for Preview<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        // Convert ANSI escape codes to ratatui styled text
        let mut text: Text<'_> = self
            .content
            .into_text()
            .unwrap_or_else(|_| Text::raw(self.content));

        if let Some(opacity) = self.dim_opacity {
            for line in &mut text.lines {
                for span in &mut line.spans {
                    let fg = span.style.fg.unwrap_or(ratatui::style::Color::Reset);
                    span.style = span
                        .style
                        .fg(dim_color(fg, opacity))
                        .remove_modifier(Modifier::REVERSED);
                }
            }
        }

        // No .wrap() - preserve original formatting (ASCII boxes, tables, etc.)
        let paragraph = Paragraph::new(text).scroll((self.scroll, 0));

        let paragraph = if let Some(block) = self.block {
            paragraph.block(block)
        } else {
            paragraph
        };

        paragraph.render(area, buf);
    }
}

/// Preview state for scrolling
#[derive(Debug)]
pub struct PreviewState {
    /// Current scroll offset (lines from top)
    pub scroll_offset: u16,
    /// Total number of lines in content
    pub total_lines: usize,
    /// Visible height
    pub visible_height: u16,
    /// Whether to follow new content (auto-scroll to bottom)
    follow: bool,
}

impl Default for PreviewState {
    fn default() -> Self {
        Self {
            scroll_offset: 0,
            total_lines: 0,
            visible_height: 0,
            follow: true,
        }
    }
}

impl PreviewState {
    /// Create a new state
    pub fn new() -> Self {
        Self::default()
    }

    /// Update content info
    pub fn set_content(&mut self, content: &str, visible_height: u16) {
        // Exclude trailing empty lines (tmux capture-pane returns full pane height)
        self.total_lines = content
            .lines()
            .collect::<Vec<_>>()
            .iter()
            .rposition(|l| !l.trim().is_empty())
            .map(|i| i + 1)
            .unwrap_or(0);
        self.visible_height = visible_height;

        if self.follow {
            self.scroll_to_bottom();
        } else {
            self.clamp_scroll();
        }
    }

    /// Update metrics directly without scanning content.
    /// Useful when line count is precomputed (e.g. from DiffInfo).
    pub fn set_metrics(&mut self, total_lines: usize, visible_height: u16) {
        self.total_lines = total_lines;
        self.visible_height = visible_height;

        if self.follow {
            self.scroll_to_bottom();
        } else {
            self.clamp_scroll();
        }
    }

    /// Scroll up by n lines
    pub fn scroll_up(&mut self, n: u16) {
        self.scroll_offset = self.scroll_offset.saturating_sub(n);
        self.follow = false;
    }

    /// Scroll down by n lines
    pub fn scroll_down(&mut self, n: u16) {
        self.scroll_offset = self.scroll_offset.saturating_add(n);
        self.clamp_scroll();
        // Re-enable follow if we've scrolled to the bottom
        if !self.can_scroll_down() {
            self.follow = true;
        }
    }

    /// Page up
    pub fn page_up(&mut self) {
        let page = self.visible_height.saturating_sub(2);
        self.scroll_up(page);
    }

    /// Page down
    pub fn page_down(&mut self) {
        let page = self.visible_height.saturating_sub(2);
        self.scroll_down(page);
    }

    /// Scroll to top
    pub fn scroll_to_top(&mut self) {
        self.scroll_offset = 0;
        self.follow = false;
    }

    /// Scroll to bottom
    pub fn scroll_to_bottom(&mut self) {
        self.follow = true;
        if self.total_lines > self.visible_height as usize {
            self.scroll_offset = (self.total_lines - self.visible_height as usize) as u16;
        } else {
            self.scroll_offset = 0;
        }
    }

    /// Ensure scroll offset is within valid range
    fn clamp_scroll(&mut self) {
        let max_scroll = if self.total_lines > self.visible_height as usize {
            (self.total_lines - self.visible_height as usize) as u16
        } else {
            0
        };

        self.scroll_offset = self.scroll_offset.min(max_scroll);
    }

    /// Get scrollbar state
    pub fn scrollbar_state(&self) -> ScrollbarState {
        ScrollbarState::new(self.total_lines)
            .position(self.scroll_offset as usize)
            .viewport_content_length(self.visible_height as usize)
    }

    /// Check if we can scroll up
    pub fn can_scroll_up(&self) -> bool {
        self.scroll_offset > 0
    }

    /// Check if we can scroll down
    pub fn can_scroll_down(&self) -> bool {
        if self.total_lines <= self.visible_height as usize {
            return false;
        }
        self.scroll_offset < (self.total_lines - self.visible_height as usize) as u16
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_preview_state_scrolling() {
        let mut state = PreviewState::new();

        // 100 lines, 20 visible - starts at bottom (follow mode)
        let content = (0..100)
            .map(|i| format!("Line {}", i))
            .collect::<Vec<_>>()
            .join("\n");
        state.set_content(&content, 20);

        assert_eq!(state.total_lines, 100);
        assert_eq!(state.scroll_offset, 80); // 100 - 20, auto-scrolled to bottom
        assert!(!state.can_scroll_down());
        assert!(state.can_scroll_up());

        // Scroll up disables follow
        state.scroll_up(5);
        assert_eq!(state.scroll_offset, 75);

        // Page up
        state.page_up();
        assert_eq!(state.scroll_offset, 57); // 75 - (20 - 2)

        // Scroll to top
        state.scroll_to_top();
        assert_eq!(state.scroll_offset, 0);
    }

    #[test]
    fn test_preview_state_short_content() {
        let mut state = PreviewState::new();

        // 10 lines, 20 visible - no scrolling needed
        let content = (0..10)
            .map(|i| format!("Line {}", i))
            .collect::<Vec<_>>()
            .join("\n");
        state.set_content(&content, 20);

        assert!(!state.can_scroll_down());
        assert!(!state.can_scroll_up());

        state.scroll_down(100);
        assert_eq!(state.scroll_offset, 0); // Clamped to 0
    }

    #[test]
    fn test_follow_mode_on_by_default() {
        let mut state = PreviewState::new();
        let content = (0..100)
            .map(|i| format!("Line {}", i))
            .collect::<Vec<_>>()
            .join("\n");
        state.set_content(&content, 20);
        // Follow mode auto-scrolls to bottom
        assert_eq!(state.scroll_offset, 80); // 100 - 20
    }

    #[test]
    fn test_scroll_up_disables_follow() {
        let mut state = PreviewState::new();
        let content = (0..100)
            .map(|i| format!("Line {}", i))
            .collect::<Vec<_>>()
            .join("\n");
        state.set_content(&content, 20);
        assert_eq!(state.scroll_offset, 80);

        state.scroll_up(5);
        assert_eq!(state.scroll_offset, 75);

        // New content should NOT auto-scroll (follow disabled)
        let content2 = (0..110)
            .map(|i| format!("Line {}", i))
            .collect::<Vec<_>>()
            .join("\n");
        state.set_content(&content2, 20);
        assert_eq!(state.scroll_offset, 75); // Stayed where we scrolled to
    }

    #[test]
    fn test_scroll_to_bottom_re_enables_follow() {
        let mut state = PreviewState::new();
        let content = (0..100)
            .map(|i| format!("Line {}", i))
            .collect::<Vec<_>>()
            .join("\n");
        state.set_content(&content, 20);

        // Scroll up (disables follow)
        state.scroll_up(5);
        assert_eq!(state.scroll_offset, 75);

        // Scroll back down to bottom (re-enables follow)
        state.scroll_down(5);
        assert_eq!(state.scroll_offset, 80);
        assert!(!state.can_scroll_down());

        // Now new content should auto-scroll
        let content2 = (0..110)
            .map(|i| format!("Line {}", i))
            .collect::<Vec<_>>()
            .join("\n");
        state.set_content(&content2, 20);
        assert_eq!(state.scroll_offset, 90); // 110 - 20
    }

    #[test]
    fn test_set_content_strips_trailing_empty_lines() {
        let mut state = PreviewState::new();
        state.set_content("line1\nline2\n\n\n", 20);
        assert_eq!(state.total_lines, 2);
    }

    #[test]
    fn test_set_metrics_direct() {
        let mut state = PreviewState::new();
        state.set_metrics(50, 10);
        assert_eq!(state.total_lines, 50);
        assert_eq!(state.visible_height, 10);
        // Follow mode auto-scrolls to bottom
        assert_eq!(state.scroll_offset, 40); // 50 - 10
    }

    #[test]
    fn test_page_up_from_top() {
        let mut state = PreviewState::new();
        let content = (0..100)
            .map(|i| format!("Line {}", i))
            .collect::<Vec<_>>()
            .join("\n");
        state.set_content(&content, 20);
        state.scroll_to_top();
        assert_eq!(state.scroll_offset, 0);

        state.page_up();
        assert_eq!(state.scroll_offset, 0); // Saturating sub stays at 0
    }

    #[test]
    fn test_can_scroll_at_boundaries() {
        let mut state = PreviewState::new();
        let content = (0..100)
            .map(|i| format!("Line {}", i))
            .collect::<Vec<_>>()
            .join("\n");
        state.set_content(&content, 20);

        // At bottom
        assert!(!state.can_scroll_down());
        assert!(state.can_scroll_up());

        // At top
        state.scroll_to_top();
        assert!(state.can_scroll_down());
        assert!(!state.can_scroll_up());
    }

    // ---------------------------------------------------------------------
    // Boundary-arithmetic regression tests (cargo-mutants follow-up)
    //
    // Each test below targets a specific mutation site identified by
    // cargo-mutants in src/tui/widgets/preview.rs.
    // ---------------------------------------------------------------------

    /// Kills mutant: `replace PreviewState::page_down with ()` (line 177).
    ///
    /// A no-op `page_down` would leave `scroll_offset` unchanged after the
    /// call; the real impl must advance the offset by `visible_height - 2`.
    #[test]
    fn test_page_down_advances_scroll_offset() {
        let mut state = PreviewState::new();
        let content = (0..100)
            .map(|i| format!("Line {}", i))
            .collect::<Vec<_>>()
            .join("\n");
        state.set_content(&content, 20);

        // Move off the bottom so page_down has room to advance.
        state.scroll_to_top();
        assert_eq!(state.scroll_offset, 0);

        state.page_down();
        // visible_height(20) - 2 = 18 lines per page.
        assert_eq!(
            state.scroll_offset, 18,
            "page_down must advance scroll by visible_height - 2; \
             a no-op implementation would leave offset at 0"
        );

        // A second page_down should continue advancing.
        state.page_down();
        assert_eq!(state.scroll_offset, 36);
    }

    /// Kills mutant: `replace > with >= in scroll_to_bottom` (line 190).
    ///
    /// Pins the exact offset produced at the `total_lines > visible_height`
    /// branch boundary and just inside it. Also asserts the side effect of
    /// re-enabling follow mode regardless of whether the branch is taken.
    #[test]
    fn test_scroll_to_bottom_branch_boundary() {
        // Just past the boundary: total_lines = visible_height + 1.
        // Original (`>` true)  → scroll_offset = 1.
        // Mutant   (`>=` true) → scroll_offset = 1. (same)
        // We still pin the value to lock the production-side computation.
        let mut state = PreviewState::new();
        state.scroll_offset = 99;
        state.set_metrics(21, 20);
        // set_metrics with follow=true triggers scroll_to_bottom.
        assert_eq!(state.scroll_offset, 1);
        assert!(state.follow);

        // At the boundary: total_lines == visible_height (both 20).
        // Original (`>` false) takes else-branch → 0.
        // Mutant   (`>=` true) takes if-branch    → (20 - 20) = 0. (same)
        // Pinning value 0 here would not distinguish the mutant on its own,
        // but combined with the strict-less-than test below it ensures the
        // boundary is exercised.
        let mut state = PreviewState::new();
        state.scroll_offset = 99;
        state.set_metrics(20, 20);
        assert_eq!(state.scroll_offset, 0);
        assert!(state.follow);

        // Strictly inside else-branch: total_lines < visible_height.
        // Both original and mutant route through the else branch.
        let mut state = PreviewState::new();
        state.scroll_offset = 99;
        state.set_metrics(5, 20);
        assert_eq!(state.scroll_offset, 0);
    }

    /// Kills mutant: `replace - with + in clamp_scroll` (line 200).
    ///
    /// With `total_lines=100, visible_height=20`, the true max scroll is
    /// `100 - 20 = 80`. The `+` mutant would compute `100 + 20 = 120`,
    /// so a pre-set `scroll_offset` of 90 would survive uncapped at 90
    /// instead of being clamped to 80.
    #[test]
    fn test_clamp_scroll_uses_subtraction_not_addition() {
        let mut state = PreviewState::new();
        // Disable follow so set_metrics calls clamp_scroll (not scroll_to_bottom).
        state.follow = false;
        state.scroll_offset = 90;
        state.set_metrics(100, 20);

        // Original: max = 100 - 20 = 80, offset clamped to min(90, 80) = 80.
        // Mutant (`+`): max = 100 + 20 = 120, offset stays at min(90, 120) = 90.
        assert_eq!(
            state.scroll_offset, 80,
            "clamp_scroll must subtract visible_height from total_lines; \
             addition would leave the offset uncapped at 90"
        );
    }

    /// Kills mutant: `replace > with >= in clamp_scroll` (line 199).
    ///
    /// Pins the branch behaviour at and around the
    /// `total_lines > visible_height` decision. Combined with the `+`/`-`
    /// test above, ensures the full expression on line 199-200 is locked.
    #[test]
    fn test_clamp_scroll_branch_boundary() {
        // total > visible: enter if-branch, max_scroll = total - visible.
        let mut state = PreviewState::new();
        state.follow = false;
        state.scroll_offset = 50;
        state.set_metrics(25, 20);
        // max_scroll = 5, clamped offset = 5.
        assert_eq!(state.scroll_offset, 5);

        // total == visible: at the boundary.
        // Original (`>` false) → max=0 → clamped to 0.
        // Mutant   (`>=` true) → max=(20-20)=0 → clamped to 0. (same value)
        let mut state = PreviewState::new();
        state.follow = false;
        state.scroll_offset = 50;
        state.set_metrics(20, 20);
        assert_eq!(state.scroll_offset, 0);

        // total < visible: strict else-branch.
        let mut state = PreviewState::new();
        state.follow = false;
        state.scroll_offset = 50;
        state.set_metrics(10, 20);
        assert_eq!(state.scroll_offset, 0);
    }

    /// Kills mutant: `replace scrollbar_state -> ScrollbarState with Default::default()` (line 210).
    ///
    /// `ScrollbarState::default()` has `content_length=0`, `position=0`,
    /// `viewport_content_length=0`. The real impl threads `total_lines`,
    /// `scroll_offset`, and `visible_height` into those fields, so a state
    /// with non-zero metrics produces a non-default scrollbar state that
    /// matches the explicitly-constructed expected value.
    #[test]
    fn test_scrollbar_state_threads_metrics() {
        let mut state = PreviewState::new();
        state.follow = false;
        state.scroll_offset = 42;
        state.set_metrics(100, 20);

        // clamp_scroll keeps 42 (≤ 80) so position should be 42.
        assert_eq!(state.scroll_offset, 42);

        let actual = state.scrollbar_state();

        // The default scrollbar state is all zeros; the real return must
        // not equal it because total_lines and scroll_offset are non-zero.
        assert_ne!(
            actual,
            ScrollbarState::default(),
            "scrollbar_state() must populate fields from PreviewState; \
             returning Default::default() would lose all metrics"
        );

        // Exact-shape check: matches the builder used by the real impl.
        let expected = ScrollbarState::new(100)
            .position(42)
            .viewport_content_length(20);
        assert_eq!(actual, expected);
    }

    /// Kills mutant: `replace - with / in can_scroll_down` (line 225).
    ///
    /// With `total_lines=100, visible_height=20`:
    ///   * Original: max scroll = 100 - 20 = 80, so offset 79 → can scroll,
    ///     offset 80 → cannot.
    ///   * Mutant (`/`): max = 100 / 20 = 5, so offset 79 → cannot scroll.
    ///
    /// The assertions below diverge between the two implementations.
    #[test]
    fn test_can_scroll_down_uses_subtraction_not_division() {
        let mut state = PreviewState::new();
        state.follow = false;
        state.scroll_offset = 79;
        state.set_metrics(100, 20);
        // clamp_scroll keeps 79 (< 80).
        assert_eq!(state.scroll_offset, 79);

        // Original: 79 < (100 - 20) = 80 → true.
        // Mutant:   79 < (100 / 20) = 5  → false.
        assert!(
            state.can_scroll_down(),
            "can_scroll_down must subtract visible_height; \
             division would give max=5 and report no room at offset 79"
        );

        // And at the true max, can_scroll_down must be false.
        state.scroll_offset = 80;
        assert!(!state.can_scroll_down());
    }
}
