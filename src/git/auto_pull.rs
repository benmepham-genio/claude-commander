//! Background fast-forward of a project's main branch.
//!
//! Periodically advances local `<main>` to match `origin/<main>` so that
//! `git status` in the project repo stays sensible. Two execution paths:
//!
//! 1. If `<main>` is not the currently checked-out branch in `repo_path`
//!    (or the repo is bare), fast-forward with `git update-ref`. Cheap,
//!    leaves the working tree untouched.
//! 2. If `<main>` is the active checkout, `git merge --ff-only` so ref,
//!    index, and worktree advance together. Only attempted when the working
//!    tree is clean; otherwise we skip and surface a "blocked" reason.

use std::path::Path;

use tokio::process::Command;
use tracing::{debug, warn};

/// Relationship between the local main ref and `origin/<main>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BranchRelation {
    /// Local and remote point at the same commit.
    UpToDate,
    /// Local is strictly behind remote (fast-forward possible).
    LocalBehind,
    /// Local has commits not in remote, or histories disagree.
    Diverged,
}

/// What the executor should do for a project this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullAction {
    /// Same commit on both sides — nothing to do.
    UpToDate,
    /// Local branch isn't the active checkout: advance via `git update-ref`.
    FastForwardRef,
    /// Local branch is the active checkout and clean: advance via
    /// `git merge --ff-only`.
    FastForwardCheckout,
    /// Local branch is the active checkout but the working tree is dirty.
    SkipDirty,
    /// Local has commits not on the remote (or histories have diverged).
    SkipDiverged,
    /// Local branch is checked out in a *different* worktree. `git update-ref`
    /// has no in-use safety check, so advancing the ref would leave that
    /// worktree's index and `git status` stale — skip instead.
    SkipWorktreeConflict,
}

impl PullAction {
    /// Whether this outcome should surface the "blocked" badge on the project row.
    pub fn is_blocked(self) -> bool {
        self.block_reason().is_some()
    }

    /// The block reason this action maps to, if any. Single source of truth
    /// for the user-visible string (shared with [`BlockReason`]).
    pub fn block_reason(self) -> Option<BlockReason> {
        match self {
            PullAction::SkipDirty => Some(BlockReason::Dirty),
            PullAction::SkipDiverged => Some(BlockReason::Diverged),
            PullAction::SkipWorktreeConflict => Some(BlockReason::WorktreeConflict),
            _ => None,
        }
    }
}

/// Pure decision: given the observable state, what should we do?
pub fn decide_pull_action(
    relation: BranchRelation,
    checked_out_at_repo_path: bool,
    checked_out_elsewhere: bool,
    worktree_dirty: bool,
) -> PullAction {
    match relation {
        BranchRelation::UpToDate => PullAction::UpToDate,
        BranchRelation::Diverged => PullAction::SkipDiverged,
        BranchRelation::LocalBehind => {
            if checked_out_at_repo_path {
                if worktree_dirty {
                    PullAction::SkipDirty
                } else {
                    PullAction::FastForwardCheckout
                }
            } else if checked_out_elsewhere {
                PullAction::SkipWorktreeConflict
            } else {
                PullAction::FastForwardRef
            }
        }
    }
}

/// Outcome of a single pull attempt for a project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PullOutcome {
    /// Fast-forward applied (either via update-ref or merge --ff-only).
    Advanced,
    /// No work needed.
    UpToDate,
    /// Skipped with a user-visible reason (drives the row badge).
    Blocked(BlockReason),
    /// Soft fail (network, no remote, fetch error). Logged, no badge.
    SoftFail,
}

/// Why an FF was held back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockReason {
    Dirty,
    Diverged,
    WorktreeConflict,
}

impl BlockReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            BlockReason::Dirty => "Working tree dirty",
            BlockReason::Diverged => "Branch diverged from origin",
            BlockReason::WorktreeConflict => "Checked out in another worktree",
        }
    }
}

/// Execute one pull attempt for a project. Always best-effort: any
/// unexpected git error returns `SoftFail` and logs at debug.
///
/// Steps:
///   1. `git fetch origin <main>` — bail out softly if it fails.
///   2. Resolve `refs/heads/<main>` and `refs/remotes/origin/<main>`.
///   3. Classify the relation with `git merge-base --is-ancestor`.
///   4. Inspect HEAD and (if relevant) `git status --porcelain`.
///   5. Apply `decide_pull_action` and run the corresponding git command.
pub async fn run_project_pull(repo_path: &Path, main_branch: &str) -> PullOutcome {
    if !fetch_main(repo_path, main_branch).await {
        return PullOutcome::SoftFail;
    }

    let Some(local_sha) = rev_parse(repo_path, &format!("refs/heads/{main_branch}")).await else {
        debug!(
            "auto_pull: local branch {} not found at {}",
            main_branch,
            repo_path.display()
        );
        return PullOutcome::SoftFail;
    };
    let Some(origin_sha) =
        rev_parse(repo_path, &format!("refs/remotes/origin/{main_branch}")).await
    else {
        debug!(
            "auto_pull: origin/{} not found at {}",
            main_branch,
            repo_path.display()
        );
        return PullOutcome::SoftFail;
    };

    let relation = classify_relation(repo_path, &local_sha, &origin_sha).await;
    let checked_out = head_is_branch(repo_path, main_branch).await;
    // Only the FF-relevant `LocalBehind` case needs the extra git probes.
    let behind = relation == BranchRelation::LocalBehind;
    let dirty = if behind && checked_out {
        worktree_is_dirty(repo_path).await
    } else {
        false
    };
    let checked_out_elsewhere = if behind && !checked_out {
        branch_checked_out_in_worktree(repo_path, main_branch).await
    } else {
        false
    };

    let action = decide_pull_action(relation, checked_out, checked_out_elsewhere, dirty);
    if let Some(reason) = action.block_reason() {
        return PullOutcome::Blocked(reason);
    }
    match action {
        PullAction::UpToDate => PullOutcome::UpToDate,
        PullAction::FastForwardRef => {
            if update_ref(repo_path, main_branch, &origin_sha).await {
                PullOutcome::Advanced
            } else {
                PullOutcome::SoftFail
            }
        }
        PullAction::FastForwardCheckout => {
            if merge_ff_only(repo_path, main_branch).await {
                PullOutcome::Advanced
            } else {
                PullOutcome::SoftFail
            }
        }
        // Blocked variants are returned above via `block_reason`.
        PullAction::SkipDirty | PullAction::SkipDiverged | PullAction::SkipWorktreeConflict => {
            unreachable!("blocked actions handled by early return")
        }
    }
}

async fn fetch_main(repo_path: &Path, main_branch: &str) -> bool {
    let out = Command::new("git")
        .current_dir(repo_path)
        .args(["fetch", "origin", main_branch])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await;
    match out {
        Ok(o) if o.status.success() => true,
        Ok(o) => {
            debug!(
                "auto_pull: git fetch origin {} failed: {}",
                main_branch,
                String::from_utf8_lossy(&o.stderr).trim()
            );
            false
        }
        Err(e) => {
            debug!("auto_pull: git fetch failed to spawn: {}", e);
            false
        }
    }
}

async fn rev_parse(repo_path: &Path, refname: &str) -> Option<String> {
    let out = Command::new("git")
        .current_dir(repo_path)
        .args(["rev-parse", "--verify", "--quiet", refname])
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

async fn classify_relation(repo_path: &Path, local: &str, origin: &str) -> BranchRelation {
    if local == origin {
        return BranchRelation::UpToDate;
    }
    if is_ancestor(repo_path, local, origin).await {
        BranchRelation::LocalBehind
    } else {
        BranchRelation::Diverged
    }
}

async fn is_ancestor(repo_path: &Path, ancestor: &str, descendant: &str) -> bool {
    let out = Command::new("git")
        .current_dir(repo_path)
        .args(["merge-base", "--is-ancestor", ancestor, descendant])
        .output()
        .await;
    matches!(out, Ok(o) if o.status.success())
}

async fn head_is_branch(repo_path: &Path, main_branch: &str) -> bool {
    let out = Command::new("git")
        .current_dir(repo_path)
        .args(["symbolic-ref", "--quiet", "HEAD"])
        .output()
        .await;
    match out {
        Ok(o) if o.status.success() => {
            let head = String::from_utf8_lossy(&o.stdout).trim().to_string();
            head == format!("refs/heads/{main_branch}")
        }
        _ => false,
    }
}

/// Whether `<main>` is checked out in any worktree linked to this repo.
/// Used to avoid `git update-ref`-ing a branch that another worktree has
/// checked out (which would leave that worktree's index/status stale).
/// On any git error we fail *closed* (assume a conflict) so we never move a
/// ref we're unsure about.
async fn branch_checked_out_in_worktree(repo_path: &Path, main_branch: &str) -> bool {
    let out = Command::new("git")
        .current_dir(repo_path)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .await;
    match out {
        Ok(o) if o.status.success() => {
            let target = format!("branch refs/heads/{main_branch}");
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .any(|line| line.trim_end() == target)
        }
        Ok(o) => {
            debug!(
                "auto_pull: git worktree list failed: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            );
            true
        }
        Err(e) => {
            debug!("auto_pull: git worktree list failed to spawn: {}", e);
            true
        }
    }
}

async fn worktree_is_dirty(repo_path: &Path) -> bool {
    let out = Command::new("git")
        .current_dir(repo_path)
        .args(["status", "--porcelain"])
        .output()
        .await;
    match out {
        Ok(o) if o.status.success() => !o.stdout.is_empty(),
        Ok(o) => {
            debug!(
                "auto_pull: git status failed: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            );
            true
        }
        Err(_) => true,
    }
}

async fn update_ref(repo_path: &Path, main_branch: &str, new_sha: &str) -> bool {
    let refname = format!("refs/heads/{main_branch}");
    let out = Command::new("git")
        .current_dir(repo_path)
        .args(["update-ref", &refname, new_sha])
        .output()
        .await;
    match out {
        Ok(o) if o.status.success() => true,
        Ok(o) => {
            warn!(
                "auto_pull: git update-ref {} {} failed: {}",
                refname,
                new_sha,
                String::from_utf8_lossy(&o.stderr).trim()
            );
            false
        }
        Err(e) => {
            warn!("auto_pull: git update-ref failed to spawn: {}", e);
            false
        }
    }
}

async fn merge_ff_only(repo_path: &Path, main_branch: &str) -> bool {
    let upstream = format!("origin/{main_branch}");
    let out = Command::new("git")
        .current_dir(repo_path)
        .args(["merge", "--ff-only", &upstream])
        .output()
        .await;
    match out {
        Ok(o) if o.status.success() => true,
        Ok(o) => {
            warn!(
                "auto_pull: git merge --ff-only {} failed: {}",
                upstream,
                String::from_utf8_lossy(&o.stderr).trim()
            );
            false
        }
        Err(e) => {
            warn!("auto_pull: git merge --ff-only failed to spawn: {}", e);
            false
        }
    }
}

#[cfg(test)]
mod decision_tests {
    use super::*;

    #[test]
    fn up_to_date_is_no_op() {
        let a = decide_pull_action(BranchRelation::UpToDate, false, false, false);
        assert_eq!(a, PullAction::UpToDate);
        let a = decide_pull_action(BranchRelation::UpToDate, true, true, true);
        assert_eq!(a, PullAction::UpToDate);
    }

    #[test]
    fn diverged_always_skips() {
        for &co in &[true, false] {
            for &elsewhere in &[true, false] {
                for &dirty in &[true, false] {
                    assert_eq!(
                        decide_pull_action(BranchRelation::Diverged, co, elsewhere, dirty),
                        PullAction::SkipDiverged
                    );
                }
            }
        }
    }

    #[test]
    fn behind_and_not_checked_out_uses_ref_update() {
        assert_eq!(
            decide_pull_action(BranchRelation::LocalBehind, false, false, false),
            PullAction::FastForwardRef
        );
        // Dirty is irrelevant when main isn't the active checkout here.
        assert_eq!(
            decide_pull_action(BranchRelation::LocalBehind, false, false, true),
            PullAction::FastForwardRef
        );
    }

    #[test]
    fn behind_and_checked_out_elsewhere_skips() {
        // Main is checked out in another worktree: a ref update would
        // desync that worktree, so we must skip rather than fast-forward.
        assert_eq!(
            decide_pull_action(BranchRelation::LocalBehind, false, true, false),
            PullAction::SkipWorktreeConflict
        );
        assert_eq!(
            decide_pull_action(BranchRelation::LocalBehind, false, true, true),
            PullAction::SkipWorktreeConflict
        );
    }

    #[test]
    fn behind_and_checked_out_here_ignores_elsewhere() {
        // When main is the active checkout at repo_path, the merge path is
        // taken regardless of any other worktree claim.
        assert_eq!(
            decide_pull_action(BranchRelation::LocalBehind, true, true, false),
            PullAction::FastForwardCheckout
        );
    }

    #[test]
    fn behind_and_checked_out_clean_merges() {
        assert_eq!(
            decide_pull_action(BranchRelation::LocalBehind, true, false, false),
            PullAction::FastForwardCheckout
        );
    }

    #[test]
    fn behind_and_checked_out_dirty_skips() {
        assert_eq!(
            decide_pull_action(BranchRelation::LocalBehind, true, false, true),
            PullAction::SkipDirty
        );
    }

    #[test]
    fn is_blocked_matches_skip_variants() {
        assert!(PullAction::SkipDirty.is_blocked());
        assert!(PullAction::SkipDiverged.is_blocked());
        assert!(PullAction::SkipWorktreeConflict.is_blocked());
        assert!(!PullAction::UpToDate.is_blocked());
        assert!(!PullAction::FastForwardRef.is_blocked());
        assert!(!PullAction::FastForwardCheckout.is_blocked());
    }

    #[test]
    fn block_reason_maps_to_block_reason_enum() {
        assert_eq!(
            PullAction::SkipDirty.block_reason(),
            Some(BlockReason::Dirty)
        );
        assert_eq!(
            PullAction::SkipDiverged.block_reason(),
            Some(BlockReason::Diverged)
        );
        assert_eq!(
            PullAction::SkipWorktreeConflict.block_reason(),
            Some(BlockReason::WorktreeConflict)
        );
        assert_eq!(PullAction::FastForwardRef.block_reason(), None);
    }
}

#[cfg(test)]
mod executor_tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::process::Command as StdCommand;
    use tempfile::TempDir;

    /// Spawn a synchronous git command in `dir`, panicking on failure.
    /// The auto-pull executor uses async tokio commands; tests stay
    /// synchronous so we don't need a runtime for setup.
    fn git(dir: &Path, args: &[&str]) {
        let out = StdCommand::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("git invocation failed to spawn");
        assert!(
            out.status.success(),
            "git {:?} failed in {}:\nstdout: {}\nstderr: {}",
            args,
            dir.display(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn git_capture(dir: &Path, args: &[&str]) -> String {
        let out = StdCommand::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("git invocation failed to spawn");
        assert!(out.status.success(), "git {:?} failed", args);
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Create a bare "remote" repo with one commit on `main` and a "local"
    /// clone of it (in the same TempDir, side-by-side). Returns the local
    /// repo path; the remote path is captured by the closure that drives
    /// the test once setup is done.
    fn setup_origin_and_local() -> (TempDir, PathBuf, PathBuf) {
        let tmp = TempDir::new().expect("tempdir");
        let remote = tmp.path().join("remote.git");
        let seed = tmp.path().join("seed");
        let local = tmp.path().join("local");

        // Bare remote
        git(tmp.path(), &["init", "--bare", "-b", "main", "remote.git"]);

        // Seed repo to produce an initial commit, then push to the bare remote
        git(tmp.path(), &["init", "-b", "main", "seed"]);
        git(&seed, &["config", "user.email", "t@t"]);
        git(&seed, &["config", "user.name", "t"]);
        git(&seed, &["config", "commit.gpgsign", "false"]);
        std::fs::write(seed.join("README"), "v1\n").unwrap();
        git(&seed, &["add", "README"]);
        git(&seed, &["commit", "-m", "initial"]);
        git(
            &seed,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git(&seed, &["push", "origin", "main"]);

        // Clone the bare remote as the "local" project repo under test
        git(
            tmp.path(),
            &["clone", remote.to_str().unwrap(), local.to_str().unwrap()],
        );
        git(&local, &["config", "user.email", "t@t"]);
        git(&local, &["config", "user.name", "t"]);
        git(&local, &["config", "commit.gpgsign", "false"]);

        (tmp, remote, local)
    }

    /// Push an extra commit to the remote so the local clone is one
    /// commit behind `origin/main`.
    fn advance_remote_by_one(remote: &Path) {
        // Cheapest way to add a commit to a bare repo: temporary work clone.
        let tmp = TempDir::new().expect("work tempdir");
        let work = tmp.path().join("work");
        git(
            tmp.path(),
            &["clone", remote.to_str().unwrap(), work.to_str().unwrap()],
        );
        git(&work, &["config", "user.email", "t@t"]);
        git(&work, &["config", "user.name", "t"]);
        git(&work, &["config", "commit.gpgsign", "false"]);
        std::fs::write(work.join("README"), "v2\n").unwrap();
        git(&work, &["add", "README"]);
        git(&work, &["commit", "-m", "second"]);
        git(&work, &["push", "origin", "main"]);
    }

    #[tokio::test]
    async fn pull_up_to_date_is_noop() {
        let (_tmp, _remote, local) = setup_origin_and_local();
        let outcome = run_project_pull(&local, "main").await;
        assert_eq!(outcome, PullOutcome::UpToDate);
    }

    #[tokio::test]
    async fn pull_advances_when_main_checked_out_clean() {
        let (_tmp, remote, local) = setup_origin_and_local();
        let before = git_capture(&local, &["rev-parse", "HEAD"]);
        advance_remote_by_one(&remote);

        let outcome = run_project_pull(&local, "main").await;
        assert_eq!(outcome, PullOutcome::Advanced);

        let after = git_capture(&local, &["rev-parse", "HEAD"]);
        assert_ne!(before, after, "HEAD should have moved forward");
        // Working tree must reflect the new commit
        let readme = std::fs::read_to_string(local.join("README")).unwrap();
        assert_eq!(readme, "v2\n");
    }

    #[tokio::test]
    async fn pull_blocks_when_checked_out_main_dirty() {
        let (_tmp, remote, local) = setup_origin_and_local();
        advance_remote_by_one(&remote);
        std::fs::write(local.join("README"), "uncommitted\n").unwrap();

        let outcome = run_project_pull(&local, "main").await;
        assert_eq!(outcome, PullOutcome::Blocked(BlockReason::Dirty));

        // Local main ref must NOT have moved (would corrupt git status).
        let local_sha = git_capture(&local, &["rev-parse", "refs/heads/main"]);
        let origin_sha = git_capture(&local, &["rev-parse", "refs/remotes/origin/main"]);
        assert_ne!(local_sha, origin_sha);
    }

    #[tokio::test]
    async fn pull_advances_via_update_ref_when_main_not_checked_out() {
        let (_tmp, remote, local) = setup_origin_and_local();
        // Switch off main onto a feature branch with uncommitted noise on top.
        git(&local, &["checkout", "-b", "feature"]);
        std::fs::write(local.join("README"), "dirty-on-feature\n").unwrap();
        advance_remote_by_one(&remote);

        let outcome = run_project_pull(&local, "main").await;
        assert_eq!(outcome, PullOutcome::Advanced);

        // Local main ref advanced to origin, even though feature was checked out and dirty.
        let local_main = git_capture(&local, &["rev-parse", "refs/heads/main"]);
        let origin_main = git_capture(&local, &["rev-parse", "refs/remotes/origin/main"]);
        assert_eq!(local_main, origin_main);

        // Working tree untouched: still on `feature` with the dirty edit intact.
        let head = git_capture(&local, &["symbolic-ref", "HEAD"]);
        assert_eq!(head, "refs/heads/feature");
        let readme = std::fs::read_to_string(local.join("README")).unwrap();
        assert_eq!(readme, "dirty-on-feature\n");
    }

    #[tokio::test]
    async fn pull_blocks_when_local_diverges() {
        let (_tmp, remote, local) = setup_origin_and_local();
        // Local moves ahead on main; remote also moves — histories diverge.
        std::fs::write(local.join("local-only"), "x\n").unwrap();
        git(&local, &["add", "local-only"]);
        git(&local, &["commit", "-m", "local-only commit"]);
        advance_remote_by_one(&remote);

        let outcome = run_project_pull(&local, "main").await;
        assert_eq!(outcome, PullOutcome::Blocked(BlockReason::Diverged));
    }

    #[tokio::test]
    async fn pull_soft_fails_without_remote_ref() {
        // Repo with no `origin` configured at all — fetch must fail softly.
        let tmp = TempDir::new().unwrap();
        let local = tmp.path().join("solo");
        git(tmp.path(), &["init", "-b", "main", "solo"]);
        git(&local, &["config", "user.email", "t@t"]);
        git(&local, &["config", "user.name", "t"]);
        git(&local, &["config", "commit.gpgsign", "false"]);
        std::fs::write(local.join("f"), "x\n").unwrap();
        git(&local, &["add", "f"]);
        git(&local, &["commit", "-m", "c"]);

        let outcome = run_project_pull(&local, "main").await;
        assert_eq!(outcome, PullOutcome::SoftFail);
    }

    #[tokio::test]
    async fn pull_blocks_when_main_checked_out_in_other_worktree() {
        let (tmp, remote, local) = setup_origin_and_local();
        // Move `local` off main, then link a second worktree that checks out
        // main. `update-ref` on main would now desync that worktree.
        git(&local, &["checkout", "-b", "parking"]);
        let wt_main = tmp.path().join("wt-main");
        git(
            &local,
            &["worktree", "add", wt_main.to_str().unwrap(), "main"],
        );
        advance_remote_by_one(&remote);

        let outcome = run_project_pull(&local, "main").await;
        assert_eq!(outcome, PullOutcome::Blocked(BlockReason::WorktreeConflict));

        // Local main ref must NOT have moved — the other worktree still owns it.
        let local_main = git_capture(&local, &["rev-parse", "refs/heads/main"]);
        let origin_main = git_capture(&local, &["rev-parse", "refs/remotes/origin/main"]);
        assert_ne!(local_main, origin_main);
    }
}
