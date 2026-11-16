use super::*;
use crate::config::{AppState, Config, ConfigStore, StateStore};
use crate::git::fixture::fixture_git_std;
use claude_commander_protocol::github::canonical_repo_slug;
use tempfile::TempDir;

fn test_store() -> (TempDir, Arc<StateStore>) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("state.json");
    let store = Arc::new(StateStore::with_path(AppState::new(), path));
    (dir, store)
}

fn test_config_store(mut config: Config) -> (TempDir, Arc<ConfigStore>) {
    let dir = TempDir::new().unwrap();
    // `projects_dir` defaults to the user's REAL `~/Projects`, which the
    // repo-clone paths write into. Pin it under `dir` — applied after the
    // caller's config arrives, so an explicitly-passed `Config` can't lose it.
    config.projects_dir = Some(dir.path().join("projects"));
    let path = dir.path().join("config.toml");
    let toml = toml::to_string_pretty(&config).unwrap();
    std::fs::write(&path, toml).unwrap();
    let store = Arc::new(ConfigStore::with_path(config, path));
    (dir, store)
}

#[test]
fn test_sanitize_name() {
    let (_cdir, config_store) = test_config_store(Config::default());
    let (_dir, store) = test_store();
    let manager = SessionManager::new(config_store, store, "");

    assert_eq!(manager.sanitize_name("Hello World"), "hello-world");
    assert_eq!(manager.sanitize_name("Feature/Auth"), "feature-auth");
    assert_eq!(manager.sanitize_name("--test--"), "test");
}

#[test]
fn test_generate_branch_name() {
    let (_cdir, config_store) = test_config_store(Config::default());
    let (_dir, store) = test_store();

    // Without prefix
    let manager = SessionManager::new(config_store, store.clone(), "");
    assert_eq!(manager.generate_branch_name("Feature Auth"), "feature-auth");

    // With prefix
    let config = Config {
        branch_prefix: "cc".to_string(),
        ..Config::default()
    };
    let (_cdir2, config_store2) = test_config_store(config);
    let manager = SessionManager::new(config_store2, store, "");
    assert_eq!(
        manager.generate_branch_name("Feature Auth"),
        "cc/feature-auth"
    );
}

#[test]
fn test_sanitize_name_underscores_preserved() {
    let (_cdir, config_store) = test_config_store(Config::default());
    let (_dir, store) = test_store();
    let manager = SessionManager::new(config_store, store, "");

    assert_eq!(manager.sanitize_name("hello_world"), "hello_world");
}

#[test]
fn test_sanitize_name_consecutive_specials() {
    let (_cdir, config_store) = test_config_store(Config::default());
    let (_dir, store) = test_store();
    let manager = SessionManager::new(config_store, store, "");

    assert_eq!(manager.sanitize_name("a!!b"), "a--b");
}

#[test]
fn test_sanitize_name_all_special() {
    let (_cdir, config_store) = test_config_store(Config::default());
    let (_dir, store) = test_store();
    let manager = SessionManager::new(config_store, store, "");

    assert_eq!(manager.sanitize_name("!!!"), "");
}

#[test]
fn test_sanitize_name_unicode() {
    let (_cdir, config_store) = test_config_store(Config::default());
    let (_dir, store) = test_store();
    let manager = SessionManager::new(config_store, store, "");

    // Unicode alphanumeric chars should be preserved
    let result = manager.sanitize_name("café");
    assert!(result.contains("caf"));
    assert!(result.contains('é'));
}

#[test]
fn test_display_branch_hides_exact_sanitized_match() {
    assert_eq!(display_branch("Feature Auth", "feature-auth"), None);
}

#[test]
fn test_display_branch_hides_when_sanitization_changes_specials() {
    // dot replaced by hyphen — still considered the deterministic sanitization
    assert_eq!(display_branch("Fix bug v2.0", "fix-bug-v2-0"), None);
}

#[test]
fn test_display_branch_hides_when_prefixed() {
    assert_eq!(display_branch("Feature Auth", "user/feature-auth"), None);
    assert_eq!(display_branch("Feature Auth", "cc/feature-auth"), None);
}

#[test]
fn test_display_branch_shows_when_branch_renamed() {
    assert_eq!(
        display_branch("Feature Auth", "something-else"),
        Some("something-else")
    );
}

#[test]
fn test_display_branch_shows_when_suffix_differs() {
    assert_eq!(
        display_branch("Feature Auth", "feature-auth-v2"),
        Some("feature-auth-v2")
    );
}

#[test]
fn test_display_branch_shows_when_title_sanitizes_to_empty() {
    // All-special title sanitizes to "" — we can't meaningfully compare,
    // so always show the branch.
    assert_eq!(display_branch("!!!", "fallback"), Some("fallback"));
}

#[test]
fn test_display_branch_shows_when_prefix_segment_doesnt_match() {
    // Branch has a slash but the tail doesn't match the sanitized title
    assert_eq!(
        display_branch("Feature Auth", "user/something-else"),
        Some("user/something-else")
    );
}

#[test]
fn test_match_existing_branch_local_hit() {
    let existing = vec!["feature-auth".to_string(), "main".to_string()];
    assert_eq!(
        match_existing_branch("Feature Auth", "", &existing),
        Some("feature-auth")
    );
}

#[test]
fn test_match_existing_branch_with_prefix() {
    let existing = vec!["cc/feature-auth".to_string()];
    assert_eq!(
        match_existing_branch("Feature Auth", "cc", &existing),
        Some("cc/feature-auth")
    );
    // Without the prefix configured, the bare candidate doesn't match the
    // prefixed entry.
    assert_eq!(match_existing_branch("Feature Auth", "", &existing), None);
}

#[test]
fn test_match_existing_branch_no_match() {
    let existing = vec!["main".to_string(), "develop".to_string()];
    assert_eq!(match_existing_branch("brand-new", "", &existing), None);
}

#[test]
fn test_match_existing_branch_empty_value() {
    let existing = vec!["main".to_string()];
    assert_eq!(match_existing_branch("", "", &existing), None);
    // All-special title sanitizes to empty — no spurious match against an
    // empty-string entry either.
    assert_eq!(match_existing_branch("!!!", "", &existing), None);
}

#[test]
fn test_display_branch_hides_when_title_equals_branch() {
    // Checkout flow sets title == branch verbatim — no annotation even
    // if the branch contains characters sanitize_name() would rewrite.
    assert_eq!(display_branch("Feature-Auth", "Feature-Auth"), None);
    assert_eq!(display_branch("fix.bug.v2", "fix.bug.v2"), None);
    assert_eq!(display_branch("user/JIRA-123", "user/JIRA-123"), None);
}

#[tokio::test]
async fn test_remove_creating_session_clears_session() {
    let (_cdir, config_store) = test_config_store(Config::default());
    let (_dir, store) = test_store();
    let manager = SessionManager::new(config_store, store.clone(), "");

    // Seed a session stuck in `Creating` state, as `prepare_session` would
    // leave it if a later step (link/finalize) fails.
    let session = WorktreeSession::new_creating(ProjectId::new(), "Zombie", "zombie", "claude");
    let session_id = session.id;
    store
        .mutate(move |state| state.add_session(session))
        .await
        .unwrap();
    assert!(store.read().await.get_session(&session_id).is_some());

    // Cleanup on the CLI failure path must remove the zombie record.
    manager.remove_creating_session(&session_id).await.unwrap();
    assert!(store.read().await.get_session(&session_id).is_none());
}

#[tokio::test]
async fn test_toggle_keep_alive_flips_and_returns_new_value() {
    let (_cdir, config_store) = test_config_store(Config::default());
    let (_dir, store) = test_store();
    let manager = SessionManager::new(config_store, store.clone(), "");

    let session = WorktreeSession::new(
        ProjectId::new(),
        "Keep",
        "keep",
        std::path::PathBuf::from("/tmp/wt"),
        "claude",
    );
    let session_id = session.id;
    store
        .mutate(move |state| state.add_session(session))
        .await
        .unwrap();

    // Defaults off; first toggle turns it on, second turns it off.
    assert!(manager.toggle_keep_alive(&session_id).await.unwrap());
    assert!(
        store
            .read()
            .await
            .get_session(&session_id)
            .unwrap()
            .keep_alive
    );
    assert!(!manager.toggle_keep_alive(&session_id).await.unwrap());
    assert!(
        !store
            .read()
            .await
            .get_session(&session_id)
            .unwrap()
            .keep_alive
    );
}

#[tokio::test]
async fn test_set_keep_alive_sets_explicit_value() {
    let (_cdir, config_store) = test_config_store(Config::default());
    let (_dir, store) = test_store();
    let manager = SessionManager::new(config_store, store.clone(), "");

    let session = WorktreeSession::new(
        ProjectId::new(),
        "Keep",
        "keep",
        std::path::PathBuf::from("/tmp/wt"),
        "claude",
    );
    let session_id = session.id;
    store
        .mutate(move |state| state.add_session(session))
        .await
        .unwrap();

    manager.set_keep_alive(&session_id, true).await.unwrap();
    assert!(
        store
            .read()
            .await
            .get_session(&session_id)
            .unwrap()
            .keep_alive
    );
    manager.set_keep_alive(&session_id, false).await.unwrap();
    assert!(
        !store
            .read()
            .await
            .get_session(&session_id)
            .unwrap()
            .keep_alive
    );
}

#[tokio::test]
async fn test_toggle_keep_alive_missing_session_errors() {
    let (_cdir, config_store) = test_config_store(Config::default());
    let (_dir, store) = test_store();
    let manager = SessionManager::new(config_store, store, "");

    let missing = SessionId::new();
    assert!(manager.toggle_keep_alive(&missing).await.is_err());
}

#[tokio::test]
async fn test_set_keep_alive_missing_session_errors() {
    let (_cdir, config_store) = test_config_store(Config::default());
    let (_dir, store) = test_store();
    let manager = SessionManager::new(config_store, store, "");

    // Setting keep-alive on an absent session must not report success — it
    // returns NotFound so a CLI/TUI caller can't print "keep-alive on" for a
    // session that was never modified.
    let missing = SessionId::new();
    assert!(manager.set_keep_alive(&missing, true).await.is_err());
}

#[test]
fn test_generate_branch_name_empty_prefix() {
    let (_cdir, config_store) = test_config_store(Config::default());
    let (_dir, store) = test_store();
    let manager = SessionManager::new(config_store, store, "");

    assert_eq!(manager.generate_branch_name("Foo Bar"), "foo-bar");
}

#[test]
fn test_generate_branch_name_slash_in_prefix() {
    let config = Config {
        branch_prefix: "user/cc".to_string(),
        ..Config::default()
    };
    let (_cdir, config_store) = test_config_store(config);
    let (_dir, store) = test_store();
    let manager = SessionManager::new(config_store, store, "");

    assert_eq!(manager.generate_branch_name("Foo"), "user/cc/foo");
}

#[tokio::test]
async fn delete_session_mutates_state_once_removal_first() {
    // Regression: delete must remove the session from state *before* the slow
    // tmux/worktree teardown, so the tree row disappears immediately rather than
    // lingering until `git worktree remove` finishes. Observable via the store
    // generation counter: the fix mutates state exactly once (the removal),
    // whereas the old kill-first path mutated twice (a `Stopped` transition
    // inside `kill_session`, then the removal).
    let (_dirs, manager, _pid, sid) = manager_with_watched_session().await;
    let store = manager.store.clone();

    let gen_before = *store.subscribe().borrow();
    manager.delete_session(&sid).await.unwrap();
    let gen_after = *store.subscribe().borrow();

    assert_eq!(
        gen_after - gen_before,
        1,
        "delete must mutate state once (removal), not transition through Stopped first"
    );
    assert!(store.read().await.get_session(&sid).is_none());
}

/// A manager whose tmux teardown is isolated, plus a seeded project and
/// session whose worktree and repo paths exist (so a diff watch can be held)
/// but are not repositories (so worktree removal is a no-op).
async fn manager_with_watched_session() -> (Vec<TempDir>, SessionManager, ProjectId, SessionId) {
    let mut config = Config::default();
    config.telemetry.enabled = false;
    let tmux_tmpdir = TempDir::new().unwrap();
    config.tmux_tmpdir = Some(tmux_tmpdir.path().to_path_buf());
    let (cdir, config_store) = test_config_store(config);
    let (dir, store) = test_store();
    let manager = SessionManager::new(config_store, store.clone(), "");
    let repo = TempDir::new().unwrap();
    let worktree = TempDir::new().unwrap();
    let project = Project::new("repo", repo.path().to_path_buf(), "main");
    let pid = project.id;
    let session =
        WorktreeSession::new(pid, "task", "task", worktree.path().to_path_buf(), "claude");
    let sid = session.id;
    store
        .mutate(move |state| {
            state.add_project(project);
            state.add_session(session);
        })
        .await
        .unwrap();
    manager
        .diff_cache
        .watch_for_test(sid, worktree.path())
        .await;
    manager
        .project_diff_cache
        .watch_for_test(pid, repo.path())
        .await;
    assert!(manager.diff_cache.is_watching(&sid));
    assert!(manager.project_diff_cache.is_watching(&pid));
    (
        vec![tmux_tmpdir, cdir, dir, repo, worktree],
        manager,
        pid,
        sid,
    )
}

#[tokio::test]
async fn delete_session_releases_its_diff_watcher() {
    let (_dirs, manager, _pid, sid) = manager_with_watched_session().await;
    manager.delete_session(&sid).await.unwrap();
    assert!(!manager.diff_cache.is_watching(&sid));
}

#[tokio::test]
async fn remove_creating_session_releases_its_diff_watcher() {
    let (_dirs, manager, _pid, sid) = manager_with_watched_session().await;
    manager.remove_creating_session(&sid).await.unwrap();
    assert!(!manager.diff_cache.is_watching(&sid));
}

#[tokio::test]
async fn remove_project_releases_project_and_session_diff_watchers() {
    let (_dirs, manager, pid, sid) = manager_with_watched_session().await;
    manager.remove_project(&pid).await.unwrap();
    assert!(!manager.project_diff_cache.is_watching(&pid));
    assert!(!manager.diff_cache.is_watching(&sid));
}

// -- `Project.origin_url` capture and backfill --

/// Run a git command in `dir`, panicking with its output on failure.
fn git(dir: &std::path::Path, args: &[&str]) {
    let out = fixture_git_std()
        .current_dir(dir)
        .args(args)
        .output()
        .expect("git invocation failed to spawn");
    assert!(
        out.status.success(),
        "git {:?} failed in {}:\nstdout: {}\nstderr: {}",
        args,
        dir.display(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Give a fresh repo the local identity the test harness needs. (Signing is
/// already off: [`git`] runs through [`fixture_git_std`].)
fn git_identity(dir: &std::path::Path) {
    git(dir, &["config", "user.email", "t@t"]);
    git(dir, &["config", "user.name", "t"]);
}

/// A bare "remote" plus a clone of it, both inside one `TempDir`. Mirrors the
/// fixture shape in `git/auto_pull.rs` — the clone source is a plain local
/// path, so nothing here touches the network.
///
/// Returns `(tmp, remote_path, local_clone_path)`.
fn repo_with_remote() -> (TempDir, PathBuf, PathBuf) {
    let tmp = TempDir::new().expect("tempdir");
    let remote = tmp.path().join("remote.git");
    let seed = tmp.path().join("seed");
    let local = tmp.path().join("local");

    git(tmp.path(), &["init", "--bare", "-b", "main", "remote.git"]);

    git(tmp.path(), &["init", "-b", "main", "seed"]);
    git_identity(&seed);
    std::fs::write(seed.join("README"), "v1\n").unwrap();
    git(&seed, &["add", "README"]);
    git(&seed, &["commit", "-m", "initial"]);
    git(
        &seed,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    git(&seed, &["push", "origin", "main"]);

    git(
        tmp.path(),
        &["clone", remote.to_str().unwrap(), local.to_str().unwrap()],
    );
    git_identity(&local);

    (tmp, remote, local)
}

/// A `SessionManager` whose worktrees dir and projects dir are both pinned
/// inside `tmp`, so nothing it does can reach the developer's real trees.
fn manager_for(tmp: &TempDir) -> (TempDir, TempDir, Arc<StateStore>, SessionManager) {
    let mut config = Config::default();
    config.telemetry.enabled = false;
    config.worktrees_dir = Some(tmp.path().join("worktrees"));
    let (cdir, config_store) = test_config_store(config);
    let (sdir, store) = test_store();
    let manager = SessionManager::new(config_store, store.clone(), "");
    (cdir, sdir, store, manager)
}

#[tokio::test]
async fn add_project_records_origin_url() {
    let (tmp, remote, local) = repo_with_remote();
    let (_cdir, _sdir, store, manager) = manager_for(&tmp);

    let id = manager.add_project(local, None).await.unwrap();

    let state = store.read().await;
    let origin = state
        .get_project(&id)
        .unwrap()
        .origin_url
        .as_deref()
        .expect("origin url recorded at add time");
    // The fixture's remote is a local path, so this is the strong assertion:
    // the recorded origin resolves to the very repo the clone came from.
    assert_eq!(
        std::fs::canonicalize(origin).unwrap(),
        std::fs::canonicalize(&remote).unwrap()
    );
    // …and it agrees with the remote under the identity the repo picker will
    // actually match on (both `None` here — a local path has no GitHub slug).
    assert_eq!(
        canonical_repo_slug(origin),
        canonical_repo_slug(remote.to_str().unwrap())
    );
}

#[tokio::test]
async fn add_project_records_origin_url_matching_a_github_remote_by_slug() {
    // The picker badges a row when a project's stored origin and the API's
    // `clone_url` canonicalise to the same slug. Pin that end to end with a
    // real GitHub-shaped remote (never contacted — nothing fetches).
    let tmp = TempDir::new().unwrap();
    let local = tmp.path().join("repo");
    git(tmp.path(), &["init", "-b", "main", "repo"]);
    git_identity(&local);
    std::fs::write(local.join("f"), "x\n").unwrap();
    git(&local, &["add", "f"]);
    git(&local, &["commit", "-m", "c"]);
    git(
        &local,
        &[
            "remote",
            "add",
            "origin",
            "git@github.com:sizeak/claude-commander.git",
        ],
    );

    let (_cdir, _sdir, store, manager) = manager_for(&tmp);
    let id = manager.add_project(local, None).await.unwrap();

    let state = store.read().await;
    let origin = state.get_project(&id).unwrap().origin_url.clone().unwrap();
    assert_eq!(
        canonical_repo_slug(&origin),
        canonical_repo_slug("https://github.com/SizeAk/Claude-Commander"),
        "ssh origin must match the API's https clone_url"
    );
}

#[tokio::test]
async fn add_project_without_a_remote_records_no_origin_url() {
    // A repo with no `origin` is a valid resting state, not a retry loop.
    let tmp = TempDir::new().unwrap();
    let local = tmp.path().join("solo");
    git(tmp.path(), &["init", "-b", "main", "solo"]);
    git_identity(&local);
    std::fs::write(local.join("f"), "x\n").unwrap();
    git(&local, &["add", "f"]);
    git(&local, &["commit", "-m", "c"]);

    let (_cdir, _sdir, store, manager) = manager_for(&tmp);
    let id = manager.add_project(local, None).await.unwrap();

    assert_eq!(
        store.read().await.get_project(&id).unwrap().origin_url,
        None
    );
    // Backfill must tolerate it too, and still leave `None` — *without*
    // rewriting state on every pass. The store generation counter is the
    // observable: a repo that genuinely has no `origin` must settle, not churn
    // a `state.json` write (and a redraw) on every sync.
    let gen_before = *store.subscribe().borrow();
    manager.sync_worktrees(&id).await.unwrap();
    manager.sync_worktrees(&id).await.unwrap();
    assert_eq!(
        store.read().await.get_project(&id).unwrap().origin_url,
        None
    );
    assert_eq!(
        *store.subscribe().borrow(),
        gen_before,
        "a repo with no origin must settle at None without writing state"
    );
}

#[tokio::test]
async fn backfill_updates_origin_url_when_the_remote_is_repointed() {
    // Repos get renamed, transferred between owners, and switched between ssh
    // and https. A stored origin that never refreshes would silently mis-badge
    // the repo in the picker this field exists to serve, so the repair has to
    // *correct* a stale value, not only fill an absent one.
    let (tmp, _remote, local) = repo_with_remote();
    let (_cdir, _sdir, store, manager) = manager_for(&tmp);
    let id = manager.add_project(local.clone(), None).await.unwrap();
    assert!(
        store
            .read()
            .await
            .get_project(&id)
            .unwrap()
            .origin_url
            .is_some()
    );

    git(
        &local,
        &[
            "remote",
            "set-url",
            "origin",
            "git@github.com:sizeak/renamed-repo.git",
        ],
    );
    manager.sync_worktrees(&id).await.unwrap();

    let stored = store
        .read()
        .await
        .get_project(&id)
        .unwrap()
        .origin_url
        .clone();
    assert_eq!(
        canonical_repo_slug(stored.as_deref().expect("origin still recorded")),
        canonical_repo_slug("https://github.com/sizeak/renamed-repo"),
        "stored origin must follow the repo's current remote"
    );

    // Still idempotent: with the value now current, a further pass writes nothing.
    let gen_before = *store.subscribe().borrow();
    manager.sync_worktrees(&id).await.unwrap();
    assert_eq!(
        store.read().await.get_project(&id).unwrap().origin_url,
        stored
    );
    assert_eq!(
        *store.subscribe().borrow(),
        gen_before,
        "an unchanged origin must not rewrite state"
    );

    // Dropping the remote entirely settles back to `None` rather than pinning
    // an origin the repo no longer has.
    git(&local, &["remote", "remove", "origin"]);
    manager.sync_worktrees(&id).await.unwrap();
    assert_eq!(
        store.read().await.get_project(&id).unwrap().origin_url,
        None
    );
}

#[tokio::test]
async fn backfill_refills_origin_url_after_an_older_binary_drops_the_field() {
    let (tmp, _remote, local) = repo_with_remote();
    let (_cdir, _sdir, store, manager) = manager_for(&tmp);
    let id = manager.add_project(local, None).await.unwrap();

    // Simulate an older binary (or a pre-`origin_url` state.json) writing the
    // project back without the field. The fill must re-fire: it is never
    // version-gated, because `state.json` is multi-writer.
    store
        .mutate(move |state| {
            state.projects.get_mut(&id).unwrap().origin_url = None;
        })
        .await
        .unwrap();

    manager.sync_worktrees(&id).await.unwrap();
    let first = store
        .read()
        .await
        .get_project(&id)
        .unwrap()
        .origin_url
        .clone();
    assert!(first.is_some(), "backfill must refill the dropped field");

    // Idempotent: a second pass changes nothing and does not error.
    manager.sync_worktrees(&id).await.unwrap();
    assert_eq!(
        store.read().await.get_project(&id).unwrap().origin_url,
        first
    );
}

#[test]
fn status_bar_workspace_is_hidden_while_the_host_has_only_main() {
    let config = Config::default();
    let state = AppState::new();
    assert_eq!(super::status_bar_workspace(&config, &state, None), None);
}

#[test]
fn status_bar_workspace_names_the_tag_or_mains_label() {
    use claude_commander_protocol::workspace::WorkspaceDef;
    let config = Config {
        workspaces: vec![WorkspaceDef::named("Work")],
        ..Config::default()
    };
    let state = AppState::new();
    assert_eq!(
        super::status_bar_workspace(&config, &state, Some("Work")).as_deref(),
        Some("Work")
    );
    assert_eq!(
        super::status_bar_workspace(&config, &state, None).as_deref(),
        Some("Main")
    );
    let relabelled = Config {
        main_workspace: Some(WorkspaceDef::named("Home")),
        ..config
    };
    assert_eq!(
        super::status_bar_workspace(&relabelled, &state, None).as_deref(),
        Some("Home")
    );
}

#[test]
fn status_bar_workspace_shows_for_a_tag_with_no_definition() {
    let config = Config::default();
    let mut state = AppState::new();
    let mut project = crate::session::Project::new("p", std::path::PathBuf::from("/tmp/p"), "main");
    project.workspace = Some("Orphan".to_string());
    state.add_project(project);
    assert_eq!(
        super::status_bar_workspace(&config, &state, None).as_deref(),
        Some("Main")
    );
}
