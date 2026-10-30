//! Nix flake integration: wrap launch commands in `nix develop` so sessions
//! run inside the project's dev shell when the project is a flake.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::sync::{Mutex, OnceLock};

use super::*;

/// Wrap a shell command string so it runs inside the Nix dev shell of the
/// flake at `flake_ref`, relative to the pane's working directory.
///
/// The command is a full shell string (it may already contain quoting), not
/// an argv list, so it must go through `sh -c` as a single argument. `exec`
/// replaces that `sh` with the program, avoiding an extra process layer.
pub(super) fn wrap_in_nix_develop(cmd: &str, flake_ref: &str) -> String {
    let escaped = lifecycle::shell_escape_single_quote(&format!("exec {cmd}"));
    format!("nix develop {flake_ref} --command sh -c '{escaped}'")
}

/// Whether a `nix` executable exists in any directory of the given PATH value.
fn nix_in_path(path_var: Option<&OsStr>) -> bool {
    path_var.is_some_and(|paths| std::env::split_paths(paths).any(|dir| dir.join("nix").is_file()))
}

/// Cached check for `nix` on the current process PATH. Cached because every
/// session launch and restart consults it and PATH doesn't change mid-run.
fn nix_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| nix_in_path(std::env::var_os("PATH").as_deref()))
}

/// The flake ref a launch in `dir` should run `nix develop` against, if any:
/// the config option is on, nix is installed, and the directory has a
/// `flake.nix` at its root or, failing that, in a `nix/` subdirectory.
fn nix_develop_flake_ref(enabled: bool, dir: &Path, nix_available: bool) -> Option<&'static str> {
    if !enabled || !nix_available {
        return None;
    }
    [".", "./nix"]
        .into_iter()
        .find(|flake_ref| dir.join(flake_ref).join("flake.nix").is_file())
}

/// The flake ref to pre-warm for `dir`, if any: the launch would use nix
/// develop (see `nix_develop_flake_ref`) *and* we haven't already attempted a
/// warm for this directory this process. Records the directory in `seen` on
/// the first `Some`, so each repo is warm-attempted at most once per process —
/// rapidly creating several sessions in one project only warms once. Note this
/// dedups on *attempt*, not success: a failed warm won't be retried.
fn prewarm_flake_ref(
    enabled: bool,
    dir: &Path,
    nix_available: bool,
    seen: &Mutex<HashSet<PathBuf>>,
) -> Option<&'static str> {
    let flake_ref = nix_develop_flake_ref(enabled, dir, nix_available)?;
    // insert() returns true when the value was newly inserted (not yet warmed).
    seen.lock()
        .unwrap()
        .insert(dir.to_path_buf())
        .then_some(flake_ref)
}

/// Process-global set of repo paths already pre-warmed this run.
fn prewarmed_repos() -> &'static Mutex<HashSet<PathBuf>> {
    static PREWARMED: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    PREWARMED.get_or_init(|| Mutex::new(HashSet::new()))
}

impl SessionManager {
    /// Wrap `cmd` in `nix develop --command` when `dir` is a flake project
    /// (see `nix_develop_flake_ref`); otherwise return it unchanged. Applied
    /// as the outermost layer of every launch command, after Claude-specific
    /// flag injection.
    pub(super) fn maybe_wrap_nix_develop(&self, cmd: &str, dir: &Path) -> String {
        match nix_develop_flake_ref(self.config_store.read().nix_develop, dir, nix_available()) {
            Some(flake_ref) => {
                info!("Launching inside nix develop shell at {}", dir.display());
                wrap_in_nix_develop(cmd, flake_ref)
            }
            None => cmd.to_string(),
        }
    }

    /// Kick off a background `nix develop --command true` in `repo_path` so the
    /// project's dev shell is evaluated and its dependencies fetched/built into
    /// `/nix/store` *before* a freshly-created session's pane runs `nix develop`
    /// itself. The new worktree checks out the same commit, so the toolchain
    /// store paths its dev shell needs are typically already built by this warm
    /// (the store is content-addressed and shared) — the pane's own `nix
    /// develop` then reuses the warm store instead of building from cold. The
    /// win is bounded by the overlap window (git fetch + worktree add): a
    /// genuinely cold, long build isn't eliminated, just started earlier.
    ///
    /// Fire-and-forget and best-effort: no-op when the project isn't a nix
    /// flake (or `nix_develop` is off / nix is absent), and warms each repo at
    /// most once per process (see `prewarm_flake_ref`).
    pub(super) fn prewarm_nix_shell(&self, repo_path: &Path) {
        let enabled = self.config_store.read().nix_develop;
        let Some(flake_ref) =
            prewarm_flake_ref(enabled, repo_path, nix_available(), prewarmed_repos())
        else {
            return;
        };
        info!("Pre-warming nix dev shell at {}", repo_path.display());
        let dir = repo_path.to_path_buf();
        tokio::spawn(async move {
            let result = tokio::process::Command::new("nix")
                .current_dir(&dir)
                .args(["develop", flake_ref, "--command", "true"])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .output()
                .await;
            match result {
                Ok(output) if output.status.success() => {
                    debug!("Pre-warmed nix dev shell at {}", dir.display());
                }
                Ok(output) => {
                    warn!(
                        "nix develop pre-warm at {} exited with {}: {}",
                        dir.display(),
                        output.status,
                        String::from_utf8_lossy(&output.stderr).trim()
                    );
                }
                Err(e) => warn!(
                    "Failed to spawn nix develop pre-warm at {}: {}",
                    dir.display(),
                    e
                ),
            }
        });
    }
}

#[cfg(test)]
mod nix_tests {
    use super::*;

    #[test]
    fn wrap_simple_command() {
        assert_eq!(
            wrap_in_nix_develop("claude", "."),
            "nix develop . --command sh -c 'exec claude'"
        );
    }

    #[test]
    fn wrap_uses_given_flake_ref() {
        assert_eq!(
            wrap_in_nix_develop("claude", "./nix"),
            "nix develop ./nix --command sh -c 'exec claude'"
        );
    }

    #[test]
    fn wrap_escapes_single_quotes_in_command() {
        assert_eq!(
            wrap_in_nix_develop("claude -n 'my session' --resume", "."),
            "nix develop . --command sh -c 'exec claude -n '\\''my session'\\'' --resume'"
        );
    }

    #[test]
    fn flake_ref_requires_flake_file() {
        let dir = tempfile::TempDir::new().unwrap();
        assert_eq!(nix_develop_flake_ref(true, dir.path(), true), None);

        std::fs::write(dir.path().join("flake.nix"), "{}").unwrap();
        assert_eq!(nix_develop_flake_ref(true, dir.path(), true), Some("."));
    }

    #[test]
    fn flake_ref_finds_nix_subdirectory() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("nix")).unwrap();
        std::fs::write(dir.path().join("nix/flake.nix"), "{}").unwrap();
        assert_eq!(nix_develop_flake_ref(true, dir.path(), true), Some("./nix"));
    }

    #[test]
    fn flake_ref_prefers_root_flake_over_subdirectory() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("flake.nix"), "{}").unwrap();
        std::fs::create_dir(dir.path().join("nix")).unwrap();
        std::fs::write(dir.path().join("nix/flake.nix"), "{}").unwrap();
        assert_eq!(nix_develop_flake_ref(true, dir.path(), true), Some("."));
    }

    #[test]
    fn flake_ref_respects_config_and_nix_availability() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("flake.nix"), "{}").unwrap();

        assert_eq!(nix_develop_flake_ref(false, dir.path(), true), None);
        assert_eq!(nix_develop_flake_ref(true, dir.path(), false), None);
    }

    #[test]
    fn flake_ref_ignores_flake_directory() {
        // A directory named flake.nix is not a flake
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("flake.nix")).unwrap();
        assert_eq!(nix_develop_flake_ref(true, dir.path(), true), None);
    }

    #[test]
    fn prewarm_warms_flake_dir_once() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("flake.nix"), "{}").unwrap();
        let seen = Mutex::new(HashSet::new());

        // First call warms; second call for the same dir is deduped.
        assert_eq!(prewarm_flake_ref(true, dir.path(), true, &seen), Some("."));
        assert_eq!(prewarm_flake_ref(true, dir.path(), true, &seen), None);
    }

    #[test]
    fn prewarm_uses_nix_subdirectory_flake() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("nix")).unwrap();
        std::fs::write(dir.path().join("nix/flake.nix"), "{}").unwrap();
        let seen = Mutex::new(HashSet::new());

        assert_eq!(
            prewarm_flake_ref(true, dir.path(), true, &seen),
            Some("./nix")
        );
    }

    #[test]
    fn prewarm_respects_config_flake_and_nix() {
        let seen = Mutex::new(HashSet::new());

        // Not a flake.
        let plain = tempfile::TempDir::new().unwrap();
        assert_eq!(prewarm_flake_ref(true, plain.path(), true, &seen), None);

        let flake = tempfile::TempDir::new().unwrap();
        std::fs::write(flake.path().join("flake.nix"), "{}").unwrap();
        // Config disabled, or nix unavailable — no warm, and nothing recorded.
        assert_eq!(prewarm_flake_ref(false, flake.path(), true, &seen), None);
        assert_eq!(prewarm_flake_ref(true, flake.path(), false, &seen), None);
    }

    #[test]
    fn prewarm_tracks_dirs_independently() {
        let a = tempfile::TempDir::new().unwrap();
        let b = tempfile::TempDir::new().unwrap();
        std::fs::write(a.path().join("flake.nix"), "{}").unwrap();
        std::fs::write(b.path().join("flake.nix"), "{}").unwrap();
        let seen = Mutex::new(HashSet::new());

        assert!(prewarm_flake_ref(true, a.path(), true, &seen).is_some());
        // A different flake dir still warms once of its own.
        assert!(prewarm_flake_ref(true, b.path(), true, &seen).is_some());
        assert!(prewarm_flake_ref(true, a.path(), true, &seen).is_none());
        assert!(prewarm_flake_ref(true, b.path(), true, &seen).is_none());
    }

    #[test]
    fn nix_in_path_finds_executable() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("nix"), "").unwrap();
        let path_var = std::env::join_paths([dir.path()]).unwrap();
        assert!(nix_in_path(Some(path_var.as_os_str())));
    }

    #[test]
    fn nix_in_path_misses_when_absent() {
        let dir = tempfile::TempDir::new().unwrap();
        let path_var = std::env::join_paths([dir.path()]).unwrap();
        assert!(!nix_in_path(Some(path_var.as_os_str())));
        assert!(!nix_in_path(None));
    }
}
