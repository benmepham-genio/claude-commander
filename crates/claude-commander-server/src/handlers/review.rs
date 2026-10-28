//! Review-diff + comment handlers.
//!
//! Thin wrappers over `CommanderService`: `open_review`,
//! `refresh_review_if_changed`, comment CRUD
//! (`list_comments`/`create_comment`/`delete_comment`), `apply_comments`, and
//! `toggle_file_reviewed`.
//!
//! `toggle_file_reviewed` takes a [`ToggleReviewed`] body carrying only the
//! display path — the server resolves the file in the *current* review diff
//! itself, so clients never echo (or cache) the full `FileDiff` and a mark
//! can't be recorded against a stale copy of the file.

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use claude_commander_core::api::{NewComment, ReviewSnapshot, ToggleReviewed};
use claude_commander_core::comment::Comment;
use claude_commander_protocol::api::{CreatedId, ReviewedToggle};
use serde::Deserialize;
use uuid::Uuid;

use crate::error::ApiError;
use crate::state::AppState;

use super::parse_session_id;

/// `GET /sessions/{id}/review` → `open_review`.
pub async fn open(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<ReviewSnapshot>, ApiError> {
    let id = parse_session_id(&id)?;
    Ok(Json(state.service.open_review(&id).await?))
}

#[derive(Debug, Deserialize)]
pub struct RefreshQuery {
    #[serde(default)]
    pub prev_hash: u64,
}

/// `GET /sessions/{id}/review/refresh?prev_hash=` → `refresh_review_if_changed`
/// (204 when unchanged, else the fresh snapshot).
pub async fn refresh(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<RefreshQuery>,
) -> Result<Response, ApiError> {
    let id = parse_session_id(&id)?;
    match state
        .service
        .refresh_review_if_changed(&id, q.prev_hash)
        .await?
    {
        Some(snapshot) => Ok(Json(snapshot).into_response()),
        None => Ok(StatusCode::NO_CONTENT.into_response()),
    }
}

/// `GET /sessions/{id}/comments` → `list_comments`.
pub async fn list_comments(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<Comment>>, ApiError> {
    let id = parse_session_id(&id)?;
    Ok(Json(state.service.list_comments(&id).await?))
}

/// `POST /sessions/{id}/comments` → `create_comment` → 201 `{ "id": ... }`.
///
/// The body deserializes straight into [`NewComment`] (a shared wire type in
/// `claude-commander-protocol`) — no server-side mirror DTO.
pub async fn create_comment(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<NewComment>,
) -> Result<Response, ApiError> {
    let id = parse_session_id(&id)?;
    let cid = state.service.create_comment(&id, body).await?;
    Ok((StatusCode::CREATED, Json(CreatedId { id: cid })).into_response())
}

/// `DELETE /sessions/{id}/comments/{cid}` → `delete_comment` → 204.
pub async fn delete_comment(
    State(state): State<AppState>,
    Path((id, cid)): Path<(String, Uuid)>,
) -> Result<StatusCode, ApiError> {
    let id = parse_session_id(&id)?;
    state.service.delete_comment(&id, cid).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /sessions/{id}/comments/apply` → `apply_comments` → `ApplyOutcome`.
pub async fn apply(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let id = parse_session_id(&id)?;
    let outcome = state.service.apply_comments(&id).await?;
    Ok(Json(outcome).into_response())
}

/// `POST /sessions/{id}/files/reviewed` → `toggle_file_reviewed_by_path`, or
/// `set_file_reviewed_by_path` when the body says which state it wants →
/// [`ReviewedToggle`]. The body is a [`ToggleReviewed`] display path; the
/// server resolves the file in the current review diff (404 when the path
/// isn't in it).
pub async fn toggle_reviewed(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<ToggleReviewed>,
) -> Result<Response, ApiError> {
    let id = parse_session_id(&id)?;
    let reviewed = match body.reviewed {
        Some(want) => {
            state
                .service
                .set_file_reviewed_by_path(&id, &body.display_path, want)
                .await?
        }
        None => {
            state
                .service
                .toggle_file_reviewed_by_path(&id, &body.display_path)
                .await?
        }
    };
    Ok(Json(ReviewedToggle { reviewed }).into_response())
}

/// `GET /sessions/{id}/files/reviewed` → `reviewed_marks` → the session's
/// stored [`ReviewedFileMark`](claude_commander_protocol::api::ReviewedFileMark)s, unpruned; an open review view polls this and
/// keeps only marks whose hash matches the diff it is showing.
pub async fn reviewed_marks(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let id = parse_session_id(&id)?;
    let marks = state.service.reviewed_marks(&id).await?;
    Ok(Json(marks).into_response())
}

/// `GET /comments/pending` → session ids with at least one not-yet-applied
/// review comment, sorted for a deterministic response.
pub async fn pending(
    State(state): State<AppState>,
) -> Result<Json<Vec<claude_commander_core::session::SessionId>>, ApiError> {
    let mut ids: Vec<_> = state
        .service
        .sessions_with_pending_comments()
        .await?
        .into_iter()
        .collect();
    ids.sort();
    Ok(Json(ids))
}

#[cfg(test)]
mod tests {
    use axum::{Router, routing::get};
    use claude_commander_core::comment::Comment;
    use tempfile::TempDir;

    use crate::handlers::test_support::{get as do_get, json, test_state};

    /// `list_comments` on an unseen session returns an empty list (the store
    /// treats an absent file as empty), so the route is 200 with `[]`.
    #[tokio::test]
    async fn list_comments_empty_is_200_empty_array() {
        let dir = TempDir::new().unwrap();
        let id = uuid::Uuid::new_v4();
        let router = Router::new()
            .route("/sessions/{id}/comments", get(super::list_comments))
            .with_state(test_state(&dir));
        let (status, body) = do_get(router, &format!("/sessions/{id}/comments")).await;
        assert_eq!(status, 200);
        let comments: Vec<Comment> = json(&body);
        assert!(comments.is_empty());
    }

    /// `GET /comments/pending` over empty state is a 200 empty array.
    #[tokio::test]
    async fn pending_comments_empty_is_200_empty_array() {
        let dir = TempDir::new().unwrap();
        let router = Router::new()
            .route("/comments/pending", get(super::pending))
            .with_state(test_state(&dir));
        let (status, body) = do_get(router, "/comments/pending").await;
        assert_eq!(status, 200);
        let ids: Vec<claude_commander_core::session::SessionId> = json(&body);
        assert!(ids.is_empty());
    }

    /// A malformed session id on an id-route maps to 400, not 404.
    #[tokio::test]
    async fn comments_bad_id_is_400() {
        let dir = TempDir::new().unwrap();
        let router = Router::new()
            .route("/sessions/{id}/comments", get(super::list_comments))
            .with_state(test_state(&dir));
        let (status, _) = do_get(router, "/sessions/not-a-uuid/comments").await;
        assert_eq!(status, 400);
    }

    /// `toggle_reviewed` takes only a display path — the server resolves the
    /// `FileDiff` itself from the current review diff, so clients never echo
    /// (or cache) the full file. Unknown session → 404 through the same body.
    #[tokio::test]
    async fn toggle_reviewed_takes_display_path_unknown_session_is_404() {
        let dir = TempDir::new().unwrap();
        let id = uuid::Uuid::new_v4();
        let router = Router::new()
            .route(
                "/sessions/{id}/files/reviewed",
                axum::routing::post(super::toggle_reviewed),
            )
            .with_state(test_state(&dir));
        let req = axum::http::Request::post(format!("/sessions/{id}/files/reviewed"))
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                serde_json::json!({ "display_path": "src/main.rs" }).to_string(),
            ))
            .unwrap();
        let (status, _) = crate::handlers::test_support::send(router, req).await;
        assert_eq!(
            status, 404,
            "a display_path body for an unknown session must 404 (not 422)"
        );
    }

    /// A body that says which state it wants is accepted too (it reaches the
    /// service, which 404s the unknown session) rather than rejected as 422.
    #[tokio::test]
    async fn toggle_reviewed_accepts_an_explicit_state() {
        let dir = TempDir::new().unwrap();
        let id = uuid::Uuid::new_v4();
        let router = Router::new()
            .route(
                "/sessions/{id}/files/reviewed",
                axum::routing::post(super::toggle_reviewed),
            )
            .with_state(test_state(&dir));
        let req = axum::http::Request::post(format!("/sessions/{id}/files/reviewed"))
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                serde_json::json!({ "display_path": "src/main.rs", "reviewed": true }).to_string(),
            ))
            .unwrap();
        let (status, _) = crate::handlers::test_support::send(router, req).await;
        assert_eq!(status, 404);
    }

    /// `GET /sessions/{id}/files/reviewed` 404s an unknown session, as the
    /// POST does: a poller must not read a deleted session as "no marks".
    #[tokio::test]
    async fn reviewed_marks_unknown_session_is_404() {
        let dir = TempDir::new().unwrap();
        let id = uuid::Uuid::new_v4();
        let router = Router::new()
            .route("/sessions/{id}/files/reviewed", get(super::reviewed_marks))
            .with_state(test_state(&dir));
        let (status, _) = do_get(router, &format!("/sessions/{id}/files/reviewed")).await;
        assert_eq!(status, 404);
    }
}
