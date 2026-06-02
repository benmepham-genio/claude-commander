//! Concurrent-safe state store with file locking
//!
//! Provides `StateStore`, which wraps `AppState` with:
//! - Advisory file locking (`flock`) for cross-process safety
//! - Atomic writes (write-to-temp + rename) for crash safety
//! - An in-memory cache for fast reads on the render path
//! - Periodic reload to pick up changes from other instances

use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use nix::fcntl::{Flock, FlockArg};
use tokio::sync::RwLock;
use tracing::debug;

use super::Config;
use super::storage::AppState;
use crate::error::{ConfigError, Result};

/// Concurrent-safe state persistence backed by a JSON file with advisory locking.
///
/// Multiple instances of the application can safely share the same state file.
/// Writes use a read-modify-write cycle under an exclusive file lock; reads
/// come from an in-memory cache that is periodically refreshed.
pub struct StateStore {
    /// Path to the state JSON file
    state_path: PathBuf,
    /// Path to the lock file (separate from state file because atomic rename changes inodes)
    lock_path: PathBuf,
    /// In-memory cached state for fast reads
    state: Arc<RwLock<AppState>>,
    /// Modification time of the state file at last read/write (for change detection)
    last_mtime: Arc<RwLock<Option<SystemTime>>>,
}

impl StateStore {
    /// Create a new StateStore from an already-loaded AppState.
    pub fn new(app_state: AppState) -> Result<Self> {
        let state_path = Config::state_file_path()?;
        let lock_path = state_path.with_extension("json.lock");

        // Ensure parent directory exists
        if let Some(parent) = state_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                ConfigError::SaveFailed(format!("Failed to create state directory: {}", e))
            })?;
        }

        let mtime = std::fs::metadata(&state_path)
            .and_then(|m| m.modified())
            .ok();

        Ok(Self {
            state_path,
            lock_path,
            state: Arc::new(RwLock::new(app_state)),
            last_mtime: Arc::new(RwLock::new(mtime)),
        })
    }

    /// Create a StateStore with a custom path (for testing).
    pub fn with_path(app_state: AppState, state_path: PathBuf) -> Self {
        let lock_path = state_path.with_extension("json.lock");
        Self {
            state_path,
            lock_path,
            state: Arc::new(RwLock::new(app_state)),
            last_mtime: Arc::new(RwLock::new(None)),
        }
    }

    /// Get a read lock on the in-memory state.
    ///
    /// This is fast (no disk I/O) and safe to call on every render frame.
    pub async fn read(&self) -> tokio::sync::RwLockReadGuard<'_, AppState> {
        self.state.read().await
    }

    /// Non-blocking read of the in-memory state.
    ///
    /// Returns `None` if a writer currently holds the lock. Use from
    /// synchronous render paths where awaiting isn't possible — callers
    /// must tolerate a missed frame on contention.
    pub fn try_read(&self) -> Option<tokio::sync::RwLockReadGuard<'_, AppState>> {
        self.state.try_read().ok()
    }

    /// Apply a mutation to the persisted state.
    ///
    /// Acquires an exclusive file lock, re-reads the current state from disk
    /// (to pick up changes from other instances), applies the closure, writes
    /// back atomically, and updates the in-memory cache.
    pub async fn mutate<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&mut AppState) -> R + Send + 'static,
        R: Send + 'static,
    {
        let state_path = self.state_path.clone();
        let lock_path = self.lock_path.clone();

        // Run the entire lock-read-modify-write cycle on a blocking thread
        // because flock() is a blocking syscall
        let (result, new_state, mtime) = tokio::task::spawn_blocking(move || {
            let lock_file = open_lock_file(&lock_path)?;
            let _lock = Flock::lock(lock_file, FlockArg::LockExclusive).map_err(|(_, e)| {
                crate::error::Error::Config(ConfigError::SaveFailed(format!(
                    "Failed to acquire file lock: {}",
                    e
                )))
            })?;

            let mut disk_state = read_state_from_disk(&state_path)?;
            let result = f(&mut disk_state);
            disk_state.version = env!("CARGO_PKG_VERSION").to_string();
            atomic_write(&state_path, &disk_state)?;

            let mtime = std::fs::metadata(&state_path)
                .and_then(|m| m.modified())
                .ok();

            // _lock dropped here → flock released
            Ok::<_, crate::error::Error>((result, disk_state, mtime))
        })
        .await
        .map_err(|e| ConfigError::SaveFailed(format!("Blocking task panicked: {}", e)))??;

        // Update in-memory cache
        *self.state.write().await = new_state;
        *self.last_mtime.write().await = mtime;

        Ok(result)
    }

    /// Apply a fallible mutation to the persisted state.
    ///
    /// Like `mutate()`, but the closure returns a `Result`. If the closure fails,
    /// the state is **not** written back to disk.
    pub async fn try_mutate<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&mut AppState) -> std::result::Result<R, crate::error::Error> + Send + 'static,
        R: Send + 'static,
    {
        let state_path = self.state_path.clone();
        let lock_path = self.lock_path.clone();

        let (result, new_state, mtime) =
            tokio::task::spawn_blocking(move || -> std::result::Result<_, crate::error::Error> {
                let lock_file = open_lock_file(&lock_path)?;
                let _lock = Flock::lock(lock_file, FlockArg::LockExclusive).map_err(|(_, e)| {
                    crate::error::Error::Config(ConfigError::SaveFailed(format!(
                        "Failed to acquire file lock: {}",
                        e
                    )))
                })?;

                let mut disk_state = read_state_from_disk(&state_path)?;

                // Apply the closure — if it fails, bail without writing
                let result = f(&mut disk_state)?;

                disk_state.version = env!("CARGO_PKG_VERSION").to_string();
                atomic_write(&state_path, &disk_state)?;

                let mtime = std::fs::metadata(&state_path)
                    .and_then(|m| m.modified())
                    .ok();

                Ok((result, disk_state, mtime))
            })
            .await
            .map_err(|e| ConfigError::SaveFailed(format!("Blocking task panicked: {}", e)))??;

        *self.state.write().await = new_state;
        *self.last_mtime.write().await = mtime;

        Ok(result)
    }

    /// Check if the on-disk state has changed and reload if so.
    ///
    /// Returns `true` if the in-memory state was updated.
    pub async fn reload_if_changed(&self) -> Result<bool> {
        let current_mtime = tokio::fs::metadata(&self.state_path)
            .await
            .and_then(|m| m.modified())
            .ok();

        let last = *self.last_mtime.read().await;

        if current_mtime == last {
            return Ok(false);
        }

        debug!("State file mtime changed, reloading");

        let state_path = self.state_path.clone();
        let lock_path = self.lock_path.clone();

        let (new_state, mtime) = tokio::task::spawn_blocking(move || {
            let lock_file = open_lock_file(&lock_path)?;
            let _lock = Flock::lock(lock_file, FlockArg::LockShared).map_err(|(_, e)| {
                crate::error::Error::Config(ConfigError::LoadFailed(format!(
                    "Failed to acquire shared lock: {}",
                    e
                )))
            })?;

            let state = read_state_from_disk(&state_path)?;

            let mtime = std::fs::metadata(&state_path)
                .and_then(|m| m.modified())
                .ok();

            Ok::<_, crate::error::Error>((state, mtime))
        })
        .await
        .map_err(|e| ConfigError::LoadFailed(format!("Blocking task panicked: {}", e)))??;

        *self.state.write().await = new_state;
        *self.last_mtime.write().await = mtime;

        Ok(true)
    }
}

/// Open (or create) the lock file.
fn open_lock_file(lock_path: &PathBuf) -> Result<File> {
    File::options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)
        .map_err(|e| ConfigError::SaveFailed(format!("Failed to open lock file: {}", e)).into())
}

/// Read state from disk, returning a fresh default if the file doesn't exist.
fn read_state_from_disk(state_path: &PathBuf) -> Result<AppState> {
    if !state_path.exists() {
        return Ok(AppState::new());
    }

    let content = std::fs::read_to_string(state_path)
        .map_err(|e| ConfigError::LoadFailed(format!("Failed to read state file: {}", e)))?;

    let state: AppState = serde_json::from_str(&content)
        .map_err(|e| ConfigError::LoadFailed(format!("Failed to parse state file: {}", e)))?;

    Ok(state)
}

/// Atomically write state to a file (write to temp + fsync + rename).
fn atomic_write(path: &PathBuf, state: &AppState) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            ConfigError::SaveFailed(format!("Failed to create state directory: {}", e))
        })?;
    }

    let content = serde_json::to_string_pretty(state)
        .map_err(|e| ConfigError::SaveFailed(format!("Failed to serialize state: {}", e)))?;

    let tmp_path = path.with_extension(format!("json.tmp.{}", std::process::id()));

    {
        let mut tmp_file = File::create(&tmp_path)
            .map_err(|e| ConfigError::SaveFailed(format!("Failed to create temp file: {}", e)))?;
        tmp_file
            .write_all(content.as_bytes())
            .map_err(|e| ConfigError::SaveFailed(format!("Failed to write temp file: {}", e)))?;
        tmp_file
            .sync_all()
            .map_err(|e| ConfigError::SaveFailed(format!("Failed to fsync temp file: {}", e)))?;
    }

    std::fs::rename(&tmp_path, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp_path);
        ConfigError::SaveFailed(format!("Failed to rename temp file: {}", e))
    })?;

    Ok(())
}

impl Clone for StateStore {
    fn clone(&self) -> Self {
        Self {
            state_path: self.state_path.clone(),
            lock_path: self.lock_path.clone(),
            state: self.state.clone(),
            last_mtime: self.last_mtime.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Project;
    use tempfile::TempDir;

    fn create_test_store(temp_dir: &TempDir) -> StateStore {
        let state_path = temp_dir.path().join("state.json");
        StateStore::with_path(AppState::new(), state_path)
    }

    #[tokio::test]
    async fn test_mutate_persists_to_disk() {
        let temp_dir = TempDir::new().unwrap();
        let store = create_test_store(&temp_dir);

        let project = Project::new("test", PathBuf::from("/tmp/test"), "main");
        let project_id = project.id;

        store
            .mutate(move |state| {
                state.add_project(project);
            })
            .await
            .unwrap();

        // Verify in-memory state
        {
            let state = store.read().await;
            assert_eq!(state.project_count(), 1);
        }

        // Verify on-disk state by loading fresh
        let state_path = temp_dir.path().join("state.json");
        let disk_state = AppState::load_from(&state_path).unwrap();
        assert_eq!(disk_state.project_count(), 1);
        assert!(disk_state.get_project(&project_id).is_some());
    }

    #[tokio::test]
    async fn test_concurrent_mutates_no_lost_updates() {
        let temp_dir = TempDir::new().unwrap();
        let state_path = temp_dir.path().join("state.json");

        // Two stores pointing at the same file (simulating two instances)
        let store_a = StateStore::with_path(AppState::new(), state_path.clone());
        let store_b = StateStore::with_path(AppState::new(), state_path.clone());

        // Instance A adds a project
        let project_a = Project::new("project-a", PathBuf::from("/tmp/a"), "main");
        let id_a = project_a.id;
        store_a
            .mutate(move |state| {
                state.add_project(project_a);
            })
            .await
            .unwrap();

        // Instance B adds a different project (reads A's state from disk first)
        let project_b = Project::new("project-b", PathBuf::from("/tmp/b"), "main");
        let id_b = project_b.id;
        store_b
            .mutate(move |state| {
                state.add_project(project_b);
            })
            .await
            .unwrap();

        // Verify both projects exist on disk
        let disk_state = AppState::load_from(&state_path).unwrap();
        assert_eq!(disk_state.project_count(), 2);
        assert!(disk_state.get_project(&id_a).is_some());
        assert!(disk_state.get_project(&id_b).is_some());
    }

    #[tokio::test]
    async fn test_reload_if_changed() {
        let temp_dir = TempDir::new().unwrap();
        let state_path = temp_dir.path().join("state.json");

        let store = StateStore::with_path(AppState::new(), state_path.clone());

        // Write initial state
        store.mutate(|_| {}).await.unwrap();

        // External modification (simulating another instance)
        let mut external_state = AppState::load_from(&state_path).unwrap();
        let project = Project::new("external", PathBuf::from("/tmp/ext"), "main");
        external_state.add_project(project);
        atomic_write(&state_path, &external_state).unwrap();

        // Trigger reload
        let changed = store.reload_if_changed().await.unwrap();
        assert!(changed);

        // Verify the store picked up the external change
        let state = store.read().await;
        assert_eq!(state.project_count(), 1);
    }

    #[tokio::test]
    async fn test_atomic_write_crash_safety() {
        let temp_dir = TempDir::new().unwrap();
        let state_path = temp_dir.path().join("state.json");

        // Write initial valid state
        let mut state = AppState::new();
        let project = Project::new("original", PathBuf::from("/tmp/orig"), "main");
        state.add_project(project);
        atomic_write(&state_path, &state).unwrap();

        // Verify it's valid JSON
        let loaded = AppState::load_from(&state_path).unwrap();
        assert_eq!(loaded.project_count(), 1);
    }
}
