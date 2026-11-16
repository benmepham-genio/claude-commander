//! Pure Rust git operations using gitoxide
//!
//! Provides git functionality without any CLI dependencies:
//! - `GitBackend` - Core gitoxide operations
//! - `WorktreeManager` - Worktree lifecycle management
//! - `DiffCache` - Cached diff computation

mod auto_pull;
mod backend;
mod bounded;
mod clone;
mod clone_jobs;
mod diff;
// Test scaffolding: `git` that never signs, for fixtures. Gated so no release
// build can reach it -- production commits keep the operator's signing policy.
#[cfg(any(test, feature = "test-support"))]
pub mod fixture;
mod github;
pub mod hosting;
// Public as a module rather than glob-re-exported like its siblings: the TUI
// kicks off a background `git lfs pull`, and `git::lfs::pull` says what it does
// where a glob-exported `git::pull` would sit ambiguously beside `auto_pull`'s.
pub mod lfs;
mod pr;
mod review_diff;
mod spawn;
mod summary;
mod worktree;
mod worktree_include;

pub use auto_pull::*;
pub use backend::*;
pub use clone::*;
pub use clone_jobs::*;
pub use diff::*;
pub use github::*;
pub use pr::*;
pub use review_diff::*;
pub use spawn::*;
pub use summary::*;
pub use worktree::*;
