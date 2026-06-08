//! Pure Rust git operations using gitoxide
//!
//! Provides git functionality without any CLI dependencies:
//! - `GitBackend` - Core gitoxide operations
//! - `WorktreeManager` - Worktree lifecycle management
//! - `DiffCache` - Cached diff computation

mod auto_pull;
mod backend;
mod diff;
mod pr;
mod summary;
mod worktree;
mod worktree_include;

pub use auto_pull::*;
pub use backend::*;
pub use diff::*;
pub use pr::*;
pub use summary::*;
pub use worktree::*;
