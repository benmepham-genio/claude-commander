//! Session lifecycle + query handlers.
//!
//! Thin wrappers over `CommanderService`: `list_sessions`,
//! `find_session`/`find_session_exact`, `get_session_detail`,
//! `get_pane_content`, `create_session`, `kill_session`, `restart_session`,
//! `restart_session_fresh`, `delete_session`.

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use claude_commander_core::api::{
    CreateSessionOpts, PreviewData, PreviewTarget, SessionInfo, SetSessionBase,
    SetSessionBaseOutcome,
};
use claude_commander_core::session::SessionLookup;
use claude_commander_protocol::api::{CreatedId, MarkUnread, PatchSession};
use serde::Deserialize;

use crate::error::{ApiError, error_response};
use crate::extract::SafeJson;
use crate::state::AppState;

use super::{parse_session_id, run_local};

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    pub include_stopped: bool,
}

/// `GET /sessions?include_stopped=` → `list_sessions`.
pub async fn list(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Result<Json<Vec<SessionInfo>>, ApiError> {
    Ok(Json(state.service.list_sessions(q.include_stopped).await?))
}

#[derive(Debug, Deserialize)]
pub struct FindQuery {
    pub q: String,
    #[serde(default)]
    pub exact: bool,
}

/// `GET /sessions/find?q=&exact=` → `find_session` / `find_session_exact`.
///
/// Loose (default) match: 404 when nothing matches. Exact match: 404 when
/// nothing matches, 409 when the query is ambiguous (several sessions share a
/// title), else the matched session.
pub async fn find(
    State(state): State<AppState>,
    Query(q): Query<FindQuery>,
) -> Result<Response, ApiError> {
    if q.exact {
        match state.service.find_session_exact(&q.q).await? {
            SessionLookup::Found(info) => Ok(Json(info).into_response()),
            SessionLookup::NotFound => Ok(StatusCode::NOT_FOUND.into_response()),
            SessionLookup::Ambiguous(n) => Ok(error_response(
                StatusCode::CONFLICT,
                "session",
                format!("{n} sessions match {:?}", q.q),
            )),
        }
    } else {
        match state.service.find_session(&q.q).await? {
            Some(info) => Ok(Json(info).into_response()),
            None => Ok(StatusCode::NOT_FOUND.into_response()),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct LinesQuery {
    pub lines: Option<usize>,
}

/// `GET /sessions/{q}/detail?lines=` → `get_session_detail` (404 if None).
pub async fn detail(
    State(state): State<AppState>,
    Path(q): Path<String>,
    Query(lq): Query<LinesQuery>,
) -> Result<Response, ApiError> {
    match state.service.get_session_detail(&q, lq.lines).await? {
        Some(detail) => Ok(Json(detail).into_response()),
        None => Ok(StatusCode::NOT_FOUND.into_response()),
    }
}

/// `GET /sessions/{q}/pane?lines=` → `get_pane_content` (404 if None).
pub async fn pane(
    State(state): State<AppState>,
    Path(q): Path<String>,
    Query(lq): Query<LinesQuery>,
) -> Result<Response, ApiError> {
    match state.service.get_pane_content(&q, lq.lines).await? {
        Some(content) => Ok(content.into_response()),
        None => Ok(StatusCode::NOT_FOUND.into_response()),
    }
}

/// `POST /sessions` → `create_session` → 201 `{ "id": ... }`.
///
/// The body deserializes straight into [`CreateSessionOpts`] (a shared wire type
/// in `claude-commander-protocol`) — no server-side mirror DTO.
pub async fn create(
    State(state): State<AppState>,
    Json(opts): Json<CreateSessionOpts>,
) -> Result<Response, ApiError> {
    // `create_session` builds a `gix::Repository` (non-`Send`) across an await.
    let id = run_local(move || async move { state.service.create_session(opts).await }).await?;
    Ok((StatusCode::CREATED, Json(CreatedId { id })).into_response())
}

/// `POST /sessions/{id}/kill` → `kill_session` → 204.
pub async fn kill(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let id = parse_session_id(&id)?;
    run_local(move || async move { state.service.kill_session(&id).await }).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /sessions/{id}/restart` → `restart_session` → 204.
pub async fn restart(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let id = parse_session_id(&id)?;
    run_local(move || async move { state.service.restart_session(&id).await }).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /sessions/{id}/restart-fresh` → `restart_session_fresh` → 204.
///
/// Distinct from `/restart`: this one never passes the agent's resume flag, so
/// the relaunched pane starts a *fresh* conversation regardless of the server's
/// `resume_session` config or whether the session was hibernated.
pub async fn restart_fresh(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let id = parse_session_id(&id)?;
    run_local(move || async move { state.service.restart_session_fresh(&id).await }).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /sessions/{id}` → `delete_session` → 204.
pub async fn delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let id = parse_session_id(&id)?;
    run_local(move || async move { state.service.delete_session(&id).await }).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
pub struct PreviewQuery {
    /// Capture this many pane lines directly instead of the cached snapshot.
    pub lines: Option<usize>,
    #[serde(default)]
    pub part: claude_commander_protocol::preview::PreviewPart,
}

/// `GET /sessions/{id}/preview?lines=` → session `preview`.
pub async fn preview(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<PreviewQuery>,
) -> Result<Json<PreviewData>, ApiError> {
    let id = parse_session_id(&id)?;
    Ok(Json(
        state
            .service
            .preview_part(PreviewTarget::Session { id, lines: q.lines }, q.part)
            .await?,
    ))
}

/// `GET /sessions/{id}/branch-diff` → `branch_diff` (text/plain).
pub async fn branch_diff(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<String, ApiError> {
    let id = parse_session_id(&id)?;
    Ok(state.service.branch_diff(&id).await?)
}

/// `PATCH /sessions/{id}` saves edits (200 with optional base-retarget outcome),
/// or performs a legacy single-setting operation (204).
pub async fn patch(
    State(state): State<AppState>,
    Path(id): Path<String>,
    SafeJson(body): SafeJson<PatchSession>,
) -> Result<Response, ApiError> {
    let id = parse_session_id(&id)?;
    match body {
        PatchSession::Edit(edit) => {
            let outcome =
                run_local(move || async move { state.service.edit_session(&id, edit).await })
                    .await?;
            return Ok(Json(outcome).into_response());
        }
        PatchSession::Rename(r) => state.service.rename_session(&id, r.title).await?,
        PatchSession::SetSection(s) => state.service.set_section(&id, s.section).await?,
        // Relaunches the pane (tmux) → not `Send`; run on the local pool.
        PatchSession::ChangeProgram(c) => {
            run_local(move || async move { state.service.change_program(&id, c.program).await })
                .await?;
        }
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// `POST /sessions/{id}/read` → `mark_read` → 204.
pub async fn read(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let id = parse_session_id(&id)?;
    state.service.mark_read(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /sessions/{id}/keep-alive` → toggle the hibernation-exempt flag,
/// returning the new value.
pub async fn keep_alive(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<bool>, ApiError> {
    let id = parse_session_id(&id)?;
    Ok(Json(state.service.toggle_keep_alive(&id).await?))
}

/// `POST /sessions/{id}/base` → retarget the session's stack base, returning
/// what happened (including whether the PR edit landed).
///
/// A POST sub-resource rather than a `PatchSession` variant because it answers
/// with a body: `PATCH /sessions/{id}` is uniformly 204, and the caller cannot
/// compute the PR outcome for itself.
pub async fn set_base(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<SetSessionBase>,
) -> Result<Json<SetSessionBaseOutcome>, ApiError> {
    let id = parse_session_id(&id)?;
    Ok(Json(
        state
            .service
            .set_session_base(&id, body.parent_session_id)
            .await?,
    ))
}

/// `POST /sessions/unread` with `{ "ids": [...] }` → `mark_unread` → 204.
///
/// The batch counterpart to `read`: the remote client's palette bulk
/// "mark unread" action posts every target id here. Unknown ids are silently
/// skipped by the service (a no-op), matching the local backend.
pub async fn unread(
    State(state): State<AppState>,
    Json(body): Json<MarkUnread>,
) -> Result<StatusCode, ApiError> {
    let ids = body
        .ids
        .iter()
        .map(|id| parse_session_id(id))
        .collect::<Result<Vec<_>, _>>()?;
    state.service.mark_unread(ids).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use axum::{
        Router,
        routing::{get, post},
    };
    use tempfile::TempDir;

    use crate::handlers::test_support::{get as do_get, test_state};
    use crate::state::AppState;

    fn router(state: AppState) -> Router {
        Router::new()
            .route("/sessions", get(super::list).post(super::create))
            .route("/sessions/find", get(super::find))
            .route("/sessions/{q}/detail", get(super::detail))
            .route("/sessions/{q}/pane", get(super::pane))
            .route("/sessions/{id}/kill", post(super::kill))
            .route("/sessions/{id}/restart", post(super::restart))
            .route("/sessions/{id}/restart-fresh", post(super::restart_fresh))
            .route(
                "/sessions/{id}",
                axum::routing::delete(super::delete).patch(super::patch),
            )
            .route("/sessions/{id}/preview", get(super::preview))
            .route("/sessions/{id}/branch-diff", get(super::branch_diff))
            .route("/sessions/{id}/read", post(super::read))
            .route("/sessions/unread", post(super::unread))
            .with_state(state)
    }

    /// A hermetic [`AppState`] seeded with one project + one (read) session,
    /// plus that session's id — for exercising the batch mark-unread route.
    fn seeded_state(dir: &TempDir) -> (AppState, claude_commander_core::session::SessionId) {
        use claude_commander_core::api::CommanderService;
        use claude_commander_core::config::storage::AppState as CoreState;
        use claude_commander_core::config::{Config, ConfigStore, StateStore};
        use claude_commander_core::session::{Project, WorktreeSession};
        use claude_commander_core::telemetry::FrontendInfo;
        use std::sync::Arc;

        let mut config = Config::default();
        config.telemetry.enabled = false;
        // `projects_dir` defaults to the user's REAL `~/Projects`, which the
        // repo-clone paths write into. Pin it under `dir`, matching the shared
        // `handlers/test_support.rs` fixture.
        config.projects_dir = Some(dir.path().join("projects"));
        let mut core = CoreState::default();
        let mut project = Project::new("p", std::path::PathBuf::from("/tmp/p"), "main");
        let pid = project.id;
        let mut sess = WorktreeSession::new(pid, "s", "s-br", std::path::PathBuf::new(), "claude");
        sess.status = claude_commander_core::SessionStatus::Running;
        sess.unread = false;
        let sid = sess.id;
        project.add_worktree(sid);
        core.projects.insert(pid, project);
        core.sessions.insert(sid, sess);

        let config_store = Arc::new(ConfigStore::with_path(
            config,
            dir.path().join("config.toml"),
        ));
        // The store's `mutate` re-reads state from disk (crash-safe write cycle),
        // so persist the seeded state before wrapping it — otherwise the first
        // mutation would read an empty file and drop the session.
        let state_path = dir.path().join("state.json");
        std::fs::write(&state_path, serde_json::to_string(&core).unwrap()).unwrap();
        let store = Arc::new(StateStore::with_path(core, state_path));
        let service =
            CommanderService::new(config_store, store, FrontendInfo::new("test", "0.0.0"));
        (
            AppState::new(service, crate::auth::AuthConfig::Disabled),
            sid,
        )
    }

    #[tokio::test]
    async fn edit_patch_saves_pending_program_and_returns_base_outcome() {
        use axum::body::Body;
        use axum::http::Request;
        let dir = TempDir::new().unwrap();
        let (state, sid) = seeded_state(&dir);
        let before = state
            .service
            .store()
            .read()
            .await
            .get_session(&sid)
            .unwrap()
            .program
            .clone();
        let body = serde_json::json!({ "op": "edit", "title": "edited", "program": "codex",
            "section": null, "keep_alive": true, "restart": false });
        let req = Request::patch(format!("/sessions/{}", sid.as_uuid()))
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let (status, body) = crate::handlers::test_support::send(router(state.clone()), req).await;
        assert_eq!(status, 200);
        let outcome: claude_commander_protocol::api::EditSessionOutcome =
            serde_json::from_slice(&body).unwrap();
        assert!(outcome.base.is_none());
        assert!(outcome.warnings.is_empty());
        assert!(!outcome.restarted);
        let stored = state.service.store().read().await;
        let session = stored.get_session(&sid).unwrap();
        assert_eq!(session.title, "edited");
        assert_eq!(session.program, before);
        assert_eq!(session.pending_program.as_deref(), Some("codex"));
        assert!(session.keep_alive);
    }

    #[tokio::test]
    async fn edit_patch_does_not_echo_malformed_program_credentials() {
        use axum::body::Body;
        use axum::http::Request;
        let dir = TempDir::new().unwrap();
        let (state, sid) = seeded_state(&dir);
        let secret = "test-credential-must-not-be-echoed";
        let body = serde_json::json!({ "op": "edit", "title": "edited", "program": {"token": secret},
            "section": null, "keep_alive": false });
        let req = Request::patch(format!("/sessions/{}", sid.as_uuid()))
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let (status, body) = crate::handlers::test_support::send(router(state), req).await;
        assert_eq!(status, 400);
        assert!(!String::from_utf8(body).unwrap().contains(secret));
    }

    #[tokio::test]
    async fn unread_marks_seeded_sessions() {
        use axum::body::Body;
        use axum::http::Request;
        let dir = TempDir::new().unwrap();
        let (state, sid) = seeded_state(&dir);

        // Precondition: the seeded session is not unread.
        let before = state.service.list_sessions(true).await.unwrap();
        assert!(!before.iter().find(|s| s.session_id == sid).unwrap().unread);

        let body = serde_json::json!({ "ids": [sid.as_uuid().to_string()] }).to_string();
        let req = Request::post("/sessions/unread")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap();
        let (status, _) = crate::handlers::test_support::send(router(state.clone()), req).await;
        assert_eq!(status, 204);

        let after = state.service.list_sessions(true).await.unwrap();
        assert!(
            after.iter().find(|s| s.session_id == sid).unwrap().unread,
            "session should be marked unread after POST /sessions/unread"
        );
    }

    #[tokio::test]
    async fn unread_bad_uuid_is_400() {
        use axum::body::Body;
        use axum::http::Request;
        let dir = TempDir::new().unwrap();
        let body = serde_json::json!({ "ids": ["not-a-uuid"] }).to_string();
        let req = Request::post("/sessions/unread")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap();
        let (status, _) = crate::handlers::test_support::send(router(test_state(&dir)), req).await;
        assert_eq!(status, 400);
    }

    #[tokio::test]
    async fn list_empty_is_200_empty_array() {
        let dir = TempDir::new().unwrap();
        let (status, body) = do_get(router(test_state(&dir)), "/sessions").await;
        assert_eq!(status, 200);
        // `SessionInfo` is `Serialize`-only (a response DTO), so assert the
        // wire shape directly: an empty state yields an empty JSON array.
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value, serde_json::json!([]));
    }

    #[tokio::test]
    async fn find_loose_unknown_is_404() {
        let dir = TempDir::new().unwrap();
        let (status, _) = do_get(router(test_state(&dir)), "/sessions/find?q=nope").await;
        assert_eq!(status, 404);
    }

    #[tokio::test]
    async fn find_exact_unknown_is_404() {
        let dir = TempDir::new().unwrap();
        let (status, _) =
            do_get(router(test_state(&dir)), "/sessions/find?q=nope&exact=true").await;
        assert_eq!(status, 404);
    }

    #[tokio::test]
    async fn detail_unknown_is_404() {
        let dir = TempDir::new().unwrap();
        let (status, _) = do_get(router(test_state(&dir)), "/sessions/nope/detail").await;
        assert_eq!(status, 404);
    }

    #[tokio::test]
    async fn pane_unknown_is_404() {
        let dir = TempDir::new().unwrap();
        let (status, _) = do_get(router(test_state(&dir)), "/sessions/nope/pane").await;
        assert_eq!(status, 404);
    }

    /// `detail` resolves its `{q}` as a free-form query, so an unmatched value
    /// is a 404 (not a parse error) — even one shaped like a non-UUID.
    #[tokio::test]
    async fn detail_treats_non_uuid_as_query_404() {
        let dir = TempDir::new().unwrap();
        let (status, _) = do_get(router(test_state(&dir)), "/sessions/not-a-uuid/detail").await;
        assert_eq!(status, 404);
    }

    /// A malformed UUID on an *id* route (kill) maps to 400 via
    /// `parse_session_id`, before any service call.
    #[tokio::test]
    async fn kill_invalid_id_is_400() {
        use axum::body::Body;
        use axum::http::Request;
        let dir = TempDir::new().unwrap();
        let req = Request::post("/sessions/not-a-uuid/kill")
            .body(Body::empty())
            .unwrap();
        let (status, _) = crate::handlers::test_support::send(router(test_state(&dir)), req).await;
        assert_eq!(status, 400);
    }

    /// The no-resume restart is its own route, reachable and validated like the
    /// resuming one: a malformed id is a 400 and an unknown-but-valid id a 404
    /// (which also proves `/restart-fresh` is wired and not shadowed by
    /// `/restart`).
    #[tokio::test]
    async fn restart_fresh_validates_id_and_is_wired() {
        use axum::body::Body;
        use axum::http::Request;
        let dir = TempDir::new().unwrap();

        let req = Request::post("/sessions/not-a-uuid/restart-fresh")
            .body(Body::empty())
            .unwrap();
        let (status, _) = crate::handlers::test_support::send(router(test_state(&dir)), req).await;
        assert_eq!(status, 400);

        let req = Request::post(format!("/sessions/{}/restart-fresh", uuid::Uuid::new_v4()))
            .body(Body::empty())
            .unwrap();
        let (status, _) = crate::handlers::test_support::send(router(test_state(&dir)), req).await;
        assert_eq!(status, 404);
    }

    /// Preview for an unknown session id is a 404.
    #[tokio::test]
    async fn preview_unknown_is_404() {
        let dir = TempDir::new().unwrap();
        let (status, _) = do_get(
            router(test_state(&dir)),
            &format!("/sessions/{}/preview", uuid::Uuid::new_v4()),
        )
        .await;
        assert_eq!(status, 404);
    }

    /// `read` on an unknown session id is a 404.
    #[tokio::test]
    async fn read_unknown_is_404() {
        use axum::body::Body;
        use axum::http::Request;
        let dir = TempDir::new().unwrap();
        let req = Request::post(format!("/sessions/{}/read", uuid::Uuid::new_v4()))
            .body(Body::empty())
            .unwrap();
        let (status, _) = crate::handlers::test_support::send(router(test_state(&dir)), req).await;
        assert_eq!(status, 404);
    }

    /// A rename PATCH with an empty title is a 400 (from the service guard).
    #[tokio::test]
    async fn patch_rename_empty_title_is_400() {
        use axum::body::Body;
        use axum::http::Request;
        let dir = TempDir::new().unwrap();
        // Well-formed id that doesn't exist would 404, so use a real one? The
        // empty-title guard fires before the existence check, so any id yields
        // 400 here.
        let req = Request::patch(format!("/sessions/{}", uuid::Uuid::new_v4()))
            .header("content-type", "application/json")
            .body(Body::from(r#"{"op":"rename","title":"  "}"#))
            .unwrap();
        let (status, _) = crate::handlers::test_support::send(router(test_state(&dir)), req).await;
        assert_eq!(status, 400);
    }

    /// A `change_program` PATCH with an empty program is a 400 (service guard,
    /// which fires before the relaunch so no tmux is touched).
    #[tokio::test]
    async fn patch_change_program_empty_is_400() {
        use axum::body::Body;
        use axum::http::Request;
        let dir = TempDir::new().unwrap();
        let req = Request::patch(format!("/sessions/{}", uuid::Uuid::new_v4()))
            .header("content-type", "application/json")
            .body(Body::from(r#"{"op":"change_program","program":"  "}"#))
            .unwrap();
        let (status, _) = crate::handlers::test_support::send(router(test_state(&dir)), req).await;
        assert_eq!(status, 400);
    }

    /// A base retarget on an unknown session is a 404, not the rejection enum's
    /// 400/409 — the service maps `SessionNotFound` back to `NotFound` precisely
    /// so this convention holds across every session route.
    #[tokio::test]
    async fn post_base_unknown_session_is_404() {
        use axum::body::Body;
        use axum::http::Request;
        let dir = TempDir::new().unwrap();
        let req = Request::post(format!("/sessions/{}/base", uuid::Uuid::new_v4()))
            .header("content-type", "application/json")
            .body(Body::from(r#"{"parent_session_id":null}"#))
            .unwrap();
        let (status, _) = crate::handlers::test_support::send(router(test_state(&dir)), req).await;
        assert_eq!(status, 404);
    }

    /// A `set_section` PATCH on an unknown session id is a 404 (existence check).
    #[tokio::test]
    async fn patch_set_section_unknown_is_404() {
        use axum::body::Body;
        use axum::http::Request;
        let dir = TempDir::new().unwrap();
        let req = Request::patch(format!("/sessions/{}", uuid::Uuid::new_v4()))
            .header("content-type", "application/json")
            .body(Body::from(r#"{"op":"set_section","section":null}"#))
            .unwrap();
        let (status, _) = crate::handlers::test_support::send(router(test_state(&dir)), req).await;
        assert_eq!(status, 404);
    }
}
