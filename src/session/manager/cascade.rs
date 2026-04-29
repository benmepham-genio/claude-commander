//! Cascade-merge orchestration for PR stacks.
//!
//! Given a stack base session, merge the project's main branch into it and
//! propagate that merge up through each stacked child in order. On the first
//! conflict, mark the session as `CascadePaused`, persist the pause to
//! `AppState::cascade_paused_at`, and stop — the user resolves the conflict
//! in place (typically asking the attached Claude), commits, then runs the
//! resume command to continue propagating up the chain.

use std::path::{Path, PathBuf};

use tokio::process::Command;
use tracing::{info, instrument, warn};

use super::*;
use crate::error::SessionError;
use crate::session::{AgentState, stack_chain_from_base};

/// Result of running `git merge` on a worktree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeOutcome {
    /// Merge produced a merge commit cleanly.
    Clean,
    /// Nothing to merge — the target was already up-to-date.
    AlreadyUpToDate,
    /// Merge stopped with conflicts; the worktree is mid-merge
    /// (`.git/MERGE_HEAD` exists) and the user must resolve.
    Conflict,
}

/// Final outcome of a cascade-merge run.
#[derive(Debug, Clone)]
pub enum CascadeOutcome {
    /// Every session in the chain was merged cleanly.
    Complete { sessions_merged: usize },
    /// Cascade paused at the named session because its merge conflicted.
    /// Sessions earlier in the chain still have their merge commits; later
    /// ones were not touched and await a `cascade_resume`.
    PausedOnConflict {
        at: SessionId,
        sessions_merged: usize,
    },
}

/// Summary of a completed push-stack run.
#[derive(Debug, Clone)]
pub struct PushStackOutcome {
    pub sessions_pushed: usize,
}

impl SessionManager {
    /// Cascade-merge main → base → base's child → grandchild → …
    ///
    /// `start_from` can be any session in the stack; the cascade always runs
    /// from the stack's base walking upward. This matches the `t` hotkey
    /// behaviour (work on top of the stack) — users shouldn't need to know
    /// which member they selected.
    #[instrument(skip(self))]
    pub async fn cascade_merge_stack(
        &self,
        start_from: &SessionId,
        agent_states: &std::collections::HashMap<SessionId, AgentState>,
    ) -> Result<CascadeOutcome> {
        // Resolve chain + repo metadata under one read lock so the plan is
        // consistent before we start mutating.
        let (chain, repo_path, main_branch, worktrees) = {
            let state = self.store.read().await;
            let start_session = state
                .get_session(start_from)
                .ok_or(SessionError::NotFound(*start_from))?;
            let project_id = start_session.project_id;
            let project = state
                .get_project(&project_id)
                .ok_or_else(|| SessionError::ProjectNotFound(project_id.to_string()))?;

            let project_sessions: Vec<&WorktreeSession> = project
                .worktrees
                .iter()
                .filter_map(|sid| state.sessions.get(sid))
                .collect();

            // Walk to the stack base, then linearise from there so the
            // cascade starts at the bottom regardless of where the user
            // triggered it.
            let base_id = crate::session::resolve_stack_parent(start_session, &project_sessions)
                .map_or(*start_from, |_| {
                    walk_to_stack_base(*start_from, &project_sessions)
                });
            let chain = stack_chain_from_base(base_id, &project_sessions);

            let worktrees: Vec<(SessionId, PathBuf, String, String)> = chain
                .iter()
                .filter_map(|sid| {
                    state.sessions.get(sid).map(|s| {
                        (
                            s.id,
                            s.worktree_path.clone(),
                            s.branch.clone(),
                            s.title.clone(),
                        )
                    })
                })
                .collect();

            (
                chain,
                project.repo_path.clone(),
                project.main_branch.clone(),
                worktrees,
            )
        };

        if chain.len() < 2 {
            // Not really a stack — nothing useful to cascade. Caller should
            // gate on "selected session is in a stack"; belt-and-braces
            // check here too.
            return Ok(CascadeOutcome::Complete { sessions_merged: 0 });
        }

        // Pre-flight: fetch origin once, then validate every session in the
        // chain before touching any worktree.
        fetch_origin(&repo_path).await;
        for (sid, wt_path, _, title) in &worktrees {
            preflight_session(*sid, wt_path, title, agent_states)?;
        }

        // Walk the chain. Step i merges the previous step's branch into
        // chain[i]'s worktree. For the base (i == 0) the upstream is
        // `origin/<main>` — the remote ref just refreshed by `fetch_origin`,
        // not the local `main` branch which may be stale (a plain `git fetch`
        // updates `origin/main` but leaves local `main` alone).
        let mut sessions_merged = 0usize;
        for (i, (sid, wt_path, _branch, title)) in worktrees.iter().enumerate() {
            let upstream = if i == 0 {
                format!("origin/{main_branch}")
            } else {
                worktrees[i - 1].2.clone()
            };

            self.set_status(sid, SessionStatus::Merging).await;
            let outcome = run_git_merge(wt_path, &upstream).await;
            match outcome {
                Ok(MergeOutcome::Clean) | Ok(MergeOutcome::AlreadyUpToDate) => {
                    self.set_status(sid, SessionStatus::Running).await;
                    sessions_merged += 1;
                    info!(
                        "cascade: merged {} into session '{}' ({}/{})",
                        upstream,
                        title,
                        i + 1,
                        worktrees.len()
                    );
                }
                Ok(MergeOutcome::Conflict) => {
                    self.set_status(sid, SessionStatus::CascadePaused).await;
                    self.mark_cascade_paused(*sid).await;
                    warn!(
                        "cascade: paused at '{}' due to merge conflicts from {}",
                        title, upstream
                    );
                    return Ok(CascadeOutcome::PausedOnConflict {
                        at: *sid,
                        sessions_merged,
                    });
                }
                Err(e) => {
                    self.set_status(sid, SessionStatus::Running).await;
                    return Err(SessionError::CascadeMergeFailed {
                        session: *sid,
                        reason: e.to_string(),
                    }
                    .into());
                }
            }
        }

        Ok(CascadeOutcome::Complete { sessions_merged })
    }

    /// Resume a paused cascade from just after the session it stopped on.
    ///
    /// Assumes the user has resolved conflicts and committed the merge in
    /// that session's worktree. Verifies that state before continuing.
    #[instrument(skip(self))]
    pub async fn cascade_resume(
        &self,
        agent_states: &std::collections::HashMap<SessionId, AgentState>,
    ) -> Result<CascadeOutcome> {
        let paused_at = {
            let state = self.store.read().await;
            state
                .cascade_paused_at
                .ok_or(SessionError::NoCascadeInProgress)?
        };

        let worktree_path = {
            let state = self.store.read().await;
            state
                .get_session(&paused_at)
                .map(|s| s.worktree_path.clone())
                .ok_or(SessionError::NotFound(paused_at))?
        };

        // Refuse to resume while the worktree is still mid-merge.
        if merge_in_progress(&worktree_path) {
            return Err(SessionError::CascadeMergeIncomplete(paused_at).into());
        }

        // Clear the paused status + flag; we'll re-cascade from the session
        // after the paused one. Re-entering the chain from `paused_at` will
        // cleanly re-walk children since the paused session's branch now
        // carries the resolved merge commit.
        self.set_status(&paused_at, SessionStatus::Running).await;
        self.clear_cascade_paused().await;

        // Walk chain again and continue from the one after paused_at.
        let (chain, repo_path, main_branch, worktrees) = {
            let state = self.store.read().await;
            let start_session = state
                .get_session(&paused_at)
                .ok_or(SessionError::NotFound(paused_at))?;
            let project_id = start_session.project_id;
            let project = state
                .get_project(&project_id)
                .ok_or_else(|| SessionError::ProjectNotFound(project_id.to_string()))?;

            let project_sessions: Vec<&WorktreeSession> = project
                .worktrees
                .iter()
                .filter_map(|sid| state.sessions.get(sid))
                .collect();
            let base_id = walk_to_stack_base(paused_at, &project_sessions);
            let chain = stack_chain_from_base(base_id, &project_sessions);

            let worktrees: Vec<(SessionId, PathBuf, String, String)> = chain
                .iter()
                .filter_map(|sid| {
                    state.sessions.get(sid).map(|s| {
                        (
                            s.id,
                            s.worktree_path.clone(),
                            s.branch.clone(),
                            s.title.clone(),
                        )
                    })
                })
                .collect();

            (
                chain,
                project.repo_path.clone(),
                project.main_branch.clone(),
                worktrees,
            )
        };

        let Some(resume_idx) = chain.iter().position(|id| *id == paused_at) else {
            // The paused session isn't in the chain anymore (deleted? re-parented?).
            // Safest is to consider the cascade complete.
            return Ok(CascadeOutcome::Complete { sessions_merged: 0 });
        };

        // Pre-flight the remaining tail before we touch anything.
        let tail = &worktrees[resume_idx + 1..];
        for (sid, wt_path, _, title) in tail {
            preflight_session(*sid, wt_path, title, agent_states)?;
        }
        fetch_origin(&repo_path).await;

        let mut sessions_merged = 0usize;
        for (i, (sid, wt_path, _branch, title)) in tail.iter().enumerate() {
            // Upstream for the first tail session is the paused session's
            // branch (which now has the merged commit the user just made).
            // If resume happens to re-enter at the base (rare — paused on
            // the base itself), use `origin/<main>` for the same reason as
            // in `cascade_merge_stack`.
            let upstream_idx = resume_idx + i;
            let upstream = if upstream_idx == 0 {
                format!("origin/{main_branch}")
            } else {
                worktrees[upstream_idx].2.clone()
            };

            self.set_status(sid, SessionStatus::Merging).await;
            match run_git_merge(wt_path, &upstream).await {
                Ok(MergeOutcome::Clean) | Ok(MergeOutcome::AlreadyUpToDate) => {
                    self.set_status(sid, SessionStatus::Running).await;
                    sessions_merged += 1;
                    info!(
                        "cascade resume: merged {} into '{}' ({}/{})",
                        upstream,
                        title,
                        i + 1,
                        tail.len()
                    );
                }
                Ok(MergeOutcome::Conflict) => {
                    self.set_status(sid, SessionStatus::CascadePaused).await;
                    self.mark_cascade_paused(*sid).await;
                    return Ok(CascadeOutcome::PausedOnConflict {
                        at: *sid,
                        sessions_merged,
                    });
                }
                Err(e) => {
                    self.set_status(sid, SessionStatus::Running).await;
                    return Err(SessionError::CascadeMergeFailed {
                        session: *sid,
                        reason: e.to_string(),
                    }
                    .into());
                }
            }
        }

        Ok(CascadeOutcome::Complete { sessions_merged })
    }

    /// Abandon a paused cascade: clear `cascade_paused_at` and reset the
    /// paused session's status to `Running` without continuing the cascade.
    #[instrument(skip(self))]
    pub async fn cascade_abandon(&self) -> Result<()> {
        let paused_at = {
            let state = self.store.read().await;
            state
                .cascade_paused_at
                .ok_or(SessionError::NoCascadeInProgress)?
        };
        self.set_status(&paused_at, SessionStatus::Running).await;
        self.clear_cascade_paused().await;
        info!("cascade: abandoned pause at {}", paused_at);
        Ok(())
    }

    async fn set_status(&self, session_id: &SessionId, status: SessionStatus) {
        let sid = *session_id;
        let _ = self
            .store
            .mutate(move |state| {
                if let Some(session) = state.get_session_mut(&sid) {
                    session.set_status(status);
                }
            })
            .await;
    }

    async fn mark_cascade_paused(&self, session_id: SessionId) {
        let _ = self
            .store
            .mutate(move |state| {
                state.cascade_paused_at = Some(session_id);
            })
            .await;
    }

    async fn clear_cascade_paused(&self) {
        let _ = self
            .store
            .mutate(move |state| {
                state.cascade_paused_at = None;
            })
            .await;
    }

    /// Push every branch in the selected session's stack to `origin`, in
    /// base→leaf order so GitHub sees the base ref before any PR that targets
    /// it. Same walk / pre-flight / background-task shape as
    /// `cascade_merge_stack`. Stops on the first failed push and surfaces the
    /// git error; the user can fix and re-run (`git push` is idempotent).
    #[instrument(skip(self))]
    pub async fn push_stack(
        &self,
        start_from: &SessionId,
        agent_states: &std::collections::HashMap<SessionId, AgentState>,
    ) -> Result<PushStackOutcome> {
        let (chain, worktrees) = {
            let state = self.store.read().await;
            let start_session = state
                .get_session(start_from)
                .ok_or(SessionError::NotFound(*start_from))?;
            let project_id = start_session.project_id;
            let project = state
                .get_project(&project_id)
                .ok_or_else(|| SessionError::ProjectNotFound(project_id.to_string()))?;

            let project_sessions: Vec<&WorktreeSession> = project
                .worktrees
                .iter()
                .filter_map(|sid| state.sessions.get(sid))
                .collect();

            let base_id = crate::session::resolve_stack_parent(start_session, &project_sessions)
                .map_or(*start_from, |_| {
                    walk_to_stack_base(*start_from, &project_sessions)
                });
            let chain = stack_chain_from_base(base_id, &project_sessions);

            let worktrees: Vec<(SessionId, PathBuf, String, String)> = chain
                .iter()
                .filter_map(|sid| {
                    state.sessions.get(sid).map(|s| {
                        (
                            s.id,
                            s.worktree_path.clone(),
                            s.branch.clone(),
                            s.title.clone(),
                        )
                    })
                })
                .collect();

            (chain, worktrees)
        };

        if chain.is_empty() {
            return Ok(PushStackOutcome { sessions_pushed: 0 });
        }

        // Pre-flight: don't push while a live agent is busy (it might still
        // be writing to the worktree, and we'd push a half-committed state).
        // Uncommitted changes aren't fatal — `git push` ignores them — but
        // they're usually a sign the user wasn't ready, so reject them too.
        for (sid, wt_path, _, title) in &worktrees {
            preflight_session(*sid, wt_path, title, agent_states)?;
        }

        let mut sessions_pushed = 0usize;
        for (i, (sid, wt_path, branch, title)) in worktrees.iter().enumerate() {
            self.set_status(sid, SessionStatus::Pushing).await;
            match run_git_push(wt_path, branch).await {
                Ok(()) => {
                    self.set_status(sid, SessionStatus::Running).await;
                    sessions_pushed += 1;
                    info!(
                        "push_stack: pushed '{}' ({}/{})",
                        title,
                        i + 1,
                        worktrees.len()
                    );
                }
                Err(reason) => {
                    self.set_status(sid, SessionStatus::Running).await;
                    warn!("push_stack: failed at '{}': {}", title, reason);
                    return Err(SessionError::PushFailed {
                        session: *sid,
                        reason,
                    }
                    .into());
                }
            }
        }

        Ok(PushStackOutcome { sessions_pushed })
    }
}

/// Walk `resolve_stack_parent` upward until the base is found.
fn walk_to_stack_base(start: SessionId, project_sessions: &[&WorktreeSession]) -> SessionId {
    let mut current = start;
    for _ in 0..project_sessions.len() {
        let Some(current_session) = project_sessions.iter().find(|s| s.id == current) else {
            break;
        };
        match crate::session::resolve_stack_parent(current_session, project_sessions) {
            Some(parent) => current = parent,
            None => break,
        }
    }
    current
}

/// `true` if the worktree at `path` has a `.git/MERGE_HEAD` marker, i.e. a
/// merge is currently in progress.
fn merge_in_progress(worktree_path: &Path) -> bool {
    // In a linked worktree `.git` is a file pointing to `gitdir: …`; inside
    // that gitdir the merge state files live. Just probe both locations.
    let dot_git = worktree_path.join(".git");
    if dot_git.is_dir() {
        return dot_git.join("MERGE_HEAD").exists();
    }
    if dot_git.is_file()
        && let Ok(content) = std::fs::read_to_string(&dot_git)
        && let Some(line) = content.lines().next()
        && let Some(gitdir) = line.strip_prefix("gitdir: ")
    {
        let gitdir = worktree_path.join(gitdir.trim());
        return gitdir.join("MERGE_HEAD").exists();
    }
    false
}

/// Non-blocking best-effort `git fetch origin`. Failures are logged and
/// treated as soft errors — the cascade still runs against whatever main
/// happens to be at locally.
async fn fetch_origin(repo_path: &Path) {
    match Command::new("git")
        .current_dir(repo_path)
        .args(["fetch", "origin"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
    {
        Ok(out) if out.status.success() => {}
        Ok(out) => {
            warn!(
                "cascade: git fetch origin failed (continuing): {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Err(e) => {
            warn!("cascade: git fetch origin failed to spawn: {}", e);
        }
    }
}

/// Pre-flight one session: refuse if the live Claude is active (racing file
/// writes against a `git merge` is unrecoverable) or the worktree has
/// uncommitted changes.
fn preflight_session(
    session: SessionId,
    worktree_path: &Path,
    title: &str,
    agent_states: &std::collections::HashMap<SessionId, AgentState>,
) -> Result<()> {
    if let Some(state) = agent_states.get(&session) {
        match state {
            AgentState::Working => {
                return Err(SessionError::CascadePreflightFailed {
                    session,
                    reason: format!(
                        "'{title}' agent is Working — wait for it to finish before cascading"
                    ),
                }
                .into());
            }
            AgentState::WaitingForInput => {
                return Err(SessionError::CascadePreflightFailed {
                    session,
                    reason: format!(
                        "'{title}' agent is WaitingForInput — dismiss the prompt first"
                    ),
                }
                .into());
            }
            AgentState::Idle | AgentState::Unknown => {}
        }
    }

    let status_output = std::process::Command::new("git")
        .current_dir(worktree_path)
        .args(["status", "--porcelain"])
        .stdin(std::process::Stdio::null())
        .output();

    match status_output {
        Ok(out) if out.status.success() => {
            if !out.stdout.is_empty() {
                return Err(SessionError::CascadePreflightFailed {
                    session,
                    reason: format!(
                        "'{title}' worktree has uncommitted changes — commit or stash first"
                    ),
                }
                .into());
            }
        }
        Ok(out) => {
            return Err(SessionError::CascadePreflightFailed {
                session,
                reason: format!(
                    "'{title}' git status failed: {}",
                    String::from_utf8_lossy(&out.stderr)
                ),
            }
            .into());
        }
        Err(e) => {
            return Err(SessionError::CascadePreflightFailed {
                session,
                reason: format!("'{title}' git status failed to spawn: {e}"),
            }
            .into());
        }
    }
    Ok(())
}

/// Run `git merge <upstream> --no-edit --no-ff` in `worktree_path` and map
/// the outcome to `MergeOutcome`.
///
/// - exit 0, stdout contains "Already up to date" → `AlreadyUpToDate`
/// - exit 0, merge commit created → `Clean`
/// - exit 1 with "CONFLICT" in stdout or the worktree ends up with `MERGE_HEAD` → `Conflict`
/// - any other non-zero exit → error
pub async fn run_git_merge(worktree_path: &Path, upstream: &str) -> Result<MergeOutcome> {
    let output = Command::new("git")
        .current_dir(worktree_path)
        .args(["merge", upstream, "--no-edit", "--no-ff"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
        .map_err(|e| SessionError::CascadeMergeFailed {
            session: SessionId::new(), // placeholder; caller wraps with real id
            reason: format!("git merge spawn failed: {e}"),
        })?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    if output.status.success() {
        if stdout.contains("Already up to date") || stdout.contains("Already up-to-date") {
            return Ok(MergeOutcome::AlreadyUpToDate);
        }
        return Ok(MergeOutcome::Clean);
    }

    // Non-zero exit — distinguish conflict from real failure. A conflict
    // leaves MERGE_HEAD in the worktree; a fatal error (bad branch, etc.)
    // does not.
    if merge_in_progress(worktree_path) || stdout.contains("CONFLICT") {
        return Ok(MergeOutcome::Conflict);
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(SessionError::CascadeMergeFailed {
        session: SessionId::new(),
        reason: format!(
            "git merge {upstream} failed (exit {:?}): {}",
            output.status.code(),
            stderr.trim()
        ),
    }
    .into())
}

/// Run `git push -u origin <branch>` from `worktree_path`.
///
/// Returns `Ok(())` on success. On failure returns the trimmed stderr (or a
/// spawn-error description) as a human-readable string — the caller wraps it
/// into a `SessionError::PushFailed` with the session id.
///
/// `-u` sets the upstream tracking ref on first push and is a no-op on
/// subsequent pushes, so repeated invocations of push-stack are idempotent.
pub async fn run_git_push(worktree_path: &Path, branch: &str) -> std::result::Result<(), String> {
    let output = Command::new("git")
        .current_dir(worktree_path)
        .args(["push", "-u", "origin", branch])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
        .map_err(|e| format!("git push spawn failed: {e}"))?;

    if output.status.success() {
        return Ok(());
    }

    // Git prints the most useful diagnostics to stderr (rejected, auth,
    // non-fast-forward hints). Surface them verbatim so the user knows how
    // to recover.
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(format!(
        "git push origin {branch} failed (exit {:?}): {}",
        output.status.code(),
        stderr.trim()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_repo_with_main(repo: &Path, main: &str) {
        run(repo, &["init", "-q", "-b", main]);
        run(repo, &["config", "user.email", "test@example.com"]);
        run(repo, &["config", "user.name", "Test"]);
        run(repo, &["config", "commit.gpgsign", "false"]);
        std::fs::write(repo.join("README.md"), "initial\n").unwrap();
        run(repo, &["add", "."]);
        run(repo, &["commit", "-q", "-m", "initial"]);
    }

    fn run(cwd: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .current_dir(cwd)
            .args(args)
            .output()
            .expect("git invocation");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[tokio::test]
    async fn run_git_merge_clean() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        init_repo_with_main(repo, "main");
        // Branch off main, add a commit, merge main back in — always up-to-date.
        run(repo, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(repo.join("feature.txt"), "feature\n").unwrap();
        run(repo, &["add", "."]);
        run(repo, &["commit", "-q", "-m", "feature"]);

        // Add a divergent commit to main.
        run(repo, &["checkout", "-q", "main"]);
        std::fs::write(repo.join("main.txt"), "main\n").unwrap();
        run(repo, &["add", "."]);
        run(repo, &["commit", "-q", "-m", "main edit"]);

        // Merge main into feature cleanly.
        run(repo, &["checkout", "-q", "feature"]);
        let outcome = run_git_merge(repo, "main").await.unwrap();
        assert_eq!(outcome, MergeOutcome::Clean);
    }

    #[tokio::test]
    async fn run_git_merge_already_up_to_date() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        init_repo_with_main(repo, "main");
        run(repo, &["checkout", "-q", "-b", "feature"]);
        // No divergence on main; merge is a no-op.
        let outcome = run_git_merge(repo, "main").await.unwrap();
        assert_eq!(outcome, MergeOutcome::AlreadyUpToDate);
    }

    #[tokio::test]
    async fn run_git_merge_conflict() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        init_repo_with_main(repo, "main");
        // Both branches edit the same line of the same file.
        run(repo, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(repo.join("README.md"), "feature edit\n").unwrap();
        run(repo, &["add", "."]);
        run(repo, &["commit", "-q", "-m", "feature edit"]);

        run(repo, &["checkout", "-q", "main"]);
        std::fs::write(repo.join("README.md"), "main edit\n").unwrap();
        run(repo, &["add", "."]);
        run(repo, &["commit", "-q", "-m", "main edit"]);

        run(repo, &["checkout", "-q", "feature"]);
        let outcome = run_git_merge(repo, "main").await.unwrap();
        assert_eq!(outcome, MergeOutcome::Conflict);
        assert!(merge_in_progress(repo));
    }

    #[tokio::test]
    async fn run_git_merge_unknown_upstream_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        init_repo_with_main(repo, "main");
        let result = run_git_merge(repo, "no-such-branch").await;
        assert!(result.is_err(), "expected failure for missing upstream");
    }
}
