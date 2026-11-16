//! GitLab repository discovery through `glab`.

use chrono::{DateTime, Utc};
use claude_commander_protocol::hosting::{HostedRepository, RepositoryVisibility};
use serde::Deserialize;
use tokio::process::Command;

use crate::error::{GitError, Result};
use crate::git::bounded::{self, Bounded};
use crate::git::{ChecksStatus, EnrichedPrInfo, PrCheckResult, PrInfo, PrLabel, PrState};

pub async fn is_available() -> bool {
    let mut cmd = crate::git::spawn::detached("glab");
    cmd.arg("version").env("GLAB_NO_PROMPT", "1");
    available_from(cmd).await
}

async fn available_from(mut cmd: Command) -> bool {
    cmd.output()
        .await
        .is_ok_and(|output| output.status.success())
}

pub async fn list_repositories(
    hostname: Option<&str>,
    timeout: std::time::Duration,
) -> Result<Vec<HostedRepository>> {
    use claude_commander_protocol::hosting::{CodeHostProvider, validate_gitlab_hostname};

    if let Some(hostname) = hostname {
        validate_gitlab_hostname(hostname)
            .map_err(|error| GitError::CodeHostHostnameRejected(error.to_string()))?;
    }

    if !is_available().await {
        return Err(GitError::CodeHostCliUnavailable {
            provider: CodeHostProvider::Gitlab,
        }
        .into());
    }

    repositories_from(api_projects_command(hostname), timeout).await
}

async fn repositories_from(
    cmd: Command,
    timeout: std::time::Duration,
) -> Result<Vec<HostedRepository>> {
    use claude_commander_protocol::hosting::CodeHostProvider;
    let outcome = bounded::run_bounded(cmd, "glab api", timeout)
        .await
        .map_err(|_| GitError::CodeHostCliUnavailable {
            provider: CodeHostProvider::Gitlab,
        })?;
    let Bounded::Finished {
        status,
        stdout,
        stderr,
    } = outcome
    else {
        return Err(GitError::RepoListTimedOut {
            provider: CodeHostProvider::Gitlab,
            secs: timeout.as_secs(),
        }
        .into());
    };
    if !status.success() {
        return Err(GitError::OperationFailed(format!(
            "glab api projects failed: {}",
            String::from_utf8_lossy(&stderr).trim()
        ))
        .into());
    }
    parse_project_stream(&String::from_utf8_lossy(&stdout))
}

/// Build the GitLab project-membership query confirmed by the glab 1.113.0 help receipt.
pub(super) fn api_projects_command(hostname: Option<&str>) -> Command {
    let mut cmd = crate::git::spawn::detached("glab");
    cmd.args([
        "api",
        "projects",
        "--method",
        "GET",
        "--field",
        "membership=true",
        "--field",
        "order_by=last_activity_at",
        "--field",
        "sort=desc",
        "--field",
        "per_page=100",
        "--paginate",
        "--output",
        "ndjson",
    ]);
    if let Some(hostname) = hostname {
        cmd.args(["--hostname", hostname]);
    }
    cmd.env("GLAB_NO_PROMPT", "1");
    cmd
}

#[derive(Deserialize)]
struct GitlabNamespace {
    full_path: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum GitlabVisibility {
    Public,
    Internal,
    Private,
}

impl From<GitlabVisibility> for RepositoryVisibility {
    fn from(value: GitlabVisibility) -> Self {
        match value {
            GitlabVisibility::Public => Self::Public,
            GitlabVisibility::Internal => Self::Internal,
            GitlabVisibility::Private => Self::Private,
        }
    }
}

#[derive(Deserialize)]
struct GitlabProject {
    path_with_namespace: String,
    namespace: GitlabNamespace,
    path: String,
    #[serde(default)]
    description: Option<String>,
    visibility: GitlabVisibility,
    #[serde(default)]
    forked_from_project: Option<serde_json::Value>,
    archived: bool,
    #[serde(default)]
    default_branch: Option<String>,
    http_url_to_repo: String,
    ssh_url_to_repo: String,
    #[serde(default)]
    last_activity_at: Option<DateTime<Utc>>,
}

impl From<GitlabProject> for HostedRepository {
    fn from(project: GitlabProject) -> Self {
        Self {
            full_name: project.path_with_namespace,
            namespace: project.namespace.full_path,
            name: project.path,
            description: project.description,
            visibility: project.visibility.into(),
            fork: project.forked_from_project.is_some(),
            archived: project.archived,
            default_branch: project.default_branch,
            clone_url: project.http_url_to_repo,
            ssh_url: project.ssh_url_to_repo,
            activity_at: project.last_activity_at,
        }
    }
}

fn parse_project_stream(output: &str) -> Result<Vec<HostedRepository>> {
    serde_json::Deserializer::from_str(output)
        .into_iter::<GitlabProject>()
        .map(|project| project.map(HostedRepository::from))
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|error| {
            GitError::OperationFailed(format!("failed to parse glab project list: {error}")).into()
        })
}

fn merge_requests_command(repo_path: &std::path::Path, branch: &str) -> Command {
    let mut cmd = crate::git::spawn::detached("glab");
    cmd.args([
        "api",
        "projects/:fullpath/merge_requests",
        "--method",
        "GET",
        "--raw-field",
        &format!("source_branch={branch}"),
        "--field",
        "state=all",
        "--field",
        "per_page=5",
        "--field",
        "order_by=created_at",
        "--field",
        "sort=desc",
    ])
    .env("GLAB_NO_PROMPT", "1")
    .current_dir(repo_path);
    cmd
}

pub async fn check_review_for_branch(
    repo_path: &std::path::Path,
    branch: &str,
    branch_owned_since: DateTime<Utc>,
) -> PrCheckResult {
    let output = match merge_requests_command(repo_path, branch).output().await {
        Ok(output) if output.status.success() => output,
        _ => return PrCheckResult::FetchFailed,
    };
    let Ok(json) = String::from_utf8(output.stdout) else {
        return PrCheckResult::FetchFailed;
    };
    parse_merge_request_list(&json, branch, branch_owned_since)
}

fn parse_merge_request_list(json: &str, branch: &str, owned_since: DateTime<Utc>) -> PrCheckResult {
    let Ok(values) = serde_json::from_str::<Vec<serde_json::Value>>(json) else {
        return PrCheckResult::FetchFailed;
    };
    // The query already filters on `source_branch`; checking again means a
    // query that ever came back unfiltered attaches nothing rather than some
    // other branch's merge request.
    let candidates = values
        .iter()
        .filter(|value| value["source_branch"].as_str() == Some(branch))
        .filter(|value| !gitlab_review_settled_before(value, owned_since))
        .collect::<Vec<_>>();
    let Some(first) = candidates.first() else {
        return PrCheckResult::NotFound;
    };
    let chosen = candidates
        .iter()
        .find(|value| value["state"].as_str() == Some("opened"))
        .unwrap_or(first);
    parse_merge_request(chosen)
        .map(PrCheckResult::Found)
        .unwrap_or(PrCheckResult::FetchFailed)
}

fn timestamp(value: &serde_json::Value) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value.as_str()?)
        .ok()
        .map(|value| value.with_timezone(&Utc))
}

fn gitlab_review_settled_before(value: &serde_json::Value, owned_since: DateTime<Utc>) -> bool {
    if value["state"].as_str() == Some("opened") {
        return false;
    }
    ["closed_at", "merged_at", "created_at"]
        .iter()
        .find_map(|field| timestamp(&value[field]))
        .is_some_and(|settled| settled < owned_since)
}

fn parse_merge_request(value: &serde_json::Value) -> Option<PrInfo> {
    let number = u32::try_from(value["iid"].as_u64()?).ok()?;
    let state = match value["state"].as_str()? {
        "opened" => PrState::Open,
        "closed" => PrState::Closed,
        "merged" => PrState::Merged,
        _ => return None,
    };
    let mut reviewers = value["reviewers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|reviewer| reviewer["username"].as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    reviewers.sort();
    reviewers.dedup();
    Some(PrInfo {
        number,
        url: value["web_url"].as_str()?.to_owned(),
        state,
        is_draft: value["draft"]
            .as_bool()
            .or_else(|| value["work_in_progress"].as_bool())
            .unwrap_or(false),
        labels: value["labels"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|label| label.as_str().map(str::to_owned))
            .collect(),
        review_decision: None,
        reviewers,
        base_ref_name: value["target_branch"].as_str().map(str::to_owned),
    })
}

pub async fn fetch_enriched_review(
    repo_path: &std::path::Path,
    number: u32,
) -> Option<EnrichedPrInfo> {
    let output = enriched_review_command(repo_path, number)
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_enriched_merge_request(&String::from_utf8(output.stdout).ok()?, number)
}

fn enriched_review_command(repo_path: &std::path::Path, number: u32) -> Command {
    let mut cmd = crate::git::spawn::detached("glab");
    cmd.args([
        "api",
        &format!("projects/:fullpath/merge_requests/{number}"),
        "--method",
        "GET",
    ])
    .env("GLAB_NO_PROMPT", "1")
    .current_dir(repo_path);
    cmd
}

fn parse_enriched_merge_request(json: &str, number: u32) -> Option<EnrichedPrInfo> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let state = match value["state"].as_str()? {
        "opened" => PrState::Open,
        "closed" => PrState::Closed,
        "merged" => PrState::Merged,
        _ => return None,
    };
    Some(EnrichedPrInfo {
        number,
        url: value["web_url"].as_str()?.to_owned(),
        title: value["title"].as_str()?.to_owned(),
        state,
        is_draft: value["draft"]
            .as_bool()
            .or_else(|| value["work_in_progress"].as_bool())
            .unwrap_or(false),
        labels: value["labels"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|label| {
                label.as_str().map(|name| PrLabel {
                    name: name.to_owned(),
                    color: String::new(),
                })
            })
            .collect(),
        checks_status: gitlab_pipeline_status(&value["head_pipeline"]),
        body: value["description"].as_str().unwrap_or("").to_owned(),
    })
}

fn gitlab_pipeline_status(pipeline: &serde_json::Value) -> ChecksStatus {
    match pipeline["status"].as_str() {
        None | Some("") => ChecksStatus::None,
        Some("success" | "skipped") => ChecksStatus::Passing,
        Some("failed" | "canceled") => ChecksStatus::Failing,
        Some(_) => ChecksStatus::Pending,
    }
}

pub async fn try_retarget_review_base(
    repo_path: &std::path::Path,
    number: u32,
    new_base: &str,
) -> std::result::Result<(), String> {
    let output = super::review_output(retarget_review_command(repo_path, number, new_base), "glab")
        .await
        .map_err(|error| format!("could not run glab: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        Err(if stderr.is_empty() {
            "glab mr update failed".to_owned()
        } else {
            stderr
        })
    }
}

fn retarget_review_command(repo_path: &std::path::Path, number: u32, new_base: &str) -> Command {
    let mut cmd = crate::git::spawn::detached("glab");
    cmd.args([
        "mr",
        "update",
        &number.to_string(),
        "--target-branch",
        new_base,
        "--yes",
    ])
    .env("GLAB_NO_PROMPT", "1")
    .current_dir(repo_path);
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::GitError;
    use claude_commander_protocol::hosting::RepositoryVisibility;
    use nix::{sys::signal::kill, unistd::Pid};
    use std::time::Instant;
    use tempfile::TempDir;

    #[test]
    fn listing_command_matches_the_recorded_glab_1_113_shape() {
        let cmd = api_projects_command(Some("gitlab.example.com"));
        assert_eq!(cmd.as_std().get_program(), "glab");
        let args = cmd
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            args,
            [
                "api",
                "projects",
                "--method",
                "GET",
                "--field",
                "membership=true",
                "--field",
                "order_by=last_activity_at",
                "--field",
                "sort=desc",
                "--field",
                "per_page=100",
                "--paginate",
                "--output",
                "ndjson",
                "--hostname",
                "gitlab.example.com",
            ]
        );
        assert_eq!(
            cmd.as_std()
                .get_envs()
                .find(|(key, _)| *key == "GLAB_NO_PROMPT")
                .unwrap()
                .1,
            Some(std::ffi::OsStr::new("1"))
        );
    }

    #[test]
    fn parses_ndjson_projects_with_nested_namespaces_and_all_visibilities() {
        let output = r#"{"path_with_namespace":"group/sub/project","namespace":{"full_path":"group/sub"},"path":"project","description":"Nested","visibility":"internal","forked_from_project":{"id":1},"archived":true,"default_branch":null,"http_url_to_repo":"https://gitlab.example.com/group/sub/project.git","ssh_url_to_repo":"git@gitlab.example.com:group/sub/project.git","last_activity_at":"2026-08-01T12:00:00Z"}
{"path_with_namespace":"me/public","namespace":{"full_path":"me"},"path":"public","description":null,"visibility":"public","forked_from_project":null,"archived":false,"default_branch":"main","http_url_to_repo":"https://gitlab.com/me/public.git","ssh_url_to_repo":"git@gitlab.com:me/public.git","last_activity_at":null}
{"path_with_namespace":"me/private","namespace":{"full_path":"me"},"path":"private","visibility":"private","forked_from_project":null,"archived":false,"default_branch":"trunk","http_url_to_repo":"https://gitlab.com/me/private.git","ssh_url_to_repo":"git@gitlab.com:me/private.git","last_activity_at":null}"#;
        let repos = parse_project_stream(output).unwrap();
        assert_eq!(repos.len(), 3);
        assert_eq!(repos[0].namespace, "group/sub");
        assert_eq!(repos[0].visibility, RepositoryVisibility::Internal);
        assert!(repos[0].fork);
        assert!(repos[0].archived);
        assert_eq!(repos[0].default_branch, None);
        assert_eq!(repos[1].visibility, RepositoryVisibility::Public);
        assert_eq!(repos[2].visibility, RepositoryVisibility::Private);
        assert!(repos[1].activity_at.is_none());
    }

    #[test]
    fn empty_output_is_an_empty_project_list() {
        assert!(parse_project_stream("").unwrap().is_empty());
    }

    #[test]
    fn malformed_or_incomplete_projects_are_errors() {
        assert!(parse_project_stream("{").is_err());
        assert!(parse_project_stream(r#"{"path_with_namespace":"group/project"}"#).is_err());
    }

    #[tokio::test]
    async fn availability_distinguishes_success_nonzero_and_spawn_failure() {
        let mut success = Command::new("sh");
        success.args(["-c", "exit 0"]);
        assert!(available_from(success).await);

        let mut nonzero = Command::new("sh");
        nonzero.args(["-c", "exit 1"]);
        assert!(!available_from(nonzero).await);

        assert!(!available_from(Command::new("definitely-no-such-glab-test-binary")).await);
    }

    #[tokio::test]
    async fn effectful_listing_rejects_an_invalid_hostname_before_spawning_glab() {
        let err = list_repositories(
            Some("https://user:secret@gitlab.example.com"),
            std::time::Duration::from_secs(1),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("invalid code-host hostname"));
        assert!(!err.to_string().contains("secret"));
    }

    #[tokio::test]
    async fn prepared_listing_keeps_spawn_failure_api_failure_and_timeout_distinct() {
        let spawn = repositories_from(
            Command::new("definitely-no-such-glab-list-test-binary"),
            std::time::Duration::from_secs(1),
        )
        .await
        .unwrap_err();
        assert!(matches!(
            spawn,
            crate::error::Error::Git(GitError::CodeHostCliUnavailable { .. })
        ));

        let mut failed = Command::new("sh");
        failed.args(["-c", "echo unauthenticated >&2; exit 1"]);
        let failed = repositories_from(failed, std::time::Duration::from_secs(1))
            .await
            .unwrap_err();
        assert!(matches!(
            failed,
            crate::error::Error::Git(GitError::OperationFailed(_))
        ));

        let mut hanging = Command::new("sh");
        hanging.args(["-c", "sleep 300"]);
        let timed_out = repositories_from(hanging, std::time::Duration::from_millis(100))
            .await
            .unwrap_err();
        assert!(matches!(
            timed_out,
            crate::error::Error::Git(GitError::RepoListTimedOut { .. })
        ));
    }

    #[tokio::test]
    async fn a_timed_out_listing_kills_glab_and_its_descendants() {
        let tmp = TempDir::new().unwrap();
        let pidfile = tmp.path().join("grandchild.pid");
        let mut hanging = Command::new("sh");
        hanging
            .arg("-c")
            .arg(format!("sleep 300 & echo $! > {}; wait", pidfile.display()));

        let err = repositories_from(hanging, std::time::Duration::from_millis(300))
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            crate::error::Error::Git(GitError::RepoListTimedOut { .. })
        ));
        let grandchild = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse::<i32>()
            .unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if kill(Pid::from_raw(grandchild), None).is_err() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "grandchild {grandchild} survived the timeout kill"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    #[test]
    fn merge_request_command_treats_branch_as_a_literal_string() {
        // `glab api --field value=@path` reads the named local file. Branch
        // names may legally begin with `@`, so only --raw-field is safe here.
        let cmd = merge_requests_command(std::path::Path::new("/repo"), "@.env");
        let args = cmd
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            args,
            [
                "api",
                "projects/:fullpath/merge_requests",
                "--method",
                "GET",
                "--raw-field",
                "source_branch=@.env",
                "--field",
                "state=all",
                "--field",
                "per_page=5",
                "--field",
                "order_by=created_at",
                "--field",
                "sort=desc"
            ]
        );
        assert_eq!(
            cmd.as_std().get_current_dir(),
            Some(std::path::Path::new("/repo"))
        );
        assert_eq!(
            cmd.as_std()
                .get_envs()
                .find(|(key, _)| *key == "GLAB_NO_PROMPT")
                .unwrap()
                .1,
            Some(std::ffi::OsStr::new("1"))
        );
    }

    #[test]
    fn merge_requests_from_other_branches_are_ignored() {
        let json = r#"[{"source_branch":"other","iid":7,"web_url":"u","state":"opened","labels":[],"reviewers":[],"target_branch":"main"}]"#;
        assert!(matches!(
            parse_merge_request_list(json, "feat", Utc::now()),
            PrCheckResult::NotFound
        ));
    }

    #[test]
    fn merge_request_parser_matches_gitlab_fields() {
        let json = r#"[
          {"source_branch":"feat","iid":1,"web_url":"old","state":"merged","draft":false,"labels":[],"reviewers":[],"target_branch":"main","created_at":"2020-01-01T00:00:00Z","merged_at":"2020-01-02T00:00:00Z"},
          {"source_branch":"feat","iid":2,"web_url":"new","state":"opened","work_in_progress":true,"labels":["review"],"reviewers":[{"username":"bob"},{"username":"alice"}],"target_branch":"parent","created_at":"2026-01-02T00:00:00Z"}
        ]"#;
        let result = parse_merge_request_list(
            json,
            "feat",
            DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        );
        let info = result.info().unwrap();
        assert_eq!(info.number, 2);
        assert_eq!(info.state, PrState::Open);
        assert!(info.is_draft);
        assert_eq!(info.labels, ["review"]);
        assert_eq!(info.reviewers, ["alice", "bob"]);
        assert_eq!(info.base_ref_name.as_deref(), Some("parent"));
        assert_eq!(info.review_decision, None);
    }

    #[test]
    fn merge_request_iid_overflow_and_unknown_state_fail_safely() {
        let owned = Utc::now();
        assert!(
            parse_merge_request_list(
                r#"[{"source_branch":"feat","iid":4294967296,"web_url":"u","state":"opened"}]"#,
                "feat",
                owned
            )
            .is_fetch_failed()
        );
        assert!(
            parse_merge_request_list(
                r#"[{"source_branch":"feat","iid":1,"web_url":"u","state":"locked"}]"#,
                "feat",
                owned
            )
            .is_fetch_failed()
        );
    }

    #[test]
    fn empty_and_recent_settled_merge_request_results_are_preserved() {
        let owned = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert!(matches!(
            parse_merge_request_list("[]", "feat", owned),
            PrCheckResult::NotFound
        ));

        for (state, timestamp_field, expected) in [
            ("closed", "closed_at", PrState::Closed),
            ("merged", "merged_at", PrState::Merged),
        ] {
            let json = format!(
                r#"[{{"source_branch":"feat","iid":3,"web_url":"u","state":"{state}","labels":[],"reviewers":[],"target_branch":"main","created_at":"2026-02-01T00:00:00Z","{timestamp_field}":"2026-02-02T00:00:00Z"}}]"#
            );
            assert_eq!(
                parse_merge_request_list(&json, "feat", owned)
                    .info()
                    .unwrap()
                    .state,
                expected
            );
        }
    }

    #[test]
    fn enriched_merge_request_maps_labels_and_every_pipeline_class() {
        for (status, expected) in [
            (None, ChecksStatus::None),
            (Some("success"), ChecksStatus::Passing),
            (Some("skipped"), ChecksStatus::Passing),
            (Some("failed"), ChecksStatus::Failing),
            (Some("canceled"), ChecksStatus::Failing),
            (Some("running"), ChecksStatus::Pending),
            (Some("future_status"), ChecksStatus::Pending),
        ] {
            let pipeline = status
                .map(|value| format!(r#"{{"status":"{value}"}}"#))
                .unwrap_or_else(|| "null".to_owned());
            let json = format!(
                r#"{{"title":"MR","description":"body","web_url":"u","state":"opened","draft":false,"labels":["bug"],"head_pipeline":{pipeline}}}"#
            );
            let info = parse_enriched_merge_request(&json, 7).unwrap();
            assert_eq!(info.checks_status, expected, "status={status:?}");
            assert_eq!(info.labels[0].color, "");
        }
        assert!(parse_enriched_merge_request("not json", 7).is_none());
    }

    #[test]
    fn rich_detail_and_retarget_commands_match_the_glab_receipt() {
        let rich = enriched_review_command(std::path::Path::new("/repo"), 17);
        assert_eq!(
            rich.as_std()
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            [
                "api",
                "projects/:fullpath/merge_requests/17",
                "--method",
                "GET"
            ]
        );
        assert_eq!(
            rich.as_std().get_current_dir(),
            Some(std::path::Path::new("/repo"))
        );

        let retarget = retarget_review_command(std::path::Path::new("/repo"), 17, "parent");
        assert_eq!(
            retarget
                .as_std()
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            ["mr", "update", "17", "--target-branch", "parent", "--yes"]
        );
        for cmd in [&rich, &retarget] {
            assert_eq!(
                cmd.as_std()
                    .get_envs()
                    .find(|(key, _)| *key == "GLAB_NO_PROMPT")
                    .unwrap()
                    .1,
                Some(std::ffi::OsStr::new("1"))
            );
        }
    }
}
