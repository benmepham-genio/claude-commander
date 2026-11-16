//! GitHub adapter. Existing command behavior remains in `git::github` during extraction.

use std::time::Duration;

use claude_commander_protocol::hosting::HostedRepository;

use crate::error::Result;

pub async fn list_repositories(timeout: Duration) -> Result<Vec<HostedRepository>> {
    crate::git::list_repos(timeout)
        .await
        .map(|repos| repos.into_iter().map(HostedRepository::from).collect())
}
