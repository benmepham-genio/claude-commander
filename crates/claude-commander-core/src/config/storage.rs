//! Persistent state storage
//!
//! Manages session state persistence in JSON format

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::{ConfigError, Result};
use crate::session::{Project, ProjectId, SessionId, SetBaseRejection, WorktreeSession};

use super::Config;

/// A pending GitHub PR base-branch retarget, produced when deleting a session
/// that has PR-stacked children. The async delete path runs `gh pr edit` for
/// each so the retarget survives the next PR sync.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrBaseRetarget {
    /// PR number of the child whose base branch should change.
    pub pr_number: u32,
    /// Repository the `gh pr edit` runs against.
    pub repo_path: PathBuf,
    /// The branch the PR should now target (the deleted session's parent, or
    /// the project's main branch when the deleted session was the stack root).
    pub new_base_branch: String,
}

/// The computed plan for retargeting a deleted session's *direct* stacked
/// children. Built once by [`AppState::plan_stack_retarget`] and shared by the
/// delete-confirm preview, the durable PR-edit planner, and the local metadata
/// retarget, so all three agree on the destination and the affected child set
/// rather than recomputing it three ways.
struct StackRetargetPlan {
    /// New stack parent for each direct child — `None` when the deleted session
    /// was the stack root, so children become top-level roots.
    new_parent_id: Option<SessionId>,
    /// Branch the children should now be based on: the new parent's branch, or
    /// the project's main branch when the deleted session was the stack root.
    new_base_branch: String,
    /// The deleted session's branch, used to spot children stacked via a live
    /// PR pointing at it.
    deleted_branch: String,
    /// Repository the children's `gh pr edit` retargets run against.
    repo_path: PathBuf,
    /// The deleted session's direct stacked children.
    child_ids: Vec<SessionId>,
}

/// The computed plan for retargeting one session's stack base, built and
/// consumed inside a single [`AppState::set_session_base`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetBasePlan {
    /// The session being retargeted.
    pub session_id: SessionId,
    /// Its new stack parent — `None` when it is being unstacked onto main.
    pub new_parent_id: Option<SessionId>,
    /// Branch it should now be based on: the new parent's branch, or the
    /// project's main branch when unstacking.
    pub new_base_branch: String,
    /// The branch it was based on before, for reporting.
    pub old_base_branch: Option<String>,
}

/// Persistent application state
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppState {
    /// All registered projects
    #[serde(default)]
    pub projects: HashMap<ProjectId, Project>,

    /// All worktree sessions
    #[serde(default)]
    pub sessions: HashMap<SessionId, WorktreeSession>,

    /// Whether the user has seen the help screen
    #[serde(default)]
    pub seen_help: bool,

    /// Last selected project ID
    #[serde(default)]
    pub last_selected_project: Option<ProjectId>,

    /// Last selected session ID
    #[serde(default)]
    pub last_selected_session: Option<SessionId>,

    /// Session where an in-flight cascade-merge hit a conflict and paused.
    /// While set, `CascadeResume` (or `CascadeAbandon`) is available; cleared
    /// once resume succeeds or the user abandons. Pairs with the affected
    /// session's `SessionStatus::CascadePaused`.
    #[serde(default)]
    pub cascade_paused_at: Option<SessionId>,

    /// Application version that last wrote this state. Informational only —
    /// **never gate a migration on this field.** It is stamped on every load
    /// and every save but never read to make a decision, so a comparison
    /// against it can't tell whether a given migration has run. For a one-time
    /// mutation use a dedicated schema counter (see the "Migrations" note in
    /// CLAUDE.md); for `state.json` specifically, prefer repair-on-every-read
    /// (see [`Self::backfill_base_branch`]) because an older binary may still
    /// write this file after any gate would have flipped.
    #[serde(default)]
    pub version: String,

    /// Anonymous, resettable install identifier for usage telemetry. Lazily
    /// generated on first run (a random UUID) and persisted so events from the
    /// same install can be grouped without identifying the user. Not tied to
    /// anything personal; reset with [`AppState::reset_install_id`].
    #[serde(default)]
    pub install_id: Option<String>,

    /// Path to save state to (not serialized, set at load time)
    #[serde(skip)]
    state_path: Option<PathBuf>,
}

impl AppState {
    /// The state as clients can observe it: everything persisted except
    /// bookkeeping no client reads — currently only each session's
    /// `pr_checked_at`, which every successful PR poll advances.
    /// [`StateStore`](super::store::StateStore) wakes its change-feed only when
    /// this view changes, so a poll that found nothing new still persists its
    /// stamp (the cross-process race guard reads it from disk) without waking
    /// every client.
    ///
    /// A `serde_json::Value` rather than serialised bytes because it is
    /// compared across *different* `AppState` instances (the cache against a
    /// fresh read of disk), whose `HashMap`s need not iterate in the same
    /// order; `Value`'s map equality ignores order.
    pub(crate) fn client_visible(&self) -> serde_json::Result<serde_json::Value> {
        let mut visible = self.clone();
        for session in visible.sessions.values_mut() {
            session.pr_checked_at = None;
        }
        serde_json::to_value(&visible)
    }

    /// Create a new empty state
    pub fn new() -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION").to_string(),
            ..Default::default()
        }
    }

    /// Set the anonymous install id to `id` if none is currently stored (or the
    /// stored one is empty). Returns `true` if it was set. This is the single
    /// guard for install-id presence, shared by the telemetry seeding path.
    pub fn set_install_id_if_absent(&mut self, id: &str) -> bool {
        if self.install_id.as_deref().is_none_or(str::is_empty) {
            self.install_id = Some(id.to_string());
            true
        } else {
            false
        }
    }

    /// Forget the current install id; a fresh one is seeded on next launch.
    pub fn reset_install_id(&mut self) {
        self.install_id = None;
    }

    /// Load state from the default location
    pub fn load() -> Result<Self> {
        let path = Config::state_file_path()?;
        Self::load_from(&path)
    }

    /// Load state, or print a clear refusal to stderr and exit non-zero
    /// when the state file exists but cannot be parsed. Use this from CLI
    /// entry points instead of `.unwrap_or_else(|_| AppState::new())`,
    /// which silently drops the user's projects into a fresh empty state
    /// and then risks overwriting the real file on the next save.
    ///
    /// "File doesn't exist" is not an error — `load_from` already returns
    /// a fresh `AppState` in that case.
    pub fn load_or_exit() -> Self {
        let path = match Config::state_file_path() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("Failed to determine state file path: {}", e);
                std::process::exit(2);
            }
        };
        match Self::load_from(&path) {
            Ok(state) => state,
            Err(e) => {
                eprintln!(
                    "Refusing to start: state file exists but failed to load.\n\
                     Path: {}\n\
                     Error: {}\n\
                     \n\
                     Your data is still on disk. To investigate, open the file\n\
                     above; to start fresh, move it aside (e.g. `mv … ….bak`)\n\
                     and relaunch.",
                    path.display(),
                    e,
                );
                std::process::exit(2);
            }
        }
    }

    /// Load state from a specific path
    pub fn load_from(path: &PathBuf) -> Result<Self> {
        if !path.exists() {
            let mut state = Self::new();
            state.state_path = Some(path.clone());
            return Ok(state);
        }

        let content = std::fs::read_to_string(path).map_err(|e| {
            ConfigError::LoadFailed(format!(
                "Failed to read state file {}: {}",
                path.display(),
                e
            ))
        })?;

        let mut state: AppState = serde_json::from_str(&content).map_err(|e| {
            ConfigError::LoadFailed(format!(
                "Failed to parse state file {}: {}",
                path.display(),
                e
            ))
        })?;

        // Update version and remember path
        state.version = env!("CARGO_PKG_VERSION").to_string();
        state.state_path = Some(path.clone());
        state.backfill_base_branch();

        Ok(state)
    }

    /// Populate [`WorktreeSession::base_branch`] for sessions persisted before
    /// the field existed, so the review diff resolves its base against a *live*
    /// branch instead of the frozen `base_commit` SHA.
    ///
    /// Derives the branch the session was forked from: the PR target branch if
    /// known, else the stack parent's branch (so a stacked session diffs against
    /// its parent, not main), else the project's main branch. Sessions that
    /// already carry `base_branch` are left untouched.
    pub(crate) fn backfill_base_branch(&mut self) {
        let mut assignments: Vec<(SessionId, String)> = Vec::new();
        for project in self.projects.values() {
            let project_sessions: Vec<&WorktreeSession> = project
                .worktrees
                .iter()
                .filter_map(|id| self.sessions.get(id))
                .collect();
            for s in &project_sessions {
                if s.base_branch.is_some() {
                    continue;
                }
                let derived = if let Some(pr_base) = s.pr_base_branch.clone() {
                    pr_base
                } else if let Some(parent_id) =
                    crate::session::resolve_stack_parent(*s, &project_sessions)
                {
                    match project_sessions.iter().find(|p| p.id == parent_id) {
                        Some(parent) => parent.branch.clone(),
                        None => project.main_branch.clone(),
                    }
                } else {
                    project.main_branch.clone()
                };
                assignments.push((s.id, derived));
            }
        }
        for (id, branch) in assignments {
            if let Some(s) = self.sessions.get_mut(&id) {
                s.base_branch = Some(branch);
            }
        }
    }

    /// Compute the single source-of-truth plan for retargeting `deleted_id`'s
    /// *direct* stacked children: where they move (the deleted session's own
    /// parent and that parent's branch, or `None` + the project's main branch
    /// when the deleted session was the stack root) and which sessions are
    /// affected. Returns `None` only when the session or its project is missing;
    /// an empty `child_ids` is a valid plan (nothing to retarget).
    ///
    /// The preview, the PR-edit planner, and the local metadata retarget all
    /// build on this so they can never disagree on the destination or the set
    /// of children.
    fn plan_stack_retarget(&self, deleted_id: &SessionId) -> Option<StackRetargetPlan> {
        let project_id = self.sessions.get(deleted_id)?.project_id;
        let (main_branch, repo_path) = {
            let project = self.projects.get(&project_id)?;
            (project.main_branch.clone(), project.repo_path.clone())
        };
        let project_sessions = self.get_project_sessions(&project_id);
        let deleted = project_sessions.iter().find(|s| s.id == *deleted_id)?;
        let deleted_branch = deleted.branch.clone();

        let new_parent_id = crate::session::resolve_stack_parent(*deleted, &project_sessions);
        let new_base_branch = new_parent_id
            .and_then(|pid| project_sessions.iter().find(|s| s.id == pid))
            .map(|p| p.branch.clone())
            .unwrap_or(main_branch);
        let child_ids = project_sessions
            .iter()
            .filter(|s| {
                crate::session::resolve_stack_parent(**s, &project_sessions) == Some(*deleted_id)
            })
            .map(|s| s.id)
            .collect();

        Some(StackRetargetPlan {
            new_parent_id,
            new_base_branch,
            deleted_branch,
            repo_path,
            child_ids,
        })
    }

    /// Apply a precomputed retarget plan to the deleted session's *direct*
    /// children: re-point each onto the new parent/base so they stay stacked
    /// instead of orphaning into top-level roots.
    ///
    /// Metadata-only: updates each child's `stack_parent_session_id`,
    /// `base_branch`, and — for children stacked via a live PR pointing at the
    /// deleted branch — the local `pr_base_branch` mirror so the tree re-stacks
    /// immediately. The durable GitHub PR edit is handled separately via
    /// [`AppState::pr_retargets_from_plan`].
    fn apply_stack_retarget(&mut self, plan: &StackRetargetPlan) {
        for child_id in &plan.child_ids {
            if let Some(child) = self.sessions.get_mut(child_id) {
                child.stack_parent_session_id = plan.new_parent_id;
                child.base_branch = Some(plan.new_base_branch.clone());
                // A child stacked via a live PR resolves its parent by branch
                // name, ignoring the local hint — mirror the new base locally so
                // it re-stacks before the next PR sync overwrites it.
                if child.pr_base_branch.as_deref() == Some(plan.deleted_branch.as_str()) {
                    child.pr_base_branch = Some(plan.new_base_branch.clone());
                }
            }
        }
    }

    /// Durable GitHub PR-base edits implied by a plan: for each *direct* child
    /// stacked via a live PR (its `pr_base_branch` matches the deleted branch)
    /// with a known PR number, a [`PrBaseRetarget`] pointing the PR at the new
    /// base branch.
    ///
    /// Must be read from the *pre-retarget* child state — call before
    /// [`AppState::apply_stack_retarget`] rewrites `pr_base_branch`.
    fn pr_retargets_from_plan(&self, plan: &StackRetargetPlan) -> Vec<PrBaseRetarget> {
        plan.child_ids
            .iter()
            .filter_map(|id| self.sessions.get(id))
            .filter(|s| s.pr_base_branch.as_deref() == Some(plan.deleted_branch.as_str()))
            .filter_map(|s| {
                s.pr_number.map(|pr_number| PrBaseRetarget {
                    pr_number,
                    repo_path: plan.repo_path.clone(),
                    new_base_branch: plan.new_base_branch.clone(),
                })
            })
            .collect()
    }

    /// Retarget one session's stack base onto `new_parent` (or onto the
    /// project's main branch when `None`), returning the plan that was applied
    /// and the durable `gh pr edit` the async caller must still run.
    ///
    /// Validation and mutation happen together, in one `&mut self` pass, for the
    /// same reason [`AppState::remove_session_retargeting_children`] does it:
    /// [`crate::config::StateStore::mutate`] re-reads the state from disk under
    /// its lock, so anything planned against an earlier snapshot could be stale
    /// by the time it is applied. Splitting the cycle check from the write would
    /// let a concurrent retarget or PR sync slip between them and close a loop
    /// that the check had just rejected.
    ///
    /// Only the named session moves. Its own descendants stack on *its branch*,
    /// which does not change, so they follow it automatically and need no write.
    pub fn set_session_base(
        &mut self,
        session_id: &SessionId,
        new_parent: Option<SessionId>,
    ) -> std::result::Result<(SetBasePlan, Option<PrBaseRetarget>), SetBaseRejection> {
        let plan = self.plan_set_base(session_id, new_parent)?;
        // Read the PR-edit plan before applying. `pr_retarget_for_set_base`
        // happens to read only `pr_number`/`repo_path`, which apply leaves
        // alone — but keeping the read first means it stays correct if it ever
        // needs a field apply does rewrite.
        let pr_retarget = self.pr_retarget_for_set_base(&plan);
        self.apply_set_base(&plan);
        Ok((plan, pr_retarget))
    }

    /// Validate a base retarget and compute where the session lands.
    ///
    /// Rejections are ordered cheapest-first, and each is distinct so the UI can
    /// say *why* rather than just refusing.
    fn plan_set_base(
        &self,
        session_id: &SessionId,
        new_parent: Option<SessionId>,
    ) -> std::result::Result<SetBasePlan, SetBaseRejection> {
        let session = self
            .sessions
            .get(session_id)
            .ok_or(SetBaseRejection::SessionNotFound)?;
        let project_id = session.project_id;

        if new_parent == Some(*session_id) {
            return Err(SetBaseRejection::SelfParent);
        }

        // Refuse a settled PR. The load-bearing reason is in this repo and does
        // not depend on GitHub's behaviour: `check_pr_for_branch` polls with
        // `--state all`, so a merged/closed PR still returns `Found`, and
        // every accepted PR check (`apply_pr_results_at_epoch`, api.rs)
        // re-asserts its `base_ref_name` over `pr_base_branch` — which
        // outranks the local hint in both `resolve_stack_parent` and
        // `ReviewBase::of`. Only a failed fetch, a stale-provider sweep or a
        // check that lost a race to a newer one is skipped, and the next
        // accepted check re-asserts the field. So any change we made here
        // would be undone at the next poll.
        // `gh pr edit --base` is also expected to fail on a merged PR, which
        // would leave nothing durable behind; that half is untested here.
        if let Some(state) = session.pr_state {
            match state {
                crate::git::PrState::Merged => {
                    return Err(SetBaseRejection::PrNotOpen { state: "merged" });
                }
                crate::git::PrState::Closed => {
                    return Err(SetBaseRejection::PrNotOpen { state: "closed" });
                }
                crate::git::PrState::Open => {}
            }
        }

        // A paused cascade is mid-flight over this stack, and `cascade_resume`
        // re-derives its chain from current topology — a re-parented session is
        // silently treated as "cascade complete", so merges the tail still needs
        // never happen. Make the user resolve the cascade first.
        if let Some(paused_at) = self.cascade_paused_at
            && self.sessions.get(&paused_at).map(|s| s.project_id) == Some(project_id)
        {
            return Err(SetBaseRejection::CascadeInProgress);
        }

        let project = self
            .projects
            .get(&project_id)
            .ok_or(SetBaseRejection::SessionNotFound)?;
        let main_branch = project.main_branch.clone();
        let project_sessions = self.sorted_project_sessions(&project_id);

        let (new_parent_id, new_base_branch) = match new_parent {
            None => (None, main_branch),
            Some(pid) => {
                // Looked up across *all* sessions, not just this project's, so a
                // cross-project target reports `DifferentProject` rather than
                // the misleading "no longer exists" a project-scoped find gives.
                let parent = self
                    .sessions
                    .get(&pid)
                    .ok_or(SetBaseRejection::ParentNotFound)?;
                if parent.project_id != project_id {
                    return Err(SetBaseRejection::DifferentProject);
                }
                if crate::session::is_descendant_of(pid, *session_id, &project_sessions) {
                    return Err(SetBaseRejection::WouldCycle {
                        parent_title: parent.title.clone(),
                    });
                }
                // `resolve_stack_parent` matches a parent by *branch name*, not
                // id, so a candidate that merely shares a branch with one of this
                // session's descendants would still close a loop once applied.
                // The id check above cannot see that; this one can.
                if project_sessions.iter().any(|s| {
                    s.branch == parent.branch
                        && s.id != pid
                        && (s.id == *session_id
                            || crate::session::is_descendant_of(
                                s.id,
                                *session_id,
                                &project_sessions,
                            ))
                }) {
                    return Err(SetBaseRejection::WouldCycle {
                        parent_title: parent.title.clone(),
                    });
                }
                (Some(pid), parent.branch.clone())
            }
        };

        // "Already based" means *applying would change nothing* — not merely
        // that the resolved base name matches. The three fields can disagree
        // (a PR retargeted on GitHub moves `pr_base_branch` while the local
        // hint and `base_branch` stay stale), and repairing exactly that
        // disagreement is what this command is for. Comparing only the resolved
        // name would refuse the repair and leave the session to snap back to
        // its stale parent the moment the PR closes.
        let old_base_branch = session
            .pr_base_branch
            .clone()
            .or_else(|| session.base_branch.clone());
        let would_change = session.stack_parent_session_id != new_parent_id
            || session.base_branch.as_deref() != Some(new_base_branch.as_str())
            || (session.pr_base_branch.is_some()
                && session.pr_base_branch.as_deref() != Some(new_base_branch.as_str()));
        if !would_change {
            return Err(SetBaseRejection::AlreadyBased {
                branch: new_base_branch,
            });
        }

        Ok(SetBasePlan {
            session_id: *session_id,
            new_parent_id,
            new_base_branch,
            old_base_branch,
        })
    }

    /// Apply a validated plan: move all three stack fields together.
    ///
    /// `pr_base_branch` is mirrored *only* when it was already set. It is
    /// GitHub's value, and it outranks the local hint in `resolve_stack_parent`
    /// — so with a live PR the mirror is what re-stacks the tree immediately
    /// (until `gh pr edit` makes it durable), while with no PR there is nothing
    /// to mirror and leaving it `None` is exactly what lets the local hint win.
    fn apply_set_base(&mut self, plan: &SetBasePlan) {
        if let Some(session) = self.sessions.get_mut(&plan.session_id) {
            session.stack_parent_session_id = plan.new_parent_id;
            session.base_branch = Some(plan.new_base_branch.clone());
            if session.pr_base_branch.is_some() {
                session.pr_base_branch = Some(plan.new_base_branch.clone());
            }
        }
    }

    /// The durable `gh pr edit` a plan implies: present whenever the session has
    /// a PR number at all.
    ///
    /// Gated on `pr_number`, not `pr_base_branch` — GitHub's `baseRefName` is
    /// optional in the fetched JSON, so a session can have a PR whose base we
    /// never recorded. Skipping those would leave a local-only change that the
    /// next sync reverts.
    fn pr_retarget_for_set_base(&self, plan: &SetBasePlan) -> Option<PrBaseRetarget> {
        let session = self.sessions.get(&plan.session_id)?;
        let pr_number = session.pr_number?;
        let repo_path = self.projects.get(&session.project_id)?.repo_path.clone();
        Some(PrBaseRetarget {
            pr_number,
            repo_path,
            new_base_branch: plan.new_base_branch.clone(),
        })
    }

    /// A project's sessions in a deterministic order.
    ///
    /// [`Self::get_project_sessions`] iterates a `HashMap`, so ties in any
    /// downstream `find`/`max_by_key` resolve differently run to run. The
    /// base-retarget walks use this instead; the older `plan_stack_retarget`
    /// and `backfill_base_branch` still take the unordered form.
    fn sorted_project_sessions(&self, project_id: &ProjectId) -> Vec<&WorktreeSession> {
        let mut sessions = self.get_project_sessions(project_id);
        sessions.sort_by_key(|s| (s.created_at, s.id));
        sessions
    }

    /// Preview the stack-retarget that deleting `session_id` would trigger:
    /// `(number of direct stacked children, branch they'd be retargeted onto)`.
    ///
    /// Returns `None` when the session has no direct stacked children, so the
    /// delete confirmation only mentions retargeting when it actually applies.
    pub fn stack_retarget_preview(&self, session_id: &SessionId) -> Option<(usize, String)> {
        let plan = self.plan_stack_retarget(session_id)?;
        if plan.child_ids.is_empty() {
            return None;
        }
        Some((plan.child_ids.len(), plan.new_base_branch))
    }

    /// Plan the durable GitHub PR-base edits needed when deleting `deleted_id`,
    /// computed from the current snapshot *before* removal so the async delete
    /// path can run `gh pr edit` afterwards. See [`AppState::pr_retargets_from_plan`].
    pub fn pr_retargets_for_delete(&self, deleted_id: &SessionId) -> Vec<PrBaseRetarget> {
        self.plan_stack_retarget(deleted_id)
            .map(|plan| self.pr_retargets_from_plan(&plan))
            .unwrap_or_default()
    }

    /// Save state to the remembered location (or default if none)
    pub fn save(&self) -> Result<()> {
        let path = match &self.state_path {
            Some(p) => p.clone(),
            None => Config::state_file_path()?,
        };
        self.save_to(&path)
    }

    /// Save state to a specific path
    pub fn save_to(&self, path: &PathBuf) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                ConfigError::SaveFailed(format!("Failed to create state directory: {}", e))
            })?;
        }

        let content = serde_json::to_string_pretty(self)
            .map_err(|e| ConfigError::SaveFailed(format!("Failed to serialize state: {}", e)))?;

        std::fs::write(path, content)
            .map_err(|e| ConfigError::SaveFailed(format!("Failed to write state file: {}", e)))?;

        Ok(())
    }

    /// Add a project
    pub fn add_project(&mut self, project: Project) {
        self.projects.insert(project.id, project);
    }

    /// Remove a project and all its sessions
    pub fn remove_project(&mut self, project_id: &ProjectId) -> Option<Project> {
        if let Some(project) = self.projects.remove(project_id) {
            // Remove all associated sessions
            for session_id in &project.worktrees {
                self.sessions.remove(session_id);
            }
            Some(project)
        } else {
            None
        }
    }

    /// Get a project by ID
    pub fn get_project(&self, id: &ProjectId) -> Option<&Project> {
        self.projects.get(id)
    }

    /// Get a mutable reference to a project
    pub fn get_project_mut(&mut self, id: &ProjectId) -> Option<&mut Project> {
        self.projects.get_mut(id)
    }

    /// Add a session
    pub fn add_session(&mut self, session: WorktreeSession) {
        let project_id = session.project_id;
        let session_id = session.id;

        self.sessions.insert(session_id, session);

        // Add to parent project
        if let Some(project) = self.projects.get_mut(&project_id) {
            project.add_worktree(session_id);
        }
    }

    /// Remove a session. Pure removal: drops the session and unlinks it from its
    /// project, touching nothing else. Crash-recovery cleanup of half-created
    /// sessions uses this directly so it never rewrites surviving siblings.
    ///
    /// A user-initiated delete should call [`AppState::remove_session_retargeting_children`]
    /// instead, which also re-points the deleted session's stacked children.
    pub fn remove_session(&mut self, session_id: &SessionId) -> Option<WorktreeSession> {
        if let Some(session) = self.sessions.remove(session_id) {
            // Remove from parent project
            if let Some(project) = self.projects.get_mut(&session.project_id) {
                project.remove_worktree(session_id);
            }
            Some(session)
        } else {
            None
        }
    }

    /// Remove a session as a user-initiated delete: re-point its *direct*
    /// stacked children onto the deleted session's parent (or the project's main
    /// branch when it was the stack root), then remove it. Returns the removed
    /// session together with the durable GitHub PR-base edits the async caller
    /// must run so the retarget survives the next PR sync.
    ///
    /// The local metadata retarget and the removal happen here as one step, and
    /// the PR-edit plan is handed back, so the two halves of the delete stay
    /// co-located and can't drift apart. Computing the plan inside the same
    /// `mutate` closure as the removal also keeps it atomic — there is no
    /// read-then-remove window for a concurrent task to invalidate.
    pub fn remove_session_retargeting_children(
        &mut self,
        session_id: &SessionId,
    ) -> (Option<WorktreeSession>, Vec<PrBaseRetarget>) {
        let Some(plan) = self.plan_stack_retarget(session_id) else {
            return (self.remove_session(session_id), Vec::new());
        };
        // Read the PR-edit plan from the pre-retarget child state, then apply the
        // local retarget and remove the session.
        let pr_retargets = self.pr_retargets_from_plan(&plan);
        self.apply_stack_retarget(&plan);
        (self.remove_session(session_id), pr_retargets)
    }

    /// Get a session by ID
    pub fn get_session(&self, id: &SessionId) -> Option<&WorktreeSession> {
        self.sessions.get(id)
    }

    /// Get a mutable reference to a session
    pub fn get_session_mut(&mut self, id: &SessionId) -> Option<&mut WorktreeSession> {
        self.sessions.get_mut(id)
    }

    /// Get all sessions for a project
    pub fn get_project_sessions(&self, project_id: &ProjectId) -> Vec<&WorktreeSession> {
        self.sessions
            .values()
            .filter(|s| s.project_id == *project_id)
            .collect()
    }

    /// Get all active sessions
    pub fn get_active_sessions(&self) -> Vec<&WorktreeSession> {
        self.sessions
            .values()
            .filter(|s| s.status.is_active())
            .collect()
    }

    /// Count total sessions
    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    /// Count total projects
    pub fn project_count(&self) -> usize {
        self.projects.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn create_test_project() -> Project {
        Project::new("test-project", PathBuf::from("/tmp/test"), "main")
    }

    #[test]
    fn set_install_id_if_absent_sets_once_and_keeps_first() {
        let mut state = AppState::new();
        assert!(state.install_id.is_none());

        // First set takes effect and reports a change.
        assert!(state.set_install_id_if_absent("install-aaa"));
        assert_eq!(state.install_id.as_deref(), Some("install-aaa"));

        // A second set is a no-op: the first id wins.
        assert!(!state.set_install_id_if_absent("install-bbb"));
        assert_eq!(state.install_id.as_deref(), Some("install-aaa"));
    }

    #[test]
    fn install_id_survives_save_and_reload_and_resets() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("state.json");

        let mut state = AppState::load_from(&path).unwrap();
        state.set_install_id_if_absent("install-xyz");
        state.save_to(&path).unwrap();

        // Reload from disk → same id.
        let reloaded = AppState::load_from(&path).unwrap();
        assert_eq!(reloaded.install_id.as_deref(), Some("install-xyz"));

        // Reset → a fresh id can be seeded again.
        let mut state = reloaded;
        state.reset_install_id();
        assert!(state.install_id.is_none());
        assert!(state.set_install_id_if_absent("install-new"));
        assert_eq!(state.install_id.as_deref(), Some("install-new"));
    }

    fn create_test_session(project_id: ProjectId) -> WorktreeSession {
        WorktreeSession::new(
            project_id,
            "Test Session",
            "test-branch",
            PathBuf::from("/tmp/worktree"),
            "claude",
        )
    }

    fn create_test_session_with_status(
        project_id: ProjectId,
        status: crate::session::SessionStatus,
    ) -> WorktreeSession {
        let mut session = create_test_session(project_id);
        session.set_status(status);
        session
    }

    #[test]
    fn test_new_state() {
        let state = AppState::new();
        assert!(state.projects.is_empty());
        assert!(state.sessions.is_empty());
        assert!(!state.seen_help);
    }

    #[test]
    fn backfill_base_branch_derives_parent_for_stacked_and_main_for_root() {
        // Sessions persisted before `base_branch` existed carry only a frozen
        // `base_commit`. Backfill must derive the *branch* each was forked from
        // so the review diff resolves against its live tip: a stacked session
        // against its parent's branch, a root session against main. Without the
        // backfill the stacked child would fall back to the frozen SHA, which
        // drifts stale and re-includes the parent's later commits.
        let mut state = AppState::new();
        let project = create_test_project(); // main_branch = "main"
        let project_id = project.id;
        state.add_project(project);

        // Root session, no PR, no base_branch.
        let mut root = WorktreeSession::new(
            project_id,
            "Root",
            "root-feature",
            PathBuf::from("/tmp/root"),
            "claude",
        );
        root.base_commit = Some("rootsha".to_string());
        let root_id = root.id;
        state.add_session(root);

        // Stacked child pointing at the root via the local hint, no PR yet.
        let mut child = WorktreeSession::new(
            project_id,
            "Child",
            "child-feature",
            PathBuf::from("/tmp/child"),
            "claude",
        );
        child.base_commit = Some("childsha".to_string());
        child.stack_parent_session_id = Some(root_id);
        let child_id = child.id;
        state.add_session(child);

        // Pre-existing field must not be clobbered.
        let mut explicit = WorktreeSession::new(
            project_id,
            "Explicit",
            "explicit-feature",
            PathBuf::from("/tmp/explicit"),
            "claude",
        );
        explicit.base_branch = Some("develop".to_string());
        let explicit_id = explicit.id;
        state.add_session(explicit);

        state.backfill_base_branch();

        assert_eq!(
            state.get_session(&root_id).unwrap().base_branch.as_deref(),
            Some("main"),
            "root session should diff against main"
        );
        assert_eq!(
            state.get_session(&child_id).unwrap().base_branch.as_deref(),
            Some("root-feature"),
            "stacked child should diff against its parent's branch, not main"
        );
        assert_eq!(
            state
                .get_session(&explicit_id)
                .unwrap()
                .base_branch
                .as_deref(),
            Some("develop"),
            "existing base_branch must be preserved"
        );
    }

    /// Build `count` sessions linked into a chain via the local
    /// `stack_parent_session_id` hint: returned[0] is the root, each subsequent
    /// session is stacked on the previous one. `base_branch` is seeded to mirror
    /// what creation would set (parent's branch / main for the root).
    fn build_local_stack(
        state: &mut AppState,
        project_id: ProjectId,
        branches: &[&str],
    ) -> Vec<SessionId> {
        let main_branch = state.get_project(&project_id).unwrap().main_branch.clone();
        let mut ids = Vec::new();
        for (i, branch) in branches.iter().enumerate() {
            let mut s = WorktreeSession::new(
                project_id,
                format!("S{i}"),
                *branch,
                PathBuf::from(format!("/tmp/{branch}")),
                "claude",
            );
            if i == 0 {
                s.base_branch = Some(main_branch.clone());
            } else {
                s.stack_parent_session_id = Some(ids[i - 1]);
                s.base_branch = Some(branches[i - 1].to_string());
            }
            ids.push(s.id);
            state.add_session(s);
        }
        ids
    }

    #[test]
    fn deleting_mid_stack_retargets_child_onto_grandparent() {
        // A <- B <- C <- D; deleting C should re-point D onto B (not orphan it).
        let mut state = AppState::new();
        let project = create_test_project(); // main_branch = "main"
        let project_id = project.id;
        state.add_project(project);
        let ids = build_local_stack(&mut state, project_id, &["a", "b", "c", "d"]);
        let (b, c, d) = (ids[1], ids[2], ids[3]);

        state.remove_session_retargeting_children(&c);

        let dsn = state.get_session(&d).unwrap();
        assert_eq!(
            dsn.stack_parent_session_id,
            Some(b),
            "D should re-stack onto B"
        );
        assert_eq!(
            dsn.base_branch.as_deref(),
            Some("b"),
            "D should now diff against B's branch"
        );
    }

    #[test]
    fn deleting_root_drops_child_to_top_level_against_main() {
        // A <- B; deleting the root A leaves B unstacked against main.
        let mut state = AppState::new();
        let project = create_test_project();
        let project_id = project.id;
        state.add_project(project);
        let ids = build_local_stack(&mut state, project_id, &["a", "b"]);
        let (a, b) = (ids[0], ids[1]);

        state.remove_session_retargeting_children(&a);

        let bsn = state.get_session(&b).unwrap();
        assert_eq!(bsn.stack_parent_session_id, None, "B should become a root");
        assert_eq!(
            bsn.base_branch.as_deref(),
            Some("main"),
            "B should diff against main"
        );
    }

    #[test]
    fn deleting_mid_stack_retargets_all_direct_children_but_not_grandchildren() {
        // A <- B <- C, with C having two direct children D and E, and E having a
        // grandchild F. Deleting C retargets D and E onto B; F stays on E.
        let mut state = AppState::new();
        let project = create_test_project();
        let project_id = project.id;
        state.add_project(project);
        let ids = build_local_stack(&mut state, project_id, &["a", "b", "c"]);
        let (b, c) = (ids[1], ids[2]);

        // Second direct child of C.
        let mut e = WorktreeSession::new(project_id, "E", "e", PathBuf::from("/tmp/e"), "claude");
        e.stack_parent_session_id = Some(c);
        e.base_branch = Some("c".to_string());
        let e_id = e.id;
        state.add_session(e);
        // First direct child of C.
        let mut d = WorktreeSession::new(project_id, "D", "d", PathBuf::from("/tmp/d"), "claude");
        d.stack_parent_session_id = Some(c);
        d.base_branch = Some("c".to_string());
        let d_id = d.id;
        state.add_session(d);
        // Grandchild stacked on E.
        let mut f = WorktreeSession::new(project_id, "F", "f", PathBuf::from("/tmp/f"), "claude");
        f.stack_parent_session_id = Some(e_id);
        f.base_branch = Some("e".to_string());
        let f_id = f.id;
        state.add_session(f);

        state.remove_session_retargeting_children(&c);

        for child in [d_id, e_id] {
            let s = state.get_session(&child).unwrap();
            assert_eq!(
                s.stack_parent_session_id,
                Some(b),
                "direct child re-stacks onto B"
            );
            assert_eq!(s.base_branch.as_deref(), Some("b"));
        }
        let fsn = state.get_session(&f_id).unwrap();
        assert_eq!(
            fsn.stack_parent_session_id,
            Some(e_id),
            "grandchild untouched"
        );
        assert_eq!(fsn.base_branch.as_deref(), Some("e"));
    }

    #[test]
    fn deleting_mid_stack_rewrites_pr_stacked_child_base_locally() {
        // D is stacked on C via a live PR (pr_base_branch == C.branch), not the
        // local hint. Deleting C must mirror the new base onto pr_base_branch so
        // the tree re-stacks immediately.
        let mut state = AppState::new();
        let project = create_test_project();
        let project_id = project.id;
        state.add_project(project);
        let ids = build_local_stack(&mut state, project_id, &["a", "b", "c"]);
        let (b, c) = (ids[1], ids[2]);

        let mut d = WorktreeSession::new(project_id, "D", "d", PathBuf::from("/tmp/d"), "claude");
        d.pr_base_branch = Some("c".to_string());
        d.pr_number = Some(42);
        d.base_branch = Some("c".to_string());
        let d_id = d.id;
        state.add_session(d);

        // The user-delete path retargets children and hands back the PR-edit
        // plan in one atomic step.
        let (_, plan) = state.remove_session_retargeting_children(&c);

        let dsn = state.get_session(&d_id).unwrap();
        assert_eq!(
            dsn.pr_base_branch.as_deref(),
            Some("b"),
            "PR-stacked child should now point at B's branch"
        );
        assert_eq!(dsn.stack_parent_session_id, Some(b));
        assert_eq!(
            plan,
            vec![PrBaseRetarget {
                pr_number: 42,
                repo_path: state.get_project(&project_id).unwrap().repo_path.clone(),
                new_base_branch: "b".to_string(),
            }],
            "should plan a gh pr edit retargeting PR #42 onto B"
        );
    }

    #[test]
    fn deleting_unstacked_session_is_a_noop_for_others() {
        // Lone session with no children: deletion touches nothing else.
        let mut state = AppState::new();
        let project = create_test_project();
        let project_id = project.id;
        state.add_project(project);
        let ids = build_local_stack(&mut state, project_id, &["a", "b"]);
        let (a, b) = (ids[0], ids[1]);

        // Delete the leaf B (no children) — A is unaffected.
        state.remove_session_retargeting_children(&b);
        let asn = state.get_session(&a).unwrap();
        assert_eq!(asn.stack_parent_session_id, None);
        assert_eq!(asn.base_branch.as_deref(), Some("main"));
        assert!(state.pr_retargets_for_delete(&a).is_empty());
    }

    #[test]
    fn plain_remove_session_does_not_retarget_children() {
        // The pure removal primitive (used by crash-recovery cleanup of stale
        // Creating sessions) must NOT rewrite surviving siblings — only the
        // user-delete path retargets. Deleting B via remove_session leaves C's
        // stack metadata untouched (C orphans rather than re-stacking onto A).
        let mut state = AppState::new();
        let project = create_test_project();
        let project_id = project.id;
        state.add_project(project);
        let ids = build_local_stack(&mut state, project_id, &["a", "b", "c"]);
        let (b, c) = (ids[1], ids[2]);

        state.remove_session(&b);

        let csn = state.get_session(&c).unwrap();
        assert_eq!(
            csn.stack_parent_session_id,
            Some(b),
            "pure removal must not re-point C onto A"
        );
        assert_eq!(
            csn.base_branch.as_deref(),
            Some("b"),
            "pure removal must not rewrite C's base branch"
        );
    }

    #[test]
    fn test_load_from_corrupt_file_errors_and_names_the_path() {
        // Regression: `for_cli` and the (since removed) popup picker swallowed
        // this error
        // with `.unwrap_or_else(|_| AppState::new())`, so a corrupt
        // state.json silently reported "no sessions" — and creating a
        // session from that empty view persisted a duplicate project. The
        // error must propagate, and should name the file so the CLI's
        // printed error tells the user what to look at.
        let temp_dir = TempDir::new().unwrap();
        let state_path = temp_dir.path().join("state.json");
        std::fs::write(&state_path, "{ not valid json").unwrap();

        let err = AppState::load_from(&state_path).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("state.json"),
            "error should name the state file: {msg}"
        );
    }

    #[test]
    fn test_add_remove_project() {
        let mut state = AppState::new();
        let project = create_test_project();
        let project_id = project.id;

        state.add_project(project);
        assert_eq!(state.project_count(), 1);

        let removed = state.remove_project(&project_id);
        assert!(removed.is_some());
        assert_eq!(state.project_count(), 0);
    }

    #[test]
    fn test_add_remove_session() {
        let mut state = AppState::new();
        let project = create_test_project();
        let project_id = project.id;
        state.add_project(project);

        let session = create_test_session(project_id);
        let session_id = session.id;

        state.add_session(session);
        assert_eq!(state.session_count(), 1);

        // Check session is linked to project
        let project = state.get_project(&project_id).unwrap();
        assert!(project.worktrees.contains(&session_id));

        // Remove session
        let removed = state.remove_session(&session_id);
        assert!(removed.is_some());
        assert_eq!(state.session_count(), 0);

        // Check session is unlinked from project
        let project = state.get_project(&project_id).unwrap();
        assert!(!project.worktrees.contains(&session_id));
    }

    #[test]
    fn test_section_fields_roundtrip() {
        let temp_dir = TempDir::new().unwrap();
        let state_path = temp_dir.path().join("state.json");

        let mut state = AppState::new();
        let project = create_test_project();
        let project_id = project.id;
        state.add_project(project);

        let mut session = create_test_session(project_id);
        session.section_override = Some("Needs Review".to_string());
        session.current_section = Some("In Progress".to_string());
        let stamp = session.entered_section_at;
        let session_id = session.id;
        state.add_session(session);

        state.save_to(&state_path).unwrap();
        let loaded = AppState::load_from(&state_path).unwrap();
        let loaded_session = loaded.get_session(&session_id).unwrap();

        assert_eq!(
            loaded_session.section_override.as_deref(),
            Some("Needs Review")
        );
        assert_eq!(
            loaded_session.current_section.as_deref(),
            Some("In Progress")
        );
        assert_eq!(loaded_session.entered_section_at, stamp);
    }

    #[test]
    fn test_load_tolerates_removed_view_mode_and_left_pane_pct_keys() {
        // The board redesign removed the persisted `view_mode` and
        // `left_pane_pct` fields. State files written by an older build still
        // carry those keys; since `AppState` has no `deny_unknown_fields`,
        // serde must ignore them rather than failing the whole load — which
        // would drop the user's entire project list. The legacy
        // `SectionGroupedWithStacks` view_mode value must also be tolerated.
        let temp_dir = TempDir::new().unwrap();
        let state_path = temp_dir.path().join("state.json");
        std::fs::write(
            &state_path,
            r#"{"seen_help": true, "left_pane_pct": 42, "view_mode": "SectionGroupedWithStacks"}"#,
        )
        .unwrap();

        let loaded = AppState::load_from(&state_path).unwrap();
        assert!(loaded.seen_help);
    }

    #[test]
    fn test_save_load_roundtrip() {
        let temp_dir = TempDir::new().unwrap();
        let state_path = temp_dir.path().join("state.json");

        let mut state = AppState::new();
        let project = create_test_project();
        let project_id = project.id;
        state.add_project(project);

        let session = create_test_session(project_id);
        state.add_session(session);

        state.save_to(&state_path).unwrap();

        let loaded = AppState::load_from(&state_path).unwrap();
        assert_eq!(loaded.project_count(), 1);
        assert_eq!(loaded.session_count(), 1);
    }

    #[test]
    fn test_get_project_sessions() {
        let mut state = AppState::new();

        let project1 = create_test_project();
        let project1_id = project1.id;
        state.add_project(project1);

        let mut project2 = create_test_project();
        project2.name = "other-project".to_string();
        let project2_id = project2.id;
        state.add_project(project2);

        // Add sessions to project1
        state.add_session(create_test_session(project1_id));
        state.add_session(create_test_session(project1_id));

        // Add session to project2
        state.add_session(create_test_session(project2_id));

        let p1_sessions = state.get_project_sessions(&project1_id);
        assert_eq!(p1_sessions.len(), 2);

        let p2_sessions = state.get_project_sessions(&project2_id);
        assert_eq!(p2_sessions.len(), 1);
    }

    #[test]
    fn test_remove_project_cascades_sessions() {
        let mut state = AppState::new();
        let project = create_test_project();
        let project_id = project.id;
        state.add_project(project);

        let s1 = create_test_session(project_id);
        let s1_id = s1.id;
        let s2 = create_test_session(project_id);
        let s2_id = s2.id;
        let s3 = create_test_session(project_id);
        let s3_id = s3.id;
        state.add_session(s1);
        state.add_session(s2);
        state.add_session(s3);

        assert_eq!(state.session_count(), 3);
        state.remove_project(&project_id);
        assert_eq!(state.session_count(), 0);
        assert!(state.get_session(&s1_id).is_none());
        assert!(state.get_session(&s2_id).is_none());
        assert!(state.get_session(&s3_id).is_none());
    }

    #[test]
    fn test_remove_project_only_cascades_own_sessions() {
        let mut state = AppState::new();

        let project_a = create_test_project();
        let a_id = project_a.id;
        state.add_project(project_a);

        let mut project_b = create_test_project();
        project_b.name = "other".to_string();
        let b_id = project_b.id;
        state.add_project(project_b);

        state.add_session(create_test_session(a_id));
        state.add_session(create_test_session(a_id));
        let b_session = create_test_session(b_id);
        let b_session_id = b_session.id;
        state.add_session(b_session);

        state.remove_project(&a_id);
        assert_eq!(state.session_count(), 1);
        assert!(state.get_session(&b_session_id).is_some());
    }

    #[test]
    fn test_add_session_bidirectional_link() {
        let mut state = AppState::new();
        let project = create_test_project();
        let project_id = project.id;
        state.add_project(project);

        let session = create_test_session(project_id);
        let session_id = session.id;
        state.add_session(session);

        assert!(state.sessions.contains_key(&session_id));
        assert!(
            state
                .get_project(&project_id)
                .unwrap()
                .worktrees
                .contains(&session_id)
        );
    }

    #[test]
    fn test_remove_session_bidirectional_unlink() {
        let mut state = AppState::new();
        let project = create_test_project();
        let project_id = project.id;
        state.add_project(project);

        let session = create_test_session(project_id);
        let session_id = session.id;
        state.add_session(session);

        state.remove_session(&session_id);
        assert!(state.sessions.is_empty());
        assert!(state.get_project(&project_id).unwrap().worktrees.is_empty());
    }

    #[test]
    fn test_add_session_nonexistent_project_no_panic() {
        let mut state = AppState::new();
        let orphan_project_id = ProjectId::new();
        let session = create_test_session(orphan_project_id);
        let session_id = session.id;

        state.add_session(session);
        assert_eq!(state.session_count(), 1);
        assert!(state.get_session(&session_id).is_some());
        assert!(state.get_project(&orphan_project_id).is_none());
    }

    #[test]
    fn test_get_active_sessions_filters_correctly() {
        use crate::session::SessionStatus;

        let mut state = AppState::new();
        let project = create_test_project();
        let project_id = project.id;
        state.add_project(project);

        state.add_session(create_test_session_with_status(
            project_id,
            SessionStatus::Running,
        ));
        state.add_session(create_test_session_with_status(
            project_id,
            SessionStatus::Stopped,
        ));

        let active = state.get_active_sessions();
        assert_eq!(active.len(), 1);
        assert!(active.iter().all(|s| s.status != SessionStatus::Stopped));
    }

    #[test]
    fn test_get_project_sessions_empty_for_unknown_project() {
        let mut state = AppState::new();
        let project = create_test_project();
        let project_id = project.id;
        state.add_project(project);

        let sessions = state.get_project_sessions(&project_id);
        assert!(sessions.is_empty());
    }

    #[test]
    fn test_remove_nonexistent_session_returns_none() {
        let mut state = AppState::new();
        assert!(state.remove_session(&SessionId::new()).is_none());
    }

    #[test]
    fn test_remove_nonexistent_project_returns_none() {
        let mut state = AppState::new();
        assert!(state.remove_project(&ProjectId::new()).is_none());
    }

    #[test]
    fn test_cascade_paused_at_defaults_to_none() {
        let state = AppState::new();
        assert!(state.cascade_paused_at.is_none());
    }

    #[test]
    fn test_cascade_paused_at_roundtrips_through_json() {
        let temp_dir = TempDir::new().unwrap();
        let state_path = temp_dir.path().join("state.json");

        let mut state = AppState::new();
        let sid = SessionId::new();
        state.cascade_paused_at = Some(sid);
        state.save_to(&state_path).unwrap();

        let loaded = AppState::load_from(&state_path).unwrap();
        assert_eq!(loaded.cascade_paused_at, Some(sid));
    }

    #[test]
    fn test_cascade_paused_at_missing_from_json_defaults_to_none() {
        // A state file written before this feature must still load cleanly.
        let temp_dir = TempDir::new().unwrap();
        let state_path = temp_dir.path().join("state.json");
        std::fs::write(&state_path, r#"{"seen_help": true, "version": "0.1.0"}"#).unwrap();
        let loaded = AppState::load_from(&state_path).unwrap();
        assert!(loaded.cascade_paused_at.is_none());
    }

    /// A standalone root session in `project_id`, for use as a retarget target
    /// that is *not* a descendant of the session under test.
    fn add_root_session(state: &mut AppState, project_id: ProjectId, branch: &str) -> SessionId {
        let mut s = WorktreeSession::new(
            project_id,
            format!("Root {branch}"),
            branch,
            PathBuf::from(format!("/tmp/{branch}")),
            "claude",
        );
        s.base_branch = Some("main".to_string());
        let id = s.id;
        state.add_session(s);
        id
    }

    // -- set_session_base --

    /// Retargeting a session with no PR moves the local hint and `base_branch`
    /// but must leave `pr_base_branch` `None` — there is nothing to mirror, and
    /// a `None` there is exactly what lets `resolve_stack_parent` honour the hint.
    #[test]
    fn set_session_base_without_a_pr_sets_hint_and_base_branch_only() {
        let mut state = AppState::new();
        let project = create_test_project();
        let pid = project.id;
        state.add_project(project);
        let ids = build_local_stack(&mut state, pid, &["a", "b", "c"]);
        let target = add_root_session(&mut state, pid, "z");

        let (plan, pr) = state.set_session_base(&ids[1], Some(target)).unwrap();
        assert_eq!(plan.new_base_branch, "z");
        assert_eq!(plan.old_base_branch.as_deref(), Some("a"));
        assert!(pr.is_none(), "no PR number means no gh pr edit to run");

        let b = state.get_session(&ids[1]).unwrap();
        assert_eq!(b.stack_parent_session_id, Some(target));
        assert_eq!(b.base_branch.as_deref(), Some("z"));
        assert!(
            b.pr_base_branch.is_none(),
            "nothing to mirror when the session has no PR"
        );
    }

    /// With a live PR the mirror is what re-stacks the tree immediately, and the
    /// returned retarget must be read from the *pre*-apply state so it names the
    /// PR to edit rather than the value we just wrote.
    #[test]
    fn set_session_base_mirrors_pr_base_and_plans_the_gh_edit() {
        let mut state = AppState::new();
        let project = create_test_project();
        let pid = project.id;
        state.add_project(project);
        let ids = build_local_stack(&mut state, pid, &["a", "b", "c"]);
        let target = add_root_session(&mut state, pid, "z");
        {
            let b = state.get_session_mut(&ids[1]).unwrap();
            b.pr_number = Some(42);
            b.pr_state = Some(crate::git::PrState::Open);
            b.pr_base_branch = Some("a".to_string());
        }

        let (_, pr) = state.set_session_base(&ids[1], Some(target)).unwrap();
        assert_eq!(
            pr,
            Some(PrBaseRetarget {
                pr_number: 42,
                repo_path: PathBuf::from("/tmp/test"),
                new_base_branch: "z".to_string(),
            })
        );
        assert_eq!(
            state
                .get_session(&ids[1])
                .unwrap()
                .pr_base_branch
                .as_deref(),
            Some("z"),
            "the local mirror must re-stack the tree before the next PR sync"
        );
    }

    /// The `gh pr edit` is gated on `pr_number`, not `pr_base_branch`: GitHub's
    /// `baseRefName` is optional in the fetched JSON, so a session can carry a PR
    /// whose base was never recorded. Gating on the base would skip it and leave
    /// a local-only change for the next poll to revert. Nothing is mirrored in
    /// that case, because there is no recorded GitHub value to mirror over.
    #[test]
    fn set_session_base_plans_the_gh_edit_when_only_the_pr_number_is_known() {
        let mut state = AppState::new();
        let project = create_test_project();
        let pid = project.id;
        state.add_project(project);
        let ids = build_local_stack(&mut state, pid, &["a", "b"]);
        let target = add_root_session(&mut state, pid, "z");
        {
            let b = state.get_session_mut(&ids[1]).unwrap();
            b.pr_number = Some(11);
            b.pr_state = Some(crate::git::PrState::Open);
            b.pr_base_branch = None;
        }

        let (_, pr) = state.set_session_base(&ids[1], Some(target)).unwrap();
        assert_eq!(
            pr,
            Some(PrBaseRetarget {
                pr_number: 11,
                repo_path: PathBuf::from("/tmp/test"),
                new_base_branch: "z".to_string(),
            }),
            "a known PR number alone must still schedule the durable edit"
        );
        assert!(
            state.get_session(&ids[1]).unwrap().pr_base_branch.is_none(),
            "nothing to mirror when GitHub never reported a base"
        );
    }

    /// A closed PR is refused for the same reason a merged one is.
    #[test]
    fn set_session_base_rejects_a_closed_pr() {
        let mut state = AppState::new();
        let project = create_test_project();
        let pid = project.id;
        state.add_project(project);
        let ids = build_local_stack(&mut state, pid, &["a", "b"]);
        let target = add_root_session(&mut state, pid, "z");
        state.get_session_mut(&ids[1]).unwrap().pr_state = Some(crate::git::PrState::Closed);

        assert!(matches!(
            state.set_session_base(&ids[1], Some(target)).unwrap_err(),
            SetBaseRejection::PrNotOpen { state: "closed" }
        ));
    }

    /// `None` unstacks onto the project's main branch.
    #[test]
    fn set_session_base_none_unstacks_onto_main() {
        let mut state = AppState::new();
        let project = create_test_project();
        let pid = project.id;
        state.add_project(project);
        let ids = build_local_stack(&mut state, pid, &["a", "b"]);

        let (plan, _) = state.set_session_base(&ids[1], None).unwrap();
        assert_eq!(plan.new_base_branch, "main");
        let b = state.get_session(&ids[1]).unwrap();
        assert!(b.stack_parent_session_id.is_none());
        assert_eq!(b.base_branch.as_deref(), Some("main"));
    }

    /// A session cannot be based on one of its own descendants: that closes a
    /// cycle, and a cycle makes every member vanish from the board entirely
    /// (`build_session_order` treats a cycle as having no root).
    #[test]
    fn set_session_base_rejects_a_descendant_parent() {
        let mut state = AppState::new();
        let project = create_test_project();
        let pid = project.id;
        state.add_project(project);
        let ids = build_local_stack(&mut state, pid, &["a", "b", "c"]);

        // C is stacked on B; basing B on C would close the loop.
        assert!(matches!(
            state.set_session_base(&ids[1], Some(ids[2])).unwrap_err(),
            SetBaseRejection::WouldCycle { .. }
        ));
    }

    /// The id-based cycle check cannot see a collision by *branch name*, but
    /// `resolve_stack_parent` matches parents by branch — so a candidate sharing
    /// a descendant's branch would still close a loop once applied.
    #[test]
    fn set_session_base_rejects_a_parent_sharing_a_descendant_branch() {
        let mut state = AppState::new();
        let project = create_test_project();
        let pid = project.id;
        state.add_project(project);
        let ids = build_local_stack(&mut state, pid, &["a", "b", "c"]);

        // An unrelated session that happens to carry C's branch name.
        let mut twin = WorktreeSession::new(pid, "Twin", "c", PathBuf::from("/tmp/twin"), "claude");
        twin.base_branch = Some("main".to_string());
        let twin_id = twin.id;
        state.add_session(twin);

        assert!(matches!(
            state.set_session_base(&ids[1], Some(twin_id)).unwrap_err(),
            SetBaseRejection::WouldCycle { .. }
        ));
    }

    /// Every accepted plan must leave a topology that `build_session_order` can
    /// still render — a cycle silently drops all of its members.
    #[test]
    fn accepted_set_session_base_never_hides_sessions_from_the_board() {
        let mut state = AppState::new();
        let project = create_test_project();
        let pid = project.id;
        state.add_project(project);
        let ids = build_local_stack(&mut state, pid, &["a", "b", "c", "d"]);

        state.set_session_base(&ids[3], Some(ids[0])).unwrap();

        let sessions = state.sorted_project_sessions(&pid);
        let ordered = crate::session::board::build_session_order(&sessions);
        assert_eq!(
            ordered.len(),
            ids.len(),
            "a retarget must never drop sessions from the rendered order"
        );
    }

    #[test]
    fn set_session_base_rejects_self_and_unknown_targets() {
        let mut state = AppState::new();
        let project = create_test_project();
        let pid = project.id;
        state.add_project(project);
        let ids = build_local_stack(&mut state, pid, &["a", "b"]);

        assert!(matches!(
            state.set_session_base(&ids[1], Some(ids[1])).unwrap_err(),
            SetBaseRejection::SelfParent
        ));
        assert!(matches!(
            state
                .set_session_base(&ids[1], Some(SessionId::new()))
                .unwrap_err(),
            SetBaseRejection::ParentNotFound
        ));
        assert!(matches!(
            state.set_session_base(&SessionId::new(), None).unwrap_err(),
            SetBaseRejection::SessionNotFound
        ));
        // And a refusal must not have written anything.
        assert_eq!(
            state.get_session(&ids[1]).unwrap().base_branch.as_deref(),
            Some("a")
        );
    }

    /// A merged or closed PR's base cannot be changed on GitHub, and its
    /// `pr_base_branch` outranks the local hint — so a local-only change would be
    /// re-asserted by the next PR sync. Refuse instead of appearing to work.
    #[test]
    fn set_session_base_rejects_a_settled_pr() {
        let mut state = AppState::new();
        let project = create_test_project();
        let pid = project.id;
        state.add_project(project);
        let ids = build_local_stack(&mut state, pid, &["a", "b", "c"]);
        state.get_session_mut(&ids[1]).unwrap().pr_state = Some(crate::git::PrState::Merged);

        assert!(matches!(
            state.set_session_base(&ids[1], Some(ids[2])).unwrap_err(),
            SetBaseRejection::PrNotOpen { state: "merged" }
        ));
    }

    /// `cascade_resume` re-derives its chain from current topology and treats a
    /// re-parented session as "cascade complete", skipping merges the tail still
    /// needs. Restacking mid-pause must be refused.
    #[test]
    fn set_session_base_rejects_while_a_cascade_is_paused() {
        let mut state = AppState::new();
        let project = create_test_project();
        let pid = project.id;
        state.add_project(project);
        let ids = build_local_stack(&mut state, pid, &["a", "b", "c"]);
        state.cascade_paused_at = Some(ids[0]);

        assert!(matches!(
            state.set_session_base(&ids[1], Some(ids[2])).unwrap_err(),
            SetBaseRejection::CascadeInProgress
        ));
    }

    /// Descendants stack on the retargeted session's *branch*, which does not
    /// move — so they follow it with no write of their own.
    #[test]
    fn set_session_base_leaves_descendants_untouched() {
        let mut state = AppState::new();
        let project = create_test_project();
        let pid = project.id;
        state.add_project(project);
        let ids = build_local_stack(&mut state, pid, &["a", "b", "c"]);
        let before = state.get_session(&ids[2]).unwrap().clone();

        // Unstack B onto main; C is stacked on B and must not change.
        state.set_session_base(&ids[1], None).unwrap();

        let after = state.get_session(&ids[2]).unwrap();
        assert_eq!(
            after.stack_parent_session_id,
            before.stack_parent_session_id
        );
        assert_eq!(after.base_branch, before.base_branch);
        assert_eq!(after.pr_base_branch, before.pr_base_branch);

        let sessions = state.sorted_project_sessions(&pid);
        assert_eq!(
            crate::session::resolve_stack_parent(
                *sessions.iter().find(|s| s.id == ids[2]).unwrap(),
                &sessions
            ),
            Some(ids[1]),
            "C must still resolve to B"
        );
    }

    /// The repair this command exists for: a PR retargeted on GitHub moves
    /// `pr_base_branch` while the local hint and `base_branch` stay stale.
    /// Selecting the branch the PR already targets must still be accepted, so
    /// the stale fields are fixed — otherwise the session snaps back to its old
    /// parent the moment the PR closes and `pr_base_branch` clears.
    #[test]
    fn set_session_base_repairs_stale_local_fields_matching_the_pr_base() {
        let mut state = AppState::new();
        let project = create_test_project();
        let pid = project.id;
        state.add_project(project);
        let ids = build_local_stack(&mut state, pid, &["a", "b"]);
        {
            // The PR was retargeted onto main outside Commander; the local
            // fields still say "stacked on a".
            let b = state.get_session_mut(&ids[1]).unwrap();
            b.pr_number = Some(9);
            b.pr_state = Some(crate::git::PrState::Open);
            b.pr_base_branch = Some("main".to_string());
        }

        let (plan, _) = state
            .set_session_base(&ids[1], None)
            .expect("unstacking onto main must be allowed even though the PR already targets it");
        assert_eq!(plan.new_base_branch, "main");

        let b = state.get_session(&ids[1]).unwrap();
        assert!(
            b.stack_parent_session_id.is_none(),
            "the stale parent hint must be cleared"
        );
        assert_eq!(
            b.base_branch.as_deref(),
            Some("main"),
            "and the stale base_branch repaired"
        );
    }

    /// A target in another project names the real reason rather than claiming
    /// the session does not exist.
    #[test]
    fn set_session_base_rejects_a_target_in_another_project() {
        let mut state = AppState::new();
        let project = create_test_project();
        let pid = project.id;
        state.add_project(project);
        let ids = build_local_stack(&mut state, pid, &["a", "b"]);

        let other_project = Project::new("other", PathBuf::from("/tmp/other"), "main");
        let other_pid = other_project.id;
        state.add_project(other_project);
        let outsider = add_root_session(&mut state, other_pid, "z");

        assert!(matches!(
            state.set_session_base(&ids[1], Some(outsider)).unwrap_err(),
            SetBaseRejection::DifferentProject
        ));
    }

    /// Re-selecting the base the session already has is refused rather than
    /// written, so the UI can say "already based on x" instead of no-oping.
    #[test]
    fn set_session_base_rejects_the_current_base() {
        let mut state = AppState::new();
        let project = create_test_project();
        let pid = project.id;
        state.add_project(project);
        let ids = build_local_stack(&mut state, pid, &["a", "b"]);

        assert!(matches!(
            state.set_session_base(&ids[1], Some(ids[0])).unwrap_err(),
            SetBaseRejection::AlreadyBased { .. }
        ));
    }
}
