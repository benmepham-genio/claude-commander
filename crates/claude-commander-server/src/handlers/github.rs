//! Hosted-repository listing and repository-clone handlers.
//!
//! Three thin wrappers over `CommanderService`: [`repos`] lists what the
//! authenticated `gh` user can clone, [`clone`] starts a clone, and
//! [`clone_status`] is the poll a frontend drives until the job reaches a
//! terminal state.
//!
//! **None of these needs [`run_local`](crate::handlers::run_local)**, unlike
//! every handler in the neighbouring [`projects`](super::projects) module. That
//! is not an oversight: the non-`Send` `gix` work in this feature is confined to
//! the background job future, which `CloneJobs::spawn` already requires to be
//! `Send + 'static` (`git/clone_jobs.rs:147-149`), so it can never be held
//! across one of these handlers' awaits. What is left here only runs a
//! subprocess, creates a directory, and reads an in-memory map.

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use claude_commander_protocol::github::{CloneJobId, CloneRequest, GithubRepo};
use claude_commander_protocol::hosting::RepositoryListing;

use crate::error::{ApiError, error_response};
use crate::extract::SafeJson;
use crate::handlers::parse_id;
use crate::state::AppState;

/// `GET /github/repos` → `list_github_repos`.
///
/// Failures keep their core meaning: a missing `gh` is a 503 (the backing tool
/// is unavailable, as with tmux), anything else gh reports is a 500 carrying
/// gh's own message.
pub async fn repos(State(state): State<AppState>) -> Result<Json<Vec<GithubRepo>>, ApiError> {
    Ok(Json(state.service.list_github_repos().await?))
}

/// `GET /repositories` → provider-neutral repository listing.
pub async fn repositories(
    State(state): State<AppState>,
) -> Result<Json<RepositoryListing>, ApiError> {
    Ok(Json(state.service.list_repositories().await?))
}

/// `POST /projects/clone` → `start_clone` → **202** with the created
/// [`CloneJob`](claude_commander_protocol::github::CloneJob).
///
/// 202 rather than 201: the clone has been *accepted* and is running, and the
/// resource it will create does not exist yet. The body is the whole job rather
/// than just its id so a client gets the initial status in the same round trip
/// and can start polling [`clone_status`] without a second request to learn
/// where it stands.
///
/// **A pre-flight "destination already exists" is a 202 too, not a 409.** The
/// client learns that outcome by polling, exactly as it learns a success or a
/// failure — one code path instead of two, and the `is_git_repo` flag it needs to
/// offer "register the existing checkout" arrives by the same route as everything
/// else. A 409 would buy nothing and cost the client a second path.
///
/// **The status in this body is not a terminal status.** Every outcome —
/// including an occupied destination, which `start_clone` already knows about
/// before it returns — is reported through the spawned job, which has not been
/// scheduled by the time this response is built. So the body says `running`
/// essentially always, and a client that reads it as final will miss every
/// result. The body exists to hand over the id, the destination and the redacted
/// label in one round trip; the *outcome* comes from [`clone_status`].
///
/// A repeat POST for a destination that is already being cloned is also a 202
/// carrying the *in-flight* job: `CloneJobs::spawn` dedupes by destination and
/// returns the existing id, so a double-submit joins the running clone instead of
/// starting a second one.
pub async fn clone(
    State(state): State<AppState>,
    // `SafeJson`, not `Json`: `source` routinely carries a credential, and axum's
    // own extractor rejection quotes the offending value verbatim — below every
    // redaction in this codebase, since a `FromRequest` rejection short-circuits
    // before the handler runs. See `crate::extract::SafeJson`.
    SafeJson(req): SafeJson<CloneRequest>,
) -> Result<Response, ApiError> {
    // The 400 for a rejected source comes from `start_clone`'s own validation,
    // rendered through `ApiError`. Deliberately not re-derived here from
    // `req.source`: that string may be a credentialed URL, and the rejection
    // messages are redacted where they are *built* (in protocol) precisely so no
    // hop like this one has to remember to do it. The raw request is likewise
    // never logged.
    // `start_clone` answers with the whole job, not just its id: the 202 body and
    // the `CommanderBackend` seam need the same shape, so the service owns that
    // read (including the documented-unreachable "job vanished" case) rather than
    // each caller repeating it.
    let job = state.service.start_clone(req).await?;
    Ok((StatusCode::ACCEPTED, Json(job)).into_response())
}

/// `GET /projects/clone/{job}` → `clone_job`.
///
/// A malformed id is a 400 and an unknown-but-well-formed one a 404, matching
/// the session/project id routes.
pub async fn clone_status(
    State(state): State<AppState>,
    Path(job): Path<String>,
) -> Result<Response, ApiError> {
    let id = parse_clone_job_id(&job)?;
    Ok(match state.service.clone_job(id).await {
        Some(job) => Json(job).into_response(),
        // Not backed by a `CoreError`: `clone_job` reports absence as `None`, and
        // whether that is a 404 is an HTTP decision, not core's. `error_response`
        // keeps the shared `{"error":{...}}` envelope a client parses — the same
        // reason the bearer-auth 401 uses it.
        None => error_response(StatusCode::NOT_FOUND, "clone", format!("no clone job {id}")),
    })
}

/// Parse a `{job}` path param into a [`CloneJobId`].
fn parse_clone_job_id(raw: &str) -> Result<CloneJobId, ApiError> {
    parse_id(raw, "clone job", CloneJobId::from_uuid)
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use axum::{Router, routing::get, routing::post};
    use claude_commander_protocol::github::{CloneJob, CloneJobId, CloneStatus};
    use tempfile::TempDir;

    use crate::handlers::test_support::{get as do_get, json, send, test_state};
    use crate::state::AppState;

    const CLONE_PATH: &str = "/projects/clone";

    /// A router carrying both clone routes, so a test can POST a clone and then
    /// poll the id it got back — the round trip Tasks 9-13 depend on.
    ///
    /// Takes the state rather than building it, because `send` consumes the
    /// router: polling the id from a POST needs a second router over the **same**
    /// service, since the job registry lives in it. A fresh `test_state` would
    /// have an empty registry and turn that assertion into a guaranteed 404.
    fn clone_router(state: AppState) -> Router {
        Router::new()
            .route(CLONE_PATH, post(super::clone))
            .route("/projects/clone/{job}", get(super::clone_status))
            .with_state(state)
    }

    /// Poll the status route until the job leaves `Running`.
    ///
    /// **The 202's own body is not guaranteed to be terminal**, not even for a
    /// destination that was already occupied: `start_clone` reports every outcome
    /// through the spawned job, which has not been scheduled by the time the
    /// response is built, so the body almost always says `running`. That is the
    /// design — one polling path for every outcome — and it is what a client has to
    /// implement, so the test drives it the same way rather than asserting a status
    /// the wire does not promise.
    async fn poll_until_terminal(state: AppState, id: CloneJobId) -> CloneJob {
        for _ in 0..200 {
            let (status, body) = do_get(
                clone_router(state.clone()),
                &format!("/projects/clone/{id}"),
            )
            .await;
            assert_eq!(status, 200, "body={}", String::from_utf8_lossy(&body));
            let job: CloneJob = json(&body);
            if job.status != CloneStatus::Running {
                return job;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("clone job {id} never reached a terminal state");
    }

    /// POST a `{ source: { kind: "url", url } }` clone request.
    fn post_url_clone(url: &str) -> Request<Body> {
        Request::post(CLONE_PATH)
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({ "source": { "kind": "url", "url": url } }).to_string(),
            ))
            .unwrap()
    }

    /// POST a `{ source: { kind: "github", full_name } }` clone request.
    fn post_slug_clone(full_name: &str) -> Request<Body> {
        Request::post(CLONE_PATH)
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({ "source": { "kind": "github", "full_name": full_name } })
                    .to_string(),
            ))
            .unwrap()
    }

    /// An accepted clone is a **202** carrying the whole job, and the id in that
    /// job is immediately pollable on the status route.
    ///
    /// The source is a `file://` URL inside the temp dir: it passes validation
    /// (so a job is really created) and needs no network. Whether the clone then
    /// succeeds is irrelevant here — the response is built before the subprocess
    /// finishes, which is the point of a 202.
    #[tokio::test]
    async fn clone_returns_202_with_a_job() {
        let dir = TempDir::new().unwrap();
        let state = test_state(&dir);
        let source = format!("file://{}", dir.path().join("upstream.git").display());

        let (status, body) = send(clone_router(state.clone()), post_url_clone(&source)).await;
        assert_eq!(status, 202, "body={}", String::from_utf8_lossy(&body));
        let job: CloneJob = json(&body);
        assert_eq!(job.source_label, source);
        assert!(
            job.dest.starts_with(dir.path()),
            "clone escaped the temp dir: {}",
            job.dest.display()
        );
        assert_eq!(job.dest.file_name().unwrap(), "upstream");

        // The id in the 202 is what a client polls, so it must resolve on the
        // status route — the whole reason the body carries the job and not just
        // an acknowledgement.
        let (status, body) =
            do_get(clone_router(state), &format!("/projects/clone/{}", job.id)).await;
        assert_eq!(status, 200, "body={}", String::from_utf8_lossy(&body));
        assert_eq!(json::<CloneJob>(&body).id, job.id);
    }

    /// An occupied destination is a **202**, not a 409, and resolves to
    /// `destination_exists` on the *polling* route.
    ///
    /// This is the one outcome that might reasonably have been a 409, and it
    /// deliberately is not: a client that has to handle a conflict status as well
    /// as a job status needs two code paths for one user-visible situation. Here
    /// it gets `is_git_repo` — which decides whether to offer "register the
    /// existing checkout" or "pick another name" — through the same poll that
    /// delivers a success or a failure.
    #[tokio::test]
    async fn destination_exists_is_still_202_and_polls_to_a_terminal_job() {
        let dir = TempDir::new().unwrap();
        let state = test_state(&dir);
        let dest = dir.path().join("projects").join("taken");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("precious"), "mine\n").unwrap();
        let source = format!("file://{}", dir.path().join("taken").display());

        let (status, body) = send(clone_router(state.clone()), post_url_clone(&source)).await;
        assert_eq!(status, 202, "body={}", String::from_utf8_lossy(&body));
        let accepted: CloneJob = json(&body);
        assert_eq!(accepted.dest, dest);

        let job = poll_until_terminal(state, accepted.id).await;
        assert_eq!(
            job.status,
            CloneStatus::DestinationExists {
                dest: dest.clone(),
                is_git_repo: false,
            }
        );
        // Nothing was cloned over the existing directory.
        assert_eq!(
            std::fs::read_to_string(dest.join("precious")).unwrap(),
            "mine\n"
        );
    }

    /// An unknown-but-well-formed job id is a 404.
    #[tokio::test]
    async fn unknown_job_id_is_404() {
        let dir = TempDir::new().unwrap();
        let (status, _) = do_get(
            clone_router(test_state(&dir)),
            &format!("/projects/clone/{}", uuid::Uuid::new_v4()),
        )
        .await;
        assert_eq!(status, 404);
    }

    /// A malformed job id is a 400 — the client sent something syntactically
    /// wrong, which is a different thing from asking for a job that is gone.
    #[tokio::test]
    async fn malformed_job_id_is_400() {
        let dir = TempDir::new().unwrap();
        let (status, _) =
            do_get(clone_router(test_state(&dir)), "/projects/clone/not-a-uuid").await;
        assert_eq!(status, 400);
    }

    /// A source the validators refuse is a **400**, not a 500: the request is
    /// wrong, not the server. Each of these fails for a different reason
    /// (unsupported scheme, argv hazard, non-slug), so the mapping cannot be
    /// passing by covering only one rejection variant.
    #[tokio::test]
    async fn rejected_url_is_400() {
        let dir = TempDir::new().unwrap();
        for req in [
            post_url_clone("ftp://example.com/o/r"),
            post_url_clone("--upload-pack=evil"),
            post_url_clone("relative/path"),
            post_slug_clone("not-a-slug"),
        ] {
            let uri = req.uri().clone();
            let (status, body) = send(clone_router(test_state(&dir)), req).await;
            assert_eq!(
                status,
                400,
                "uri={uri} body={}",
                String::from_utf8_lossy(&body)
            );
        }
    }

    /// A body that never reaches the handler must not echo a credential either.
    ///
    /// This is the hop *below* every redaction this feature built. A body that is
    /// valid JSON but the wrong shape — `source` as a bare string instead of the
    /// tagged object, an ordinary client mistake — is rejected by the `Json`
    /// extractor's `FromRequest`, which short-circuits with its own
    /// `IntoResponse`. `ApiError`, `clone_source_rejected` and
    /// `redact_credentials` never run, and serde_json's message quotes the
    /// offending value in full. Verified against axum 0.8.9: before `SafeJson`
    /// this returned 422 with
    /// `invalid type: string "https://sizeak:ghp_s3cr3tt0ken@github.com/o/r"`.
    ///
    /// Both shapes below are the same hazard through different serde paths: a
    /// wrong *type* for a field, and an unknown tag value.
    #[tokio::test]
    async fn a_malformed_body_is_400_without_the_token() {
        const SECRET: &str = "ghp_s3cr3tt0ken";
        let credentialed = format!("https://sizeak:{SECRET}@github.com/o/r");
        let dir = TempDir::new().unwrap();

        for body in [
            // `source` as a bare string rather than the tagged object.
            serde_json::json!({ "source": credentialed }),
            // Right shape, unrecognised tag — serde echoes the whole variant.
            serde_json::json!({ "source": { "kind": "clone_url", "url": credentialed } }),
            // A credential in a field that is not `source` at all.
            serde_json::json!({
                "source": { "kind": "url", "url": "https://example.com/o/r" },
                "dest_name": { "unexpected": credentialed },
            }),
        ] {
            let req = Request::post(CLONE_PATH)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap();
            let (status, raw) = send(clone_router(test_state(&dir)), req).await;
            let text = String::from_utf8_lossy(&raw);

            assert_eq!(status, 400, "body={body} response={text}");
            assert!(!text.contains(SECRET), "token leaked: {text}");
            assert!(!text.contains("sizeak:"), "userinfo leaked: {text}");
        }
    }

    /// The last hop where the credential guarantee can be broken.
    ///
    /// A URL pasted into the slug field always fails as `MalformedSlug`, whose
    /// message echoes the raw input — protocol redacts it at construction, and
    /// this pins that the 400 body a client actually receives carries no token.
    /// Protocol tests the redaction; this tests that the route does not undo it
    /// by reformatting the request.
    #[tokio::test]
    async fn a_credentialed_source_is_400_without_the_token() {
        const SECRET: &str = "ghp_s3cr3tt0ken";
        let dir = TempDir::new().unwrap();

        let (status, body) = send(
            clone_router(test_state(&dir)),
            post_slug_clone(&format!("https://sizeak:{SECRET}@github.com/o/r")),
        )
        .await;
        let body = String::from_utf8_lossy(&body);

        assert_eq!(status, 400, "body={body}");
        assert!(!body.contains(SECRET), "token leaked in a 400 body: {body}");
        assert!(!body.contains("sizeak:"), "userinfo leaked: {body}");
        // Redacted, not swallowed: the user still has to see which source failed.
        assert!(body.contains("***@github.com/o/r"), "unusable 400: {body}");
    }

    /// A credentialed source that *validates* rides out in the 202 body as the
    /// job's `source_label`, so that hop needs the same guarantee as the 400.
    ///
    /// The realistic shape is `https://user:token@host/o/r`, but cloning that
    /// would touch the network. A `file://` URL with a userinfo is the same shape
    /// through the same code (`redact_credentials` redacts any userinfo
    /// containing `:` on every scheme) and stays local.
    #[tokio::test]
    async fn a_credentialed_source_that_validates_is_redacted_in_the_202() {
        const SECRET: &str = "ghp_s3cr3tt0ken";
        let dir = TempDir::new().unwrap();

        let (status, body) = send(
            clone_router(test_state(&dir)),
            post_url_clone(&format!("file://sizeak:{SECRET}@/srv/mirrors/r")),
        )
        .await;
        let text = String::from_utf8_lossy(&body);
        assert_eq!(status, 202, "body={text}");
        assert!(!text.contains(SECRET), "token leaked in a 202 body: {text}");

        let job: CloneJob = json(&body);
        assert_eq!(job.source_label, "file://***@/srv/mirrors/r");
    }
}
