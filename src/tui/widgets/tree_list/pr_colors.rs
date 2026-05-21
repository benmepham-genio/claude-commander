//! PR badge and pill colour selection.

use ratatui::style::Color;

use crate::git::{PrState, effective_pr_state};
use crate::tui::theme::Theme;

/// Does the PR have any label matching the "review needed" list?
pub(super) fn needs_review(labels: &[String], review_labels: &[String]) -> bool {
    !labels.is_empty()
        && labels
            .iter()
            .any(|l| review_labels.iter().any(|r| r.eq_ignore_ascii_case(l)))
}

/// Pick the pill background colour for a PR badge from the same state
/// logic as [`pr_badge_color`], but reading the darker `pr_pill_*_bg`
/// theme fields so bold near-white text remains legible.
pub(crate) fn pr_pill_bg_color(
    theme: &Theme,
    state: Option<PrState>,
    pr_merged: bool,
    is_draft: bool,
    labels: &[String],
    review_labels: &[String],
) -> Color {
    match effective_pr_state(state, pr_merged) {
        PrState::Merged => theme.pr_pill_merged_bg,
        PrState::Closed => theme.pr_pill_closed_bg,
        PrState::Open => {
            if is_draft {
                theme.pr_pill_draft_bg
            } else if needs_review(labels, review_labels) {
                theme.pr_pill_review_bg
            } else {
                theme.pr_pill_open_bg
            }
        }
    }
}

/// Pick the PR badge text colour from PR state, draft flag, and label-based
/// review-needed signalling.
///
/// Priority: merged > closed > draft (within open) > review-needed > open.
/// Falls back to `pr_open` when state is unknown but `pr_merged` is false,
/// and `status_pr_merged` when state is unknown but `pr_merged` is true
/// (handles state.json files written before pr_state was added).
pub(crate) fn pr_badge_color(
    theme: &Theme,
    state: Option<PrState>,
    pr_merged: bool,
    is_draft: bool,
    labels: &[String],
    review_labels: &[String],
) -> Color {
    match effective_pr_state(state, pr_merged) {
        PrState::Merged => theme.status_pr_merged,
        PrState::Closed => theme.pr_closed,
        PrState::Open => {
            if is_draft {
                theme.pr_draft
            } else if needs_review(labels, review_labels) {
                theme.status_pr
            } else {
                theme.pr_open
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_theme() -> Theme {
        Theme::basic()
    }

    /// `Theme::basic()` uses concrete ANSI colours for every `pr_pill_*_bg`
    /// field, none of which equal `Color::default()` (== `Color::Reset`).
    /// Asserting equality to those specific theme fields kills the
    /// `replace pr_pill_bg_color -> Color with Default::default()` mutant.
    #[test]
    fn pr_pill_bg_color_open_returns_theme_open_bg() {
        let theme = test_theme();
        let color = pr_pill_bg_color(&theme, Some(PrState::Open), false, false, &[], &[]);
        assert_eq!(color, theme.pr_pill_open_bg);
        assert_ne!(color, Color::default());
    }

    #[test]
    fn pr_pill_bg_color_merged_returns_theme_merged_bg() {
        let theme = test_theme();
        let color = pr_pill_bg_color(&theme, Some(PrState::Merged), true, false, &[], &[]);
        assert_eq!(color, theme.pr_pill_merged_bg);
        assert_ne!(color, Color::default());
    }

    #[test]
    fn pr_pill_bg_color_closed_returns_theme_closed_bg() {
        let theme = test_theme();
        let color = pr_pill_bg_color(&theme, Some(PrState::Closed), false, false, &[], &[]);
        assert_eq!(color, theme.pr_pill_closed_bg);
    }

    #[test]
    fn pr_pill_bg_color_draft_returns_theme_draft_bg() {
        let theme = test_theme();
        let color = pr_pill_bg_color(&theme, Some(PrState::Open), false, true, &[], &[]);
        assert_eq!(color, theme.pr_pill_draft_bg);
    }

    #[test]
    fn pr_pill_bg_color_review_returns_theme_review_bg() {
        let theme = test_theme();
        let labels = vec!["needs-review".to_string()];
        let review_labels = vec!["needs-review".to_string()];
        let color = pr_pill_bg_color(
            &theme,
            Some(PrState::Open),
            false,
            false,
            &labels,
            &review_labels,
        );
        assert_eq!(color, theme.pr_pill_review_bg);
    }
}
