//! Route API exposed to Flutter via flutter_rust_bridge.
//!
//! Every `pub fn` here becomes an async-callable function on the Dart side (frb
//! runs the Rust body on a worker thread). They all drive a
//! [`claude_commander_client::RemoteClient`] resolved from the opaque server
//! `handle` (see [`crate::api::registry`]); the transport, auth, ret/timeout, and
//! error classification all live in that shared crate, so this module is a thin
//! handle-resolve → call → DTO-convert layer.
//!
//! The two `health*` probes are the exception: the connect screen calls them
//! *before* a handle exists, so they build a throwaway client from the raw
//! URL/token.

use std::path::PathBuf;

use anyhow::Result;
use claude_commander_client::{RemoteClient, RemoteServerSpec, SecretString};
use claude_commander_protocol::api::{
    BranchInfo, CreateOptions, CreateSessionOpts, ProgramInfo, SessionDetail, SessionInfo,
};
use claude_commander_protocol::github::{CloneJobId, GithubRepo};
use claude_commander_protocol::hosting::RepositoryListing;
use claude_commander_protocol::session::{SessionId, SessionStatus};

use crate::api::mirrors::{
    AgentStatesSnapshotDto, CloneJobDto, CloneRequestDto, OperationStatusDto, PreviewDataDto,
    SetWorkspacesRequestDto, SnapshotDto,
};
use crate::api::registry::{call, map_client_err, parse_project_id, parse_session_id, with_client};

#[flutter_rust_bridge::frb(init)]
pub fn init_app() {
    // Default utilities (logging, panic backtraces) for the bridge.
    flutter_rust_bridge::setup_default_user_utils();
}

/// Build a throwaway [`RemoteClient`] for an unauthenticated/pre-connect probe
/// from a raw base URL + optional token. An empty token means "no bearer".
fn probe_client(base_url: String, token: Option<String>) -> Result<RemoteClient> {
    let token = token.filter(|t| !t.is_empty()).map(SecretString::from);
    RemoteClient::new(RemoteServerSpec {
        name: "probe".to_string(),
        base_url,
        token,
    })
    .map_err(map_client_err)
}

/// Liveness probe: `GET {base_url}/health` (no auth). Returns true on a 2xx.
/// Called by the connect screen before any server handle exists.
pub fn health(base_url: String) -> Result<bool> {
    let client = probe_client(base_url, None)?;
    call(client.health())
}

/// Authenticated tmux probe: `GET {base_url}/api/health/tmux`. 200 → true, 503 →
/// false; a 401/403 surfaces as an auth error. Doubles as an auth check for the
/// connect screen.
pub fn health_tmux(base_url: String, token: String) -> Result<bool> {
    let client = probe_client(base_url, Some(token))?;
    call(client.health_tmux())
}

// -- Snapshot surface --

/// The whole server snapshot (projects, sessions, cascade/pending/pull state,
/// operations ledger, server health, workspace config) in one shot.
pub fn snapshot(handle: String) -> Result<SnapshotDto> {
    let client = with_client(&handle)?;
    Ok(call(client.snapshot())?.into())
}

/// Bulk agent-state snapshot (the commander sentinel entry is stripped by the
/// DTO). `fresh` forces a re-detection rather than a cached read.
pub fn agent_states(handle: String, fresh: bool) -> Result<AgentStatesSnapshotDto> {
    let client = with_client(&handle)?;
    Ok(call(client.agent_states(fresh))?.into())
}

/// Compatibility shim for the current session-list page: the sessions from the
/// workspace snapshot, optionally filtering out stopped ones client-side. The
/// app moves to snapshot-driven state in Phase 3; until then this keeps the list
/// page + its tests unchanged.
pub fn list_sessions(handle: String, include_stopped: bool) -> Result<Vec<SessionInfo>> {
    let client = with_client(&handle)?;
    let mut sessions = call(client.snapshot())?.sessions;
    if !include_stopped {
        sessions.retain(|s| s.status != SessionStatus::Stopped);
    }
    Ok(sessions)
}

/// A session's live detail (agent state, diff summary, pane snapshot). `query`
/// is matched loosely server-side (full id, branch, or title prefix); a 404
/// returns `None` so a deleted session reads as "gone".
pub fn get_session_detail(
    handle: String,
    query: String,
    lines: Option<u32>,
) -> Result<Option<SessionDetail>> {
    let client = with_client(&handle)?;
    call(client.session_detail(&query, lines.map(|n| n as usize)))
}

/// Preview payload for a session (agent pane + diff text/stat + shell pane).
pub fn session_preview(handle: String, id: String, lines: Option<u32>) -> Result<PreviewDataDto> {
    let client = with_client(&handle)?;
    let sid = parse_session_id(&id)?;
    Ok(call(client.session_preview(sid, lines.map(|n| n as usize)))?.into())
}

/// Preview payload for a project (diff text/stat; no agent pane).
pub fn project_preview(handle: String, id: String) -> Result<PreviewDataDto> {
    let client = with_client(&handle)?;
    let pid = parse_project_id(&id)?;
    Ok(call(client.project_preview(pid))?.into())
}

/// The raw unified diff (base → working tree) for a session's branch.
pub fn branch_diff(handle: String, id: String) -> Result<String> {
    let client = with_client(&handle)?;
    let sid = parse_session_id(&id)?;
    call(client.branch_diff(sid))
}

/// Branches for a project's base-branch picker. `fetch` runs a `git fetch` first.
pub fn list_branches(handle: String, project_id: String, fetch: bool) -> Result<Vec<BranchInfo>> {
    let client = with_client(&handle)?;
    let pid = parse_project_id(&project_id)?;
    call(client.list_branches(pid, fetch))
}

/// Options for the new-session dialog (default program, program list, sections).
pub fn create_options(handle: String) -> Result<CreateOptions> {
    let client = with_client(&handle)?;
    call(client.create_options())
}

/// Replace the server's configured program list wholesale.
pub fn set_programs(handle: String, programs: Vec<ProgramInfo>) -> Result<()> {
    let client = with_client(&handle)?;
    call(client.set_programs(programs))
}

/// Sessions with at least one not-yet-applied review comment.
pub fn pending_comment_sessions(handle: String) -> Result<Vec<SessionId>> {
    let client = with_client(&handle)?;
    call(client.pending_comment_sessions())
}

// -- Session mutations --

/// Create a session; returns the new session's full-id string. `project_path` is
/// a path on the *server's* filesystem.
#[allow(clippy::too_many_arguments)]
pub fn create_session(
    handle: String,
    project_path: String,
    title: String,
    program: Option<String>,
    initial_prompt: Option<String>,
    effort: Option<String>,
    mode: Option<String>,
    base_branch: Option<String>,
) -> Result<String> {
    let client = with_client(&handle)?;
    let opts = CreateSessionOpts {
        project_path: PathBuf::from(project_path),
        title,
        program,
        initial_prompt,
        effort,
        mode,
        model: None,
        base_branch,
        section: None,
        stack_parent: None,
    };
    let id = call(client.create_session(opts))?;
    Ok(id.as_uuid().to_string())
}

/// Stop a running session (its worktree is kept).
pub fn kill_session(handle: String, id: String) -> Result<()> {
    let client = with_client(&handle)?;
    call(client.kill_session(parse_session_id(&id)?))
}

/// Restart a session's program.
pub fn restart_session(handle: String, id: String) -> Result<()> {
    let client = with_client(&handle)?;
    call(client.restart_session(parse_session_id(&id)?))
}

/// Delete a session, its branch, and its worktree.
pub fn delete_session(handle: String, id: String) -> Result<()> {
    let client = with_client(&handle)?;
    call(client.delete_session(parse_session_id(&id)?))
}

/// Save only the changed fields produced by the shared editor planner.
pub fn edit_session(handle: String, id: String, patch_json: String) -> Result<Option<String>> {
    let client = with_client(&handle)?;
    let edit = serde_json::from_str(&patch_json)?;
    let outcome = call(client.edit_session(parse_session_id(&id)?, edit))?;
    Ok((!outcome.warnings.is_empty()).then(|| outcome.warnings.join("\n")))
}

/// Rename a session's title.
pub fn rename_session(handle: String, id: String, title: String) -> Result<()> {
    let client = with_client(&handle)?;
    call(client.rename_session(parse_session_id(&id)?, title))
}

/// Move a session to a section; `section: None` clears the manual override.
pub fn set_section(handle: String, id: String, section: Option<String>) -> Result<()> {
    let client = with_client(&handle)?;
    call(client.set_section(parse_session_id(&id)?, section))
}

/// Mark a session read (clears its unread indicator).
pub fn mark_read(handle: String, id: String) -> Result<()> {
    let client = with_client(&handle)?;
    call(client.mark_read(parse_session_id(&id)?))
}

/// Mark a batch of sessions unread (unknown ids are skipped server-side).
pub fn mark_unread(handle: String, ids: Vec<String>) -> Result<()> {
    let client = with_client(&handle)?;
    let ids = ids
        .iter()
        .map(|id| parse_session_id(id))
        .collect::<Result<Vec<_>>>()?;
    call(client.mark_unread(ids))
}

/// Toggle a session's keep-alive (idle-hibernation exemption); returns the new
/// state.
pub fn toggle_keep_alive(handle: String, id: String) -> Result<bool> {
    let client = with_client(&handle)?;
    call(client.toggle_keep_alive(parse_session_id(&id)?))
}

/// Upload an image to a session's agent pane (`POST /paste-image`).
///
/// The server writes the bytes to a temp file and types the path into the agent
/// pane without pressing Enter, so the user can add prompt text around it — the
/// form the Claude CLI accepts. The path shows up in the terminal view through
/// the normal attach output stream, so a caller needs no success feedback.
///
/// `bytes` are whatever the platform picker or clipboard produced;
/// [`RemoteClient::paste_image`] sniffs the content and refuses anything that
/// isn't an allow-listed image (or is over
/// [`claude_commander_protocol::paste::MAX_IMAGE_BYTES`]) *before* uploading, so
/// a doomed transfer never leaves the device.
pub fn paste_image(handle: String, id: String, bytes: Vec<u8>) -> Result<()> {
    let client = with_client(&handle)?;
    call(client.paste_image(parse_session_id(&id)?, bytes))
}

/// The pasted-image size cap in bytes, so the UI can reject an oversized pick
/// from its file length — without reading a 50 MB phone photo into memory just
/// to discard it — using the shared wire contract rather than a hardcoded Dart
/// mirror that could drift.
///
/// `u32` (not `usize`/`u64`) so this lands in Dart as a plain `int` rather than a
/// `BigInt`; the cap is single-digit MiB and will never approach 4 GiB. The
/// const assert below makes that an enforced precondition rather than a comment,
/// so raising the cap past `u32::MAX` fails the build instead of silently
/// truncating.
pub fn image_max_bytes() -> u32 {
    const _: () = assert!(claude_commander_protocol::paste::MAX_IMAGE_BYTES <= u32::MAX as usize);
    claude_commander_protocol::paste::MAX_IMAGE_BYTES as u32
}

/// How long a silent client can be away before the server is *guaranteed* to
/// have torn its terminal attach down, in milliseconds.
///
/// The mobile UI needs this on resume: a frozen background process can't answer
/// the server's heartbeat pings, so an absence longer than this means the attach
/// is certainly gone and must be re-opened, while a shorter one proves nothing —
/// leaving a live socket alone there is what keeps a scrolled tmux copy-mode view
/// in place. Sourced from the shared wire contract rather than a hardcoded Dart
/// threshold, which would drift silently the moment the heartbeat is retuned.
///
/// `u32` milliseconds (not a `Duration`) so this crosses the bridge as a plain
/// Dart `int`; the const assert makes the range a build-time precondition rather
/// than a comment.
pub fn attach_dead_after_millis() -> u32 {
    const MILLIS: u128 = claude_commander_protocol::ws::attach_dead_after().as_millis();
    const _: () = assert!(MILLIS <= u32::MAX as u128);
    MILLIS as u32
}

// -- Projects --

/// Register a project (git repo) by server-side path; returns the new project's
/// full-id string. `workspace` tags it (the app's active workspace); `None`
/// registers it in Main.
pub fn add_project(handle: String, path: String, workspace: Option<String>) -> Result<String> {
    let client = with_client(&handle)?;
    let id = call(client.add_project(PathBuf::from(path), workspace))?;
    Ok(id.as_uuid().to_string())
}

/// Register a project by server-side path, or return the id of the project
/// already registered for it; returns a full-id string either way.
///
/// The idempotent counterpart to [`add_project`], and what a "register this
/// existing checkout" offer must call: the path it was handed is frequently
/// already a project, and `add_project` would register a second entry for the
/// same repository. The dedupe (including how a path is resolved to a repository)
/// is the server's — no client restates the rule.
///
/// `workspace` tags the project only when this call newly registers it; an
/// already-registered project keeps the workspace it has.
pub fn ensure_project(handle: String, path: String, workspace: Option<String>) -> Result<String> {
    let client = with_client(&handle)?;
    let id = call(client.ensure_project(PathBuf::from(path), workspace))?;
    Ok(id.as_uuid().to_string())
}

/// Move a project to another workspace (`None` = Main). The server defines the
/// workspace on itself if it had no definition for it yet, which is how a
/// workspace created on another server reaches this one.
pub fn set_project_workspace(
    handle: String,
    project_id: String,
    workspace: Option<String>,
) -> Result<()> {
    let client = with_client(&handle)?;
    call(client.set_project_workspace(parse_project_id(&project_id)?, workspace))
}

// -- Workspace definitions --

/// Replace the server's workspace definitions wholesale (`PUT
/// /config/workspaces`). Never re-tags a project — renaming and deleting have
/// their own calls below because they must.
pub fn set_workspaces(handle: String, request: SetWorkspacesRequestDto) -> Result<()> {
    let client = with_client(&handle)?;
    call(client.set_workspaces(request.into()))
}

/// Rename a workspace and rewrite every project tagged with it. A no-op on a
/// server that has no workspace called `from`.
pub fn rename_workspace(handle: String, from: String, to: String) -> Result<()> {
    let client = with_client(&handle)?;
    call(client.rename_workspace(from, to))
}

/// Delete a workspace, moving its projects to Main. Idempotent.
pub fn delete_workspace(handle: String, name: String) -> Result<()> {
    let client = with_client(&handle)?;
    call(client.delete_workspace(name))
}

/// Remove a project (its sessions must already be gone).
pub fn remove_project(handle: String, id: String) -> Result<()> {
    let client = with_client(&handle)?;
    call(client.remove_project(parse_project_id(&id)?))
}

/// Result of scanning a directory for git repos to register.
pub struct ScanResultDto {
    pub added: u32,
    pub skipped: u32,
}

/// Scan a server-side directory for git repos, registering any new ones —
/// each tagged with `workspace` (the app's active workspace; `None` = Main), as
/// [`add_project`] tags its one.
pub fn scan_directory(
    handle: String,
    path: String,
    workspace: Option<String>,
) -> Result<ScanResultDto> {
    let client = with_client(&handle)?;
    let scan = call(client.scan_directory(PathBuf::from(path), workspace))?;
    Ok(ScanResultDto {
        added: scan.added as u32,
        skipped: scan.skipped as u32,
    })
}

// -- GitHub repos / repository clone --

/// Every repo the server-side `gh` user can clone, for the repo picker.
///
/// The list is the *server's* to produce: `gh` runs where the checkout will land,
/// so a phone with no `gh` and no GitHub credentials still gets a picker. A server
/// without `gh` answers 503, which arrives here as an error a UI can word as
/// "install gh on the server" rather than as a generic failure.
pub fn github_repos(handle: String) -> Result<Vec<GithubRepo>> {
    let client = with_client(&handle)?;
    call(client.github_repos())
}

/// Provider-neutral repository listing, including the selected host identity
/// even when no repositories are returned.
pub fn repositories(handle: String) -> Result<RepositoryListing> {
    let client = with_client(&handle)?;
    call(client.repositories())
}

/// Start a clone, returning the created job (the route answers 202 with the whole
/// job, so the id, the destination and the first status arrive together).
///
/// **The returned status is not a terminal status.** Every outcome — success,
/// failure, and an already-occupied destination — is reported through
/// [`clone_job`], so this reads `Running` essentially always. Poll from here; the
/// cadence is the caller's (nothing in this crate loops).
///
/// An unusable source or destination name is refused with a 400, which surfaces
/// as an error carrying the server's *already-redacted* reason. Nothing on this
/// path builds a message out of the request's source: a hand-pasted URL can carry
/// `user:token@` userinfo, and the rejection strings are redacted where they are
/// constructed in `claude-commander-protocol` precisely so no hop has to remember
/// to.
pub fn start_clone(handle: String, request: CloneRequestDto) -> Result<CloneJobDto> {
    let client = with_client(&handle)?;
    Ok(call(client.start_clone(request.into()))?.into())
}

/// One poll of a clone job.
///
/// **`None` is a normal answer, not an error.** The server prunes jobs a while
/// after they finish, so a client that keeps polling (or resumes with an id it
/// stored across a restart) must read "gone" rather than a failure it would
/// surface as a broken connection.
///
/// Takes the typed [`CloneJobId`] straight off the job [`start_clone`] returned,
/// unlike the session/project routes above which take a full-UUID `String`. Those
/// ids reach Dart as strings already (`SessionInfo.id`, `create_session`); a clone
/// job id only ever comes from a `CloneJobDto`, so a `String` parameter would add
/// a stringify-and-reparse round trip that can only introduce failures.
pub fn clone_job(handle: String, id: CloneJobId) -> Result<Option<CloneJobDto>> {
    let client = with_client(&handle)?;
    Ok(call(client.clone_job(id))?.map(Into::into))
}

/// Reduce a clone source to a stable `host/owner/name` identity, or `None` when it
/// has none (a local path or a `file://` URL — a local checkout has no GitHub
/// identity, so it never earns an "already added" badge).
///
/// Pure string work with no server involved, so it needs no handle. Exposed so
/// the repo picker's badge compares canonical forms through
/// [`claude_commander_protocol::github::canonical_repo_slug`] — the single
/// definition the server and the Rust client already use — rather than through a
/// second Dart implementation of the rule. Raw strings would miss most matches:
/// `gh repo clone` honours the user's configured `git_protocol`, so a repo cloned
/// by `gh` typically has an `ssh://` origin while the API reports `https://`.
///
/// Safe to feed a credentialed URL: the host is taken after the userinfo
/// delimiter (`github.rs`'s `host_of` splits on the last `@`), so a
/// `user:token@` component never reaches the returned slug.
pub fn canonical_repo_slug(url: String) -> Option<String> {
    claude_commander_protocol::github::canonical_repo_slug(&url)
}

// -- Cascade / push-stack --

/// Cascade-merge a session down its stack; returns the recorded operation.
pub fn cascade_merge(handle: String, id: String) -> Result<OperationStatusDto> {
    let client = with_client(&handle)?;
    Ok(call(client.cascade_merge(parse_session_id(&id)?))?.into())
}

/// Push a session's whole stack; returns the recorded operation.
pub fn push_stack(handle: String, id: String) -> Result<OperationStatusDto> {
    let client = with_client(&handle)?;
    Ok(call(client.push_stack(parse_session_id(&id)?))?.into())
}

/// Resume a paused cascade (after the conflict was resolved); returns the op.
pub fn cascade_resume(handle: String) -> Result<OperationStatusDto> {
    let client = with_client(&handle)?;
    Ok(call(client.cascade_resume())?.into())
}

/// Abandon a paused cascade.
pub fn cascade_abandon(handle: String) -> Result<()> {
    let client = with_client(&handle)?;
    call(client.cascade_abandon())
}

/// Ask the server to re-check PR metadata (runs its PR-status loop).
pub fn request_pr_refresh(handle: String) -> Result<()> {
    let client = with_client(&handle)?;
    call(client.request_pr_refresh())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The badge's whole reason for existing: an added repo's `ssh://` origin and
    /// the picker's `https://` clone URL must compare equal through this
    /// function. Pinned here — not only in the protocol crate — because this
    /// wrapper is what Dart actually calls, and a wrapper that trimmed, lowered
    /// or otherwise pre-chewed its argument would break the comparison while the
    /// protocol tests stayed green.
    #[test]
    fn slug_agrees_across_the_spellings_of_one_repo() {
        let expect = Some("github.com/sizeak/claude-commander".to_string());
        for form in [
            "git@github.com:sizeak/claude-commander.git",
            "ssh://git@github.com/sizeak/claude-commander",
            "https://github.com/sizeak/claude-commander.git",
            "https://github.com/SizeAk/Claude-Commander",
        ] {
            assert_eq!(
                canonical_repo_slug(form.to_string()),
                expect,
                "form: {form}"
            );
        }

        // A local checkout has no GitHub identity, so it never earns a badge.
        assert_eq!(canonical_repo_slug("/srv/mirrors/foo".to_string()), None);
        assert_eq!(
            canonical_repo_slug("file:///srv/mirrors/foo".to_string()),
            None
        );
    }

    /// A pasted credentialed URL must not smuggle its secret into the slug — the
    /// slug is a user-facing string the picker compares and could display.
    #[test]
    fn slug_never_carries_the_pasted_credential() {
        let slug = canonical_repo_slug(
            "https://sizeak:ghp_s3cr3tt0ken@github.com/sizeak/claude-commander.git".to_string(),
        )
        .expect("a credentialed https url still has a repo identity");
        assert_eq!(slug, "github.com/sizeak/claude-commander");
        assert!(
            !slug.contains("ghp_") && !slug.contains("sizeak:"),
            "the userinfo component must not reach the slug, got {slug}"
        );
    }

    /// A credentialed clone source must not reach Dart through a *failed* call.
    ///
    /// **Why this goes through the generated codec rather than asserting on the
    /// error's `Display`.** frb hands an `anyhow::Error` to Dart by serializing
    /// `format!("{:?}", self)` — `Debug`, i.e. the whole `Caused by:` chain
    /// (`frb_generated.rs`, `impl SseEncode for anyhow::Error`). So a
    /// `.context("cloning {url}")` added anywhere on this path would be invisible
    /// to a test that read `err.to_string()` (which prints only the outermost
    /// context) while being fully visible in the page's generic error render.
    /// Encoding through the real `SseEncode` impl is what closes that gap, and
    /// asserting on the serialized *bytes* means nothing between here and the
    /// wire can be the place the secret is dropped.
    ///
    /// Network-free and server-free: the handle points at a port nothing listens
    /// on, so the call fails on connect. Any route on this path errors the same
    /// way, and the request carrying the credential is what matters.
    #[test]
    fn a_failed_clone_never_encodes_the_credential_for_dart() {
        use crate::api::mirrors::{CloneRequestDto, CloneSourceDto, CloneSourceKind};
        use crate::frb_generated::SseEncode;
        use flutter_rust_bridge::for_generated::SseSerializer;

        const USERINFO_SECRET: &str = "ghp_dart_visible_leak_9f3a1c";
        const BEARER_SECRET: &str = "bearer_dart_visible_leak_4b7e2d";

        // Grab a free port and drop the listener, so connecting is refused.
        let addr = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap()
        };
        let handle = crate::api::registry::connect_server(
            format!("http://{addr}"),
            Some(BEARER_SECRET.to_string()),
        )
        .expect("connect_server does not probe reachability");

        let err = start_clone(
            handle.clone(),
            CloneRequestDto {
                source: CloneSourceDto {
                    kind: CloneSourceKind::Url,
                    value: format!(
                        "https://sizeak:{USERINFO_SECRET}@github.com/sizeak/claude-commander.git"
                    ),
                    hostname: None,
                },
                dest_name: None,
                workspace: None,
            },
        );
        crate::api::registry::disconnect_server(handle);
        let Err(err) = err else {
            panic!("a refused connection must be an error, not a job")
        };

        // The exact bytes the bridge would put on the wire for Dart.
        let mut serializer = SseSerializer::new();
        err.sse_encode(&mut serializer);
        let encoded = String::from_utf8_lossy(&serializer.cursor.into_inner()).to_string();

        for secret in [USERINFO_SECRET, BEARER_SECRET, "sizeak:"] {
            assert!(
                !encoded.contains(secret),
                "the Dart-visible error carries {secret:?}: {encoded}"
            );
        }
    }
}
