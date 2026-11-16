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
use claude_commander_protocol::hosting::{
    CodeHost, CodeHostProvider, HostedRepository, RepositoryListing,
};
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
const _: () = assert!(REQUEST_TIMEOUT.as_secs() > claude_commander_protocol::CHANGE_WAIT_SECS);
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
    async fn patch_json<T: serde::de::DeserializeOwned>(
        &self,
        url: Url,
        body: &impl serde::Serialize,
    ) -> ClientResult<T> {
        let response = self.send(self.client.patch(url).json(body)).await?;
        let response = self.check(response).await?;
        decode_json(response).await
    }

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
        let (workspace, agent_states) = tokio::try_join!(
            self.get_bytes(self.endpoint(&["workspace"])),
            self.get_bytes(self.endpoint(&["agent-states"])),
        )?;
        let mut hasher = Xxh3::new();
        hasher.update(&workspace);
        hasher.update(b"\x00");
        hasher.update(&agent_states);
        Ok(hasher.digest())
    }

    /// Wait for a server generation. A 404 means an older server: the poller
    /// continues using its configured fallback cadence.
    pub async fn wait_for_change(&self, since: Option<u64>) -> ClientResult<u64> {
        let mut url = self.endpoint(&["changes"]);
        if let Some(since) = since {
            url.query_pairs_mut()
                .append_pair("since", &since.to_string());
        }
        self.get_json(url).await
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

    pub async fn preview_part(
        &self,
        id: Option<SessionId>,
        project: Option<ProjectId>,
        lines: Option<usize>,
        part: claude_commander_protocol::preview::PreviewPart,
    ) -> ClientResult<PreviewData> {
        let mut url = match (id, project) {
            (Some(id), _) => self.session_url(id, &["preview"]),
            (_, Some(id)) => self.project_url(id, &["preview"]),
            _ => return Err(ClientError::InvalidRequest("preview needs a target".into())),
        };
        url.query_pairs_mut().append_pair("part", part.as_str());
        if let Some(lines) = lines {
            url.query_pairs_mut()
                .append_pair("lines", &lines.to_string());
        }
        self.get_json(url).await
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

    pub async fn edit_session(
        &self,
        id: SessionId,
        edit: claude_commander_protocol::api::EditSession,
    ) -> ClientResult<claude_commander_protocol::api::EditSessionOutcome> {
        let result = self
            .patch_json(self.session_url(id, &[]), &PatchSession::Edit(edit.clone()))
            .await;
        match result {
            Err(crate::ClientError::InvalidRequest(ref message))
                if message.contains("unknown variant `edit`") =>
            {
                self.edit_session_legacy(id, edit).await
            }
            result => result,
        }
    }

    async fn edit_session_legacy(
        &self,
        id: SessionId,
        edit: claude_commander_protocol::api::EditSession,
    ) -> ClientResult<claude_commander_protocol::api::EditSessionOutcome> {
        // Old servers cannot retain a deferred program. Reject before any writes.
        if edit.program.is_some() || edit.restart {
            return Err(crate::ClientError::InvalidRequest("This server needs an upgrade to save program changes through Edit session. No settings were changed.".into()));
        }
        let mut outcome = claude_commander_protocol::api::EditSessionOutcome::default();
        let mut saved = false;
        let result: ClientResult<()> = async {
            if let Some(title) = edit.title {
                self.rename_session(id, title).await?;
                saved = true;
            }
            if let Some(section) = edit.section {
                self.set_section(id, section).await?;
                saved = true;
            }
            if let Some(base) = edit.base {
                let base = self.set_session_base(id, base.parent_session_id).await?;
                if let claude_commander_protocol::api::PrRetarget::Failed { message, .. } = &base.pr
                {
                    outcome.warnings.push(format!(
                        "Session saved, but PR target update failed: {message}"
                    ));
                }
                outcome.base = Some(base);
                saved = true;
            }
            // Old servers only offer a toggle, so read the current value as
            // late as possible — after the steps above, which can take seconds
            // — and flip only if it still differs. A change landing between
            // this read and the toggle is still inverted; the toggle's answer
            // exposes that, and it is reported rather than toggled back (which
            // could race the same way).
            if let Some(value) = edit.keep_alive {
                let current = self
                    .session_detail(&id.as_uuid().to_string(), None)
                    .await?
                    .ok_or(crate::ClientError::NotFound)?
                    .info
                    .keep_alive;
                if current != value {
                    let now = self.toggle_keep_alive(id).await?;
                    saved = true;
                    if now != value {
                        outcome.warnings.push(
                            "Keep alive was changed on the server during this edit, so it no longer matches what you chose. Refresh and set it again.".into(),
                        );
                    }
                }
            }
            Ok(())
        }
        .await;
        if let Err(error) = result {
            if !saved {
                return Err(error);
            }
            outcome.warnings.push(format!("Some settings were saved, but this older server could not complete the edit: {error}. Refresh and check the remaining settings."));
        }
        Ok(outcome)
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

    // -- Hosted repositories / repository clone --

    /// Provider-neutral listing. Falls back to the legacy GitHub endpoint only
    /// when an older server answers 404; all other failures retain their meaning.
    pub async fn repositories(&self) -> ClientResult<RepositoryListing> {
        match self
            .get_json_within(self.endpoint(&["repositories"]), REPO_LIST_TIMEOUT)
            .await
        {
            Ok(listing) => Ok(listing),
            Err(ClientError::NotFound) => {
                let repos = self.github_repos().await?;
                Ok(RepositoryListing {
                    host: CodeHost {
                        provider: CodeHostProvider::Github,
                        hostname: None,
                    },
                    repositories: repos.into_iter().map(HostedRepository::from).collect(),
                })
            }
            Err(error) => Err(error),
        }
    }

    /// `GET /github/repos` — every repo the server-side `gh` user can clone, for
    /// the repo picker.
    ///
    /// The list is the server's to produce: `gh` runs where repositories will
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
    #[tokio::test]
    async fn explicit_reads_bypass_recent_poll_payloads() {
        use std::sync::atomic::Ordering;
        let server = crate::test_server::Server::start(true).await;
        server.client.poll_hashes().await.unwrap();
        server.client.snapshot().await.unwrap();
        server.client.agent_states(false).await.unwrap();
        assert_eq!(server.data.workspace.load(Ordering::SeqCst), 2);
        assert_eq!(server.data.agents.load(Ordering::SeqCst), 2);
    }

    use super::*;

    async fn test_client_with_responses(
        responses: Vec<(&'static str, &'static str)>,
    ) -> (RemoteClient, tokio::task::JoinHandle<Vec<String>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let mut requests = Vec::new();
            for (status, body) in responses {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buffer = vec![0; 4096];
                let read = socket.read(&mut buffer).await.unwrap();
                requests.push(String::from_utf8_lossy(&buffer[..read]).into_owned());
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
            requests
        });
        let client = RemoteClient::new(RemoteServerSpec {
            name: "test".into(),
            base_url: format!("http://{addr}"),
            token: None,
        })
        .unwrap();
        (client, task)
    }

    #[tokio::test]
    async fn repositories_falls_back_to_legacy_github_route_only_on_404() {
        let legacy = r#"[{"full_name":"owner/repo","owner":"owner","name":"repo","description":null,"private":false,"fork":false,"archived":false,"default_branch":"main","clone_url":"https://github.com/owner/repo.git","ssh_url":"git@github.com:owner/repo.git","pushed_at":null}]"#;
        let (client, requests) = test_client_with_responses(vec![
            ("404 Not Found", r#"{"error":{"message":"missing"}}"#),
            ("200 OK", legacy),
        ])
        .await;
        let listing = client.repositories().await.unwrap();
        assert_eq!(listing.host.provider, CodeHostProvider::Github);
        assert_eq!(listing.repositories[0].full_name, "owner/repo");
        let requests = requests.await.unwrap();
        assert!(requests[0].starts_with("GET /api/repositories "));
        assert!(requests[1].starts_with("GET /api/github/repos "));
    }

    #[tokio::test]
    async fn repositories_does_not_fallback_on_server_error() {
        let (client, requests) = test_client_with_responses(vec![(
            "500 Internal Server Error",
            r#"{"error":{"message":"boom"}}"#,
        )])
        .await;
        assert!(matches!(
            client.repositories().await,
            Err(ClientError::Server(_))
        ));
        assert_eq!(requests.await.unwrap().len(), 1);
    }

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

#[cfg(test)]
mod session_edit_tests {
    use super::*;
    use axum::{Json, Router, extract::State, http::StatusCode, routing::patch};
    use serde::Deserialize;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    #[derive(Deserialize)]
    #[serde(tag = "op", rename_all = "snake_case")]
    enum LegacyPatch {
        Rename { title: String },
        SetSection { section: Option<String> },
    }

    /// A `/detail` body as a server without `edit` serves it.
    fn legacy_detail_json(sid: SessionId, pending: Option<&str>) -> serde_json::Value {
        serde_json::json!({
            "id":sid.as_uuid().to_string(), "session_id":sid, "title":"task", "branch":"task", "status":"running",
            "program":"claude", "pending_program":pending, "project_id":claude_commander_protocol::session::ProjectId::new(),
            "project_name":"repo", "pr_number":null, "pr_url":null, "pr_state":"open", "pr_draft":false,
            "pr_labels":[], "review_decision":null, "pr_reviewers":[], "created_at":"2026-01-01T00:00:00Z",
            "agent_state":"idle", "diff_stat":null, "pane_content":null
        })
    }

    #[tokio::test]
    async fn legacy_edit_fallback_is_narrow_and_reports_partial_saves() {
        for (unsupported, program, section_failure) in [
            (true, false, false),
            (true, true, false),
            (false, false, false),
            (true, false, true),
        ] {
            let calls = Arc::new(Mutex::new(Vec::<String>::new()));
            let attempts = Arc::new(AtomicUsize::new(0));
            let counter = attempts.clone();
            let app = if unsupported {
                Router::new().route("/api/sessions/{id}", patch(move |State(calls): State<Arc<Mutex<Vec<String>>>>, Json(body): Json<LegacyPatch>| async move {
                    let op = match body {
                        LegacyPatch::Rename { title } => { assert_eq!(title, "renamed"); "rename" },
                        LegacyPatch::SetSection { section } => { assert_eq!(section.as_deref(), Some("Review")); "set_section" },
                    };
                    calls.lock().unwrap().push(op.into());
                    if section_failure && op == "set_section" {
                        (StatusCode::UNPROCESSABLE_ENTITY, Json(serde_json::json!({"error":{"kind":"bad_request","message":"validation failed"}})))
                    } else { (StatusCode::NO_CONTENT, Json(serde_json::Value::Null)) }
                }))
            } else {
                Router::new().route("/api/sessions/{id}", patch(|| async {
                    (StatusCode::UNPROCESSABLE_ENTITY, Json(serde_json::json!({"error":{"kind":"bad_request","message":"validation failed"}})))
                }))
            }.with_state(calls.clone()).layer(axum::middleware::from_fn(move |request: axum::extract::Request, next: axum::middleware::Next| {
                counter.fetch_add(1, Ordering::SeqCst);
                async move { next.run(request).await }
            }));
            let served = crate::test_server::serve(app).await;
            let (client, addr) = (&served.client, served.addr);
            let edit = claude_commander_protocol::api::EditSession {
                title: Some("renamed".into()),
                program: program.then(|| "codex".into()),
                section: section_failure.then(|| Some("Review".into())),
                ..Default::default()
            };
            if unsupported {
                let raw = reqwest::Client::new()
                    .patch(format!("http://{addr}/api/sessions/test"))
                    .json(&serde_json::json!({"op":"edit"}))
                    .send()
                    .await
                    .unwrap()
                    .text()
                    .await
                    .unwrap();
                assert!(raw.starts_with("Failed to deserialize the JSON body into the target type: op: unknown variant `edit`"), "legacy extractor response: {raw}");
                attempts.store(0, Ordering::SeqCst);
            }
            let result = client.edit_session(SessionId::new(), edit).await;
            if !unsupported || program {
                assert!(result.is_err());
                assert_eq!(attempts.load(Ordering::SeqCst), 1);
                assert!(calls.lock().unwrap().is_empty());
            } else {
                let outcome = result.unwrap();
                assert_eq!(outcome.warnings.is_empty(), !section_failure);
                assert_eq!(calls.lock().unwrap()[0], "rename");
                assert_eq!(
                    attempts.load(Ordering::SeqCst),
                    if section_failure { 3 } else { 2 }
                );
                if section_failure {
                    assert_eq!(calls.lock().unwrap()[1], "set_section");
                }
            }
        }
    }
    /// An old server only offers a keep-alive *toggle*, so the fallback must
    /// read the current value as late as possible and report a toggle that
    /// landed on the wrong value rather than claim success.
    #[tokio::test]
    async fn legacy_edit_keep_alive_reads_late_and_reports_a_lost_race() {
        use axum::routing::{get, post};
        // `flip_on_rename`: a concurrent change lands while the rename is in
        // flight. `flip_after_read`: one lands between the read and the toggle.
        for (flip_on_rename, flip_after_read) in [(true, false), (false, true)] {
            let sid = SessionId::new();
            let keep_alive = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let detail = legacy_detail_json(sid, None);
            let (on_rename, on_detail, on_toggle) =
                (keep_alive.clone(), keep_alive.clone(), keep_alive.clone());
            let app = Router::new()
                .route(
                    "/api/sessions/{id}",
                    patch(move |Json(body): Json<LegacyPatch>| async move {
                        assert!(matches!(body, LegacyPatch::Rename { .. }));
                        if flip_on_rename {
                            on_rename.fetch_xor(true, Ordering::SeqCst);
                        }
                        StatusCode::NO_CONTENT
                    }),
                )
                .route(
                    "/api/sessions/{id}/detail",
                    get(move || async move {
                        let mut detail = detail.clone();
                        detail["keep_alive"] = on_detail.load(Ordering::SeqCst).into();
                        if flip_after_read {
                            on_detail.fetch_xor(true, Ordering::SeqCst);
                        }
                        Json(detail)
                    }),
                )
                .route(
                    "/api/sessions/{id}/keep-alive",
                    post(move || async move { Json(!on_toggle.fetch_xor(true, Ordering::SeqCst)) }),
                );
            let served = crate::test_server::serve(app).await;
            let client = &served.client;
            let edit = claude_commander_protocol::api::EditSession {
                title: Some("renamed".into()),
                keep_alive: Some(true),
                ..Default::default()
            };
            let outcome = client.edit_session(sid, edit).await.unwrap();
            if flip_on_rename {
                // Read after the rename: already true, so no toggle.
                assert!(keep_alive.load(Ordering::SeqCst));
                assert!(outcome.warnings.is_empty(), "{:?}", outcome.warnings);
            } else {
                // The toggle landed on the concurrent value; say so.
                assert!(!keep_alive.load(Ordering::SeqCst));
                assert!(
                    outcome.warnings.iter().any(|w| w.contains("Keep alive")),
                    "{:?}",
                    outcome.warnings
                );
            }
        }
    }
}
