//! Integration tests for claude-commander
//!
//! These tests require tmux to be installed and available.
//! All tests use isolated state files to avoid polluting user data.

use std::path::PathBuf;
use std::sync::Arc;

use tempfile::TempDir;

use claude_commander::SessionStatus;
use claude_commander::config::{AppState, Config, ConfigStore, StateStore};
use claude_commander::git::GitBackend;
use claude_commander::session::SessionManager;

/// Helper to create an isolated StateStore that won't pollute user data
fn create_isolated_store(temp_dir: &TempDir) -> Arc<StateStore> {
    let state_path = temp_dir.path().join("state.json");
    let state = AppState::load_from(&state_path).unwrap();
    Arc::new(StateStore::with_path(state, state_path))
}

/// Helper to create an isolated ConfigStore for testing
fn create_isolated_config_store(temp_dir: &TempDir, config: Config) -> Arc<ConfigStore> {
    let config_path = temp_dir.path().join("config.toml");
    let toml = toml::to_string_pretty(&config).unwrap();
    std::fs::write(&config_path, toml).unwrap();
    Arc::new(ConfigStore::with_path(config, config_path))
}

/// Helper to create a test git repository
async fn create_test_repo() -> (TempDir, PathBuf) {
    let temp_dir = TempDir::new().unwrap();
    let repo_path = temp_dir.path().to_path_buf();

    // Initialize git repo
    tokio::process::Command::new("git")
        .current_dir(&repo_path)
        .args(["init"])
        .output()
        .await
        .unwrap();

    // Configure git user for commits
    tokio::process::Command::new("git")
        .current_dir(&repo_path)
        .args(["config", "user.email", "test@test.com"])
        .output()
        .await
        .unwrap();

    tokio::process::Command::new("git")
        .current_dir(&repo_path)
        .args(["config", "user.name", "Test User"])
        .output()
        .await
        .unwrap();

    // Create initial commit
    let readme_path = repo_path.join("README.md");
    tokio::fs::write(&readme_path, "# Test Repository\n")
        .await
        .unwrap();

    tokio::process::Command::new("git")
        .current_dir(&repo_path)
        .args(["add", "README.md"])
        .output()
        .await
        .unwrap();

    tokio::process::Command::new("git")
        .current_dir(&repo_path)
        .args(["commit", "-m", "Initial commit"])
        .output()
        .await
        .unwrap();

    (temp_dir, repo_path)
}

/// Helper to check if tmux is available
async fn tmux_available() -> bool {
    tokio::process::Command::new("tmux")
        .arg("-V")
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[tokio::test]
async fn test_git_backend_open() {
    let (_temp_dir, repo_path) = create_test_repo().await;

    let backend = GitBackend::open(&repo_path);
    assert!(backend.is_ok(), "Should open git repository");

    let backend = backend.unwrap();
    assert!(!backend.repo_name().is_empty());
}

#[tokio::test]
async fn test_git_backend_discover() {
    let (_temp_dir, repo_path) = create_test_repo().await;

    // Create a subdirectory
    let subdir = repo_path.join("subdir");
    tokio::fs::create_dir_all(&subdir).await.unwrap();

    // Discover from subdirectory
    let backend = GitBackend::discover(&subdir);
    assert!(
        backend.is_ok(),
        "Should discover git repository from subdirectory"
    );
}

#[tokio::test]
async fn test_git_backend_branch_detection() {
    let (_temp_dir, repo_path) = create_test_repo().await;

    let backend = GitBackend::open(&repo_path).unwrap();

    // Should detect main branch (git init uses 'master' or 'main' depending on config)
    let branch = backend.current_branch();
    assert!(branch.is_ok(), "Should get current branch");

    let main_branch = backend.detect_main_branch();
    assert!(main_branch.is_ok(), "Should detect main branch");
}

#[tokio::test]
async fn test_session_manager_add_project() {
    if !tmux_available().await {
        eprintln!("Skipping test: tmux not available");
        return;
    }

    let (repo_temp_dir, repo_path) = create_test_repo().await;
    let state_temp_dir = TempDir::new().unwrap();

    let config = Config::default();
    let config_store = create_isolated_config_store(&state_temp_dir, config);
    let store = create_isolated_store(&state_temp_dir);
    let manager = SessionManager::new(config_store, store.clone(), "");

    // Add project
    let result = manager.add_project(repo_path.clone()).await;
    assert!(result.is_ok(), "Should add project: {:?}", result.err());

    let project_id = result.unwrap();

    // Verify project was added
    let state = store.read().await;
    assert!(state.get_project(&project_id).is_some());

    // Keep temp dirs alive until end of test
    drop(repo_temp_dir);
    drop(state_temp_dir);
}

#[tokio::test]
async fn test_session_manager_create_session() {
    if !tmux_available().await {
        eprintln!("Skipping test: tmux not available");
        return;
    }

    let (repo_temp_dir, repo_path) = create_test_repo().await;
    let state_temp_dir = TempDir::new().unwrap();

    // Create temp worktrees dir
    let worktrees_dir = TempDir::new().unwrap();
    let config = Config {
        worktrees_dir: Some(worktrees_dir.path().to_path_buf()),
        ..Config::default()
    };

    let config_store = create_isolated_config_store(&state_temp_dir, config);
    let store = create_isolated_store(&state_temp_dir);
    let manager = SessionManager::new(config_store, store.clone(), "");

    // Add project
    let project_id = manager.add_project(repo_path).await.unwrap();

    // Create session (prepare + finalize)
    let session_id = manager
        .prepare_session(
            &project_id,
            "test-session".to_string(),
            Some("bash".to_string()),
            None,
        )
        .await
        .expect("prepare_session should succeed");

    let result = manager.finalize_session(&session_id, None).await;

    if let Err(e) = &result {
        eprintln!("Error finalizing session: {}", e);
    }

    assert!(result.is_ok(), "Should finalize session");

    let session_id = result.unwrap();

    // Verify session was created
    {
        let state = store.read().await;
        let session = state.get_session(&session_id);
        assert!(session.is_some(), "Session should exist in state");

        let session = session.unwrap();
        assert_eq!(session.title, "test-session");
        assert_eq!(session.program, "bash");
    }

    // Cleanup: kill the tmux session
    let _ = manager.kill_session(&session_id, true).await;

    // Keep temp dirs alive until end of test
    drop(repo_temp_dir);
    drop(state_temp_dir);
    drop(worktrees_dir);
}

#[tokio::test]
async fn test_session_manager_restart() {
    if !tmux_available().await {
        eprintln!("Skipping test: tmux not available");
        return;
    }

    let (repo_temp_dir, repo_path) = create_test_repo().await;
    let state_temp_dir = TempDir::new().unwrap();

    let worktrees_dir = TempDir::new().unwrap();
    let config = Config {
        worktrees_dir: Some(worktrees_dir.path().to_path_buf()),
        ..Config::default()
    };

    let store = create_isolated_store(&state_temp_dir);
    let config_store = Arc::new(ConfigStore::new(config).unwrap());
    let manager = SessionManager::new(config_store, store.clone(), "");

    // Add project and create session (prepare + finalize)
    let project_id = manager.add_project(repo_path).await.unwrap();
    let session_id = manager
        .prepare_session(
            &project_id,
            "restart-test".to_string(),
            Some("bash".to_string()),
            None,
        )
        .await
        .unwrap();
    manager.finalize_session(&session_id, None).await.unwrap();

    // Verify initial status is Running
    {
        let state = store.read().await;
        let session = state.get_session(&session_id).unwrap();
        assert_eq!(session.status, SessionStatus::Running);
    }

    // Restart from Running state
    manager
        .restart_session(&session_id)
        .await
        .expect("Should restart running session");

    // Verify still Running after restart
    {
        let state = store.read().await;
        let session = state.get_session(&session_id).unwrap();
        assert_eq!(session.status, SessionStatus::Running);
    }

    // Kill (-> Stopped), then restart from Stopped state
    manager.kill_session(&session_id, false).await.unwrap();
    {
        let state = store.read().await;
        let session = state.get_session(&session_id).unwrap();
        assert_eq!(session.status, SessionStatus::Stopped);
    }

    manager
        .restart_session(&session_id)
        .await
        .expect("Should restart stopped session");

    {
        let state = store.read().await;
        let session = state.get_session(&session_id).unwrap();
        assert_eq!(session.status, SessionStatus::Running);
    }

    // Cleanup
    let _ = manager.kill_session(&session_id, true).await;

    drop(repo_temp_dir);
    drop(state_temp_dir);
    drop(worktrees_dir);
}

#[tokio::test]
async fn test_state_persistence() {
    let temp_dir = TempDir::new().unwrap();
    let state_path = temp_dir.path().join("state.json");

    // Create and save state
    {
        let mut state = AppState::new();
        let project =
            claude_commander::Project::new("test-project", PathBuf::from("/tmp/test"), "main");
        state.add_project(project);
        state.save_to(&state_path).unwrap();
    }

    // Load and verify
    {
        let state = AppState::load_from(&state_path).unwrap();
        assert_eq!(state.project_count(), 1);
    }
}

#[tokio::test]
async fn test_config_defaults() {
    let config = Config::default();

    assert_eq!(config.default_program, "claude");
    assert_eq!(config.branch_prefix, "");
    assert_eq!(config.max_concurrent_tmux, 16);
    assert_eq!(config.capture_cache_ttl_ms, 50);
    assert_eq!(config.diff_cache_ttl_ms, 500);
    assert_eq!(config.ui_refresh_fps, 30);
}

#[tokio::test]
async fn test_sync_worktrees_imports_external() {
    if !tmux_available().await {
        eprintln!("Skipping test: tmux not available");
        return;
    }

    let (repo_temp_dir, repo_path) = create_test_repo().await;
    let state_temp_dir = TempDir::new().unwrap();

    let worktrees_dir = TempDir::new().unwrap();
    let config = Config {
        worktrees_dir: Some(worktrees_dir.path().to_path_buf()),
        ..Config::default()
    };

    let config_store = create_isolated_config_store(&state_temp_dir, config);
    let store = create_isolated_store(&state_temp_dir);
    let manager = SessionManager::new(config_store, store.clone(), "");

    // Add project (no worktrees yet)
    let project_id = manager.add_project(repo_path.clone()).await.unwrap();

    // Verify no sessions were imported (no external worktrees exist)
    {
        let st = store.read().await;
        let project = st.get_project(&project_id).unwrap();
        assert_eq!(project.worktrees.len(), 0, "No sessions should exist yet");
    }

    // Create an external worktree via git CLI (simulating Claude Code /worktree or manual creation)
    let external_wt_path = worktrees_dir.path().join("external-feature");
    let output = tokio::process::Command::new("git")
        .current_dir(&repo_path)
        .args([
            "worktree",
            "add",
            "-b",
            "external-feature",
            external_wt_path.to_str().unwrap(),
        ])
        .output()
        .await
        .unwrap();
    assert!(output.status.success(), "git worktree add should succeed");

    // Run sync_worktrees - should import the external worktree
    let imported = manager.sync_worktrees(&project_id).await.unwrap();
    assert_eq!(imported, 1, "Should import 1 external worktree");

    // Verify the imported session
    {
        let st = store.read().await;
        let project = st.get_project(&project_id).unwrap();
        assert_eq!(project.worktrees.len(), 1, "Should have 1 session");

        let session = st.get_session(&project.worktrees[0]).unwrap();
        assert_eq!(session.branch, "external-feature");
        assert_eq!(session.status, SessionStatus::Stopped);
        assert!(session.base_commit.is_some());
    }

    // Run sync again - should be idempotent
    let imported_again = manager.sync_worktrees(&project_id).await.unwrap();
    assert_eq!(
        imported_again, 0,
        "Second sync should import 0 (idempotent)"
    );

    // Keep temp dirs alive
    drop(repo_temp_dir);
    drop(state_temp_dir);
    drop(worktrees_dir);
}

/// Helper to create a bare repo as "origin" and a working repo with that remote configured.
async fn create_test_repo_with_remote() -> (TempDir, PathBuf, TempDir, PathBuf) {
    // Create bare "origin" repo
    let bare_dir = TempDir::new().unwrap();
    let bare_path = bare_dir.path().to_path_buf();

    tokio::process::Command::new("git")
        .current_dir(&bare_path)
        .args(["init", "--bare"])
        .output()
        .await
        .unwrap();

    // Create working repo
    let work_dir = TempDir::new().unwrap();
    let work_path = work_dir.path().to_path_buf();

    tokio::process::Command::new("git")
        .current_dir(&work_path)
        .args(["init"])
        .output()
        .await
        .unwrap();

    // Configure git user
    for args in [
        vec!["config", "user.email", "test@test.com"],
        vec!["config", "user.name", "Test User"],
    ] {
        tokio::process::Command::new("git")
            .current_dir(&work_path)
            .args(&args)
            .output()
            .await
            .unwrap();
    }

    // Add remote
    tokio::process::Command::new("git")
        .current_dir(&work_path)
        .args(["remote", "add", "origin", bare_path.to_str().unwrap()])
        .output()
        .await
        .unwrap();

    // Create initial commit and push
    tokio::fs::write(work_path.join("README.md"), "# Test\n")
        .await
        .unwrap();

    tokio::process::Command::new("git")
        .current_dir(&work_path)
        .args(["add", "README.md"])
        .output()
        .await
        .unwrap();

    tokio::process::Command::new("git")
        .current_dir(&work_path)
        .args(["commit", "-m", "Initial commit"])
        .output()
        .await
        .unwrap();

    tokio::process::Command::new("git")
        .current_dir(&work_path)
        .args(["push", "-u", "origin", "HEAD"])
        .output()
        .await
        .unwrap();

    (bare_dir, bare_path, work_dir, work_path)
}

#[tokio::test]
async fn test_detect_main_branch_with_remote() {
    let (_bare_dir, _bare_path, _work_dir, work_path) = create_test_repo_with_remote().await;

    // Set origin/HEAD so remote_default_branch() can resolve it
    tokio::process::Command::new("git")
        .current_dir(&work_path)
        .args(["remote", "set-head", "origin", "--auto"])
        .output()
        .await
        .unwrap();

    let backend = GitBackend::open(&work_path).unwrap();
    let main = backend.detect_main_branch().unwrap();

    // The default branch should be whatever the working repo's HEAD is
    let current = backend.current_branch().unwrap();
    assert_eq!(main, current);
}

#[tokio::test]
async fn test_create_session_no_remote_falls_back() {
    if !tmux_available().await {
        eprintln!("Skipping test: tmux not available");
        return;
    }

    // Repo with no remote — fetch_before_create: true should still succeed
    let (repo_temp_dir, repo_path) = create_test_repo().await;
    let state_temp_dir = TempDir::new().unwrap();
    let worktrees_dir = TempDir::new().unwrap();

    let config = Config {
        worktrees_dir: Some(worktrees_dir.path().to_path_buf()),
        fetch_before_create: true,
        ..Config::default()
    };

    let config_store = create_isolated_config_store(&state_temp_dir, config);
    let store = create_isolated_store(&state_temp_dir);
    let manager = SessionManager::new(config_store, store.clone(), "");

    let project_id = manager.add_project(repo_path).await.unwrap();

    let session_id = manager
        .prepare_session(
            &project_id,
            "fallback-test".to_string(),
            Some("bash".to_string()),
            None,
        )
        .await
        .expect("prepare_session should succeed");

    let result = manager.finalize_session(&session_id, None).await;

    assert!(
        result.is_ok(),
        "Session finalization should succeed without remote: {:?}",
        result.err()
    );

    let session_id = result.unwrap();
    let _ = manager.kill_session(&session_id, true).await;

    drop(repo_temp_dir);
    drop(state_temp_dir);
    drop(worktrees_dir);
}

/// When `--base-branch` matches an existing session's branch in the same
/// project, the new session should be linked as stacked via
/// `stack_parent_session_id`. This mirrors the TUI's stacked-create flow.
#[tokio::test]
async fn test_base_branch_links_stack_parent_when_session_matches() {
    if !tmux_available().await {
        eprintln!("Skipping test: tmux not available");
        return;
    }

    let (repo_temp_dir, repo_path) = create_test_repo().await;
    let state_temp_dir = TempDir::new().unwrap();
    let worktrees_dir = TempDir::new().unwrap();
    let config = Config {
        worktrees_dir: Some(worktrees_dir.path().to_path_buf()),
        ..Config::default()
    };

    let config_store = create_isolated_config_store(&state_temp_dir, config);
    let store = create_isolated_store(&state_temp_dir);
    let manager = SessionManager::new(config_store, store.clone(), "");

    let project_id = manager.add_project(repo_path).await.unwrap();

    // Create parent session
    let parent_id = manager
        .prepare_session(
            &project_id,
            "parent-session".to_string(),
            Some("bash".to_string()),
            None,
        )
        .await
        .unwrap();
    manager.finalize_session(&parent_id, None).await.unwrap();

    let parent_branch = {
        let state = store.read().await;
        state.get_session(&parent_id).unwrap().branch.clone()
    };

    // Create child session and link it to the parent via branch name
    let child_id = manager
        .prepare_session(
            &project_id,
            "child-session".to_string(),
            Some("bash".to_string()),
            None,
        )
        .await
        .unwrap();
    manager
        .link_stack_parent_by_branch(&child_id, Some(&parent_branch))
        .await
        .unwrap();
    manager.finalize_session(&child_id, None).await.unwrap();

    // Verify the child is linked to the parent
    {
        let state = store.read().await;
        let child = state.get_session(&child_id).unwrap();
        assert_eq!(
            child.stack_parent_session_id,
            Some(parent_id),
            "child session should be linked to parent via stack_parent_session_id"
        );
    }

    let _ = manager.kill_session(&child_id, true).await;
    let _ = manager.kill_session(&parent_id, true).await;
    drop(repo_temp_dir);
    drop(state_temp_dir);
    drop(worktrees_dir);
}

/// When `--base-branch` doesn't match any existing session's branch,
/// `stack_parent_session_id` should remain None.
#[tokio::test]
async fn test_base_branch_no_link_when_no_session_matches() {
    if !tmux_available().await {
        eprintln!("Skipping test: tmux not available");
        return;
    }

    let (repo_temp_dir, repo_path) = create_test_repo().await;
    let state_temp_dir = TempDir::new().unwrap();
    let worktrees_dir = TempDir::new().unwrap();
    let config = Config {
        worktrees_dir: Some(worktrees_dir.path().to_path_buf()),
        ..Config::default()
    };

    let config_store = create_isolated_config_store(&state_temp_dir, config);
    let store = create_isolated_store(&state_temp_dir);
    let manager = SessionManager::new(config_store, store.clone(), "");

    let project_id = manager.add_project(repo_path).await.unwrap();

    // Create session with base_branch that doesn't match any session
    let session_id = manager
        .prepare_session(
            &project_id,
            "standalone-session".to_string(),
            Some("bash".to_string()),
            Some("develop".to_string()),
        )
        .await
        .unwrap();

    // Link attempt — should be a no-op since no session has branch "develop"
    manager
        .link_stack_parent_by_branch(&session_id, Some("develop"))
        .await
        .unwrap();
    manager.finalize_session(&session_id, None).await.unwrap();

    // Verify no stack link
    {
        let state = store.read().await;
        let session = state.get_session(&session_id).unwrap();
        assert_eq!(
            session.stack_parent_session_id, None,
            "session should not be linked when base_branch doesn't match any session"
        );
    }

    let _ = manager.kill_session(&session_id, true).await;
    drop(repo_temp_dir);
    drop(state_temp_dir);
    drop(worktrees_dir);
}

/// When `--base-branch` matches a session's branch, the child must get its
/// own branch (not the parent's) to avoid git rejecting a second worktree on
/// the same branch. This replicates the full main.rs flow: detect match →
/// withhold base_branch from prepare_session → link → finalize.
#[tokio::test]
async fn test_stacked_session_gets_own_branch_not_parents() {
    if !tmux_available().await {
        eprintln!("Skipping test: tmux not available");
        return;
    }

    let (repo_temp_dir, repo_path) = create_test_repo().await;
    let state_temp_dir = TempDir::new().unwrap();
    let worktrees_dir = TempDir::new().unwrap();
    let config = Config {
        worktrees_dir: Some(worktrees_dir.path().to_path_buf()),
        ..Config::default()
    };

    let config_store = create_isolated_config_store(&state_temp_dir, config);
    let store = create_isolated_store(&state_temp_dir);
    let manager = SessionManager::new(config_store, store.clone(), "");

    let project_id = manager.add_project(repo_path).await.unwrap();

    // Create parent session (gets branch "parent-session")
    let parent_id = manager
        .prepare_session(
            &project_id,
            "parent-session".to_string(),
            Some("bash".to_string()),
            None,
        )
        .await
        .unwrap();
    manager.finalize_session(&parent_id, None).await.unwrap();

    let parent_branch = {
        let state = store.read().await;
        state.get_session(&parent_id).unwrap().branch.clone()
    };

    // Replicate main.rs logic: detect that base_branch matches a session,
    // so withhold it from prepare_session (child gets own branch from title)
    let base_branch = Some(parent_branch.clone());
    let is_stacked = {
        let state = store.read().await;
        base_branch.as_ref().is_some_and(|base| {
            state
                .sessions
                .values()
                .any(|s| s.project_id == project_id && s.branch == *base)
        })
    };
    assert!(is_stacked, "base_branch should match parent session");
    let branch_for_prepare = if is_stacked {
        None
    } else {
        base_branch.clone()
    };

    let child_id = manager
        .prepare_session(
            &project_id,
            "child-session".to_string(),
            Some("bash".to_string()),
            branch_for_prepare,
        )
        .await
        .unwrap();
    manager
        .link_stack_parent_by_branch(&child_id, base_branch.as_deref())
        .await
        .unwrap();

    // This would fail with "branch already used by worktree" if we had
    // passed the parent's branch to prepare_session
    manager.finalize_session(&child_id, None).await.unwrap();

    // Verify child has its own branch, not the parent's
    {
        let state = store.read().await;
        let child = state.get_session(&child_id).unwrap();
        let parent = state.get_session(&parent_id).unwrap();
        assert_ne!(
            child.branch, parent.branch,
            "child should have its own branch, not the parent's"
        );
        assert_eq!(
            child.stack_parent_session_id,
            Some(parent_id),
            "child should be linked to parent"
        );
    }

    let _ = manager.kill_session(&child_id, true).await;
    let _ = manager.kill_session(&parent_id, true).await;
    drop(repo_temp_dir);
    drop(state_temp_dir);
    drop(worktrees_dir);
}
