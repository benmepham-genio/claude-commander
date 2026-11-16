//! Concurrent-safe configuration store with file change detection
//!
//! Mirrors the `StateStore` pattern: wraps `Config` in an `RwLock` for
//! thread-safe access and tracks the config file's modification time to
//! detect external changes (e.g. the user editing `config.toml` by hand).

use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::SystemTime;

use tracing::debug;

use super::Config;
use crate::error::Result;

/// Snapshot of config values that are baked into subsystems at init time
/// and cannot be hot-reloaded. Used to detect when a restart is needed.
#[derive(Debug, Clone, PartialEq)]
struct InitSnapshot {
    max_concurrent_tmux: usize,
    capture_cache_ttl_ms: u64,
    diff_cache_ttl_ms: u64,
    ui_refresh_fps: u32,
    state_sync_interval_ms: u64,
    commander_enabled: bool,
    hibernate_enabled: bool,
    hibernate_check_interval_secs: u64,
    /// The whole `[server]` table: the embedded/standalone server binds once at
    /// startup, so every one of these is an init-time value.
    server: crate::config::ServerConfig,
}

impl InitSnapshot {
    fn capture(config: &Config) -> Self {
        Self {
            max_concurrent_tmux: config.max_concurrent_tmux,
            capture_cache_ttl_ms: config.capture_cache_ttl_ms,
            diff_cache_ttl_ms: config.diff_cache_ttl_ms,
            ui_refresh_fps: config.ui_refresh_fps,
            state_sync_interval_ms: config.state_sync_interval_ms,
            commander_enabled: config.commander_enabled,
            hibernate_enabled: config.hibernate_enabled,
            hibernate_check_interval_secs: config.hibernate_check_interval_secs,
            server: config.server.clone(),
        }
    }

    fn matches(&self, config: &Config) -> bool {
        self.max_concurrent_tmux == config.max_concurrent_tmux
            && self.capture_cache_ttl_ms == config.capture_cache_ttl_ms
            && self.diff_cache_ttl_ms == config.diff_cache_ttl_ms
            && self.ui_refresh_fps == config.ui_refresh_fps
            && self.state_sync_interval_ms == config.state_sync_interval_ms
            && self.commander_enabled == config.commander_enabled
            && self.hibernate_enabled == config.hibernate_enabled
            && self.hibernate_check_interval_secs == config.hibernate_check_interval_secs
            && self.server == config.server
    }
}

/// Concurrent-safe configuration store with mtime-based hot-reload.
///
/// # Hot-reload semantics
///
/// Values read at runtime (keybindings, dim settings, editor, theme, etc.)
/// pick up changes automatically after [`reload_if_changed`](Self::reload_if_changed)
/// detects a new mtime on the config file.
///
/// Values baked into subsystem constructors at init time require a restart:
/// - `max_concurrent_tmux` (TmuxExecutor semaphore size)
/// - `capture_cache_ttl_ms` / `diff_cache_ttl_ms` (cache durations)
/// - `ui_refresh_fps` (event loop tick rate)
/// - `state_sync_interval_ms` (state sync background task interval)
/// - `commander_enabled` (captured by the agent-state poll task at spawn)
/// - `hibernate_enabled` / `hibernate_check_interval_secs` (the hibernation
///   loop is spawned once, with a fixed interval, at construction; the idle
///   *threshold* is read live each tick and is not restart-required)
/// - the whole `[server]` table (the HTTP listener is bound once at startup)
///
/// Call [`restart_required`](Self::restart_required) to check whether any of
/// those init-time values have diverged from the running config. The flag
/// reverts to `false` if the values are changed back to match.
pub struct ConfigStore {
    config: RwLock<Config>,
    config_path: PathBuf,
    last_mtime: RwLock<Option<SystemTime>>,
    /// Snapshot of restart-required fields captured at construction time.
    init_snapshot: InitSnapshot,
    /// Called after every change to the in-memory config — a committed edit
    /// or a hot-reload that picked up a hand edit. [`crate::api::CommanderService`]
    /// registers one that wakes the state store's change feed, so a config
    /// edit from *any* frontend (the TUI's settings modal, a workspace edit, a
    /// server route) reaches every long-polling client at once. Config lives
    /// outside `state.json`, so without this nothing else would bump it.
    listeners: Mutex<Vec<ChangeListener>>,
    /// Open [`defer_change_listeners`](Self::defer_change_listeners) holds,
    /// and whether a change arrived while one was open.
    deferral: Mutex<Deferral>,
}

/// See [`ConfigStore::defer_change_listeners`]. One lock over both fields,
/// so a change that lands as the last hold is released is never lost.
#[derive(Default)]
struct Deferral {
    holds: usize,
    pending: bool,
}

/// Holds back a [`ConfigStore`]'s change listeners until dropped; see
/// [`ConfigStore::defer_change_listeners`].
#[must_use = "listeners are deferred only while the guard is held"]
pub struct DeferredListeners<'a> {
    store: &'a ConfigStore,
}

impl Drop for DeferredListeners<'_> {
    fn drop(&mut self) {
        let fire = {
            let mut deferral = self.store.deferral.lock().expect("deferral lock poisoned");
            deferral.holds -= 1;
            deferral.holds == 0 && std::mem::take(&mut deferral.pending)
        };
        if fire {
            self.store.run_listeners();
        }
    }
}

/// A callback run after the in-memory config changes.
pub type ChangeListener = Arc<dyn Fn() + Send + Sync>;

impl ConfigStore {
    /// Create a new ConfigStore from an already-loaded Config.
    pub fn new(config: Config) -> Result<Self> {
        let config_path = Config::config_file_path()?;
        let mtime = std::fs::metadata(&config_path)
            .and_then(|m| m.modified())
            .ok();
        let init_snapshot = InitSnapshot::capture(&config);
        Ok(Self {
            config: RwLock::new(config),
            config_path,
            last_mtime: RwLock::new(mtime),
            init_snapshot,
            listeners: Mutex::default(),
            deferral: Mutex::default(),
        })
    }

    /// Create a ConfigStore with a custom path (for testing).
    pub fn with_path(config: Config, config_path: PathBuf) -> Self {
        let mtime = std::fs::metadata(&config_path)
            .and_then(|m| m.modified())
            .ok();
        let init_snapshot = InitSnapshot::capture(&config);
        Self {
            config: RwLock::new(config),
            config_path,
            last_mtime: RwLock::new(mtime),
            init_snapshot,
            listeners: Mutex::default(),
            deferral: Mutex::default(),
        }
    }

    /// Register `listener` to run after every change to the in-memory config:
    /// a committed [`mutate`](Self::mutate)-family edit, or a
    /// [`reload_if_changed`](Self::reload_if_changed) that picked one up. A
    /// refused or no-op edit runs nothing. Listeners run after the config lock
    /// is released, so one may read the config.
    pub fn add_change_listener(&self, listener: ChangeListener) {
        self.listeners
            .lock()
            .expect("listener lock poisoned")
            .push(listener);
    }

    /// Hold every change listener until the returned guard drops, then run
    /// them once if any change was committed meanwhile — however many there
    /// were, and even if the caller's later steps fail.
    ///
    /// For an edit that writes config *and* `state.json`: without the hold,
    /// the config commit wakes clients before the state write, and a client
    /// that refetches in that window reads the new config against the old
    /// state (a renamed section with its sessions still under the old name).
    /// The state write wakes them again, so it would only be a flicker, but
    /// holding the config wake until after the state write removes it. The
    /// hold is store-wide: an unrelated edit committed meanwhile from another
    /// task is delayed to the drop, never lost.
    pub fn defer_change_listeners(&self) -> DeferredListeners<'_> {
        self.deferral.lock().expect("deferral lock poisoned").holds += 1;
        DeferredListeners { store: self }
    }

    fn notify_listeners(&self) {
        {
            let mut deferral = self.deferral.lock().expect("deferral lock poisoned");
            if deferral.holds > 0 {
                deferral.pending = true;
                return;
            }
        }
        self.run_listeners();
    }

    fn run_listeners(&self) {
        // Clone out so a listener never runs under the listener lock.
        let listeners = self
            .listeners
            .lock()
            .expect("listener lock poisoned")
            .clone();
        for listener in listeners {
            listener();
        }
    }

    /// Get a read guard on the current config.
    ///
    /// This is fast (no disk I/O) and safe to call on every render frame.
    pub fn read(&self) -> std::sync::RwLockReadGuard<'_, Config> {
        self.config.read().expect("config lock poisoned")
    }

    /// Apply a mutation to the config, then persist to disk.
    ///
    /// Like every mutation here, the edit is applied to a copy and committed
    /// to memory only once the save succeeds, so a failed save leaves the
    /// in-memory config in step with disk. Updates the tracked mtime so that
    /// `reload_if_changed()` won't immediately re-read our own write.
    pub fn mutate<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&mut Config) -> R,
    {
        self.try_mutate(|c| Ok(f(c)))
    }

    /// Apply a fallible mutation to a copy of the config and commit it — to
    /// memory and to disk — only if the closure succeeds and the save does.
    ///
    /// A rejected edit touches nothing: no half-applied in-memory config, no
    /// rewrite of `config.toml` (which would clobber a hand edit the
    /// hot-reload has not read yet and record an mtime that hides it for
    /// good), and no save error masking the rejection.
    pub fn try_mutate<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&mut Config) -> Result<T>,
    {
        self.commit_edit(|c| Ok((f(c)?, true)))
    }

    /// Apply a mutation to a copy of the config and commit it only when the
    /// closure reports it changed something (`true`). Like
    /// [`try_mutate`](Self::try_mutate), a refused or no-op edit writes
    /// nothing, for edits whose refusal is an `Ok(false)` rather than an error.
    pub fn mutate_if<F>(&self, f: F) -> Result<bool>
    where
        F: FnOnce(&mut Config) -> bool,
    {
        self.commit_edit(|c| {
            let changed = f(c);
            Ok((changed, changed))
        })
    }

    /// The copy-then-commit core every mutation shares: run `f` on a clone of
    /// the config under the write lock, and save and swap the clone in only
    /// when `f` returns `Ok((_, true))`. `Ok((_, false))` and `Err` leave
    /// memory and disk untouched.
    fn commit_edit<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&mut Config) -> Result<(T, bool)>,
    {
        let mut config = self.config.write().expect("config lock poisoned");
        let mut edited = config.clone();
        let (result, commit) = f(&mut edited)?;
        if commit {
            self.save_to_disk(&edited)?;
            *config = edited;
            drop(config);
            self.notify_listeners();
        }
        Ok(result)
    }

    /// Write the given config to `self.config_path` and update the tracked mtime.
    fn save_to_disk(&self, config: &Config) -> Result<()> {
        use crate::error::ConfigError;

        if let Some(parent) = self.config_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                crate::error::Error::Config(ConfigError::SaveFailed(format!(
                    "Failed to create config directory: {}",
                    e
                )))
            })?;
        }

        let toml =
            toml::to_string_pretty(config).map_err(|e| ConfigError::SaveFailed(e.to_string()))?;
        super::write_private_file(&self.config_path, toml)
            .map_err(|e| ConfigError::SaveFailed(e.to_string()))?;

        let mtime = std::fs::metadata(&self.config_path)
            .and_then(|m| m.modified())
            .ok();
        *self.last_mtime.write().expect("mtime lock poisoned") = mtime;

        Ok(())
    }

    /// Check if the config file has been modified externally and reload if so.
    ///
    /// Returns `true` if the file was reloaded. Change listeners run only when
    /// the reloaded config differs from the in-memory one: a touched but
    /// identical file changes nothing a client could observe.
    pub fn reload_if_changed(&self) -> Result<bool> {
        let current_mtime = std::fs::metadata(&self.config_path)
            .and_then(|m| m.modified())
            .ok();

        let last = *self.last_mtime.read().expect("mtime lock poisoned");

        if current_mtime == last {
            return Ok(false);
        }

        debug!("Config file mtime changed, reloading");

        let new_config = self.load_from_disk()?;
        let reloaded_mtime = std::fs::metadata(&self.config_path)
            .and_then(|m| m.modified())
            .ok();
        let changed = {
            let mut config = self.config.write().expect("config lock poisoned");
            let changed = !same_config(&config, &new_config);
            *config = new_config;
            changed
        };
        *self.last_mtime.write().expect("mtime lock poisoned") = reloaded_mtime;
        if changed {
            self.notify_listeners();
        }

        Ok(true)
    }

    /// Check whether any restart-required config values have diverged from
    /// the values that were active when the application started.
    ///
    /// Returns `false` if the values have been changed back to match, so the
    /// indicator self-heals without a restart.
    pub fn restart_required(&self) -> bool {
        let config = self.config.read().expect("config lock poisoned");
        !self.init_snapshot.matches(&config)
    }

    /// Load config from `self.config_path` using the standard layered resolution.
    fn load_from_disk(&self) -> Result<Config> {
        // `load_from_path` runs config-file migrations and validates the
        // remote-server list; `reload_if_changed` propagates any error and
        // keeps the previous in-memory config, so a bad manual edit can't
        // poison the running TUI.
        Config::load_from_path(&self.config_path)
    }
}

/// Whether two configs are the same. `Config` has no `PartialEq` (deriving
/// one reaches into `KeyBindings`, sections, and the conversation, STT and
/// telemetry tables), so compare what would be persisted. `toml::Value`
/// equality ignores table key order, so a `HashMap` field cannot make two
/// equal configs differ. A serialisation failure counts as "different": a
/// spurious wake costs a refetch, a missed one hides an edit.
fn same_config(a: &Config, b: &Config) -> bool {
    match (toml::Value::try_from(a), toml::Value::try_from(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProgramEntry;
    use tempfile::TempDir;

    fn write_config(path: &std::path::Path, config: &Config) {
        let toml = toml::to_string_pretty(config).expect("serialize config");
        std::fs::write(path, toml).expect("write config file");
    }

    #[cfg(unix)]
    #[test]
    fn save_to_disk_restricts_config_to_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let config_path = dir.path().join("config.toml");
        let store = ConfigStore::with_path(Config::default(), config_path.clone());

        store.save_to_disk(&Config::default()).unwrap();

        let mode = std::fs::metadata(&config_path)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "config file carries bearer tokens and must be owner read/write only"
        );
    }

    #[test]
    fn test_reload_if_changed_detects_external_edit() {
        let dir = TempDir::new().unwrap();
        let config_path = dir.path().join("config.toml");

        let config = Config::default();
        write_config(&config_path, &config);

        let store = ConfigStore::with_path(config, config_path.clone());

        // No change yet
        assert!(!store.reload_if_changed().unwrap());

        // Simulate external edit — change a field and rewrite the file
        // Sleep briefly so mtime differs (filesystem granularity)
        std::thread::sleep(std::time::Duration::from_millis(50));
        let edited = Config {
            programs: vec![ProgramEntry {
                label: "External Edit".to_string(),
                command: "external-edit".to_string(),
            }],
            ..Config::default()
        };
        write_config(&config_path, &edited);

        // Should detect the change
        assert!(store.reload_if_changed().unwrap());
        assert_eq!(store.read().default_session_program(), "external-edit");

        // Second call should not reload again
        assert!(!store.reload_if_changed().unwrap());
    }

    #[test]
    fn test_mutate_persists_and_updates_mtime() {
        let dir = TempDir::new().unwrap();
        let config_path = dir.path().join("config.toml");

        let config = Config::default();
        write_config(&config_path, &config);

        let store = ConfigStore::with_path(config, config_path.clone());

        store
            .mutate(|c| {
                c.programs = vec![ProgramEntry {
                    label: "Mutated".to_string(),
                    command: "mutated".to_string(),
                }];
            })
            .unwrap();

        // In-memory value updated
        assert_eq!(store.read().default_session_program(), "mutated");

        // On-disk value updated
        let disk_content = std::fs::read_to_string(&config_path).unwrap();
        assert!(disk_content.contains("mutated"));

        // No spurious reload after our own write
        assert!(!store.reload_if_changed().unwrap());
    }

    #[test]
    fn rejected_try_mutate_leaves_memory_and_disk_untouched() {
        let dir = TempDir::new().unwrap();
        let config_path = dir.path().join("config.toml");
        write_config(&config_path, &Config::default());
        let bytes = std::fs::read(&config_path).unwrap();
        let store = ConfigStore::with_path(Config::default(), config_path.clone());

        let result = store.try_mutate(|c| {
            c.ui_refresh_fps = 41;
            Err::<(), _>(crate::error::ConfigError::SaveFailed("rejected".into()).into())
        });

        assert!(result.is_err());
        assert_ne!(store.read().ui_refresh_fps, 41, "half-applied edit kept");
        assert_eq!(std::fs::read(&config_path).unwrap(), bytes);
    }

    #[test]
    fn failed_save_in_try_mutate_leaves_memory_untouched() {
        let dir = TempDir::new().unwrap();
        // A directory where the config file belongs makes the write fail.
        let config_path = dir.path().join("config.toml");
        std::fs::create_dir_all(&config_path).unwrap();
        let store = ConfigStore::with_path(Config::default(), config_path);

        let result = store.try_mutate(|c| {
            c.ui_refresh_fps = 41;
            Ok(())
        });
        assert!(result.is_err());
        assert_ne!(store.read().ui_refresh_fps, 41);
    }

    #[test]
    fn test_restart_required_false_when_unchanged() {
        let dir = TempDir::new().unwrap();
        let config_path = dir.path().join("config.toml");

        let config = Config::default();
        write_config(&config_path, &config);

        let store = ConfigStore::with_path(config, config_path);
        assert!(!store.restart_required());
    }

    #[test]
    fn test_restart_required_true_when_init_field_changes() {
        let dir = TempDir::new().unwrap();
        let config_path = dir.path().join("config.toml");

        let config = Config::default();
        write_config(&config_path, &config);

        let store = ConfigStore::with_path(config, config_path);

        // Change a restart-required field
        store
            .mutate(|c| {
                c.ui_refresh_fps = 60;
            })
            .unwrap();

        assert!(store.restart_required());
    }

    #[test]
    fn test_restart_required_true_when_commander_enabled_changes() {
        // The agent-state poll task captures `commander_enabled` at spawn, so
        // toggling it at runtime must surface the restart-required warning
        // (otherwise the chip/row would silently never update).
        let dir = TempDir::new().unwrap();
        let config_path = dir.path().join("config.toml");

        let config = Config::default();
        write_config(&config_path, &config);

        let store = ConfigStore::with_path(config, config_path);

        store
            .mutate(|c| {
                c.commander_enabled = true;
            })
            .unwrap();

        assert!(store.restart_required());
    }

    #[test]
    fn test_restart_required_true_when_hibernate_fields_change() {
        // The hibernation loop is spawned once, with a fixed interval, at
        // construction — so enabling it or changing the check interval at
        // runtime must surface the restart-required warning. (The idle timeout
        // is read live and is asserted hot-reloadable below.)
        for mutate in [
            |c: &mut Config| c.hibernate_enabled = true,
            |c: &mut Config| c.hibernate_check_interval_secs = 30,
        ] {
            let dir = TempDir::new().unwrap();
            let config_path = dir.path().join("config.toml");
            let config = Config::default();
            write_config(&config_path, &config);
            let store = ConfigStore::with_path(config, config_path);

            store.mutate(mutate).unwrap();

            assert!(store.restart_required());
        }
    }

    #[test]
    fn test_restart_not_required_when_hibernate_timeout_changes() {
        // The idle timeout is read live each tick, so changing it must NOT
        // demand a restart.
        let dir = TempDir::new().unwrap();
        let config_path = dir.path().join("config.toml");
        let config = Config::default();
        write_config(&config_path, &config);
        let store = ConfigStore::with_path(config, config_path);

        store
            .mutate(|c| {
                c.hibernate_idle_timeout_secs = 60;
            })
            .unwrap();

        assert!(!store.restart_required());
    }

    #[test]
    fn test_restart_required_reverts_when_changed_back() {
        let dir = TempDir::new().unwrap();
        let config_path = dir.path().join("config.toml");

        let config = Config::default();
        let original_fps = config.ui_refresh_fps;
        write_config(&config_path, &config);

        let store = ConfigStore::with_path(config, config_path);

        // Change it
        store
            .mutate(|c| {
                c.ui_refresh_fps = 60;
            })
            .unwrap();
        assert!(store.restart_required());

        // Change it back
        store
            .mutate(|c| {
                c.ui_refresh_fps = original_fps;
            })
            .unwrap();
        assert!(!store.restart_required());
    }

    #[test]
    fn test_restart_required_ignores_hot_reloadable_fields() {
        let dir = TempDir::new().unwrap();
        let config_path = dir.path().join("config.toml");

        let config = Config::default();
        write_config(&config_path, &config);

        let store = ConfigStore::with_path(config, config_path);

        // Change only hot-reloadable fields — should NOT require restart
        store
            .mutate(|c| {
                c.programs = vec![ProgramEntry {
                    label: "Different".to_string(),
                    command: "different".to_string(),
                }];
                c.leader_key = "f1".to_string();
            })
            .unwrap();

        assert!(!store.restart_required());
    }

    #[test]
    fn test_read_returns_current_config() {
        let dir = TempDir::new().unwrap();
        let config_path = dir.path().join("config.toml");

        let config = Config {
            programs: vec![ProgramEntry {
                label: "Test Program".to_string(),
                command: "test-program".to_string(),
            }],
            ..Config::default()
        };
        write_config(&config_path, &config);

        let store = ConfigStore::with_path(config, config_path);
        assert_eq!(store.read().default_session_program(), "test-program");
    }

    #[test]
    fn test_reload_if_changed_runs_config_migrations() {
        let dir = TempDir::new().unwrap();
        let config_path = dir.path().join("config.toml");

        let config = Config::default();
        write_config(&config_path, &config);
        let store = ConfigStore::with_path(config, config_path.clone());

        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(&config_path, "default_program = \"codex\"\n").unwrap();

        assert!(store.reload_if_changed().unwrap());
        assert_eq!(store.read().default_session_program(), "codex");
        let migrated = std::fs::read_to_string(config_path).unwrap();
        assert!(!migrated.contains("default_program"));
        assert!(migrated.contains("command = \"codex\""));
        assert!(!store.reload_if_changed().unwrap());
    }

    /// A `[server]` table must survive an unrelated settings edit.
    ///
    /// Regression test. `mutate` persists by re-serialising the whole `Config`,
    /// so any table core does not model is dropped on the next write: before
    /// `[server]` moved into `Config`, one toggle in the settings modal silently
    /// deleted the operator's bind address and bearer token, and the next launch
    /// generated a fresh token that every paired client would reject.
    #[test]
    fn mutate_preserves_the_server_table() {
        let dir = TempDir::new().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            "[server]\nauto_start = true\nbind = \"0.0.0.0\"\nport = 9999\ntoken = \"sekret\"\n",
        )
        .unwrap();

        let config = Config::load_from_path(&config_path).unwrap();
        let store = ConfigStore::with_path(config, config_path.clone());

        // Exactly what the settings modal does: mutate one unrelated field.
        store.mutate(|c| c.ui_refresh_fps = 30).unwrap();

        let reloaded = Config::load_from_path(&config_path).unwrap();
        assert!(reloaded.server.auto_start);
        assert_eq!(reloaded.server.bind.to_string(), "0.0.0.0");
        assert_eq!(reloaded.server.port, 9999);
        assert_eq!(reloaded.server.token.as_deref(), Some("sekret"));
        assert_eq!(reloaded.ui_refresh_fps, 30, "the edit itself must land");
    }

    /// `[[workspaces]]`, `[main_workspace]` and `startup_workspace` must survive
    /// an unrelated settings edit — the same whole-`Config` re-serialisation
    /// that once deleted `[server]` would otherwise drop every definition.
    #[test]
    fn mutate_preserves_the_workspaces_table() {
        use claude_commander_protocol::workspace::{StartupWorkspace, WorkspaceDef};
        let dir = TempDir::new().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            "startup_workspace = \"Work\"\n\
             [[workspaces]]\nname = \"Work\"\n\
             [[workspaces]]\nname = \"Play\"\n\
             [main_workspace]\nname = \"Home\"\n",
        )
        .unwrap();

        let config = Config::load_from_path(&config_path).unwrap();
        let store = ConfigStore::with_path(config, config_path.clone());
        store.mutate(|c| c.ui_refresh_fps = 30).unwrap();

        let reloaded = Config::load_from_path(&config_path).unwrap();
        assert_eq!(
            reloaded.workspaces,
            vec![WorkspaceDef::named("Work"), WorkspaceDef::named("Play")]
        );
        assert_eq!(reloaded.main_workspace, Some(WorkspaceDef::named("Home")));
        assert_eq!(
            reloaded.startup_workspace,
            StartupWorkspace::Named("Work".into())
        );
        assert_eq!(reloaded.ui_refresh_fps, 30, "the edit itself must land");
    }

    /// `[workspace_themes."<name>"]` (and Main's reserved key) must survive an
    /// unrelated settings edit, like every other table core models — this is
    /// the whole-`Config` re-serialisation that once deleted `[server]`.
    #[test]
    fn mutate_preserves_the_workspace_themes_table() {
        use crate::config::MAIN_WORKSPACE_THEME_KEY;
        use crate::config::theme::ColorValue;
        use ratatui_core::style::Color;
        let dir = TempDir::new().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            "[theme]\npreset = \"indexed\"\n\
             [workspace_themes.\"Side Gig\"]\npreset = \"basic\"\ntext_accent = \"#ff8800\"\n\
             [workspace_themes.main]\nborder_focused = \"red\"\n",
        )
        .unwrap();

        let config = Config::load_from_path(&config_path).unwrap();
        let store = ConfigStore::with_path(config, config_path.clone());
        store.mutate(|c| c.ui_refresh_fps = 30).unwrap();

        let reloaded = Config::load_from_path(&config_path).unwrap();
        assert_eq!(
            reloaded.workspace_themes.len(),
            2,
            "{:?}",
            reloaded.workspace_themes
        );
        let side = &reloaded.workspace_themes["Side Gig"];
        assert_eq!(side.preset.as_deref(), Some("basic"));
        assert_eq!(
            side.text_accent,
            Some(ColorValue(Color::Rgb(0xff, 0x88, 0x00)))
        );
        assert_eq!(
            reloaded.workspace_themes[MAIN_WORKSPACE_THEME_KEY].border_focused,
            Some(ColorValue(Color::Red))
        );
        assert_eq!(reloaded.theme.preset.as_deref(), Some("indexed"));
        assert_eq!(reloaded.ui_refresh_fps, 30, "the edit itself must land");
    }

    /// No per-workspace themes → no `[workspace_themes]` table written, and an
    /// absent table loads empty.
    #[test]
    fn workspace_themes_round_trip_and_stay_absent_when_empty() {
        let empty = toml::to_string(&Config::default()).unwrap();
        assert!(!empty.contains("workspace_themes"), "{empty}");

        let mut config = Config::default();
        config.workspace_themes.insert(
            "Work".into(),
            crate::config::ThemeOverrides {
                preset: Some("truecolor".into()),
                ..Default::default()
            },
        );
        let text = toml::to_string(&config).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back.workspace_themes, config.workspace_themes);
    }

    /// Main's theme key must be one no user workspace can be called, or a
    /// workspace named like it would share Main's theme.
    #[test]
    fn mains_theme_key_is_a_reserved_workspace_name() {
        use claude_commander_protocol::workspace::validate_workspace_name;
        let key = crate::config::MAIN_WORKSPACE_THEME_KEY;
        assert!(validate_workspace_name(key).is_err());
        assert!(validate_workspace_name(&key.to_uppercase()).is_err());
    }

    /// Absent workspace keys load as "only Main, open on the last one".
    #[test]
    fn a_config_without_workspaces_has_only_main() {
        use claude_commander_protocol::workspace::StartupWorkspace;
        let config = Config::default();
        assert!(config.workspaces.is_empty());
        assert!(config.main_workspace.is_none());
        assert_eq!(config.startup_workspace, StartupWorkspace::Last);
    }

    /// Changing anything under `[server]` needs a restart: the listener is bound
    /// once at startup, so the settings tab has to say so rather than implying a
    /// live rebind.
    #[test]
    fn changing_the_server_port_requires_a_restart() {
        let dir = TempDir::new().unwrap();
        let config_path = dir.path().join("config.toml");
        let config = Config::default();
        write_config(&config_path, &config);

        let store = ConfigStore::with_path(config, config_path);
        assert!(!store.restart_required());

        store.mutate(|c| c.server.port = 9999).unwrap();
        assert!(store.restart_required());

        // Self-heals when put back, like every other init-time value.
        store.mutate(|c| c.server.port = 7878).unwrap();
        assert!(!store.restart_required());
    }

    /// Count how often `store`'s change listeners fire.
    fn count_wakes(store: &ConfigStore) -> Arc<std::sync::atomic::AtomicUsize> {
        let wakes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = wakes.clone();
        store.add_change_listener(Arc::new(move || {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }));
        wakes
    }

    /// Bump `path`'s mtime without changing its content.
    fn touch(path: &std::path::Path) {
        let later = std::fs::metadata(path).unwrap().modified().unwrap()
            + std::time::Duration::from_secs(5);
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(later)
            .unwrap();
    }

    /// A touched-but-identical `config.toml` (an editor that saves without a
    /// change, a `git checkout` that rewrites the same bytes) changes nothing
    /// observable, so it must not wake every client into a refetch.
    #[test]
    fn reloading_an_unchanged_config_runs_no_listeners() {
        let dir = TempDir::new().unwrap();
        let config_path = dir.path().join("config.toml");
        write_config(&config_path, &Config::default());
        let store = ConfigStore::with_path(Config::default(), config_path.clone());
        let wakes = count_wakes(&store);

        touch(&config_path);
        store.reload_if_changed().unwrap();
        assert_eq!(wakes.load(std::sync::atomic::Ordering::SeqCst), 0);

        // A real edit still wakes them.
        let edited = Config {
            branch_prefix: "edited/".to_string(),
            ..Config::default()
        };
        write_config(&config_path, &edited);
        touch(&config_path);
        assert!(store.reload_if_changed().unwrap());
        assert_eq!(wakes.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// Edits under a hold run the listeners once, when the last hold drops —
    /// and an edit made with no hold open still runs them at once.
    #[test]
    fn deferred_listeners_run_once_when_the_last_hold_drops() {
        let dir = TempDir::new().unwrap();
        let store = ConfigStore::with_path(Config::default(), dir.path().join("config.toml"));
        let wakes = count_wakes(&store);
        let count = || wakes.load(std::sync::atomic::Ordering::SeqCst);

        let outer = store.defer_change_listeners();
        let inner = store.defer_change_listeners();
        store.mutate(|c| c.ui_refresh_fps = 11).unwrap();
        store.mutate(|c| c.ui_refresh_fps = 12).unwrap();
        drop(inner);
        assert_eq!(count(), 0, "an outer hold is still open");
        drop(outer);
        assert_eq!(count(), 1, "coalesced into one wake");

        // Nothing committed under a hold: nothing to run.
        drop(store.defer_change_listeners());
        assert_eq!(count(), 1);

        store.mutate(|c| c.ui_refresh_fps = 13).unwrap();
        assert_eq!(count(), 2);
    }
}
