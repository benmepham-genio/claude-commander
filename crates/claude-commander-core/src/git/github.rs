//! GitHub repo listing via the `gh` CLI, for the "add a project" repo picker.
//!
//! One call to `gh api --paginate` fetches every repo the user can reach —
//! their own, ones they collaborate on, and ones belonging to their orgs — and
//! projects each into the shared [`GithubRepo`] wire type.
//!
//! **This module deliberately does not swallow errors, unlike [`crate::git::pr`].**
//! That module maps every failure to `None`/`FetchFailed` because it runs on a
//! background poll where a transient hiccup must not disturb cached UI state.
//! Here the call is user-initiated and its whole output is the screen: a picker
//! that silently shows zero repos when `gh` is missing or unauthenticated is
//! indistinguishable from a user with no repos. So failures propagate, with
//! "gh is not installed" carved out as its own [`GitError::CodeHostCliUnavailable`]
//! because it is the one case the user can act on directly.
//!
//! **The run is bounded, and the bound kills the process group.** `--paginate`
//! over a large account is open-ended, and an unbounded run here was not merely
//! slow: a remote frontend's HTTP request gives up first and reports a transport
//! error, which reads as "the server is down" rather than "still listing"; and
//! abandoning that request does nothing about the `gh` on the far side, so each
//! press of the picker's retry button stacked another live `gh` on the server.
//! The budget is [`Config::repo_list_timeout_secs`](crate::config::Config), the
//! mechanism is [`crate::git::bounded`], and a timeout is its own
//! [`GitError::RepoListTimedOut`] rather than CLI unavailability — telling a user
//! to install a `gh` they already have is worse than saying nothing.

use crate::git::gh_command;
use std::time::Duration;

use claude_commander_protocol::github::GithubRepo;
use claude_commander_protocol::hosting::CodeHostProvider;
use tokio::process::Command;
use tracing::debug;

use crate::error::{GitError, Result};
use crate::git::bounded::{self, Bounded};
use crate::git::is_gh_available;

/// jq projection mapping a REST repo object onto [`GithubRepo`]'s fields.
///
/// Every field is top-level on the REST payload except `owner`, which is an
/// object — hence `.owner.login`. Verified against live output on 2026-08-09
/// (gh 2.96.0); repro with the endpoint below at `per_page=2` and compare the
/// keys to the struct.
const REPO_PROJECTION: &str = "\
.[] | {full_name, owner: .owner.login, name, description, private, fork, \
archived, default_branch, clone_url, ssh_url, pushed_at}";

/// The repos endpoint, sorted most-recently-pushed first for the picker.
///
/// The `affiliation` triple is the *documented default* for this endpoint
/// (GitHub REST docs, `GET /user/repos`: "affiliation — Default:
/// `owner,collaborator,organization_member`"). It is passed explicitly anyway,
/// so the set of repos this picker offers is legible at the call site rather
/// than inherited from a remote default that could change.
///
/// There is no page cap: capping would silently hide repos, which is a worse
/// failure than a slow first call.
const REPOS_ENDPOINT: &str = "/user/repos\
?affiliation=owner,collaborator,organization_member&per_page=100&sort=pushed";

/// List every GitHub repo the authenticated user can clone, within `timeout`.
///
/// Returns [`GitError::CodeHostCliUnavailable`] when `gh` is missing,
/// [`GitError::RepoListTimedOut`] when the listing overran `timeout` (and was
/// killed, along with anything it spawned); any other failure (not
/// authenticated, network, rate limit) surfaces as
/// [`GitError::OperationFailed`] carrying gh's own stderr, which is already
/// phrased for a human.
///
/// The timeout is a parameter rather than a config read, as in
/// [`run_clone`](crate::git::run_clone), so this stays callable from a test with
/// a 300ms budget; `CommanderService` passes `Config::repo_list_timeout_secs`.
///
/// The availability probe is [`is_gh_available`], the same one the PR poller
/// uses. `CommanderService` caches that answer in a `OnceCell` and callers
/// there should keep gating on the cached value; the check here is a backstop
/// for direct callers and costs one `gh --version` per picker open.
pub async fn list_repos(timeout: Duration) -> Result<Vec<GithubRepo>> {
    if !is_gh_available().await {
        return Err(GitError::CodeHostCliUnavailable {
            provider: CodeHostProvider::Github,
        }
        .into());
    }
    repos_from(gh_api_command(), timeout).await
}

/// The `gh api --paginate` invocation that produces the listing.
fn gh_api_command() -> Command {
    let mut cmd = gh_command();
    cmd.args(["api", "--paginate", "--jq", REPO_PROJECTION, REPOS_ENDPOINT]);
    cmd
}

/// Run a prepared listing command and project its stdout into repos.
///
/// Separated from [`list_repos`] so the run can be exercised with a stand-in
/// command: a test that called the real `gh` would hit the GitHub API on any
/// authenticated machine.
async fn repos_from(cmd: Command, timeout: Duration) -> Result<Vec<GithubRepo>> {
    let outcome = bounded::run_bounded(cmd, "gh api", timeout)
        .await
        .map_err(|e| {
            // gh passed `--version` moments ago, so a spawn failure here means
            // it vanished or became unexecutable mid-flight — still "no gh".
            debug!("gh api spawn failed: {e}");
            GitError::CodeHostCliUnavailable {
                provider: CodeHostProvider::Github,
            }
        })?;

    let Bounded::Finished {
        status,
        stdout,
        stderr,
    } = outcome
    else {
        // Emphatically *not* `GhUnavailable`: gh is present and probably fine,
        // it just had more to fetch than the budget allowed. See
        // `GitError::RepoListTimedOut`.
        return Err(GitError::RepoListTimedOut {
            provider: CodeHostProvider::Github,
            secs: timeout.as_secs(),
        }
        .into());
    };

    if !status.success() {
        let stderr = String::from_utf8_lossy(&stderr);
        return Err(GitError::OperationFailed(format!(
            "gh api {REPOS_ENDPOINT} failed: {}",
            stderr.trim()
        ))
        .into());
    }

    let stdout = String::from_utf8_lossy(&stdout);
    let repos = parse_repo_stream(&stdout)?;
    debug!("gh api listed {} repos", repos.len());
    Ok(repos)
}

/// Parse gh's `--paginate --jq` output into repos.
///
/// **The output is a stream of JSON objects, not a JSON array**, so
/// `serde_json::from_str::<Vec<_>>` fails on any real multi-page result. gh
/// applies the `--jq` filter to each page separately and concatenates what
/// comes out: `gh api --help` (gh 2.96.0) states "Each page is a separate JSON
/// array or object. Pass `--slurp` to wrap all pages of JSON arrays or objects
/// into an outer JSON array." — and `--slurp` is not in play here because it
/// wraps *pages*, not the projected objects. Repro: run
/// `gh api --paginate --jq '.[] | {name}' "/repos/cli/cli/labels?per_page=3"`
/// and observe 80-odd bare objects spanning ~28 pages, with no enclosing `[`.
///
/// `serde_json::Deserializer::into_iter` reads exactly that shape. Empty input
/// yields an empty list, not an error — a user with no repos is not a failure.
fn parse_repo_stream(output: &str) -> Result<Vec<GithubRepo>> {
    serde_json::Deserializer::from_str(output)
        .into_iter::<GithubRepo>()
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| GitError::OperationFailed(format!("failed to parse gh repo list: {e}")).into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::GitError;

    use std::time::{Duration, Instant};

    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    use tempfile::TempDir;

    /// A stand-in listing command that never finishes, and whose backgrounded
    /// grandchild outlives its parent unless the *group* is killed.
    ///
    /// Shaped after `clone.rs`'s
    /// `a_timed_out_run_kills_the_child_and_its_descendants`, and for the same
    /// reason: `gh api` spawns nothing here, but the real one does under a proxy
    /// or credential helper, and killing only the direct child leaves the
    /// descendant running.
    fn hanging_command(pidfile: &std::path::Path) -> Command {
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(format!("sleep 300 & echo $! > {}; wait", pidfile.display()));
        cmd
    }

    /// The defect this bound exists to close: an abandoned listing must not leave
    /// `gh` running, or a picker's retry button stacks one process per press.
    #[tokio::test]
    async fn a_timed_out_listing_kills_gh_and_its_descendants() {
        let tmp = TempDir::new().unwrap();
        let pidfile = tmp.path().join("grandchild.pid");

        let started = Instant::now();
        let err = repos_from(hanging_command(&pidfile), Duration::from_millis(300))
            .await
            .unwrap_err();
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_secs(10),
            "the listing waited {elapsed:?} on a 300ms budget"
        );
        assert!(
            matches!(
                err,
                crate::error::Error::Git(GitError::RepoListTimedOut { .. })
            ),
            "not reported as a repo-list timeout: {err}"
        );

        let grandchild: i32 = std::fs::read_to_string(&pidfile)
            .expect("sh never recorded its grandchild pid")
            .trim()
            .parse()
            .unwrap();

        // Poll: the kill is asynchronous, and the reparented `sleep` has to be
        // reaped by init before its pid stops resolving.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if kill(Pid::from_raw(grandchild), None).is_err() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "grandchild {grandchild} survived the timeout kill"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// A timeout is its own condition, not "gh is missing": the two get different
    /// wording in the picker, so collapsing them would tell a user to install a
    /// `gh` they already have.
    #[tokio::test]
    async fn a_timeout_is_not_reported_as_gh_being_unavailable() {
        let tmp = TempDir::new().unwrap();
        let err = repos_from(
            hanging_command(&tmp.path().join("pid")),
            Duration::from_millis(300),
        )
        .await
        .unwrap_err();
        assert!(
            !matches!(
                err,
                crate::error::Error::Git(GitError::CodeHostCliUnavailable { .. })
            ),
            "a timeout must not masquerade as GhUnavailable"
        );
        assert!(err.to_string().contains("timed out"), "{err}");
    }

    /// The bound must not cost the happy path: a command that finishes inside its
    /// budget still has its stdout captured and parsed.
    #[tokio::test]
    async fn a_listing_that_finishes_in_budget_parses_its_stdout() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(
            r#"printf '{"full_name":"me/a","owner":"me","name":"a","private":false,"fork":false,"archived":false,"default_branch":"main","clone_url":"c","ssh_url":"s","pushed_at":null}'"#,
        );
        let repos = repos_from(cmd, Duration::from_secs(30)).await.unwrap();
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0].full_name, "me/a");
    }

    /// A non-zero exit still carries the subprocess's own stderr, which is
    /// already phrased for a human — the bound must not swallow it.
    #[tokio::test]
    async fn a_failing_listing_surfaces_its_stderr() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("echo 'gh: not authenticated' >&2; exit 1");
        let err = repos_from(cmd, Duration::from_secs(30)).await.unwrap_err();
        assert!(err.to_string().contains("not authenticated"), "{err}");
    }

    #[test]
    fn parses_the_concatenated_per_page_object_stream() {
        // Two pages' worth of --jq output, concatenated exactly as gh emits it.
        // This is NOT a JSON array; `from_str::<Vec<_>>` would fail here.
        let out = r#"{"full_name":"me/a","owner":"me","name":"a","description":null,
"private":false,"fork":false,"archived":false,"default_branch":"main",
"clone_url":"https://github.com/me/a.git","ssh_url":"git@github.com:me/a.git",
"pushed_at":"2026-01-02T03:04:05Z"}
{"full_name":"org/b","owner":"org","name":"b","description":"x",
"private":true,"fork":false,"archived":false,"default_branch":"trunk",
"clone_url":"https://github.com/org/b.git","ssh_url":"git@github.com:org/b.git",
"pushed_at":null}"#;
        let repos = parse_repo_stream(out).unwrap();
        assert_eq!(repos.len(), 2);
        assert_eq!(repos[0].full_name, "me/a");
        assert!(repos[1].private);
        assert_eq!(repos[1].pushed_at, None); // empty repo
    }

    #[test]
    fn empty_output_is_an_empty_list_not_an_error() {
        assert!(parse_repo_stream("").unwrap().is_empty());
    }

    #[test]
    fn a_json_array_would_not_have_parsed_as_a_stream_of_repos() {
        // Pins the framing the module is built around: if gh ever started
        // emitting one array, this parser would reject it loudly rather than
        // returning an empty list, and this test is where that shows up.
        let arr = r#"[{"full_name":"me/a","owner":"me","name":"a","private":false,
"fork":false,"archived":false,"default_branch":"main",
"clone_url":"c","ssh_url":"s","pushed_at":null}]"#;
        assert!(parse_repo_stream(arr).is_err());
    }

    #[test]
    fn truncated_output_is_an_error_not_a_short_list() {
        // A killed gh (or a mid-stream network failure) must not look like
        // "these are all your repos".
        let truncated = r#"{"full_name":"me/a","owner":"me","name":"a","private":false,
"fork":false,"archived":false,"default_branch":"main","clone_url":"c""#;
        assert!(parse_repo_stream(truncated).is_err());
    }

    #[test]
    fn gh_unavailable_error_displays() {
        assert!(
            !GitError::CodeHostCliUnavailable {
                provider: CodeHostProvider::Github
            }
            .to_string()
            .is_empty()
        );
    }
}
