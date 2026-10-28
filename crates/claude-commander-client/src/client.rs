//! [`RemoteClient`]: the pure HTTP + WebSocket transport for one
//! `claude-commander-server`.
//!
//! This is the shared, cloneable core the background [`Poller`](crate::Poller)
//! holds (via an `Arc`) and the thin `claude-commander-remote` adapter delegates
//! every backend method to. Each per-route method maps to one route on the
//! server's `/api` surface and returns a **wire DTO** from
//! `claude-commander-protocol` (or a [`ClientError`]) — never a core type — so
//! the crate stays independent of `claude-commander-core` and can cross-compile
//! to mobile targets.

use std::path::PathBuf;
use std::time::Duration;

use claude_commander_protocol::api::{
    AddProjectRequest, AgentStatesSnapshot, BranchInfo, ChangeProgram, CreateOptions,
    CreateSessionOpts, CreatedId, DiffSide, MarkUnread, NewComment, OperationStatus, PatchSession,
    PreviewData, ProgramInfo, RenameSession, ReviewSnapshot, ReviewedFileMark, ReviewedToggle,
    ScanResponse, SessionDetail, SetProgramsRequest, SetSection, SetSessionBase,
    SetSessionBaseOutcome, Snapshot, ToggleReviewed,
};
use claude_commander_protocol::comment::{ApplyOutcome, Comment};
use claude_commander_protocol::github::{CloneJob, CloneJobId, CloneRequest, GithubRepo};
use claude_commander_protocol::session::{ProjectId, SessionId};
use claude_commander_protocol::workspace::{
    DeleteWorkspaceRequest, RenameWorkspaceRequest, SetProjectWorkspace, SetWorkspacesRequest,
};
use claude_commander_protocol::ws::AttachKind;
use reqwest::{Client, RequestBuilder, Response, StatusCode, Url};
use serde::Serialize;
use serde::de::DeserializeOwned;
use uuid::Uuid;
use xxhash_rust::xxh3::Xxh3;

use crate::attach::AttachConnection;
use crate::error::{self, ClientError, ClientResult};
use crate::spec::{RemoteServerSpec, SecretString};

/// How long to wait for a TCP connection before treating the server as
/// unreachable.
///
/// This was 5s, chosen so the poller's backoff engaged promptly — too tight for
/// the client's actual home, a phone. A handset reaching a LAN or VPN server
/// routinely needs longer than one SYN retransmit: the radio may be leaving
/// doze, Wi-Fi may still be associating, or a VPN tunnel may be re-establishing,
/// and none of that is the server being down. Giving up at 5s rendered a healthy
/// server as unreachable and dropped the poller into backoff, so the *next*
/// attempt was a second or more away as well.
///
/// 15s spans the first four SYN attempts rather than the first three, which is
/// the practical difference between "the network was waking up" and "nothing is
/// listening". Receipt for the schedule: RFC 6298 §2.1 fixes the initial RTO at
/// 1s and §5.5 doubles it per retransmit, and Linux retries a SYN
/// `net.ipv4.tcp_syn_retries` times (default 6, `tcp(7)`) — so the SYNs go out
/// at t≈0s, 1s, 3s, 7s, 15s. A *refused* connection still fails instantly; this
/// budget only bounds the no-answer case.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Overall per-request ceiling (a slow branch-diff still fits comfortably).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Tighter per-request bound for interactive review reads/writes (list/create/
/// delete comment, toggle-reviewed): these are quick store/file operations, so
/// a user who fires one and waits shouldn't sit through the 30s
/// [`REQUEST_TIMEOUT`] when a server has wedged.
///
/// It cannot go below [`CONNECT_TIMEOUT`] — reqwest counts the connect inside
/// the total request timeout, so a bound under the connect budget would fail a
/// cold-but-healthy link before it ever got a chance to answer (asserted by
/// `review_timeouts_are_bounded_between_write_and_request_ceiling`). It tracks
/// the connect budget at exactly that floor: still half the ceiling, and once
/// the pool has a live connection the request itself is the only thing spending
/// the budget.
const REVIEW_WRITE_TIMEOUT: Duration = Duration::from_secs(15);
/// Per-request bound for `apply_comments`. Longer than [`REVIEW_WRITE_TIMEOUT`]
/// because the server does more work here — it recomposes a fresh review diff,
/// re-anchors every comment, then detects the agent's state and sends keys into
/// tmux — but still well under [`REQUEST_TIMEOUT`].
const APPLY_COMMENTS_TIMEOUT: Duration = Duration::from_secs(20);
/// Per-request bound for `GET /github/repos`, the one route that runs *longer*
/// than [`REQUEST_TIMEOUT`] rather than shorter.
///
/// The value is
/// [`REPO_LIST_HTTP_TIMEOUT_SECS`](claude_commander_protocol::github::REPO_LIST_HTTP_TIMEOUT_SECS),
/// which the protocol crate defines alongside the server's own `gh` budget so the
/// ordering between them is stated and tested in one place — see
/// [`RemoteClient::github_repos`] for why the ordering matters.
///
/// A per-request bound may exceed the client-wide one: reqwest 0.13's
/// `RequestBuilder::timeout` "affects only this request and overrides the timeout
/// configured using `ClientBuilder::timeout()`" — it replaces it rather than
/// tightening it. Pinned by `a_repo_list_request_outlasts_the_client_ceiling`,
/// which would otherwise fail at 30s.
const REPO_LIST_TIMEOUT: Duration =
    Duration::from_secs(claude_commander_protocol::github::REPO_LIST_HTTP_TIMEOUT_SECS);

/// The transport client for one remote `claude-commander-server`: the HTTP
/// client, the resolved base URL, and the (redacted) bearer token. Cloneable via
/// `Arc`; the poll task holds an `Arc` of this — never of the adapter backend —
/// so there's no cycle.
pub struct RemoteClient {
    name: String,
    client: Client,
    base: Url,
    token: Option<SecretString>,
}

impl RemoteClient {
    /// Build a client for the server described by `spec`. Fails only on a
    /// malformed `base_url` or an un-buildable HTTP client — the first
    /// *reachability* result surfaces later through the poller, not here.
    pub fn new(spec: RemoteServerSpec) -> ClientResult<Self> {
        let base = Url::parse(&spec.base_url)
            .map_err(|e| ClientError::InvalidRequest(format!("invalid server url: {e}")))?;
        if base.cannot_be_a_base() || base.host().is_none() {
            return Err(ClientError::InvalidRequest(
                "server url must include a host (e.g. http://host:port)".to_string(),
            ));
        }
        // The WS attach URL is derived by rewriting the scheme (http→ws,
        // https→wss); any other scheme would yield an unusable endpoint that
        // `ws_attach_url` passes through unchanged. Reject it here.
        if !matches!(base.scheme(), "http" | "https") {
            return Err(ClientError::InvalidRequest(format!(
                "server url must use http or https (got '{}')",
                base.scheme()
            )));
        }
        let client = Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|e| ClientError::Unavailable {
                reason: format!("could not build http client: {e}"),
            })?;
        Ok(Self {
            name: spec.name,
            client,
            base,
            token: spec.token,
        })
    }

    /// The server's human-readable label (used in logging and the descriptor).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The `/ws/attach` WebSocket URL for this server (scheme mapped, path
    /// prefix preserved).
    fn ws_attach_url(&self) -> String {
        claude_commander_protocol::ws::ws_attach_url(self.base.as_str())
    }

    /// The raw bearer token, if configured. Crate-internal and only handed to
    /// the request builder / the attach handshake's `auth` frame — never logged.
    fn token(&self) -> Option<&str> {
        self.token.as_ref().map(|t| t.expose())
    }

    /// Build a `/api/<segments…>` URL against the base. `path_segments_mut`
    /// percent-encodes each segment, so a free-form session query or a diff path
    /// is escaped safely.
    fn endpoint(&self, segments: &[&str]) -> Url {
        let mut url = self.base.clone();
        {
            let mut path = url
                .path_segments_mut()
                .expect("a validated http(s) base URL is always a base");
            // Drop the trailing empty segment of a bare `http://host/` so we
            // don't produce `//api`, then append the API prefix + segments.
            path.pop_if_empty();
            path.push("api");
            path.extend(segments);
        }
        url
    }

    fn session_url(&self, id: SessionId, tail: &[&str]) -> Url {
        let sid = id.as_uuid().to_string();
        let mut segments = Vec::with_capacity(2 + tail.len());
        segments.push("sessions");
        segments.push(&sid);
        segments.extend_from_slice(tail);
        self.endpoint(&segments)
    }

    /// `/api/projects/clone/{job}` — the poll URL for one clone job.
    ///
    /// Goes through `as_uuid()` rather than `CloneJobId`'s `Display` for the same
    /// reason [`Self::session_url`] and [`Self::project_url`] do: the *full* UUID
    /// is the path segment the server routes on, and the neighbouring id types'
    /// `Display` truncates to an 8-char prefix.
    fn clone_job_url(&self, id: CloneJobId) -> Url {
        self.endpoint(&["projects", "clone", &id.as_uuid().to_string()])
    }

    fn project_url(&self, id: ProjectId, tail: &[&str]) -> Url {
        let pid = id.as_uuid().to_string();
        let mut segments = Vec::with_capacity(2 + tail.len());
        segments.push("projects");
        segments.push(&pid);
        segments.extend_from_slice(tail);
        self.endpoint(&segments)
    }

    // -- Verb helpers --

    /// Attach the bearer header (when set) and send, mapping a transport failure
    /// (no response) to [`ClientError::Unavailable`]/`Protocol`.
    async fn send(&self, request: RequestBuilder) -> ClientResult<Response> {
        let request = match &self.token {
            Some(token) => request.bearer_auth(token.expose()),
            None => request,
        };
        request.send().await.map_err(|err| {
            tracing::debug!(server = %self.name, error = %err, "remote request failed in transport");
            error::transport_error(err)
        })
    }

    /// Turn a non-success status into the matching [`ClientError`], reading the
    /// server's error body for the reason; pass a success response through.
    async fn check(&self, response: Response) -> ClientResult<Response> {
        let status = response.status();
        if status.is_success() {
            Ok(response)
        } else {
            Err(error::status_error(
                status,
                error::error_message(response).await,
            ))
        }
    }

    async fn get_json<T: DeserializeOwned>(&self, url: Url) -> ClientResult<T> {
        self.get_json_within(url, REQUEST_TIMEOUT).await
    }

    /// [`Self::get_json`] with a per-request `timeout` overriding the client-wide
    /// [`REQUEST_TIMEOUT`] (used by interactive review reads — see
    /// [`REVIEW_WRITE_TIMEOUT`]).
    async fn get_json_within<T: DeserializeOwned>(
        &self,
        url: Url,
        timeout: Duration,
    ) -> ClientResult<T> {
        let response = self.send(self.client.get(url).timeout(timeout)).await?;
        let response = self.check(response).await?;
        decode_json(response).await
    }

    /// GET where a `404` means "no such thing" rather than an error (the detail
    /// route resolves a free-form query and 404s when nothing matches).
    async fn get_json_opt<T: DeserializeOwned>(&self, url: Url) -> ClientResult<Option<T>> {
        let response = self.send(self.client.get(url)).await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let response = self.check(response).await?;
        Ok(Some(decode_json(response).await?))
    }

    /// GET where a `204 No Content` means "unchanged" (the review-refresh route).
    async fn get_json_if_present<T: DeserializeOwned>(&self, url: Url) -> ClientResult<Option<T>> {
        let response = self.send(self.client.get(url)).await?;
        if response.status() == StatusCode::NO_CONTENT {
            return Ok(None);
        }
        let response = self.check(response).await?;
        Ok(Some(decode_json(response).await?))
    }

    async fn get_text(&self, url: Url) -> ClientResult<String> {
        let response = self.send(self.client.get(url)).await?;
        let response = self.check(response).await?;
        response.text().await.map_err(error::body_error)
    }

    async fn get_bytes(&self, url: Url) -> ClientResult<Vec<u8>> {
        let response = self.send(self.client.get(url)).await?;
        let response = self.check(response).await?;
        response
            .bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(error::body_error)
    }

    /// POST with no body, decoding the JSON response (cascade routes → 202 +
    /// `OperationStatus`).
    async fn post_empty_json<T: DeserializeOwned>(&self, url: Url) -> ClientResult<T> {
        self.post_empty_json_within(url, REQUEST_TIMEOUT).await
    }

    /// [`Self::post_empty_json`] with a per-request `timeout` overriding the
    /// client-wide [`REQUEST_TIMEOUT`] (used by `apply_comments` — see
    /// [`APPLY_COMMENTS_TIMEOUT`]).
    async fn post_empty_json_within<T: DeserializeOwned>(
        &self,
        url: Url,
        timeout: Duration,
    ) -> ClientResult<T> {
        let response = self.send(self.client.post(url).timeout(timeout)).await?;
        let response = self.check(response).await?;
        decode_json(response).await
    }

    /// POST a JSON body, decoding the JSON response (create routes → 201 + id).
    async fn post_json<T: DeserializeOwned, B: Serialize>(
        &self,
        url: Url,
        body: &B,
    ) -> ClientResult<T> {
        self.post_json_within(url, body, REQUEST_TIMEOUT).await
    }

    /// [`Self::post_json`] with a per-request `timeout` overriding the
    /// client-wide [`REQUEST_TIMEOUT`] (used by interactive review writes — see
    /// [`REVIEW_WRITE_TIMEOUT`]).
    async fn post_json_within<T: DeserializeOwned, B: Serialize>(
        &self,
        url: Url,
        body: &B,
        timeout: Duration,
    ) -> ClientResult<T> {
        let response = self
            .send(self.client.post(url).json(body).timeout(timeout))
            .await?;
        let response = self.check(response).await?;
        decode_json(response).await
    }

    /// POST with no body, discarding the (204/empty) response.
    async fn post_empty_ok(&self, url: Url) -> ClientResult<()> {
        let response = self.send(self.client.post(url)).await?;
        self.check(response).await?;
        Ok(())
    }

    /// POST a JSON body, discarding the (204) response.
    async fn post_json_ok<B: Serialize>(&self, url: Url, body: &B) -> ClientResult<()> {
        let response = self.send(self.client.post(url).json(body)).await?;
        self.check(response).await?;
        Ok(())
    }

    /// PATCH a JSON body, discarding the (204) response.
    async fn patch_json_ok<B: Serialize>(&self, url: Url, body: &B) -> ClientResult<()> {
        let response = self.send(self.client.patch(url).json(body)).await?;
        self.check(response).await?;
        Ok(())
    }

    /// POST a raw byte body with an explicit `Content-Type`, discarding the
    /// response. Used to upload a pasted image (PNG bytes) to the server.
    async fn post_bytes_ok(
        &self,
        url: Url,
        bytes: Vec<u8>,
        content_type: &str,
    ) -> ClientResult<()> {
        let response = self
            .send(
                self.client
                    .post(url)
                    .header(reqwest::header::CONTENT_TYPE, content_type)
                    .body(bytes),
            )
            .await?;
        self.check(response).await?;
        Ok(())
    }

    /// PUT a JSON body, discarding the (204) response.
    async fn put_json_ok<B: Serialize>(&self, url: Url, body: &B) -> ClientResult<()> {
        let response = self.send(self.client.put(url).json(body)).await?;
        self.check(response).await?;
        Ok(())
    }

    /// DELETE, discarding the (204) response.
    async fn delete_ok(&self, url: Url) -> ClientResult<()> {
        self.delete_ok_within(url, REQUEST_TIMEOUT).await
    }

    /// [`Self::delete_ok`] with a per-request `timeout` overriding the
    /// client-wide [`REQUEST_TIMEOUT`] (used by interactive review writes — see
    /// [`REVIEW_WRITE_TIMEOUT`]).
    async fn delete_ok_within(&self, url: Url, timeout: Duration) -> ClientResult<()> {
        let response = self.send(self.client.delete(url).timeout(timeout)).await?;
        self.check(response).await?;
        Ok(())
    }

    /// Fetch the workspace + agent-state snapshots and content-hash them
    /// together. The poll loop compares this against the previous hash to decide
    /// whether observable state moved. Any HTTP/transport failure propagates so
    /// the poller can go degraded.
    pub async fn poll_hashes(&self) -> ClientResult<u64> {
        let workspace = self.get_bytes(self.endpoint(&["workspace"])).await?;
        let agent_states = self.get_bytes(self.endpoint(&["agent-states"])).await?;
        let mut hasher = Xxh3::new();
        hasher.update(&workspace);
        hasher.update(b"\x00");
        hasher.update(&agent_states);
        Ok(hasher.digest())
    }

    // -- Per-route methods --

    /// Liveness probe: `GET /health` (outside the `/api` bearer surface, so no
    /// auth needed). Returns whether the server answered a 2xx. A transport
    /// failure (unreachable) surfaces as [`ClientError::Unavailable`]. Used by
    /// the mobile connect screen before a poller exists.
    pub async fn health(&self) -> ClientResult<bool> {
        let mut url = self.base.clone();
        {
            let mut path = url
                .path_segments_mut()
                .expect("a validated http(s) base URL is always a base");
            path.pop_if_empty();
            path.push("health");
        }
        let response = self.send(self.client.get(url)).await?;
        Ok(response.status().is_success())
    }

    /// Authenticated tmux probe: `GET /api/health/tmux`. `200` → true, `503` →
    /// false; a `401`/`403` surfaces as [`ClientError::Auth`] so the connect
    /// screen can flag a bad token. Doubles as an auth check.
    pub async fn health_tmux(&self) -> ClientResult<bool> {
        let response = self
            .send(self.client.get(self.endpoint(&["health", "tmux"])))
            .await?;
        let status = response.status();
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            return Err(ClientError::Auth);
        }
        Ok(status.is_success())
    }

    /// Ask the server to re-check PR metadata (it runs the PR-status loop).
    pub async fn request_pr_refresh(&self) -> ClientResult<()> {
        self.post_empty_ok(self.endpoint(&["pr-refresh"])).await
    }

    pub async fn snapshot(&self) -> ClientResult<Snapshot> {
        self.get_json(self.endpoint(&["workspace"])).await
    }

    pub async fn agent_states(&self, fresh: bool) -> ClientResult<AgentStatesSnapshot> {
        let mut url = self.endpoint(&["agent-states"]);
        url.query_pairs_mut()
            .append_pair("fresh", if fresh { "true" } else { "false" });
        self.get_json(url).await
    }

    pub async fn session_detail(
        &self,
        query: &str,
        lines: Option<usize>,
    ) -> ClientResult<Option<SessionDetail>> {
        let mut url = self.endpoint(&["sessions", query, "detail"]);
        if let Some(lines) = lines {
            url.query_pairs_mut()
                .append_pair("lines", &lines.to_string());
        }
        self.get_json_opt(url).await
    }

    /// Preview payload for a session (`GET /api/sessions/{id}/preview?lines=`).
    pub async fn session_preview(
        &self,
        id: SessionId,
        lines: Option<usize>,
    ) -> ClientResult<PreviewData> {
        let mut url = self.session_url(id, &["preview"]);
        if let Some(lines) = lines {
            url.query_pairs_mut()
                .append_pair("lines", &lines.to_string());
        }
        self.get_json(url).await
    }

    /// Preview payload for a project (`GET /api/projects/{id}/preview`).
    pub async fn project_preview(&self, id: ProjectId) -> ClientResult<PreviewData> {
        self.get_json(self.project_url(id, &["preview"])).await
    }

    pub async fn branch_diff(&self, id: SessionId) -> ClientResult<String> {
        self.get_text(self.session_url(id, &["branch-diff"])).await
    }

    pub async fn list_branches(
        &self,
        project: ProjectId,
        fetch: bool,
    ) -> ClientResult<Vec<BranchInfo>> {
        let mut url = self.project_url(project, &["branches"]);
        url.query_pairs_mut()
            .append_pair("fetch", if fetch { "true" } else { "false" });
        self.get_json(url).await
    }

    pub async fn create_options(&self) -> ClientResult<CreateOptions> {
        self.get_json(self.endpoint(&["create-options"])).await
    }

    pub async fn set_programs(&self, programs: Vec<ProgramInfo>) -> ClientResult<()> {
        self.put_json_ok(
            self.endpoint(&["config", "programs"]),
            &SetProgramsRequest { programs },
        )
        .await
    }

    pub async fn pending_comment_sessions(&self) -> ClientResult<Vec<SessionId>> {
        self.get_json(self.endpoint(&["comments", "pending"])).await
    }

    // -- Session mutations --

    pub async fn create_session(&self, opts: CreateSessionOpts) -> ClientResult<SessionId> {
        let env: CreatedId<SessionId> = self.post_json(self.endpoint(&["sessions"]), &opts).await?;
        Ok(env.id)
    }

    pub async fn kill_session(&self, id: SessionId) -> ClientResult<()> {
        self.post_empty_ok(self.session_url(id, &["kill"])).await
    }

    pub async fn restart_session(&self, id: SessionId) -> ClientResult<()> {
        self.post_empty_ok(self.session_url(id, &["restart"])).await
    }

    /// Restart a session with a *fresh* agent conversation — the server relaunches
    /// the pane without the agent's resume flag, whatever its own
    /// `resume_session` config says.
    pub async fn restart_session_fresh(&self, id: SessionId) -> ClientResult<()> {
        self.post_empty_ok(self.session_url(id, &["restart-fresh"]))
            .await
    }

    pub async fn delete_session(&self, id: SessionId) -> ClientResult<()> {
        self.delete_ok(self.session_url(id, &[])).await
    }

    pub async fn rename_session(&self, id: SessionId, title: String) -> ClientResult<()> {
        let body = PatchSession::Rename(RenameSession { title });
        self.patch_json_ok(self.session_url(id, &[]), &body).await
    }

    pub async fn set_section(&self, id: SessionId, section: Option<String>) -> ClientResult<()> {
        let body = PatchSession::SetSection(SetSection { section });
        self.patch_json_ok(self.session_url(id, &[]), &body).await
    }

    /// Retarget a session's stack base (`POST /sessions/{id}/base`).
    ///
    /// A POST with a body rather than a PATCH op, because the response carries
    /// the outcome — notably whether the `gh pr edit` landed, which the caller
    /// cannot determine for itself.
    pub async fn set_session_base(
        &self,
        id: SessionId,
        parent: Option<SessionId>,
    ) -> ClientResult<SetSessionBaseOutcome> {
        let body = SetSessionBase {
            parent_session_id: parent,
        };
        self.post_json(self.session_url(id, &["base"]), &body).await
    }

    /// Change a session's launch program (PATCH `change_program` op).
    pub async fn change_program(&self, id: SessionId, program: String) -> ClientResult<()> {
        let body = PatchSession::ChangeProgram(ChangeProgram { program });
        self.patch_json_ok(self.session_url(id, &[]), &body).await
    }

    /// Upload a pasted image to a session (`POST /paste-image`).
    ///
    /// The bytes are checked against the shared wire contract
    /// ([`claude_commander_protocol::paste::validate`]) *before* the request, so
    /// junk or over-cap content fails immediately as
    /// [`ClientError::InvalidRequest`] rather than after a round trip — which
    /// matters most on the slow mobile links that would otherwise spend the
    /// upload only to be refused. Every remote caller (the TUI's Ctrl+V, the
    /// CLI, the Flutter client) inherits the check by going through here.
    ///
    /// The sniffed type also sets `Content-Type`. That is advisory only: the
    /// server re-sniffs the body and never trusts the header, so a wrong one
    /// cannot widen what it accepts.
    pub async fn paste_image(&self, id: SessionId, bytes: Vec<u8>) -> ClientResult<()> {
        let format = claude_commander_protocol::paste::validate(&bytes)
            .map_err(|e| ClientError::InvalidRequest(e.to_string()))?;
        self.post_bytes_ok(
            self.session_url(id, &["paste-image"]),
            bytes,
            format.content_type(),
        )
        .await
    }

    pub async fn mark_read(&self, id: SessionId) -> ClientResult<()> {
        self.post_empty_ok(self.session_url(id, &["read"])).await
    }

    pub async fn toggle_keep_alive(&self, id: SessionId) -> ClientResult<bool> {
        self.post_empty_json(self.session_url(id, &["keep-alive"]))
            .await
    }

    pub async fn mark_unread(&self, ids: Vec<SessionId>) -> ClientResult<()> {
        // Batch counterpart to `mark_read`: `POST /api/sessions/unread` with
        // `{ "ids": [...] }`. Unknown ids are silently skipped server-side,
        // matching the local backend.
        let body = MarkUnread {
            ids: ids.iter().map(|id| id.as_uuid().to_string()).collect(),
        };
        self.post_json_ok(self.endpoint(&["sessions", "unread"]), &body)
            .await
    }

    // -- Projects --

    /// `POST /projects` — register `path`, tagged with `workspace` (`None` =
    /// Main). An absent workspace is omitted from the body, so an older server
    /// sees exactly the request it always did.
    pub async fn add_project(
        &self,
        path: PathBuf,
        workspace: Option<String>,
    ) -> ClientResult<ProjectId> {
        let body = AddProjectRequest { path, workspace };
        let env: CreatedId<ProjectId> = self.post_json(self.endpoint(&["projects"]), &body).await?;
        Ok(env.id)
    }

    /// `POST /projects/ensure` — register `path`, or answer with the id of the
    /// project already registered for it.
    ///
    /// The idempotent counterpart to [`Self::add_project`], which registers
    /// unconditionally. Callers offering "register this existing checkout" want
    /// this one: the path they were handed is frequently already a project, and a
    /// second `add_project` would leave two entries for one repository. The
    /// dedupe rule (and the path resolution behind it) is the server's, so no
    /// client re-states it.
    ///
    /// `workspace` tags the project only if the server registers it here.
    pub async fn ensure_project(
        &self,
        path: PathBuf,
        workspace: Option<String>,
    ) -> ClientResult<ProjectId> {
        let body = AddProjectRequest { path, workspace };
        let env: CreatedId<ProjectId> = self
            .post_json(self.endpoint(&["projects", "ensure"]), &body)
            .await?;
        Ok(env.id)
    }

    pub async fn remove_project(&self, id: ProjectId) -> ClientResult<()> {
        self.delete_ok(self.project_url(id, &[])).await
    }

    // -- Workspaces --

    /// `PUT /config/workspaces` — replace the server's workspace definitions.
    pub async fn set_workspaces(&self, req: SetWorkspacesRequest) -> ClientResult<()> {
        self.put_json_ok(self.endpoint(&["config", "workspaces"]), &req)
            .await
    }

    /// `POST /config/workspaces/rename` — rename a workspace server-side,
    /// rewriting its projects' tags. A no-op success where it is unknown.
    pub async fn rename_workspace(&self, from: String, to: String) -> ClientResult<()> {
        self.post_json_ok(
            self.endpoint(&["config", "workspaces", "rename"]),
            &RenameWorkspaceRequest { from, to },
        )
        .await
    }

    /// `POST /config/workspaces/delete` — delete a workspace server-side,
    /// moving its projects to Main. A no-op success where it is unknown.
    pub async fn delete_workspace(&self, name: String) -> ClientResult<()> {
        self.post_json_ok(
            self.endpoint(&["config", "workspaces", "delete"]),
            &DeleteWorkspaceRequest { name },
        )
        .await
    }

    /// `PUT /projects/{id}/workspace` — move a project (`None` = Main).
    pub async fn set_project_workspace(
        &self,
        id: ProjectId,
        workspace: Option<String>,
    ) -> ClientResult<()> {
        self.put_json_ok(
            self.project_url(id, &["workspace"]),
            &SetProjectWorkspace { workspace },
        )
        .await
    }

    /// `POST /projects/scan` — register every repo under `dir`, each new one
    /// tagged with `workspace` (`None` = Main; omitted from the body, so an
    /// older server sees the body it always did).
    pub async fn scan_directory(
        &self,
        dir: PathBuf,
        workspace: Option<String>,
    ) -> ClientResult<ScanResponse> {
        let url = self.endpoint(&["projects", "scan"]);
        let body = AddProjectRequest {
            path: dir,
            workspace,
        };
        self.post_json(url, &body).await
    }

    // -- GitHub repos / repository clone --

    /// `GET /github/repos` — every repo the server-side `gh` user can clone, for
    /// the repo picker.
    ///
    /// The list is the server's to produce: `gh` runs where the repositories will
    /// be checked out, so a phone with no `gh` and no GitHub credentials still gets
    /// a picker. An absent `gh` surfaces as [`ClientError::Unavailable`] (the
    /// server's 503 for a missing backing tool, as with tmux), which a frontend can
    /// word as "install gh on the server" rather than as a generic failure.
    ///
    /// **The only route that outlasts [`REQUEST_TIMEOUT`], and it has to.** The
    /// server runs `gh api --paginate` under its own budget
    /// ([`DEFAULT_REPO_LIST_TIMEOUT_SECS`], 90s by default) — more than the 30s
    /// ceiling every other request gets. Under that ceiling a large account's
    /// listing surfaced as a transport timeout, i.e. "the server is down", when
    /// the server was merely still paginating; and giving up here does nothing
    /// about the `gh` on the far side, so each retry stacked another one. So this
    /// request is given [`REPO_LIST_TIMEOUT`], which outlasts the server's budget
    /// and lets the server win the race and answer with its real reason.
    ///
    /// [`DEFAULT_REPO_LIST_TIMEOUT_SECS`]: claude_commander_protocol::github::DEFAULT_REPO_LIST_TIMEOUT_SECS
    pub async fn github_repos(&self) -> ClientResult<Vec<GithubRepo>> {
        self.get_json_within(self.endpoint(&["github", "repos"]), REPO_LIST_TIMEOUT)
            .await
    }

    /// `POST /projects/clone` — start a clone, returning the created job.
    ///
    /// The route answers **202 Accepted carrying the whole [`CloneJob`]**, not just
    /// its id, so the caller gets the id, the destination and the initial status in
    /// one round trip and can start polling [`Self::clone_job`] without a second
    /// request to learn where it stands.
    ///
    /// **The status in the returned job is not a terminal status.** Every
    /// outcome — success, failure, and an already-occupied destination — is
    /// reported through the job, so this reads `Running` essentially always. A
    /// caller that treats it as final will miss every result.
    ///
    /// `Err` means the *request* was refused (an unusable source or destination
    /// name is a 400 → [`ClientError::InvalidRequest`], carrying the server's
    /// already-redacted reason). Nothing here builds a message out of
    /// `req.source`: a hand-pasted URL can carry `user:token@` userinfo, and the
    /// rejection strings are redacted where they are constructed in
    /// `claude-commander-protocol` precisely so no hop has to remember to.
    pub async fn start_clone(&self, req: CloneRequest) -> ClientResult<CloneJob> {
        self.post_json(self.endpoint(&["projects", "clone"]), &req)
            .await
    }

    /// `GET /projects/clone/{job}` — one poll of a clone job.
    ///
    /// **A `404` is `Ok(None)`, not an error.** An unknown job is a normal answer
    /// for a poller: the server's job registry prunes jobs a while after they
    /// finish, so a client that keeps polling (or resumes polling an id it stored
    /// across a restart) must see "gone" rather than a hard failure it would
    /// surface as a broken connection.
    ///
    /// One request per call by design — the cadence, and when to stop, belong to
    /// the caller.
    pub async fn clone_job(&self, id: CloneJobId) -> ClientResult<Option<CloneJob>> {
        self.get_json_opt(self.clone_job_url(id)).await
    }

    // -- Cascade / push-stack --

    pub async fn cascade_merge(&self, id: SessionId) -> ClientResult<OperationStatus> {
        self.post_empty_json(self.session_url(id, &["cascade"]))
            .await
    }

    pub async fn cascade_resume(&self) -> ClientResult<OperationStatus> {
        self.post_empty_json(self.endpoint(&["cascade", "resume"]))
            .await
    }

    pub async fn cascade_abandon(&self) -> ClientResult<()> {
        self.post_empty_ok(self.endpoint(&["cascade", "abandon"]))
            .await
    }

    pub async fn push_stack(&self, id: SessionId) -> ClientResult<OperationStatus> {
        self.post_empty_json(self.session_url(id, &["push-stack"]))
            .await
    }

    // -- Review / comments --

    pub async fn list_comments(&self, id: SessionId) -> ClientResult<Vec<Comment>> {
        self.get_json_within(self.session_url(id, &["comments"]), REVIEW_WRITE_TIMEOUT)
            .await
    }

    pub async fn open_review(&self, id: SessionId) -> ClientResult<ReviewSnapshot> {
        self.get_json(self.session_url(id, &["review"])).await
    }

    pub async fn refresh_review_if_changed(
        &self,
        id: SessionId,
        prev_hash: u64,
    ) -> ClientResult<Option<ReviewSnapshot>> {
        let mut url = self.session_url(id, &["review", "refresh"]);
        url.query_pairs_mut()
            .append_pair("prev_hash", &prev_hash.to_string());
        self.get_json_if_present(url).await
    }

    pub async fn create_comment(&self, id: SessionId, draft: NewComment) -> ClientResult<Uuid> {
        let env: CreatedId<Uuid> = self
            .post_json_within(
                self.session_url(id, &["comments"]),
                &draft,
                REVIEW_WRITE_TIMEOUT,
            )
            .await?;
        Ok(env.id)
    }

    pub async fn delete_comment(&self, id: SessionId, comment_id: Uuid) -> ClientResult<()> {
        let cid = comment_id.to_string();
        self.delete_ok_within(
            self.session_url(id, &["comments", &cid]),
            REVIEW_WRITE_TIMEOUT,
        )
        .await
    }

    pub async fn apply_comments(&self, id: SessionId) -> ClientResult<ApplyOutcome> {
        self.post_empty_json_within(
            self.session_url(id, &["comments", "apply"]),
            APPLY_COMMENTS_TIMEOUT,
        )
        .await
    }

    /// Set or clear a file's reviewed mark (never a toggle), returning the new
    /// state. See [`ToggleReviewed::reviewed`] for why a view says which way.
    pub async fn set_file_reviewed(
        &self,
        id: SessionId,
        display_path: String,
        reviewed: bool,
    ) -> ClientResult<bool> {
        let body = ToggleReviewed {
            display_path,
            reviewed: Some(reviewed),
        };
        let out: ReviewedToggle = self
            .post_json_within(
                self.session_url(id, &["files", "reviewed"]),
                &body,
                REVIEW_WRITE_TIMEOUT,
            )
            .await?;
        Ok(out.reviewed)
    }

    /// The session's stored reviewed marks (unpruned), for an open review view
    /// to poll. Bounded like the other review store reads.
    pub async fn reviewed_marks(&self, id: SessionId) -> ClientResult<Vec<ReviewedFileMark>> {
        self.get_json_within(
            self.session_url(id, &["files", "reviewed"]),
            REVIEW_WRITE_TIMEOUT,
        )
        .await
    }

    pub async fn fetch_diff_blob(
        &self,
        id: SessionId,
        side: DiffSide,
        path: String,
    ) -> ClientResult<Vec<u8>> {
        let mut url = self.session_url(id, &["blob"]);
        url.query_pairs_mut()
            .append_pair("side", diff_side_param(side))
            .append_pair("path", &path);
        self.get_bytes(url).await
    }

    // -- Attach --

    /// Open a live attach to a session's `kind` pane over the `/ws/attach`
    /// WebSocket, sized `cols`×`rows`. Returns a client-local [`AttachConnection`]
    /// the caller (the remote adapter) wraps into core's attach seam.
    pub async fn attach(
        &self,
        id: SessionId,
        cols: u16,
        rows: u16,
        kind: AttachKind,
    ) -> ClientResult<AttachConnection> {
        crate::attach::connect(
            &self.ws_attach_url(),
            self.token(),
            id.as_uuid().to_string(),
            cols,
            rows,
            kind,
        )
        .await
    }
}

async fn decode_json<T: DeserializeOwned>(response: Response) -> ClientResult<T> {
    response.json::<T>().await.map_err(error::body_error)
}

fn diff_side_param(side: DiffSide) -> &'static str {
    match side {
        DiffSide::Old => "old",
        DiffSide::New => "new",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The connect budget must outlast a link that is merely *waking* — the
    /// phone client's normal case. On the RFC 6298 / Linux schedule cited on
    /// [`CONNECT_TIMEOUT`] the SYNs go out at t≈0s, 1s, 3s, 7s, 15s; the old 5s
    /// budget gave up after the one at 3s, so a server that answered a moment
    /// later was rendered unreachable and the poller went into backoff on top.
    /// Asserted at the constant level (no clock, no socket) so it stays
    /// deterministic.
    #[test]
    fn the_connect_budget_outlasts_a_waking_link() {
        assert!(
            CONNECT_TIMEOUT >= Duration::from_secs(15),
            "the connect budget must reach the SYN at ~7s with room to answer, \
             got {CONNECT_TIMEOUT:?}"
        );
        assert!(
            CONNECT_TIMEOUT < REQUEST_TIMEOUT,
            "connecting must never be allowed to consume the whole request budget"
        );
    }

    /// Interactive review reads/writes must be bounded tighter than the 30s
    /// [`REQUEST_TIMEOUT`] so a wedged server surfaces promptly, and the heavier
    /// `apply_comments` must sit between the two: strictly above the quick-write
    /// bound (it recomposes a diff + drives tmux) yet strictly below the overall
    /// ceiling. Asserted at the constant level rather than by wall-clock so it
    /// stays deterministic.
    #[test]
    fn review_timeouts_are_bounded_between_write_and_request_ceiling() {
        assert!(
            REVIEW_WRITE_TIMEOUT < APPLY_COMMENTS_TIMEOUT,
            "quick review writes must be tighter than the apply bound"
        );
        assert!(
            APPLY_COMMENTS_TIMEOUT < REQUEST_TIMEOUT,
            "apply_comments must stay under the overall request ceiling"
        );
        // The tightest interactive bound must still cover the TCP connect budget
        // (which reqwest counts inside the total request timeout), or a
        // healthy-but-slow-to-connect link could time out with no budget left
        // for the response.
        assert!(
            REVIEW_WRITE_TIMEOUT >= CONNECT_TIMEOUT,
            "review-write budget must at least cover the connect budget"
        );
    }

    /// The repo listing is the one request allowed to run *past* the ceiling, and
    /// it must outlast the server's own `gh` budget or the client reports its own
    /// transport timeout ("the server is down") while a `gh` keeps paginating on
    /// the far side.
    #[test]
    fn the_repo_list_budget_outlasts_both_the_ceiling_and_the_server() {
        assert!(
            REPO_LIST_TIMEOUT > REQUEST_TIMEOUT,
            "the repo listing needs more than the {REQUEST_TIMEOUT:?} ceiling"
        );
        assert!(
            REPO_LIST_TIMEOUT.as_secs()
                > claude_commander_protocol::github::DEFAULT_REPO_LIST_TIMEOUT_SECS,
            "the client must not give up before the server kills gh"
        );
    }

    /// Pins the reqwest semantic the line above rests on: a per-request timeout
    /// **replaces** the client-wide one, so it may be *longer*. Were it clamped to
    /// the minimum of the two, `github_repos` would still die at
    /// [`REQUEST_TIMEOUT`] and the fix would be silently inert.
    ///
    /// Deliberately scaled down (50ms ceiling, 5s override, ~300ms server) so it
    /// proves the ordering in a third of a second rather than in half a minute.
    /// Loopback only, no external network.
    #[tokio::test]
    async fn a_per_request_timeout_may_exceed_the_client_ceiling() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // Answer two requests, each after a delay that outlasts the client-wide
        // ceiling but fits inside the per-request override.
        tokio::spawn(async move {
            for _ in 0..2 {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf).await;
                tokio::time::sleep(Duration::from_millis(300)).await;
                let _ = sock
                    .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n[]")
                    .await;
                let _ = sock.flush().await;
            }
        });

        let url = format!("http://{addr}/github/repos");
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(50))
            .build()
            .unwrap();

        // Without an override the client-wide ceiling bites: this is the shape of
        // the bug being fixed.
        assert!(
            client.get(&url).send().await.is_err(),
            "the client-wide ceiling should have expired at 50ms"
        );
        // With one, the request outlives that ceiling.
        let response = client
            .get(&url)
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .expect("a per-request timeout must be able to exceed the client-wide one");
        assert!(response.status().is_success());
    }

    #[test]
    fn new_rejects_non_http_scheme() {
        let spec = RemoteServerSpec {
            name: "box".to_string(),
            base_url: "ftp://box".to_string(),
            token: None,
        };
        match RemoteClient::new(spec) {
            Err(ClientError::InvalidRequest(m)) => assert!(
                m.contains("http or https"),
                "non-http(s) scheme must be rejected at construction: {m}"
            ),
            Ok(_) => panic!("non-http(s) scheme must be rejected"),
            Err(other) => panic!("expected InvalidRequest, got {other:?}"),
        }
    }

    #[test]
    fn new_rejects_url_without_host() {
        match RemoteClient::new(RemoteServerSpec {
            name: "box".to_string(),
            base_url: "not a url".to_string(),
            token: None,
        }) {
            Err(ClientError::InvalidRequest(_)) => {}
            Err(other) => panic!("expected InvalidRequest, got {other:?}"),
            Ok(_) => panic!("a hostless url must be rejected"),
        }
    }

    #[test]
    fn endpoint_builds_api_path_and_escapes_segments() {
        let client = RemoteClient::new(RemoteServerSpec {
            name: "box".to_string(),
            base_url: "http://host:8080".to_string(),
            token: None,
        })
        .unwrap();
        // A bare base must not produce `//api`, and free-form segments are
        // percent-encoded.
        assert_eq!(
            client.endpoint(&["sessions", "a b", "detail"]).as_str(),
            "http://host:8080/api/sessions/a%20b/detail"
        );
    }

    /// A clone-job poll URL must carry the **full** job UUID.
    ///
    /// `CloneJobId`'s `Display` is the full UUID today, but the neighbouring
    /// `SessionId`/`ProjectId` truncate theirs to an 8-char prefix for the session
    /// tree — so this pins that [`RemoteClient::clone_job`] goes through
    /// `as_uuid()`, as [`RemoteClient::session_url`]/[`RemoteClient::project_url`]
    /// do, rather than leaning on a `Display` whose contract differs from its
    /// siblings'. A truncated segment would 404 every poll.
    #[test]
    fn clone_job_url_carries_the_full_uuid() {
        let client = RemoteClient::new(RemoteServerSpec {
            name: "box".to_string(),
            base_url: "http://host:8080".to_string(),
            token: None,
        })
        .unwrap();
        let id =
            CloneJobId::from_uuid(Uuid::parse_str("2f3ab1c0-0000-4000-8000-00000000abcd").unwrap());
        assert_eq!(
            client.clone_job_url(id).as_str(),
            "http://host:8080/api/projects/clone/2f3ab1c0-0000-4000-8000-00000000abcd"
        );
    }

    #[test]
    fn diff_side_param_wire_forms() {
        assert_eq!(diff_side_param(DiffSide::Old), "old");
        assert_eq!(diff_side_param(DiffSide::New), "new");
    }

    /// A client pointed at a port nothing listens on. Any request that actually
    /// reaches the network fails as [`ClientError::Unavailable`], so an
    /// `InvalidRequest` from these tests proves the rejection happened locally.
    fn unreachable_client() -> RemoteClient {
        RemoteClient::new(RemoteServerSpec {
            name: "box".to_string(),
            // Port 0 is never connectable.
            base_url: "http://127.0.0.1:0".to_string(),
            token: None,
        })
        .unwrap()
    }

    #[tokio::test]
    async fn paste_image_rejects_non_image_without_a_request() {
        let err = unreachable_client()
            .paste_image(SessionId::new(), b"not an image".to_vec())
            .await
            .expect_err("a non-image must be refused");
        match err {
            ClientError::InvalidRequest(m) => assert!(
                m.contains("not a recognised image"),
                "expected the contract's wording, got {m}"
            ),
            other => panic!("expected a local InvalidRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn paste_image_rejects_oversized_without_a_request() {
        let too_big = vec![0u8; claude_commander_protocol::paste::MAX_IMAGE_BYTES + 1];
        let err = unreachable_client()
            .paste_image(SessionId::new(), too_big)
            .await
            .expect_err("an oversized body must be refused");
        match err {
            ClientError::InvalidRequest(m) => assert!(
                m.contains("over the"),
                "expected the size-cap wording, got {m}"
            ),
            other => panic!("expected a local InvalidRequest, got {other:?}"),
        }
    }

    /// An accepted image still goes to the wire — proving the local check gates
    /// on content, not on every call.
    #[tokio::test]
    async fn paste_image_sends_valid_image_to_the_wire() {
        // Only the signature + start of IHDR: the allow-list sniffs a prefix, so
        // this is the shortest body `validate` accepts. Deliberately not a
        // decodable PNG — nothing on this path decodes it.
        const PNG_MAGIC_PREFIX: &[u8] = &[
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52,
        ];
        let err = unreachable_client()
            .paste_image(SessionId::new(), PNG_MAGIC_PREFIX.to_vec())
            .await
            .expect_err("nothing is listening, so the request must fail");
        assert!(
            matches!(err, ClientError::Unavailable { .. }),
            "a valid image must reach the transport, got {err:?}"
        );
    }
}
