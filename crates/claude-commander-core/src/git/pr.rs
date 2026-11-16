//! GitHub PR detection via `gh` CLI
//!
//! Checks whether a branch has an open pull request using `gh pr list`.
//! All failures are silently swallowed — missing `gh`, auth errors, network
//! issues, or repos without a GitHub remote simply result in `None`.

use crate::git::gh_command;
use std::path::Path;

use chrono::{DateTime, Utc};
use tracing::debug;

// PR state + review decision are network wire enums; they live in the shared
// `claude-commander-protocol` crate and are re-exported here so the PR logic
// below and `crate::git::{PrState, ReviewDecision}` paths keep working.
pub use claude_commander_protocol::pr::{PrState, ReviewDecision};

/// PR metadata returned by `gh pr list` for the session list view.
#[derive(Debug, Clone)]
pub struct PrInfo {
    pub number: u32,
    pub url: String,
    pub state: PrState,
    pub is_draft: bool,
    pub labels: Vec<String>,
    /// GitHub-derived review decision; `None` when no decision has been
    /// formed (e.g. no reviews requested) or the field is absent.
    pub review_decision: Option<ReviewDecision>,
    /// Reviewer logins (users only) — union of requested reviewers and
    /// authors of any submitted review. Deduplicated, sorted.
    pub reviewers: Vec<String>,
    /// Target branch the PR is opened against (e.g. `main` or another PR branch).
    /// Used to detect PR stacks — when this matches another session's branch in
    /// the same project, the sessions are stacked.
    pub base_ref_name: Option<String>,
}

impl PrInfo {
    /// Convenience: true when the PR is merged.
    pub fn merged(&self) -> bool {
        self.state == PrState::Merged
    }
}

/// Outcome of polling GitHub for PR status of a branch.
///
/// `check_pr_for_branch` must distinguish "the repo has no PR for this branch"
/// from "we couldn't ask GitHub" (gh missing, auth error, network error,
/// malformed response). Callers handling the result treat these differently:
/// the former authoritatively clears cached PR state, the latter preserves
/// whatever was last known so a transient hiccup doesn't flatten a PR stack
/// in the UI.
#[derive(Debug, Clone)]
pub enum PrCheckResult {
    /// A PR was found for this branch.
    Found(PrInfo),
    /// `gh` reported successfully that no PR exists for this branch.
    NotFound,
    /// The poll failed to produce an authoritative answer (gh not installed,
    /// network/auth error, or an unexpected JSON payload). Cached PR state
    /// on the session must be left untouched.
    FetchFailed,
}

impl PrCheckResult {
    /// Extract the `PrInfo` when the result is `Found`, else `None`.
    pub fn info(&self) -> Option<&PrInfo> {
        match self {
            Self::Found(info) => Some(info),
            _ => None,
        }
    }

    /// True when `gh` authoritatively reported no PR for this branch.
    pub fn is_not_found(&self) -> bool {
        matches!(self, Self::NotFound)
    }

    /// True when the poll failed with a transient error — callers should
    /// leave cached PR state untouched.
    pub fn is_fetch_failed(&self) -> bool {
        matches!(self, Self::FetchFailed)
    }
}

/// Rich PR metadata returned by `gh pr view`.
#[derive(Debug, Clone)]
pub struct EnrichedPrInfo {
    pub number: u32,
    pub url: String,
    pub title: String,
    pub state: PrState,
    pub is_draft: bool,
    pub labels: Vec<PrLabel>,
    pub checks_status: ChecksStatus,
    pub body: String,
}

/// Resolve a session's effective PR state, falling back to the legacy
/// `pr_merged` bool when `pr_state` is `None`. Older `state.json` files
/// (written before `pr_state` was added) carry only the bool.
pub fn effective_pr_state(state: Option<PrState>, pr_merged: bool) -> PrState {
    state.unwrap_or(if pr_merged {
        PrState::Merged
    } else {
        PrState::Open
    })
}

/// A PR label with name and hex color.
#[derive(Debug, Clone)]
pub struct PrLabel {
    pub name: String,
    pub color: String,
}

/// Aggregate CI/checks status derived from `statusCheckRollup`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChecksStatus {
    Passing,
    Failing,
    Pending,
    None,
}

impl std::fmt::Display for ChecksStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Passing => write!(f, "Passing"),
            Self::Failing => write!(f, "Failing"),
            Self::Pending => write!(f, "Pending"),
            Self::None => write!(f, "None"),
        }
    }
}

/// Returns `true` if the `gh` CLI is installed and runnable.
///
/// Called once at startup to avoid repeated fork/exec on every tick.
pub async fn is_gh_available() -> bool {
    match gh_command().arg("--version").output().await {
        Ok(output) => {
            let ok = output.status.success();
            debug!("gh --version: available={}", ok);
            ok
        }
        Err(e) => {
            debug!("gh not available: {}", e);
            false
        }
    }
}

/// Retarget an existing PR's base branch via `gh pr edit <number> --base <branch>`,
/// reporting *why* it failed.
///
/// The reason matters to a user-initiated restack: a failed edit means the next
/// PR sync will silently revert the change, so it has to reach the user.
pub async fn try_retarget_pr_base(
    repo_path: &Path,
    pr_number: u32,
    new_base: &str,
) -> std::result::Result<(), String> {
    let mut command = gh_command();
    command
        .args(["pr", "edit", &pr_number.to_string(), "--base", new_base])
        .current_dir(repo_path);
    let output = match crate::git::hosting::review_output(command, "gh").await {
        Ok(output) => output,
        Err(e) => {
            debug!("gh pr edit #{} spawn failed: {}", pr_number, e);
            return Err(format!("could not run gh: {e}"));
        }
    };
    if output.status.success() {
        debug!("retargeted PR #{} base to {}", pr_number, new_base);
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    tracing::warn!(
        "gh pr edit #{} --base {} failed: {}",
        pr_number,
        new_base,
        stderr
    );
    Err(if stderr.is_empty() {
        "gh pr edit failed".to_string()
    } else {
        stderr
    })
}

/// Check whether `branch` has a PR (any state) in the repo at `repo_path`.
///
/// Returns a three-way result: `Found` when a PR matched, `NotFound` when gh
/// successfully reported no PR, and `FetchFailed` on any error. Callers must
/// preserve cached PR state on `FetchFailed` so transient gh/network hiccups
/// don't wipe UI state (notably the PR-stack topology).
///
/// Prefers open PRs over closed/merged when a branch has multiple PRs (rare,
/// but possible after a reopen). PRs that had already settled before
/// `branch_owned_since` — the instant the session took this branch name, per
/// [`WorktreeSession::branch_owned_since`](crate::session::WorktreeSession::branch_owned_since)
/// — are ignored; see [`pr_settled_before`].
pub async fn check_pr_for_branch(
    repo_path: &Path,
    branch: &str,
    branch_owned_since: DateTime<Utc>,
) -> PrCheckResult {
    let output = match gh_command()
        .args([
            "pr",
            "list",
            "--head",
            branch,
            "--state",
            "all",
            "--json",
            "number,url,state,isDraft,labels,baseRefName,reviewDecision,reviewRequests,latestReviews,createdAt,closedAt,mergedAt",
            "--limit",
            "5",
        ])
        .current_dir(repo_path)
        .output()
        .await
    {
        Ok(output) => output,
        Err(e) => {
            debug!("gh pr list spawn failed for branch {}: {}", branch, e);
            return PrCheckResult::FetchFailed;
        }
    };

    if !output.status.success() {
        debug!(
            "gh pr list failed for branch {}: {}",
            branch,
            String::from_utf8_lossy(&output.stderr)
        );
        return PrCheckResult::FetchFailed;
    }

    let Ok(json) = String::from_utf8(output.stdout) else {
        return PrCheckResult::FetchFailed;
    };
    parse_pr_list_json(&json, branch_owned_since)
}

/// Parse the JSON array returned by the `gh pr list --json …` query in
/// [`check_pr_for_branch`].
///
/// Empty array → `NotFound` (gh told us there's no PR). Missing/malformed
/// JSON → `FetchFailed`. Non-empty array → `Found`, preferring the first
/// open PR if any exist, otherwise the first entry (gh returns them in
/// reverse-creation order) — considering only entries that survive
/// [`pr_settled_before`]. When every entry is filtered out the branch has no PR
/// of its own, which is `NotFound`.
fn parse_pr_list_json(json: &str, branch_owned_since: DateTime<Utc>) -> PrCheckResult {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return PrCheckResult::FetchFailed;
    };
    let Some(arr) = v.as_array() else {
        return PrCheckResult::FetchFailed;
    };

    let candidates: Vec<&serde_json::Value> = arr
        .iter()
        .filter(|p| {
            if pr_settled_before(p, branch_owned_since) {
                debug!(
                    "ignoring PR #{} — settled before this session held the branch",
                    p["number"]
                );
                false
            } else {
                true
            }
        })
        .collect();
    let Some(first) = candidates.first() else {
        return PrCheckResult::NotFound;
    };

    let chosen = candidates
        .iter()
        .find(|p| p["state"].as_str() == Some("OPEN"))
        .unwrap_or(first);

    match parse_pr_entry(chosen) {
        Some(info) => PrCheckResult::Found(info),
        // Unknown/new state string — treat as malformed.
        None => PrCheckResult::FetchFailed,
    }
}

/// True when this PR entry was already closed or merged before `owned_since` —
/// the instant the session took its current branch name — meaning it cannot be
/// the session's own PR.
///
/// `gh pr list --head <branch>` matches by branch *name*, not identity, so a
/// name reused after an unrelated PR merged and its head branch was deleted
/// (delete-branch-on-merge, as in the observed genio-learn/genio#28147 instance,
/// whose `refs/heads/ts-7` is gone from origin while gh still returns the PR)
/// otherwise adopts that PR's number, URL and merged state onto a brand-new
/// session that has never been pushed.
///
/// Only *settled* PRs are filtered, and only by their settle time:
/// - An open PR is always kept: its head branch provably still exists on the
///   remote, and session creation starts a branch from `origin/<branch>` when
///   that ref exists (`session/manager/lifecycle.rs`), so the session really is
///   working on that PR's branch — this is the Checkout Branch flow.
/// - A PR that settled *after* the session started is kept even if it was
///   opened earlier, which is the same Checkout Branch flow carried through to
///   a merge.
///
/// One genuine case is filtered deliberately: checking out a branch whose PR had
/// already merged or closed before the session took the name reports no PR. A
/// merged PR can't be reopened, and surfacing "merged" on a session that only
/// later took the branch would enrol it in the delete-merged-PR-sessions sweep —
/// the exact harm this filter exists to prevent.
///
/// `closedAt` is set for merged PRs too (verified against
/// `gh pr list --head ts-7 --state all --json state,closedAt,mergedAt` on
/// genio-learn/genio#28147: `state=MERGED`, `closedAt == mergedAt`);
/// `mergedAt` and then `createdAt` are fallbacks so a missing field can't make
/// a settled PR look current. An unparseable/absent timestamp keeps the PR —
/// filtering is the destructive direction.
fn pr_settled_before(entry: &serde_json::Value, owned_since: DateTime<Utc>) -> bool {
    if entry["state"].as_str() == Some("OPEN") {
        return false;
    }
    ["closedAt", "mergedAt", "createdAt"]
        .iter()
        .find_map(|field| parse_timestamp(&entry[field]))
        .is_some_and(|settled| settled < owned_since)
}

/// Parse a gh JSON timestamp (RFC 3339) into UTC, ignoring null/absent/garbage.
fn parse_timestamp(value: &serde_json::Value) -> Option<DateTime<Utc>> {
    let raw = value.as_str()?;
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

fn parse_pr_entry(v: &serde_json::Value) -> Option<PrInfo> {
    let number = v["number"].as_u64()? as u32;
    let url = v["url"].as_str()?.to_string();
    let state = match v["state"].as_str()? {
        "OPEN" => PrState::Open,
        "CLOSED" => PrState::Closed,
        "MERGED" => PrState::Merged,
        _ => return None,
    };
    let is_draft = v["isDraft"].as_bool().unwrap_or(false);
    let labels = v["labels"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|l| l["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let review_decision = v["reviewDecision"].as_str().and_then(|s| match s {
        "APPROVED" => Some(ReviewDecision::Approved),
        "CHANGES_REQUESTED" => Some(ReviewDecision::ChangesRequested),
        "REVIEW_REQUIRED" => Some(ReviewDecision::ReviewRequired),
        _ => None,
    });

    // Union of requested reviewer user logins and submitted review authors.
    // Team reviewer requests are skipped (they have `slug` not `login`).
    let mut reviewers: Vec<String> = Vec::new();
    if let Some(arr) = v["reviewRequests"].as_array() {
        for req in arr {
            if let Some(login) = req["login"].as_str() {
                reviewers.push(login.to_string());
            }
        }
    }
    if let Some(arr) = v["latestReviews"].as_array() {
        for r in arr {
            if let Some(login) = r["author"]["login"].as_str() {
                reviewers.push(login.to_string());
            }
        }
    }
    reviewers.sort();
    reviewers.dedup();
    let base_ref_name = v["baseRefName"].as_str().map(str::to_string);

    Some(PrInfo {
        number,
        url,
        state,
        is_draft,
        labels,
        review_decision,
        reviewers,
        base_ref_name,
    })
}

/// Fetch enriched PR details for a specific PR number via `gh pr view`.
///
/// Returns `None` on any failure (gh missing, not authed, network error, etc.).
pub async fn fetch_enriched_pr(repo_path: &Path, pr_number: u32) -> Option<EnrichedPrInfo> {
    let output = gh_command()
        .args([
            "pr",
            "view",
            &pr_number.to_string(),
            "--json",
            "title,state,isDraft,labels,statusCheckRollup,body,url",
        ])
        .current_dir(repo_path)
        .output()
        .await
        .ok()?;

    if !output.status.success() {
        debug!(
            "gh pr view {} failed: {}",
            pr_number,
            String::from_utf8_lossy(&output.stderr)
        );
        return None;
    }

    let json = String::from_utf8(output.stdout).ok()?;
    parse_enriched_pr_json(&json, pr_number)
}

/// Parse the JSON object returned by `gh pr view --json ...`.
fn parse_enriched_pr_json(json: &str, pr_number: u32) -> Option<EnrichedPrInfo> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;

    let title = v["title"].as_str().unwrap_or("").to_string();
    let url = v["url"].as_str().unwrap_or("").to_string();
    let body = v["body"].as_str().unwrap_or("").to_string();
    let is_draft = v["isDraft"].as_bool().unwrap_or(false);

    let state = match v["state"].as_str().unwrap_or("") {
        "OPEN" => PrState::Open,
        "CLOSED" => PrState::Closed,
        "MERGED" => PrState::Merged,
        _ => PrState::Open,
    };

    let labels = v["labels"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|l| {
                    Some(PrLabel {
                        name: l["name"].as_str()?.to_string(),
                        color: l["color"].as_str().unwrap_or("").to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let checks_status = parse_checks_rollup(&v["statusCheckRollup"]);

    Some(EnrichedPrInfo {
        number: pr_number,
        url,
        title,
        state,
        is_draft,
        labels,
        checks_status,
        body,
    })
}

/// Derive aggregate checks status from the `statusCheckRollup` array.
///
/// - Any `FAILURE` → `Failing`
/// - Any `null` or `PENDING` (without failures) → `Pending`
/// - All `SUCCESS` or `NEUTRAL` → `Passing`
/// - Empty array → `None`
fn parse_checks_rollup(value: &serde_json::Value) -> ChecksStatus {
    let Some(arr) = value.as_array() else {
        return ChecksStatus::None;
    };
    if arr.is_empty() {
        return ChecksStatus::None;
    }

    let mut has_pending = false;
    for check in arr {
        match check["conclusion"].as_str() {
            Some("FAILURE") => return ChecksStatus::Failing,
            Some("SUCCESS") | Some("NEUTRAL") | Some("SKIPPED") => {}
            // null, "PENDING", or anything else → pending
            _ => has_pending = true,
        }
    }

    if has_pending {
        ChecksStatus::Pending
    } else {
        ChecksStatus::Passing
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(raw: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(raw)
            .expect("valid RFC 3339")
            .with_timezone(&Utc)
    }

    /// Branch-ownership instant for fixtures that carry no PR timestamps at all
    /// — those can't be classified as stale, so the value is irrelevant to them.
    /// Staleness itself is covered by the `settled_before` tests below.
    fn owned_since() -> DateTime<Utc> {
        ts("2026-01-01T00:00:00Z")
    }

    #[test]
    fn test_parse_pr_list_open() {
        let json = r#"[{"number":42,"url":"https://github.com/owner/repo/pull/42","state":"OPEN","isDraft":false,"labels":[]}]"#;
        let result = parse_pr_list_json(json, owned_since());
        let info = result.info().unwrap();
        assert_eq!(info.number, 42);
        assert_eq!(info.url, "https://github.com/owner/repo/pull/42");
        assert_eq!(info.state, PrState::Open);
        assert!(!info.is_draft);
        assert!(info.labels.is_empty());
        assert!(info.base_ref_name.is_none());
        assert!(!info.merged());
    }

    #[test]
    fn test_parse_pr_list_captures_base_ref_name() {
        // `baseRefName` is the PR's target branch — used for stack detection.
        let json = r#"[{
            "number":5,
            "url":"u",
            "state":"OPEN",
            "isDraft":false,
            "labels":[],
            "baseRefName":"feature-login"
        }]"#;
        let result = parse_pr_list_json(json, owned_since());
        let info = result.info().unwrap();
        assert_eq!(info.base_ref_name.as_deref(), Some("feature-login"));
    }

    #[test]
    fn test_parse_pr_list_without_base_ref_name() {
        // Older gh responses / tests that omit baseRefName should leave the
        // field as None rather than failing.
        let json = r#"[{"number":9,"url":"u","state":"OPEN","isDraft":false,"labels":[]}]"#;
        let result = parse_pr_list_json(json, owned_since());
        let info = result.info().unwrap();
        assert!(info.base_ref_name.is_none());
    }

    #[test]
    fn test_parse_pr_list_merged() {
        let json = r#"[{"number":7,"url":"https://x/pull/7","state":"MERGED","isDraft":false,"labels":[]}]"#;
        let result = parse_pr_list_json(json, owned_since());
        let info = result.info().unwrap();
        assert_eq!(info.state, PrState::Merged);
        assert!(info.merged());
    }

    #[test]
    fn test_parse_pr_list_draft_with_labels() {
        let json = r#"[{
            "number":3,
            "url":"https://x/pull/3",
            "state":"OPEN",
            "isDraft":true,
            "labels":[{"name":"dev-review-required","color":"abc"},{"name":"trivial","color":"def"}]
        }]"#;
        let result = parse_pr_list_json(json, owned_since());
        let info = result.info().unwrap();
        assert!(info.is_draft);
        assert_eq!(info.labels, vec!["dev-review-required", "trivial"]);
    }

    #[test]
    fn test_parse_pr_list_prefers_open_over_merged() {
        let json = r#"[
            {"number":1,"url":"u1","state":"MERGED","isDraft":false,"labels":[]},
            {"number":2,"url":"u2","state":"OPEN","isDraft":false,"labels":[]}
        ]"#;
        let result = parse_pr_list_json(json, owned_since());
        let info = result.info().unwrap();
        assert_eq!(info.number, 2);
        assert_eq!(info.state, PrState::Open);
    }

    #[test]
    fn test_parse_pr_list_closed_when_no_open() {
        let json = r#"[{"number":9,"url":"u","state":"CLOSED","isDraft":false,"labels":[]}]"#;
        let result = parse_pr_list_json(json, owned_since());
        let info = result.info().unwrap();
        assert_eq!(info.state, PrState::Closed);
    }

    #[test]
    fn test_parse_pr_list_empty_array() {
        // Empty array is an authoritative "no PR" — must be NotFound, not
        // FetchFailed. Callers rely on this to clear stale PR state when a PR
        // is deleted upstream.
        assert!(parse_pr_list_json("[]", owned_since()).is_not_found());
    }

    /// A merged PR from a *previous* life of this branch name (someone else's
    /// work, merged before the session existed) must not be adopted as the
    /// session's PR. `--head` matches by name, and GitHub frees the name when it
    /// deletes the merged head branch, so name collisions are routine.
    #[test]
    fn test_parse_pr_list_ignores_pr_merged_before_branch_owned() {
        let json = r#"[{
            "number": 28147,
            "url": "https://github.com/o/r/pull/28147",
            "state": "MERGED",
            "isDraft": false,
            "labels": [],
            "createdAt": "2026-07-08T22:52:59Z",
            "closedAt": "2026-07-09T07:48:45Z",
            "mergedAt": "2026-07-09T07:48:45Z"
        }]"#;
        let result = parse_pr_list_json(json, ts("2026-08-10T13:24:49Z"));
        assert!(
            result.is_not_found(),
            "a PR merged a month before the session was created is not its PR"
        );
    }

    #[test]
    fn test_parse_pr_list_ignores_pr_closed_before_branch_owned() {
        let json = r#"[{
            "number": 12,
            "url": "u",
            "state": "CLOSED",
            "isDraft": false,
            "labels": [],
            "createdAt": "2026-07-01T00:00:00Z",
            "closedAt": "2026-07-02T00:00:00Z"
        }]"#;
        assert!(parse_pr_list_json(json, ts("2026-08-10T00:00:00Z")).is_not_found());
    }

    #[test]
    fn test_parse_pr_list_keeps_pr_merged_after_branch_owned() {
        // The session's own PR: opened and merged during its lifetime.
        let json = r#"[{
            "number": 30,
            "url": "u",
            "state": "MERGED",
            "isDraft": false,
            "labels": [],
            "createdAt": "2026-08-11T09:00:00Z",
            "closedAt": "2026-08-11T10:00:00Z",
            "mergedAt": "2026-08-11T10:00:00Z"
        }]"#;
        let info = parse_pr_list_json(json, ts("2026-08-10T00:00:00Z"))
            .info()
            .cloned()
            .expect("the session's own merged PR is kept");
        assert_eq!(info.number, 30);
        assert!(info.merged());
    }

    #[test]
    fn test_parse_pr_list_keeps_open_pr_older_than_session() {
        // Checkout Branch: the session adopts an existing remote branch whose PR
        // was opened before it. An open PR's head branch still exists, so it is
        // this session's PR regardless of age.
        let json = r#"[{
            "number": 5,
            "url": "u",
            "state": "OPEN",
            "isDraft": false,
            "labels": [],
            "createdAt": "2026-01-05T00:00:00Z"
        }]"#;
        let info = parse_pr_list_json(json, ts("2026-08-10T00:00:00Z"))
            .info()
            .cloned()
            .expect("an open PR is never filtered by age");
        assert_eq!(info.number, 5);
    }

    #[test]
    fn test_parse_pr_list_keeps_pr_that_settled_after_branch_owned() {
        // Same Checkout Branch flow, carried through to a merge: opened before
        // the session, merged after it started.
        let json = r#"[{
            "number": 6,
            "url": "u",
            "state": "MERGED",
            "isDraft": false,
            "labels": [],
            "createdAt": "2026-08-01T00:00:00Z",
            "closedAt": "2026-08-12T00:00:00Z",
            "mergedAt": "2026-08-12T00:00:00Z"
        }]"#;
        let info = parse_pr_list_json(json, ts("2026-08-10T00:00:00Z"))
            .info()
            .cloned()
            .expect("a PR that merged during the session is kept");
        assert_eq!(info.number, 6);
    }

    #[test]
    fn test_parse_pr_list_skips_stale_and_picks_the_sessions_own_pr() {
        // gh returns newest first; the stale entry must not win just because it
        // is the only one in some state, nor hide the session's real PR.
        let json = r#"[
            {
                "number": 31,
                "url": "u31",
                "state": "OPEN",
                "isDraft": false,
                "labels": [],
                "createdAt": "2026-08-11T00:00:00Z"
            },
            {
                "number": 7,
                "url": "u7",
                "state": "MERGED",
                "isDraft": false,
                "labels": [],
                "createdAt": "2026-06-01T00:00:00Z",
                "closedAt": "2026-06-02T00:00:00Z",
                "mergedAt": "2026-06-02T00:00:00Z"
            }
        ]"#;
        let info = parse_pr_list_json(json, ts("2026-08-10T00:00:00Z"))
            .info()
            .cloned()
            .expect("the session's own PR is found");
        assert_eq!(info.number, 31);
    }

    #[test]
    fn test_parse_pr_list_settled_pr_without_close_timestamp_falls_back_to_created() {
        // No closedAt/mergedAt (shouldn't happen, but must not resurrect a stale
        // PR): createdAt long before the session is enough to filter it.
        let json = r#"[{
            "number": 8,
            "url": "u",
            "state": "MERGED",
            "isDraft": false,
            "labels": [],
            "createdAt": "2026-02-01T00:00:00Z"
        }]"#;
        assert!(parse_pr_list_json(json, ts("2026-08-10T00:00:00Z")).is_not_found());
    }

    #[test]
    fn test_parse_pr_list_settled_pr_without_timestamps_is_kept() {
        // Nothing to judge staleness by → keep the PR. Filtering is the
        // destructive direction (it clears the session's PR metadata).
        let json = r#"[{"number":9,"url":"u","state":"MERGED","isDraft":false,"labels":[]}]"#;
        let info = parse_pr_list_json(json, ts("2026-08-10T00:00:00Z"))
            .info()
            .cloned()
            .expect("a timestamp-less PR is kept");
        assert_eq!(info.number, 9);
    }

    #[test]
    fn test_parse_pr_list_unparseable_timestamp_is_kept() {
        let json = r#"[{
            "number": 10,
            "url": "u",
            "state": "MERGED",
            "isDraft": false,
            "labels": [],
            "closedAt": "not-a-date",
            "createdAt": null
        }]"#;
        assert!(
            parse_pr_list_json(json, ts("2026-08-10T00:00:00Z"))
                .info()
                .is_some()
        );
    }

    #[test]
    fn test_parse_pr_list_review_decision_each_value() {
        for (raw, expected) in [
            ("APPROVED", Some(ReviewDecision::Approved)),
            ("CHANGES_REQUESTED", Some(ReviewDecision::ChangesRequested)),
            ("REVIEW_REQUIRED", Some(ReviewDecision::ReviewRequired)),
        ] {
            let json = format!(
                r#"[{{
                    "number": 1,
                    "url": "https://x/1",
                    "state": "OPEN",
                    "isDraft": false,
                    "labels": [],
                    "reviewDecision": "{raw}"
                }}]"#
            );
            let result = parse_pr_list_json(&json, owned_since());
            let info = result.info().expect("parses");
            assert_eq!(info.review_decision, expected, "for raw={raw}");
        }
    }

    #[test]
    fn test_parse_pr_list_missing_review_decision_is_none() {
        let json = r#"[{
            "number": 1,
            "url": "https://x/1",
            "state": "OPEN",
            "isDraft": false,
            "labels": []
        }]"#;
        let result = parse_pr_list_json(json, owned_since());
        let info = result.info().expect("parses");
        assert_eq!(info.review_decision, None);
    }

    #[test]
    fn test_parse_pr_list_reviewers_unions_requests_and_submitted() {
        // Requested reviewers and submitted review authors should both end
        // up in `reviewers` (deduped). Teams in reviewRequests are skipped
        // (we surface only user logins).
        let json = r#"[{
            "number": 1,
            "url": "https://x/1",
            "state": "OPEN",
            "isDraft": false,
            "labels": [],
            "reviewRequests": [
                {"__typename": "User", "login": "alice"},
                {"__typename": "Team", "slug": "platform"}
            ],
            "latestReviews": [
                {"author": {"login": "bob"}, "state": "COMMENTED"},
                {"author": {"login": "alice"}, "state": "APPROVED"}
            ]
        }]"#;
        let result = parse_pr_list_json(json, owned_since());
        let info = result.info().expect("parses");
        let mut reviewers = info.reviewers.clone();
        reviewers.sort();
        assert_eq!(reviewers, vec!["alice".to_string(), "bob".to_string()]);
    }

    #[test]
    fn test_parse_pr_list_missing_reviewer_fields_is_empty() {
        let json = r#"[{
            "number": 1,
            "url": "https://x/1",
            "state": "OPEN",
            "isDraft": false,
            "labels": []
        }]"#;
        let result = parse_pr_list_json(json, owned_since());
        let info = result.info().expect("parses");
        assert!(info.reviewers.is_empty());
    }

    #[test]
    fn test_parse_pr_list_garbage() {
        // Malformed JSON → FetchFailed, so a gh regression/panic doesn't wipe
        // cached PR state on every poll.
        assert!(parse_pr_list_json("not json", owned_since()).is_fetch_failed());
    }

    #[test]
    fn test_parse_pr_list_not_an_array() {
        assert!(parse_pr_list_json(r#"{"oops":1}"#, owned_since()).is_fetch_failed());
    }

    #[test]
    fn test_parse_pr_list_unknown_state() {
        // An entry with an unrecognised state string would previously drop to
        // None; now it's explicitly FetchFailed so we don't mistake it for
        // "no PR" and wipe stack metadata.
        let json = r#"[{"number":1,"url":"u","state":"NEW_STATE","isDraft":false,"labels":[]}]"#;
        assert!(parse_pr_list_json(json, owned_since()).is_fetch_failed());
    }

    #[test]
    fn test_parse_enriched_pr_open() {
        let json = r#"{
            "title": "Add auth flow",
            "url": "https://github.com/org/repo/pull/42",
            "state": "OPEN",
            "isDraft": false,
            "labels": [
                {"name": "bug", "color": "d73a4a"},
                {"name": "enhancement", "color": "a2eeef"}
            ],
            "statusCheckRollup": [
                {"conclusion": "SUCCESS"},
                {"conclusion": "SUCCESS"}
            ],
            "body": "This PR adds auth."
        }"#;
        let info = parse_enriched_pr_json(json, 42).unwrap();
        assert_eq!(info.number, 42);
        assert_eq!(info.title, "Add auth flow");
        assert_eq!(info.url, "https://github.com/org/repo/pull/42");
        assert_eq!(info.state, PrState::Open);
        assert!(!info.is_draft);
        assert_eq!(info.labels.len(), 2);
        assert_eq!(info.labels[0].name, "bug");
        assert_eq!(info.labels[0].color, "d73a4a");
        assert_eq!(info.checks_status, ChecksStatus::Passing);
        assert_eq!(info.body, "This PR adds auth.");
    }

    #[test]
    fn test_parse_enriched_pr_merged_draft() {
        let json = r#"{
            "title": "Refactor",
            "url": "https://github.com/org/repo/pull/7",
            "state": "MERGED",
            "isDraft": true,
            "labels": [],
            "statusCheckRollup": [],
            "body": ""
        }"#;
        let info = parse_enriched_pr_json(json, 7).unwrap();
        assert_eq!(info.state, PrState::Merged);
        assert!(info.is_draft);
        assert!(info.labels.is_empty());
        assert_eq!(info.checks_status, ChecksStatus::None);
    }

    #[test]
    fn test_parse_enriched_pr_closed() {
        let json = r#"{
            "title": "WIP",
            "url": "",
            "state": "CLOSED",
            "isDraft": false,
            "labels": [],
            "statusCheckRollup": [],
            "body": ""
        }"#;
        let info = parse_enriched_pr_json(json, 1).unwrap();
        assert_eq!(info.state, PrState::Closed);
    }

    #[test]
    fn test_checks_rollup_all_passing() {
        let v: serde_json::Value = serde_json::from_str(
            r#"[{"conclusion":"SUCCESS"},{"conclusion":"NEUTRAL"},{"conclusion":"SKIPPED"}]"#,
        )
        .unwrap();
        assert_eq!(parse_checks_rollup(&v), ChecksStatus::Passing);
    }

    #[test]
    fn test_checks_rollup_one_failure() {
        let v: serde_json::Value =
            serde_json::from_str(r#"[{"conclusion":"SUCCESS"},{"conclusion":"FAILURE"}]"#).unwrap();
        assert_eq!(parse_checks_rollup(&v), ChecksStatus::Failing);
    }

    #[test]
    fn test_checks_rollup_pending() {
        let v: serde_json::Value =
            serde_json::from_str(r#"[{"conclusion":"SUCCESS"},{"conclusion":null}]"#).unwrap();
        assert_eq!(parse_checks_rollup(&v), ChecksStatus::Pending);
    }

    #[test]
    fn test_checks_rollup_empty() {
        let v: serde_json::Value = serde_json::from_str("[]").unwrap();
        assert_eq!(parse_checks_rollup(&v), ChecksStatus::None);
    }

    #[test]
    fn test_checks_rollup_not_array() {
        let v: serde_json::Value = serde_json::from_str("null").unwrap();
        assert_eq!(parse_checks_rollup(&v), ChecksStatus::None);
    }

    #[test]
    fn test_parse_enriched_pr_invalid_json() {
        assert!(parse_enriched_pr_json("not json", 1).is_none());
    }

    #[test]
    fn test_pr_state_display() {
        assert_eq!(PrState::Open.to_string(), "Open");
        assert_eq!(PrState::Closed.to_string(), "Closed");
        assert_eq!(PrState::Merged.to_string(), "Merged");
    }

    #[test]
    fn test_effective_pr_state() {
        // Explicit state always wins, even when pr_merged disagrees.
        assert_eq!(
            effective_pr_state(Some(PrState::Merged), false),
            PrState::Merged
        );
        assert_eq!(effective_pr_state(Some(PrState::Open), true), PrState::Open);
        assert_eq!(
            effective_pr_state(Some(PrState::Closed), true),
            PrState::Closed
        );
        // Fallback to pr_merged bool when state is missing (legacy state.json).
        assert_eq!(effective_pr_state(None, true), PrState::Merged);
        assert_eq!(effective_pr_state(None, false), PrState::Open);
    }

    #[test]
    fn test_checks_status_display() {
        assert_eq!(ChecksStatus::Passing.to_string(), "Passing");
        assert_eq!(ChecksStatus::Failing.to_string(), "Failing");
        assert_eq!(ChecksStatus::Pending.to_string(), "Pending");
        assert_eq!(ChecksStatus::None.to_string(), "None");
    }
}
