//! Core-config + health handlers.
//!
//! Thin wrappers over `CommanderService`: `read_config`, a PATCH-style
//! partial `update`, `reload_config`, and `check_tmux` (the `/health/tmux`
//! 200/503 probe).
//!
//! Note: the server's own `[server]` settings ARE part of core's `Config` (so
//! that `ConfigStore` stops deleting them on every write), which means this
//! endpoint has to keep them safe explicitly rather than by their absence:
//! `read` redacts `server.token` via [`Config::with_secrets_redacted`], and
//! `ConfigPatch` is `deny_unknown_fields` with no `server` field, so a body that
//! so much as mentions it is rejected. Both are pinned by tests below.
//!
//! `update` is a **partial** update over an explicit allow-list (see
//! [`ConfigPatch`]). A full-replace `PUT` would let a remote client rewrite
//! filesystem-path and program-launch fields (`worktrees_dir`, `editor`,
//! `programs`, …); restricting to a conservative set of benign
//! UI/timing fields keeps those off-limits even though they remain readable via
//! `GET /config`.
//!
//! The one program-launch field that IS remotely writable — `programs` — has its
//! own dedicated route, `PUT /config/programs` ([`put_programs`]), rather than
//! being folded into the general patch. This keeps the `ConfigPatch` allow-list
//! locked down while letting a client edit the new-session picker list. It is not
//! a new capability: any token-bearing client can already run an arbitrary
//! command by passing `program` to `POST /sessions`, so the picker list is a
//! convenience, not a security boundary.

use axum::{Json, extract::State, http::StatusCode};
use claude_commander_core::Config;
use claude_commander_core::api::SetProgramsRequest;
use claude_commander_core::error::SessionError;
use claude_commander_protocol::api::ConfigReloaded;
use claude_commander_protocol::config::ConfigPatch;
use claude_commander_protocol::hosting::validate_gitlab_hostname;
use claude_commander_protocol::workspace::{
    DeleteWorkspaceRequest, RenameWorkspaceRequest, SetWorkspacesRequest,
};

use crate::error::ApiError;
use crate::extract::SafeJson;
use crate::state::AppState;

/// `GET /config` → `read_config`, with credential fields cleared: the caller
/// proved it holds THIS server's token — not the remote-server tokens, STT
/// API key, or telemetry credential the shared config file may also contain.
///
/// `[workspace_themes]` is left out too. It is not a secret, but it is this
/// host's own cosmetics, which the workspace design keeps off the wire (every
/// client themes its workspaces for itself), and no client reads it; like
/// `[theme]` it cannot be set here either (`ConfigPatch` has no such field).
pub async fn read(State(state): State<AppState>) -> Json<Config> {
    let mut config = state.service.read_config().with_secrets_redacted();
    config.workspace_themes.clear();
    Json(config)
}

/// Apply a [`ConfigPatch`]'s present fields onto `cfg`, leaving everything else
/// untouched. The patch's shape (the allow-list) is protocol's; merging it into
/// core's `Config` is the server's.
fn apply_patch(patch: ConfigPatch, cfg: &mut Config) {
    // Destructured exhaustively (no `..`), so a field added to the protocol's
    // allow-list fails to compile here until it is applied, rather than being
    // accepted on the wire and silently ignored.
    let ConfigPatch {
        code_host_provider,
        gitlab_hostname,
        branch_prefix,
        max_concurrent_tmux,
        capture_cache_ttl_ms,
        diff_cache_ttl_ms,
        ui_refresh_fps,
        pr_check_interval_secs,
        project_pull_enabled,
        project_pull_interval_secs,
        pr_review_labels,
        fetch_before_create,
        resume_session,
        state_sync_interval_ms,
        agent_state_poll_interval_ms,
        invert_pr_label_color,
        show_session_program,
        session_number_debounce_ms,
        ai_summary_enabled,
        rounded_borders,
        precompute_review_caches,
        in_progress_limit,
    } = patch;
    macro_rules! set {
        ($($field:ident),* $(,)?) => {
            $(
                if let Some(v) = $field {
                    cfg.$field = v;
                }
            )*
        };
    }
    if let Some(value) = gitlab_hostname {
        cfg.gitlab_hostname = value;
    }
    set!(
        code_host_provider,
        branch_prefix,
        max_concurrent_tmux,
        capture_cache_ttl_ms,
        diff_cache_ttl_ms,
        ui_refresh_fps,
        pr_check_interval_secs,
        project_pull_enabled,
        project_pull_interval_secs,
        pr_review_labels,
        fetch_before_create,
        resume_session,
        state_sync_interval_ms,
        agent_state_poll_interval_ms,
        invert_pr_label_color,
        show_session_program,
        session_number_debounce_ms,
        ai_summary_enabled,
        rounded_borders,
        precompute_review_caches,
        in_progress_limit,
    );
}

/// Validate a merged config before persisting. Catches values that would break
/// the running app (a zero refresh rate, a zero tmux concurrency). Returns a
/// 400 `ApiError` on failure.
fn validate(cfg: &Config) -> Result<(), ApiError> {
    let invalid = |key: &str, reason: &str| {
        ApiError(
            SessionError::InvalidName {
                name: key.to_string(),
                reason: reason.to_string(),
            }
            .into(),
        )
    };
    if cfg.ui_refresh_fps == 0 {
        return Err(invalid("ui_refresh_fps", "must be greater than zero"));
    }
    if cfg.max_concurrent_tmux == 0 {
        return Err(invalid("max_concurrent_tmux", "must be greater than zero"));
    }
    if let Some(hostname) = cfg.gitlab_hostname.as_deref() {
        validate_gitlab_hostname(hostname)
            .map_err(|reason| invalid("gitlab_hostname", &reason.to_string()))?;
    }
    Ok(())
}

/// `PATCH /config` → partial, allow-listed update → 204.
///
/// Merges the supplied fields onto the current config, validates the result,
/// then persists. Sensitive/path/program fields are not in [`ConfigPatch`], so
/// they cannot be changed here.
pub async fn update(
    State(state): State<AppState>,
    SafeJson(patch): SafeJson<ConfigPatch>,
) -> Result<StatusCode, ApiError> {
    state
        .service
        .edit_code_host_config(move |merged| {
            apply_patch(patch.clone(), merged);
            validate(merged).map_err(|error| error.0)
        })
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `PUT /config/programs` → replace the configured program list wholesale → 204.
///
/// A dedicated write route for the one program-launch field that is remotely
/// editable (see the module doc). An empty list is accepted (the picker then
/// falls back to a synthesized `claude` entry).
pub async fn put_programs(
    State(state): State<AppState>,
    Json(req): Json<SetProgramsRequest>,
) -> Result<StatusCode, ApiError> {
    let programs = req.programs.into_iter().map(Into::into).collect();
    state.service.set_programs(programs)?;
    Ok(StatusCode::NO_CONTENT)
}

/// `PUT /config/workspaces` → replace the workspace definitions (plus, when
/// given, Main's label and the startup choice) → 204. Never re-tags a project;
/// a refused name or duplicate is a 400. Like `programs`, workspaces have
/// their own route rather than a `ConfigPatch` field.
pub async fn put_workspaces(
    State(state): State<AppState>,
    Json(req): Json<SetWorkspacesRequest>,
) -> Result<StatusCode, ApiError> {
    state.service.set_workspace_defs(req)?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /config/workspaces/rename` `{from, to}` → rename a workspace and
/// rewrite its projects' tags → 204. Idempotent: an unknown `from` is a no-op
/// 204, so a rename sent to every server is safe on the ones without it.
pub async fn rename_workspace(
    State(state): State<AppState>,
    Json(req): Json<RenameWorkspaceRequest>,
) -> Result<StatusCode, ApiError> {
    state.service.rename_workspace(&req.from, &req.to).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /config/workspaces/delete` `{name}` → drop a workspace, moving its
/// projects to Main → 204 (a no-op 204 when unknown). A POST with a body, not
/// `DELETE /config/workspaces/{name}`: every string is a valid name, including
/// the static segments beside that capture.
pub async fn delete_workspace(
    State(state): State<AppState>,
    Json(req): Json<DeleteWorkspaceRequest>,
) -> Result<StatusCode, ApiError> {
    state.service.delete_workspace(&req.name).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /config/reload` → `reload_config` → [`ConfigReloaded`]
/// (true when the on-disk config differed and was re-read).
pub async fn reload(State(state): State<AppState>) -> Result<Json<ConfigReloaded>, ApiError> {
    let reloaded = state.service.reload_config().await?;
    Ok(Json(ConfigReloaded { reloaded }))
}

/// `GET /health/tmux` → `check_tmux` → 200 on Ok, 503 on Err.
///
/// Distinct from the rest: a tmux probe failure is the *expected* signal of an
/// unhealthy backing service, not a 500 — so it short-circuits to 503 rather
/// than going through [`ApiError`]'s variant mapping.
pub async fn health_tmux(State(state): State<AppState>) -> StatusCode {
    match state.service.check_tmux().await {
        Ok(()) => StatusCode::OK,
        Err(_) => StatusCode::SERVICE_UNAVAILABLE,
    }
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use axum::{
        Router,
        routing::{get, post, put},
    };
    use claude_commander_core::Config;
    use claude_commander_protocol::config::ConfigView;
    use tempfile::TempDir;

    use crate::handlers::test_support::{get as do_get, json, send, test_state};
    use crate::state::AppState;

    fn router(state: AppState) -> Router {
        Router::new()
            .route("/config", get(super::read).patch(super::update))
            .route("/config/programs", put(super::put_programs))
            .route("/config/workspaces", put(super::put_workspaces))
            .route("/config/workspaces/rename", post(super::rename_workspace))
            .route("/config/workspaces/delete", post(super::delete_workspace))
            .with_state(state)
    }

    async fn patch(state: AppState, body: serde_json::Value) -> axum::http::StatusCode {
        let req = Request::patch("/config")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        send(router(state), req).await.0
    }

    async fn put_programs(state: AppState, body: serde_json::Value) -> axum::http::StatusCode {
        let req = Request::put("/config/programs")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        send(router(state), req).await.0
    }

    /// `GET /config` returns the live config as JSON (here, defaults).
    #[tokio::test]
    async fn read_config_is_200_json() {
        let dir = TempDir::new().unwrap();
        let (status, body) = do_get(router(test_state(&dir)), "/config").await;
        assert_eq!(status, 200);
        // Round-trips into a `Config` (the default), proving it's the real shape.
        let _config: Config = json(&body);
    }

    /// Credentials in the config never cross the wire: a client holding THIS
    /// server's bearer token must not be able to harvest other remote servers'
    /// tokens (or the STT/telemetry credentials) from `GET /config`.
    #[tokio::test]
    async fn read_config_redacts_secrets() {
        let dir = TempDir::new().unwrap();
        let state = test_state(&dir);
        state
            .service
            .update_config({
                let mut c = state.service.read_config();
                c.remote_servers = vec![claude_commander_core::config::RemoteServerConfig {
                    name: "other".into(),
                    url: "http://other:7878".into(),
                    token: Some("other-server-secret".into()),
                }];
                c.stt.api_key = Some("stt-secret".into());
                c.telemetry.token = Some("telemetry-secret".into());
                c
            })
            .unwrap();

        let (status, body) = do_get(router(state), "/config").await;
        assert_eq!(status, 200);
        let text = String::from_utf8(body.to_vec()).unwrap();
        for secret in ["other-server-secret", "stt-secret", "telemetry-secret"] {
            assert!(!text.contains(secret), "{secret} leaked via GET /config");
        }
        // The non-secret remote-server fields still serve normally.
        assert!(text.contains("http://other:7878"));
    }

    /// Workspace themes are the host's own cosmetics and never go on the wire
    /// (docs/configuration.md): `GET /config` leaves `[workspace_themes]` out,
    /// just as `ConfigPatch` refuses to set it.
    #[tokio::test]
    async fn read_config_leaves_out_workspace_themes() {
        let dir = TempDir::new().unwrap();
        let state = test_state(&dir);
        state
            .service
            .update_config({
                let mut c = state.service.read_config();
                c.workspace_themes.insert(
                    "Work".into(),
                    claude_commander_core::config::ThemeOverrides {
                        preset: Some("basic".into()),
                        ..Default::default()
                    },
                );
                c
            })
            .unwrap();

        let (status, body) = do_get(router(state.clone()), "/config").await;
        assert_eq!(status, 200);
        let text = String::from_utf8_lossy(&body);
        assert!(!text.contains("workspace_themes"), "{text}");
        assert!(
            !state.service.read_config().workspace_themes.is_empty(),
            "only the response omits them"
        );
    }

    /// An allow-listed field updates and persists; nothing else changes.
    #[tokio::test]
    async fn patch_updates_allowed_field() {
        let dir = TempDir::new().unwrap();
        let state = test_state(&dir);
        let before = state.service.read_config();
        assert_ne!(before.ui_refresh_fps, 45, "fixture must differ from target");

        let status = patch(state.clone(), serde_json::json!({ "ui_refresh_fps": 45 })).await;
        assert_eq!(status, 204);

        let after = state.service.read_config();
        assert_eq!(after.ui_refresh_fps, 45);
        // An untouched field keeps its prior value.
        assert_eq!(after.worktrees_dir, before.worktrees_dir);
    }

    /// The `[server]` table is part of core's `Config` now, so `GET /config`
    /// would serialise this server's own bearer token to every client holding it
    /// unless redaction covers it.
    #[tokio::test]
    async fn get_config_redacts_the_servers_own_token() {
        let dir = TempDir::new().unwrap();
        let state = test_state(&dir);
        state
            .service
            .update_config({
                let mut c = state.service.read_config();
                c.server.port = 9999;
                c.server.token = Some("own-secret".into());
                c
            })
            .unwrap();

        let (status, body) = do_get(router(state), "/config").await;
        assert_eq!(status, 200);
        let text = String::from_utf8_lossy(&body);
        assert!(!text.contains("own-secret"), "token leaked: {text}");
        // The non-secret part of the table still comes through.
        assert!(text.contains("9999"), "{text}");
    }

    /// `GET /config` serves core's whole (redacted) `Config`, which has no
    /// protocol type; `ConfigView` is the subset clients read. Its fields are
    /// required, so a rename or type change on core's side fails here rather
    /// than reaching the page as a silently missing value.
    #[tokio::test]
    async fn get_config_body_reads_as_a_config_view() {
        let dir = TempDir::new().unwrap();
        let state = test_state(&dir);
        state
            .service
            .update_config({
                let mut c = state.service.read_config();
                c.branch_prefix = "pfx/".into();
                c.fetch_before_create = !c.fetch_before_create;
                c.resume_session = !c.resume_session;
                c.project_pull_enabled = !c.project_pull_enabled;
                c
            })
            .unwrap();
        let expected = state.service.read_config();

        let (status, body) = do_get(router(state), "/config").await;
        assert_eq!(status, 200);
        let view: ConfigView = serde_json::from_slice(&body).expect("GET /config is a ConfigView");
        assert_eq!(
            view,
            ConfigView {
                branch_prefix: "pfx/".into(),
                fetch_before_create: expected.fetch_before_create,
                resume_session: expected.resume_session,
                project_pull_enabled: expected.project_pull_enabled,
            }
        );
    }

    /// A remote client must not be able to move this server's own bind address,
    /// port or token. `ConfigPatch` has no `server` field and is
    /// `deny_unknown_fields`, so naming it is a 4xx rather than a silent drop.
    #[tokio::test]
    async fn patch_rejects_the_server_table() {
        let dir = TempDir::new().unwrap();
        let state = test_state(&dir);
        let before = state.service.read_config();

        let status = patch(
            state.clone(),
            serde_json::json!({ "server": { "bind": "0.0.0.0" } }),
        )
        .await;
        assert!(
            status.is_client_error(),
            "patching [server] must be a 4xx, got {status}"
        );
        assert_eq!(state.service.read_config().server, before.server);
    }

    /// Per-workspace themes are local TUI config, never remotely writable:
    /// `ConfigPatch` has no `workspace_themes` field, so naming it is a 4xx.
    #[tokio::test]
    async fn patch_rejects_workspace_themes() {
        let dir = TempDir::new().unwrap();
        let state = test_state(&dir);

        let status = patch(
            state.clone(),
            serde_json::json!({ "workspace_themes": { "Work": { "preset": "basic" } } }),
        )
        .await;
        assert!(
            status.is_client_error(),
            "patching [workspace_themes] must be a 4xx, got {status}"
        );
        assert!(state.service.read_config().workspace_themes.is_empty());
    }

    #[tokio::test]
    async fn patch_updates_code_host_and_rejects_unsafe_gitlab_hostname() {
        let dir = TempDir::new().unwrap();
        let state = test_state(&dir);

        let status = patch(
            state.clone(),
            serde_json::json!({
                "code_host_provider": "gitlab",
                "gitlab_hostname": "gitlab.example.com:8443"
            }),
        )
        .await;
        assert_eq!(status, 204);
        let config = state.service.read_config();
        assert_eq!(
            config.code_host_provider,
            claude_commander_protocol::hosting::CodeHostProvider::Gitlab
        );
        assert_eq!(
            config.gitlab_hostname.as_deref(),
            Some("gitlab.example.com:8443")
        );

        let status = patch(
            state.clone(),
            serde_json::json!({
                "gitlab_hostname": "https://user:secret@gitlab.example.com/group"
            }),
        )
        .await;
        assert_eq!(status, 400);
        assert_eq!(
            state.service.read_config().gitlab_hostname.as_deref(),
            Some("gitlab.example.com:8443")
        );
    }

    #[tokio::test]
    async fn patch_distinguishes_an_omitted_hostname_from_explicit_null() {
        let dir = TempDir::new().unwrap();
        let state = test_state(&dir);
        let mut config = state.service.read_config();
        config.gitlab_hostname = Some("gitlab.example.com".into());
        state.service.update_config(config).unwrap();

        let status = patch(state.clone(), serde_json::json!({})).await;
        assert_eq!(status, 204);
        assert_eq!(
            state.service.read_config().gitlab_hostname.as_deref(),
            Some("gitlab.example.com"),
            "an omitted PATCH field must leave the hostname untouched"
        );

        let status = patch(
            state.clone(),
            serde_json::json!({ "gitlab_hostname": null }),
        )
        .await;
        assert_eq!(status, 204);
        assert_eq!(
            state.service.read_config().gitlab_hostname,
            None,
            "an explicit null must clear the hostname override"
        );
    }

    #[tokio::test]
    async fn malformed_gitlab_hostname_shape_never_echoes_a_credential() {
        let dir = TempDir::new().unwrap();
        let secret = "glpat-secret-value";
        let req = Request::patch("/config")
            .header("content-type", "application/json")
            .body(Body::from(format!(
                r#"{{"gitlab_hostname":["https://user:{secret}@gitlab.example.com"]}}"#
            )))
            .unwrap();
        let (status, body) = send(router(test_state(&dir)), req).await;
        assert_eq!(status, 400);
        let body = String::from_utf8(body).unwrap();
        assert!(!body.contains(secret), "credential leaked: {body}");
    }

    /// A sensitive path field cannot be changed: `deny_unknown_fields` rejects a
    /// body that even names `worktrees_dir`, and the stored config is unchanged.
    #[tokio::test]
    async fn patch_rejects_sensitive_field() {
        let dir = TempDir::new().unwrap();
        let state = test_state(&dir);
        let before = state.service.read_config();

        let status = patch(
            state.clone(),
            serde_json::json!({ "worktrees_dir": "/tmp/evil" }),
        )
        .await;
        assert!(
            status.is_client_error(),
            "patching a sensitive field must be a 4xx, got {status}"
        );

        let after = state.service.read_config();
        assert_eq!(
            after.worktrees_dir, before.worktrees_dir,
            "worktrees_dir must be unchanged after a rejected patch"
        );
    }

    /// A merged config that fails validation (zero fps) is a 400 and is not
    /// persisted.
    #[tokio::test]
    async fn patch_rejects_invalid_merged_value() {
        let dir = TempDir::new().unwrap();
        let state = test_state(&dir);
        let before = state.service.read_config();

        let status = patch(state.clone(), serde_json::json!({ "ui_refresh_fps": 0 })).await;
        assert_eq!(status, 400);

        let after = state.service.read_config();
        assert_eq!(after.ui_refresh_fps, before.ui_refresh_fps);
    }

    /// `PUT /config/programs` replaces the program list, and the change is
    /// reflected by the new-session options.
    #[tokio::test]
    async fn put_programs_updates_config_and_create_options() {
        let dir = TempDir::new().unwrap();
        let state = test_state(&dir);
        // A default config has no configured programs (create_options is verbatim).
        assert_eq!(state.service.create_options().programs.len(), 0);

        let status = put_programs(
            state.clone(),
            serde_json::json!({
                "programs": [
                    { "label": "Claude (Opus)", "command": "claude --model opus" },
                    { "label": "Shell", "command": "bash" }
                ]
            }),
        )
        .await;
        assert_eq!(status, 204);

        let opts = state.service.create_options();
        assert_eq!(opts.programs.len(), 2);
        assert_eq!(opts.programs[0].command, "claude --model opus");
        assert_eq!(opts.default_program, "claude --model opus");
    }

    /// An empty program list is accepted and persists as empty (the picker then
    /// falls back to the synthesized `claude` entry).
    #[tokio::test]
    async fn put_programs_accepts_empty_list() {
        let dir = TempDir::new().unwrap();
        let state = test_state(&dir);
        // Seed a non-empty list first, then clear it.
        assert_eq!(
            put_programs(
                state.clone(),
                serde_json::json!({ "programs": [{ "label": "X", "command": "x" }] }),
            )
            .await,
            204
        );
        assert_eq!(state.service.read_config().programs.len(), 1);

        let status = put_programs(state.clone(), serde_json::json!({ "programs": [] })).await;
        assert_eq!(status, 204);
        assert!(state.service.read_config().programs.is_empty());
    }

    async fn send_json(
        state: AppState,
        method: &str,
        uri: &str,
        body: serde_json::Value,
    ) -> axum::http::StatusCode {
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        send(router(state), req).await.0
    }

    /// `PUT /config/workspaces` replaces the definitions; the snapshot serves
    /// them back.
    #[tokio::test]
    async fn put_workspaces_updates_config_and_snapshot() {
        let dir = TempDir::new().unwrap();
        let state = test_state(&dir);
        let status = send_json(
            state.clone(),
            "PUT",
            "/config/workspaces",
            serde_json::json!({
                "workspaces": [{"name": "Work"}, {"name": "Play"}],
                "main": {"name": "Home"},
                "startup_workspace": "Work"
            }),
        )
        .await;
        assert_eq!(status, 204);
        let snap = state.service.snapshot().await.unwrap();
        let names: Vec<_> = snap.workspaces.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["Work", "Play"]);
        assert_eq!(snap.main_workspace.unwrap().name, "Home");
    }

    /// A refused list (reserved name) is a 400 and writes nothing.
    #[tokio::test]
    async fn put_workspaces_rejects_a_reserved_name() {
        let dir = TempDir::new().unwrap();
        let state = test_state(&dir);
        let status = send_json(
            state.clone(),
            "PUT",
            "/config/workspaces",
            serde_json::json!({ "workspaces": [{"name": "last"}] }),
        )
        .await;
        assert_eq!(status, 400);
        assert!(state.service.read_config().workspaces.is_empty());
    }

    /// Rename and delete are idempotent 204s, and rewrite what they name.
    #[tokio::test]
    async fn rename_and_delete_workspace_routes() {
        let dir = TempDir::new().unwrap();
        let state = test_state(&dir);
        send_json(
            state.clone(),
            "PUT",
            "/config/workspaces",
            serde_json::json!({ "workspaces": [{"name": "Work"}] }),
        )
        .await;
        let renamed = send_json(
            state.clone(),
            "POST",
            "/config/workspaces/rename",
            serde_json::json!({ "from": "Work", "to": "Job" }),
        )
        .await;
        assert_eq!(renamed, 204);
        assert_eq!(state.service.read_config().workspaces[0].name, "Job");
        let unknown = send_json(
            state.clone(),
            "POST",
            "/config/workspaces/rename",
            serde_json::json!({ "from": "Nope", "to": "Other" }),
        )
        .await;
        assert_eq!(unknown, 204, "renaming an unknown workspace is a no-op");

        // A workspace may be *named* "delete" — the body carries the name.
        for name in ["Job", "delete"] {
            let status = send_json(
                state.clone(),
                "POST",
                "/config/workspaces/delete",
                serde_json::json!({ "name": name }),
            )
            .await;
            assert_eq!(status, 204);
        }
        assert!(state.service.read_config().workspaces.is_empty());
    }

    /// Workspaces stay off the general PATCH surface, like `programs`.
    #[tokio::test]
    async fn patch_rejects_workspace_fields() {
        let dir = TempDir::new().unwrap();
        let state = test_state(&dir);
        for body in [
            serde_json::json!({ "workspaces": [{"name": "Work"}] }),
            serde_json::json!({ "startup_workspace": "main" }),
            serde_json::json!({ "main_workspace": {"name": "Home"} }),
        ] {
            let status = patch(state.clone(), body).await;
            assert!(status.is_client_error(), "got {status}");
        }
    }

    /// `programs` remains off-limits on the general PATCH surface: the dedicated
    /// PUT route is the only way to edit it, and `deny_unknown_fields` still 4xxs
    /// a patch body that names `programs`.
    #[tokio::test]
    async fn patch_still_rejects_programs_field() {
        let dir = TempDir::new().unwrap();
        let state = test_state(&dir);

        let status = patch(
            state.clone(),
            serde_json::json!({ "programs": [{ "label": "X", "command": "evil" }] }),
        )
        .await;
        assert!(
            status.is_client_error(),
            "patching `programs` via PATCH /config must be a 4xx, got {status}"
        );
        assert!(state.service.read_config().programs.is_empty());
    }
}
