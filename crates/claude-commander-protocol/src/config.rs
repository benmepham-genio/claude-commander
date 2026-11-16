//! `PATCH /config` request body.
//!
//! The allow-list of config fields a remote client may change is a rule the
//! server and every client must agree on, so it lives here rather than in the
//! server crate. Applying a patch to core's `Config` (and validating the merged
//! result) is the server's job — this crate only owns the shape.

use serde::{Deserialize, Serialize};

/// Partial config update: every field is optional, and only the fields below —
/// a conservative allow-list of benign UI/timing/behaviour options — may be
/// changed. Filesystem-path fields (`worktrees_dir`, `log_file`,
/// `commander_dir`, `per_repo_worktree_dirs`), program-launch fields
/// (`programs`, `shell_program`, `editor`, `editor_gui`,
/// `commander_program`, `commander_enabled`, `nix_develop`), credentials
/// (`server`, `remote_servers`, `stt`, `telemetry`) and complex nested tables
/// (`keybindings`, `theme`, `workspace_themes`, `sections`, `conversation`) are
/// intentionally absent, so a request can neither set nor reset them here —
/// `programs` and the workspace definitions have their own dedicated routes.
///
/// `deny_unknown_fields` means a body that even *mentions* such a field is
/// rejected (4xx) rather than silently dropped — a clear signal to the caller
/// that the field is off-limits. Serde's rejection of an *unknown* field names
/// the field without echoing its value; that is the only case it keeps quiet.
/// A type error on a *known* field does quote the value (`invalid type: string
/// "…", expected u32`), and axum's plain `Json` extractor puts that message in
/// the 4xx body. The server uses `extract::SafeJson` so a malformed hostname
/// or provider value cannot echo a pasted credential in an extraction error.
///
/// Absent fields are omitted when serialized, so a client sending a patch sends
/// only what it means to change.
///
/// `in_progress_limit` is `Option<Option<u32>>` but, as deserialized by serde, a
/// JSON `null` reads as the *outer* `None` ("leave unchanged") — the inner
/// `None` (clear the limit) is not reachable over the wire today.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(optional_fields))]
pub struct ConfigPatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code_host_provider: Option<crate::hosting::CodeHostProvider>,
    /// Omitted leaves the override unchanged; explicit null clears it.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "nullable_hostname"
    )]
    pub gitlab_hostname: Option<Option<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch_prefix: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_concurrent_tmux: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capture_cache_ttl_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff_cache_ttl_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ui_refresh_fps: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_check_interval_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_pull_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_pull_interval_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_review_labels: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fetch_before_create: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume_session: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state_sync_interval_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_state_poll_interval_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invert_pr_label_color: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show_session_program: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_number_debounce_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ai_summary_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rounded_borders: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub precompute_review_caches: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_progress_limit: Option<Option<u32>>,
}

fn nullable_hostname<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Option<String>>, D::Error> {
    Option::<String>::deserialize(deserializer).map(Some)
}

/// The fields of the `GET /config` response that a client reads.
///
/// `GET /config` serves core's whole `Config` (secrets redacted), which is not
/// a protocol type: it is host config, and most of it is no client's business.
/// This is the subset clients actually consume, so they are typed against a
/// contract rather than against `ConfigPatch`'s look-alike names. It does not
/// change the wire body: unknown fields are ignored when deserializing, and the
/// server's `get_config_body_reads_as_a_config_view` test deserializes the real
/// response into it, so a rename in core fails there. Every field is required
/// for that reason; widen it only with fields a client reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ConfigView {
    pub branch_prefix: String,
    pub fetch_before_create: bool,
    pub resume_session: bool,
    pub project_pull_enabled: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_patch_serializes_as_an_empty_object() {
        assert_eq!(
            serde_json::to_string(&ConfigPatch::default()).unwrap(),
            "{}"
        );
    }

    #[test]
    fn only_present_fields_serialize_and_round_trip() {
        let patch = ConfigPatch {
            ui_refresh_fps: Some(45),
            resume_session: Some(false),
            ..Default::default()
        };
        let json = serde_json::to_string(&patch).unwrap();
        assert_eq!(json, r#"{"ui_refresh_fps":45,"resume_session":false}"#);
        assert_eq!(serde_json::from_str::<ConfigPatch>(&json).unwrap(), patch);
    }

    /// The allow-list is the security boundary: naming any field outside it —
    /// a credential table, a path, a program — is a hard error, not a no-op.
    #[test]
    fn unknown_fields_are_rejected() {
        for body in [
            r#"{"server":{"bind":"0.0.0.0"}}"#,
            r#"{"worktrees_dir":"/x"}"#,
            r#"{"programs":[]}"#,
            r#"{"stt":{"api_key":"k"}}"#,
        ] {
            assert!(
                serde_json::from_str::<ConfigPatch>(body).is_err(),
                "{body} must be rejected"
            );
        }
    }

    /// Pinned so the quirk documented on the type can't change silently: a
    /// JSON `null` means "leave unchanged", not "clear".
    #[test]
    fn in_progress_limit_null_reads_as_unchanged() {
        let patch: ConfigPatch = serde_json::from_str(r#"{"in_progress_limit":null}"#).unwrap();
        assert_eq!(patch.in_progress_limit, None);
        let patch: ConfigPatch = serde_json::from_str(r#"{"in_progress_limit":3}"#).unwrap();
        assert_eq!(patch.in_progress_limit, Some(Some(3)));
    }

    #[test]
    fn gitlab_hostname_patch_distinguishes_missing_null_and_value() {
        let missing: ConfigPatch = serde_json::from_str("{}").unwrap();
        assert_eq!(missing.gitlab_hostname, None);
        let clear: ConfigPatch = serde_json::from_str(r#"{"gitlab_hostname":null}"#).unwrap();
        assert_eq!(clear.gitlab_hostname, Some(None));
        assert_eq!(
            serde_json::to_string(&clear).unwrap(),
            r#"{"gitlab_hostname":null}"#
        );
        let set: ConfigPatch =
            serde_json::from_str(r#"{"gitlab_hostname":"gitlab.example.com"}"#).unwrap();
        assert_eq!(
            set.gitlab_hostname
                .as_ref()
                .and_then(|hostname| hostname.as_deref()),
            Some("gitlab.example.com")
        );
    }
}
