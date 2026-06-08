//! Configuration and persistence module
//!
//! Handles:
//! - User configuration (`~/.claude-commander/config.toml`)
//! - Persistent state (`~/.claude-commander/state.json`)
//! - Worktree directory management

mod config_store;
pub mod keybindings;
mod settings;
pub mod storage;
mod store;
pub mod theme;
mod view_mode;

pub use config_store::ConfigStore;
pub use keybindings::{BindableAction, KeyBinding, KeyBindings};
pub use settings::*;
pub use storage::*;
pub use store::StateStore;
pub use theme::{ColorValue, ThemeOverrides};
pub use view_mode::ViewMode;
