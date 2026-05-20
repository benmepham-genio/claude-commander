//! GitHub PR detection via `gh` CLI
//!
//! Checks whether a branch has an open pull request using `gh pr list`.
//! All failures are silently swallowed — missing `gh`, auth errors, network
//! issues, or repos without a GitHub remote simply result in `None`.

use std::path::Path;

use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tracing::debug;

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

/// PR state as reported by the GitHub API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrState {
    Open,
    Closed,
    Merged,
}

/// GitHub `reviewDecision` field — derived state of the review process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDecision {
    /// Reviews requested, none decisive yet (includes comment-only reviews).
    ReviewRequired,
    /// At least one approving review and no outstanding changes-requested.
    Approved,
    /// At least one reviewer requested changes.
    ChangesRequested,
}

impl std::fmt::Display for PrState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Open => write!(f, "Open"),
            Self::Closed => write!(f, "Closed"),
            Self::Merged => write!(f, "Merged"),
        }
    }
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
    match Command::new("gh").arg("--version").output().await {
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

/// Check whether `branch` has a PR (any state) in the repo at `repo_path`.
///
/// Returns a three-way result: `Found` when a PR matched, `NotFound` when gh
/// successfully reported no PR, and `FetchFailed` on any error. Callers must
/// preserve cached PR state on `FetchFailed` so transient gh/network hiccups
/// don't wipe UI state (notably the PR-stack topology).
///
/// Prefers open PRs over closed/merged when a branch has multiple PRs (rare,
/// but possible after a reopen).
pub async fn check_pr_for_branch(repo_path: &Path, branch: &str) -> PrCheckResult {
    let output = match Command::new("gh")
        .args([
            "pr",
            "list",
            "--head",
            branch,
            "--state",
            "all",
            "--json",
            "number,url,state,isDraft,labels,baseRefName,reviewDecision,reviewRequests,latestReviews",
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
    parse_pr_list_json(&json)
}

/// Parse the JSON array returned by `gh pr list --json number,url,state,isDraft,labels,baseRefName`.
///
/// Empty array → `NotFound` (gh told us there's no PR). Missing/malformed
/// JSON → `FetchFailed`. Non-empty array → `Found`, preferring the first
/// open PR if any exist, otherwise the first entry (gh returns them in
/// reverse-creation order).
fn parse_pr_list_json(json: &str) -> PrCheckResult {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return PrCheckResult::FetchFailed;
    };
    let Some(arr) = v.as_array() else {
        return PrCheckResult::FetchFailed;
    };
    if arr.is_empty() {
        return PrCheckResult::NotFound;
    }

    let chosen = arr
        .iter()
        .find(|p| p["state"].as_str() == Some("OPEN"))
        .unwrap_or(&arr[0]);

    match parse_pr_entry(chosen) {
        Some(info) => PrCheckResult::Found(info),
        // Unknown/new state string — treat as malformed.
        None => PrCheckResult::FetchFailed,
    }
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
    let output = Command::new("gh")
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

    #[test]
    fn test_parse_pr_list_open() {
        let json = r#"[{"number":42,"url":"https://github.com/owner/repo/pull/42","state":"OPEN","isDraft":false,"labels":[]}]"#;
        let result = parse_pr_list_json(json);
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
        let result = parse_pr_list_json(json);
        let info = result.info().unwrap();
        assert_eq!(info.base_ref_name.as_deref(), Some("feature-login"));
    }

    #[test]
    fn test_parse_pr_list_without_base_ref_name() {
        // Older gh responses / tests that omit baseRefName should leave the
        // field as None rather than failing.
        let json = r#"[{"number":9,"url":"u","state":"OPEN","isDraft":false,"labels":[]}]"#;
        let result = parse_pr_list_json(json);
        let info = result.info().unwrap();
        assert!(info.base_ref_name.is_none());
    }

    #[test]
    fn test_parse_pr_list_merged() {
        let json = r#"[{"number":7,"url":"https://x/pull/7","state":"MERGED","isDraft":false,"labels":[]}]"#;
        let result = parse_pr_list_json(json);
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
        let result = parse_pr_list_json(json);
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
        let result = parse_pr_list_json(json);
        let info = result.info().unwrap();
        assert_eq!(info.number, 2);
        assert_eq!(info.state, PrState::Open);
    }

    #[test]
    fn test_parse_pr_list_closed_when_no_open() {
        let json = r#"[{"number":9,"url":"u","state":"CLOSED","isDraft":false,"labels":[]}]"#;
        let result = parse_pr_list_json(json);
        let info = result.info().unwrap();
        assert_eq!(info.state, PrState::Closed);
    }

    #[test]
    fn test_parse_pr_list_empty_array() {
        // Empty array is an authoritative "no PR" — must be NotFound, not
        // FetchFailed. Callers rely on this to clear stale PR state when a PR
        // is deleted upstream.
        assert!(parse_pr_list_json("[]").is_not_found());
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
            let result = parse_pr_list_json(&json);
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
        let result = parse_pr_list_json(json);
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
        let result = parse_pr_list_json(json);
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
        let result = parse_pr_list_json(json);
        let info = result.info().expect("parses");
        assert!(info.reviewers.is_empty());
    }

    #[test]
    fn test_parse_pr_list_garbage() {
        // Malformed JSON → FetchFailed, so a gh regression/panic doesn't wipe
        // cached PR state on every poll.
        assert!(parse_pr_list_json("not json").is_fetch_failed());
    }

    #[test]
    fn test_parse_pr_list_not_an_array() {
        assert!(parse_pr_list_json(r#"{"oops":1}"#).is_fetch_failed());
    }

    #[test]
    fn test_parse_pr_list_unknown_state() {
        // An entry with an unrecognised state string would previously drop to
        // None; now it's explicitly FetchFailed so we don't mistake it for
        // "no PR" and wipe stack metadata.
        let json = r#"[{"number":1,"url":"u","state":"NEW_STATE","isDraft":false,"labels":[]}]"#;
        assert!(parse_pr_list_json(json).is_fetch_failed());
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
