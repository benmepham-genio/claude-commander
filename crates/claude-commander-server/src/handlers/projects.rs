//! Project handlers.
//!
//! Thin wrappers over `CommanderService`: `add_project`, `scan_directory`,
//! `ensure_project`, `list_projects`, `remove_project`, `list_branches`, and
//! project `preview`.

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use claude_commander_core::api::{BranchInfo, PreviewData, PreviewTarget, ProjectInfo};
use claude_commander_core::session::ProjectId;
use claude_commander_protocol::api::{AddProjectRequest, CreatedId, ScanResponse};
use claude_commander_protocol::workspace::SetProjectWorkspace;
use serde::Deserialize;

use crate::error::ApiError;
use crate::handlers::{parse_id, run_local};
use crate::state::AppState;

/// Parse a `{id}` path param into a [`ProjectId`], mapping a malformed UUID to a
/// 400 rather than a 404 (the client sent a syntactically bad id). See
/// [`parse_id`].
fn parse_project_id(raw: &str) -> Result<ProjectId, ApiError> {
    parse_id(raw, "project", ProjectId::from_uuid)
}

/// `POST /projects` (`{ path, workspace? }`) → `add_project` → 201
/// `{ "id": ... }`. `workspace` tags the new project (absent = Main).
pub async fn add(
    State(state): State<AppState>,
    Json(body): Json<AddProjectRequest>,
) -> Result<Response, ApiError> {
    // `add_project` builds a `gix::Repository` (non-`Send`) across an await.
    let id =
        run_local(
            move || async move { state.service.add_project(body.path, body.workspace).await },
        )
        .await?;
    Ok((StatusCode::CREATED, Json(CreatedId { id })).into_response())
}

/// `POST /projects/ensure` (`{ path, workspace? }`) → `ensure_project` → 201
/// `{ "id": ... }`. `workspace` tags the project only if this call registers it.
pub async fn ensure(
    State(state): State<AppState>,
    Json(body): Json<AddProjectRequest>,
) -> Result<Response, ApiError> {
    let id = run_local(move || async move {
        state
            .service
            .ensure_project(body.path, body.workspace)
            .await
    })
    .await?;
    Ok((StatusCode::CREATED, Json(CreatedId { id })).into_response())
}

/// `PUT /projects/{id}/workspace` (`{ workspace }`, `null` = Main) → 204.
/// An unknown workspace name is defined on this server on the way (self-heal);
/// a reserved or malformed one is a 400, an unknown project a 404.
pub async fn set_workspace(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<SetProjectWorkspace>,
) -> Result<StatusCode, ApiError> {
    let id = parse_project_id(&id)?;
    state
        .service
        .set_project_workspace(&id, body.workspace)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /projects/scan` (body `{ path, workspace? }`) → `scan_directory`. A
/// POST, not a GET, because scanning *mutates* state: it adds every discovered
/// repo as a project, tagged with `workspace` (absent = Main) like `POST
/// /projects`.
pub async fn scan(
    State(state): State<AppState>,
    Json(body): Json<AddProjectRequest>,
) -> Result<Json<ScanResponse>, ApiError> {
    // `scan_directory` adds discovered repos via `add_project` (non-`Send` gix).
    let result = run_local(move || async move {
        state
            .service
            .scan_directory(&body.path, body.workspace)
            .await
    })
    .await?;
    Ok(Json(ScanResponse {
        added: result.added,
        skipped: result.skipped,
    }))
}

/// `GET /projects` → `list_projects`.
pub async fn list(State(state): State<AppState>) -> Json<Vec<ProjectInfo>> {
    Json(state.service.list_projects().await)
}

/// `DELETE /projects/{id}` → `remove_project` → 204.
pub async fn delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let id = parse_project_id(&id)?;
    // `remove_project` now tears down each session's worktree (opens a `gix`
    // repo → non-`Send` across an await), so drive it through `run_local`.
    run_local(move || async move { state.service.remove_project(&id).await }).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
pub struct BranchesQuery {
    /// Run `git fetch origin` first so newly-pushed remote branches appear.
    #[serde(default)]
    pub fetch: bool,
}

/// `GET /projects/{id}/branches?fetch=` → `list_branches`.
pub async fn branches(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<BranchesQuery>,
) -> Result<Json<Vec<BranchInfo>>, ApiError> {
    let id = parse_project_id(&id)?;
    // `list_branches` opens a `gix::Repository` (non-`Send`) across an await.
    let branches =
        run_local(move || async move { state.service.list_branches(&id, q.fetch).await }).await?;
    Ok(Json(branches))
}

/// `GET /projects/{id}/preview` → project `preview`.
pub async fn preview(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<super::sessions::PreviewQuery>,
) -> Result<Json<PreviewData>, ApiError> {
    let id = parse_project_id(&id)?;
    Ok(Json(
        state
            .service
            .preview_part(PreviewTarget::Project(id), q.part)
            .await?,
    ))
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use axum::{
        Router,
        routing::{get, post},
    };
    use tempfile::TempDir;

    use crate::handlers::test_support::{get as do_get, json, send, test_state};

    /// Scanning an empty temp dir touches no repos → 200 `{added:0, skipped:0}`.
    /// (`scan_directory` is filesystem-only; it needs no tmux.) Scanning mutates
    /// state (adds discovered repos), so the route is a POST with a JSON body.
    #[tokio::test]
    async fn scan_empty_dir_is_200_zero_counts() {
        let dir = TempDir::new().unwrap();
        let scan_target = TempDir::new().unwrap();
        let router = Router::new()
            .route("/projects/scan", post(super::scan))
            .with_state(test_state(&dir));
        let req = Request::post("/projects/scan")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({ "path": scan_target.path() }).to_string(),
            ))
            .unwrap();
        let (status, body) = send(router, req).await;
        assert_eq!(status, 200);
        let resp: claude_commander_protocol::api::ScanResponse = json(&body);
        assert_eq!(resp.added, 0);
        assert_eq!(resp.skipped, 0);
    }

    /// The scan body carries the caller's workspace like `POST /projects`: it
    /// is read (a reserved name is a 400 before anything is scanned) and a
    /// valid one is defined on this server.
    #[tokio::test]
    async fn scan_reads_the_workspace() {
        let dir = TempDir::new().unwrap();
        let scan_target = TempDir::new().unwrap();
        let state = test_state(&dir);
        let router = Router::new()
            .route("/projects/scan", post(super::scan))
            .with_state(state.clone());
        let scan = |workspace: &str| {
            Request::post("/projects/scan")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({ "path": scan_target.path(), "workspace": workspace })
                        .to_string(),
                ))
                .unwrap()
        };
        let (status, _) = send(router.clone(), scan("main")).await;
        assert_eq!(status, 400);
        let (status, _) = send(router, scan("Work")).await;
        assert_eq!(status, 200);
        let names: Vec<String> = state
            .service
            .read_config()
            .workspaces
            .into_iter()
            .map(|w| w.name)
            .collect();
        assert_eq!(names, ["Work"]);
    }

    /// `GET /projects` over empty state is a 200 empty array.
    #[tokio::test]
    async fn list_empty_is_200_empty_array() {
        let dir = TempDir::new().unwrap();
        let router = Router::new()
            .route("/projects", get(super::list))
            .with_state(test_state(&dir));
        let (status, body) = do_get(router, "/projects").await;
        assert_eq!(status, 200);
        let projects: Vec<claude_commander_core::api::ProjectInfo> = json(&body);
        assert!(projects.is_empty());
    }

    /// Deleting an unknown-but-well-formed project id is a 404.
    #[tokio::test]
    async fn delete_unknown_is_404() {
        let dir = TempDir::new().unwrap();
        let router = Router::new()
            .route("/projects/{id}", axum::routing::delete(super::delete))
            .with_state(test_state(&dir));
        let req = Request::delete(format!("/projects/{}", uuid::Uuid::new_v4()))
            .body(Body::empty())
            .unwrap();
        let (status, _) = send(router, req).await;
        assert_eq!(status, 404);
    }

    /// A malformed project id on the delete route is a 400.
    #[tokio::test]
    async fn delete_invalid_id_is_400() {
        let dir = TempDir::new().unwrap();
        let router = Router::new()
            .route("/projects/{id}", axum::routing::delete(super::delete))
            .with_state(test_state(&dir));
        let req = Request::delete("/projects/not-a-uuid")
            .body(Body::empty())
            .unwrap();
        let (status, _) = send(router, req).await;
        assert_eq!(status, 400);
    }

    /// `PUT /projects/{id}/workspace` moves the project and self-heals the
    /// definition; `null` moves it back to Main.
    #[tokio::test]
    async fn set_workspace_moves_the_project() {
        use claude_commander_core::session::Project;
        let dir = TempDir::new().unwrap();
        let state = test_state(&dir);
        let project = Project::new("repo", std::path::PathBuf::from("/repo"), "main");
        let pid = project.id;
        state
            .service
            .store()
            .mutate(move |s| s.add_project(project))
            .await
            .unwrap();
        let router = Router::new()
            .route(
                "/projects/{id}/workspace",
                axum::routing::put(super::set_workspace),
            )
            .with_state(state.clone());
        let put = |body: serde_json::Value| {
            Request::put(format!("/projects/{}/workspace", pid.as_uuid()))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap()
        };

        let (status, _) = send(
            router.clone(),
            put(serde_json::json!({"workspace": "Work"})),
        )
        .await;
        assert_eq!(status, 204);
        let snap = state.service.snapshot().await.unwrap();
        assert_eq!(snap.projects[0].workspace.as_deref(), Some("Work"));
        assert_eq!(snap.workspaces[0].name, "Work");

        let (status, _) = send(router.clone(), put(serde_json::json!({"workspace": null}))).await;
        assert_eq!(status, 204);
        let snap = state.service.snapshot().await.unwrap();
        assert_eq!(snap.projects[0].workspace, None);

        let (status, _) = send(
            router.clone(),
            put(serde_json::json!({"workspace": "main"})),
        )
        .await;
        assert_eq!(status, 400, "a reserved name is refused");

        let unknown = Request::put(format!("/projects/{}/workspace", uuid::Uuid::new_v4()))
            .header("content-type", "application/json")
            .body(Body::from(r#"{"workspace":"Work"}"#))
            .unwrap();
        assert_eq!(send(router, unknown).await.0, 404);
    }

    /// Branch listing for an unknown project id is a 404.
    #[tokio::test]
    async fn branches_unknown_project_is_404() {
        let dir = TempDir::new().unwrap();
        let router = Router::new()
            .route("/projects/{id}/branches", get(super::branches))
            .with_state(test_state(&dir));
        let (status, _) = do_get(
            router,
            &format!("/projects/{}/branches", uuid::Uuid::new_v4()),
        )
        .await;
        assert_eq!(status, 404);
    }
}
