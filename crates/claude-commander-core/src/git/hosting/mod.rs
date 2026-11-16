//! Provider-neutral facade over hosted repository and code-review CLIs.

pub mod github;
pub mod gitlab;

use std::time::Duration;

use claude_commander_protocol::hosting::{CodeHost, CodeHostProvider, RepositoryListing};

use crate::error::Result;
use crate::git::{EnrichedPrInfo, PrCheckResult};
use chrono::{DateTime, Utc};
use std::path::Path;

/// List repositories for one provider snapshot.
pub async fn list_repositories(mut host: CodeHost, timeout: Duration) -> Result<RepositoryListing> {
    let repositories = match host.provider {
        CodeHostProvider::Github => {
            host.hostname = None;
            github::list_repositories(timeout).await?
        }
        CodeHostProvider::Gitlab => {
            gitlab::list_repositories(host.hostname.as_deref(), timeout).await?
        }
    };
    Ok(RepositoryListing { host, repositories })
}

pub async fn is_cli_available(provider: CodeHostProvider) -> bool {
    match provider {
        CodeHostProvider::Github => crate::git::is_gh_available().await,
        CodeHostProvider::Gitlab => gitlab::is_available().await,
    }
}

pub async fn check_review_for_branch(
    provider: CodeHostProvider,
    repo_path: &Path,
    branch: &str,
    branch_owned_since: DateTime<Utc>,
) -> PrCheckResult {
    match provider {
        CodeHostProvider::Github => {
            crate::git::check_pr_for_branch(repo_path, branch, branch_owned_since).await
        }
        CodeHostProvider::Gitlab => {
            gitlab::check_review_for_branch(repo_path, branch, branch_owned_since).await
        }
    }
}

pub async fn fetch_enriched_review(
    provider: CodeHostProvider,
    repo_path: &Path,
    number: u32,
) -> Option<EnrichedPrInfo> {
    match provider {
        CodeHostProvider::Github => crate::git::fetch_enriched_pr(repo_path, number).await,
        CodeHostProvider::Gitlab => gitlab::fetch_enriched_review(repo_path, number).await,
    }
}

pub async fn try_retarget_review_base(
    provider: CodeHostProvider,
    repo_path: &Path,
    number: u32,
    new_base: &str,
) -> std::result::Result<(), String> {
    match provider {
        CodeHostProvider::Github => {
            crate::git::try_retarget_pr_base(repo_path, number, new_base).await
        }
        CodeHostProvider::Gitlab => {
            gitlab::try_retarget_review_base(repo_path, number, new_base).await
        }
    }
}

pub fn stack_instruction(provider: CodeHostProvider, parent_branch: &str) -> String {
    match provider {
        CodeHostProvider::Github => format!(
            "This branch is stacked on `{parent_branch}` (not main).\n\
             When creating a pull request for this session, use:\n\
             gh pr create --base {parent_branch}"
        ),
        CodeHostProvider::Gitlab => format!(
            "This branch is stacked on `{parent_branch}` (not main).\n\
             When creating a merge request for this session, use:\n\
             glab mr create --target-branch {parent_branch}"
        ),
    }
}

/// Retargets are best-effort; bound both the CLI and any child processes.
pub(crate) async fn review_output(
    command: tokio::process::Command,
    program: &str,
) -> std::io::Result<std::process::Output> {
    match super::bounded::run_bounded(command, program, Duration::from_secs(30)).await? {
        super::bounded::Bounded::Finished {
            status,
            stdout,
            stderr,
        } => Ok(std::process::Output {
            status,
            stdout,
            stderr,
        }),
        super::bounded::Bounded::TimedOut => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "review retarget timed out",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stack_instructions_use_each_clis_exact_create_flag() {
        assert!(
            stack_instruction(CodeHostProvider::Github, "parent")
                .ends_with("gh pr create --base parent")
        );
        assert!(
            stack_instruction(CodeHostProvider::Gitlab, "parent")
                .ends_with("glab mr create --target-branch parent")
        );
    }
}
