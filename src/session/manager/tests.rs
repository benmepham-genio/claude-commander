use super::*;
use crate::config::{AppState, Config, ConfigStore, StateStore};
use tempfile::TempDir;

fn test_store() -> (TempDir, Arc<StateStore>) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("state.json");
    let store = Arc::new(StateStore::with_path(AppState::new(), path));
    (dir, store)
}

fn test_config_store(config: Config) -> (TempDir, Arc<ConfigStore>) {
    let dir = TempDir::new().unwrap();
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
