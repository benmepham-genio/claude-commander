//! Session-list view mode (project / sections / stacks).
//!
//! Lives in `config` rather than `tui` because the user's last-selected view
//! is persisted in `AppState`. The TUI re-exports it from `crate::tui::app`
//! for ergonomics at call sites.

use serde::{Deserialize, Serialize};

/// Which list view mode is active.
///
/// Three modes are supported and the user cycles between them with the
/// `ToggleViewMode` key (default `v`):
/// * `ProjectGrouped` — flat tree, sessions indented under their project,
///   stacks indented under their parent. The default.
/// * `SectionGrouped` — sessions bucketed by user-configured sections based
///   on each session's own PR state. Stacks may be split across sections.
/// * `SectionStacks` — same section layout, but stacks are
///   grouped as a unit; the whole stack lands in the section chosen by
///   the newest leaf, and indentation is preserved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ViewMode {
    #[default]
    ProjectGrouped,
    SectionGrouped,
    // Accept the pre-rename variant name so state.json files written by
    // an earlier build of this branch (when the variant was called
    // `SectionGroupedWithStacks`) still parse. Without this alias,
    // deserialization fails and `main.rs` falls back to a fresh empty
    // AppState — making it look like every project has been wiped.
    #[serde(alias = "SectionGroupedWithStacks")]
    SectionStacks,
}

impl ViewMode {
    /// Returns the next view in the cycle:
    /// Project → Sections → Section Stacks → Project.
    pub fn next(self) -> Self {
        match self {
            Self::ProjectGrouped => Self::SectionGrouped,
            Self::SectionGrouped => Self::SectionStacks,
            Self::SectionStacks => Self::ProjectGrouped,
        }
    }

    /// Heading label rendered above the session tree so the user can see
    /// which view is active at a glance.
    pub fn heading_label(self) -> &'static str {
        match self {
            Self::ProjectGrouped => " Sessions [Project]:",
            Self::SectionGrouped => " Sessions [Sections]:",
            Self::SectionStacks => " Sessions [Section Stacks]:",
        }
    }

    /// Whether this view depends on user-configured sections.
    /// Used to decide whether a persisted/computed view is meaningful when
    /// no sections are configured, and to skip section modes in the cycle
    /// when sections are absent.
    pub fn is_section_view(self) -> bool {
        matches!(self, Self::SectionGrouped | Self::SectionStacks)
    }
}
