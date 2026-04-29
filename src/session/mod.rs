//! Session management module
//!
//! Provides the hierarchical session model:
//! - `Project` - A git repository (parent)
//! - `WorktreeSession` - A worktree session within a project (child)
//! - `SessionManager` - Coordinates session lifecycle

mod manager;
pub mod section;
mod types;

pub use manager::*;
pub use section::{
    IN_PROGRESS, RenderedSection, SectionAssignment, SectionConfig, apply_assignment,
    assign_section, build_sections,
};
pub use types::*;
