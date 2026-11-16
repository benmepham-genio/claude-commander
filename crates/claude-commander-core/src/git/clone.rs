//! Non-interactive, timeout-bounded repository clone.
//!
//! Hosted GitHub and GitLab sources use their respective authenticated CLIs;
//! an explicit URL uses plain `git clone`.
//!
//! The whole module exists to make one failure mode impossible. This clone runs
//! **unattended**, on a machine that may have no terminal at all, and the job
//! watching it (`CloneJobs`) has no way to answer a question. A `git` that stops
//! to ask for a password, or an `ssh` that stops to ask whether an unknown host
//! key is acceptable, does not fail — it waits, forever, with the job stuck in
//! `Running`. So every prompting route is closed up front, and the run is
//! additionally bounded by a wall-clock timeout with a real kill behind it.
//!
//! Nearly every statement above is a claim about code outside this repo. The
//! receipts are inline, and the ones that could be turned into tests were:
//! `spawn::the_noninteractive_recipe_stops_git_prompting` pins the env recipe
//! against a real `git`, and `a_timed_out_run_kills_the_child_and_its_descendants` pins
//! the kill.
//!
//! # Known limitation: `core.sshCommand` is not honoured
//!
//! Setting `GIT_SSH_COMMAND` overrides git's `core.sshCommand` config
//! (`git-config(1)`, `core.sshCommand`: "is overridden when the environment
//! variable is set"). So a user who configures their ssh identity there rather
//! than in `~/.ssh/config` will see an ssh clone fail to authenticate here while
//! the same clone works in their shell. **That is the expected behaviour, not a
//! bug to re-diagnose.**
//!
//! It is accepted rather than fixed because the affected population is narrower
//! than it first looks: only *global* and *system* config can apply to a clone,
//! since the repository being cloned does not exist yet and has no local config
//! to read. Preserving the value would cost a `git config --get core.sshCommand`
//! subprocess on every clone, to buy back a case where the alternative is a fast,
//! legible auth error rather than the indefinite hang this module exists to
//! prevent. An inherited `GIT_SSH_COMMAND` *is* preserved — see
//! [`super::spawn::ssh_command`]. The recipe itself lives in `spawn.rs` now,
//! because it turned out every git the TUI spawns needs it, not only a clone.

use std::path::Path;
use std::time::Duration;

use claude_commander_protocol::github::{
    redact_credentials, validate_clone_url, validate_repo_slug,
};
use claude_commander_protocol::hosting::{
    CloneSource, CodeHostProvider, validate_gitlab_hostname, validate_hosted_repo_slug,
};
use tokio::process::Command;

use super::spawn::{gh_command, git_command};
use tracing::{debug, warn};

use crate::error::{GitError, Result};
use crate::git::bounded::{self, Bounded};
use crate::git::is_gh_available;

/// Clone `source` into `dest`, non-interactively, within `timeout`.
///
/// `dest` must be absolute and is passed to git verbatim; git creates it (and
/// any missing parents). On failure a destination *this call created* is
/// removed, so a retry is not blocked by a half-written checkout; a destination
/// that already existed is left untouched.
///
/// The timeout is a parameter rather than a config read so this stays callable
/// from a test with a 300ms budget; `CommanderService` passes
/// `Config::clone_timeout_secs`.
pub async fn run_clone(source: &CloneSource, dest: &Path, timeout: Duration) -> Result<()> {
    // Re-validate at the last stop before argv. Callers validate too, but the
    // hazard being closed here (a source read as a flag) is invisible when it
    // goes wrong, and the check is pure string inspection.
    let program = match source {
        CloneSource::Github { full_name } => {
            validate_repo_slug(full_name).map_err(clone_source_rejected)?;
            if !is_gh_available().await {
                return Err(GitError::CodeHostCliUnavailable {
                    provider: CodeHostProvider::Github,
                }
                .into());
            }
            "gh repo clone"
        }
        CloneSource::Gitlab {
            full_name,
            hostname,
        } => {
            validate_hosted_repo_slug(CodeHostProvider::Gitlab, full_name)
                .map_err(clone_source_rejected)?;
            if let Some(hostname) = hostname {
                validate_gitlab_hostname(hostname).map_err(clone_source_rejected)?;
            }
            if !crate::git::hosting::gitlab::is_available().await {
                return Err(GitError::CodeHostCliUnavailable {
                    provider: CodeHostProvider::Gitlab,
                }
                .into());
            }
            "glab repo clone"
        }
        CloneSource::Url { url } => {
            validate_clone_url(url).map_err(clone_source_rejected)?;
            "git clone"
        }
    };

    // The destination, never the source: a hand-typed URL may carry
    // `user:token@` userinfo, which has no business in a log file.
    debug!(
        "{program} -> {} (timeout {}s)",
        dest.display(),
        timeout.as_secs()
    );

    let pre_existing = dest.exists();
    let result = run_bounded(clone_command(source, dest), program, timeout).await;

    if result.is_err() && !pre_existing {
        // git removes a destination it created when it fails on its own
        // (verified with git 2.53.0: `git clone -- /nonexistent /tmp/x` leaves
        // no `/tmp/x`), but it cannot do so when we SIGKILL it — SIGKILL is
        // uncatchable, so no cleanup path of its own can run. Hence this.
        if let Err(e) = std::fs::remove_dir_all(dest) {
            if e.kind() != std::io::ErrorKind::NotFound {
                warn!("failed to remove partial clone at {}: {e}", dest.display());
            }
        } else {
            debug!("removed partial clone at {}", dest.display());
        }
    }

    result
}

/// Turn a rejected source into an error that cannot quote a secret back.
///
/// `pub(crate)` because `CommanderService::start_clone` validates the same
/// strings one layer up and owes the same guarantee; a second copy of a
/// *security* helper is the last thing this feature needs.
///
/// Both validators route through here, and the `Url` arm does so even though its
/// [`CloneRejection`](claude_commander_protocol::github::CloneRejection) variants
/// look harmless today. That is the point: this module redacts where an error
/// string is *built*, rather than trusting each variant to stay harmless. The
/// `Github` arm proves why — `validate_repo_slug` splits on `/`, so a URL pasted
/// into the slug field always fails as `MalformedSlug`, whose `Display` echoes
/// the entire raw input. A credentialed URL went straight into an error message
/// through that path, and the reasoning that missed it was "these variants only
/// echo a scheme or a directory name", which was true of every variant but one.
///
/// The variant is [`GitError::CloneSourceRejected`] rather than `OperationFailed`
/// because nothing has *failed* here: the request is unusable, which a caller
/// mapping onto a transport has to answer differently (the server turns this into
/// a 400 and every other `GitError` into a 500). Folded into `OperationFailed` it
/// would be indistinguishable from git itself falling over.
///
/// Pinned by `a_rejection_message_carries_no_credentials`.
pub(crate) fn clone_source_rejected(rejection: impl std::fmt::Display) -> GitError {
    GitError::CloneSourceRejected(redact_credentials(&rejection.to_string()))
}

/// Build the clone invocation for `source`, with the non-interactive env applied.
///
/// Git and GitHub use `--` as an option terminator before the source. GitLab
/// deliberately does not: glab 1.113.0 documents `--` as introducing trailing
/// `git clone` flags after the repository and destination positionals.
///
/// * `git clone -- <source> <dest>` — standard git option terminator.
/// * `gh repo clone -- <slug> <dest>` — gh's help renders this as
///   `gh repo clone <repository> [<directory>] [-- <gitflags>...]`, which reads
///   as if `--` introduced git flags. It does not: gh takes the first positional
///   as the repository either way. Verified with gh 2.96.0 —
///   `gh repo clone -- -version /tmp/x` reports `Could not resolve to a
///   Repository with the name 'sizeak/-version'`, while dropping the `--` gives
///   `unknown shorthand flag: 'v' in -version`. Pinned by
///   `provider_clone_commands_keep_their_exact_argument_shapes` for the argv shape.
fn clone_command(source: &CloneSource, dest: &Path) -> Command {
    match source {
        CloneSource::Github { full_name } => {
            let mut cmd = gh_command();
            cmd.args(["repo", "clone", "--"]).arg(full_name).arg(dest);
            cmd
        }
        CloneSource::Gitlab {
            full_name,
            hostname,
        } => {
            let mut cmd = crate::git::spawn::detached("glab");
            cmd.args(["repo", "clone"]).arg(full_name).arg(dest);
            cmd.env("GLAB_NO_PROMPT", "1");
            if let Some(hostname) = hostname {
                cmd.env("GITLAB_HOST", hostname);
            }
            cmd
        }
        CloneSource::Url { url } => {
            let mut cmd = git_command();
            cmd.args(["clone", "--"]).arg(url).arg(dest);
            cmd
        }
    }
}

/// Run `cmd` to completion within `timeout`, killing it and everything it
/// spawned if it overruns.
///
/// `program` names the command for the error message ("git clone", …). The
/// timeout, the process-group kill and the output capture all live in
/// [`super::bounded`], shared with the repo listing, which needs the same
/// guarantee for the same reason; what stays here is *clone's* reading of the
/// outcome — a `CloneTimedOut` rather than a `RepoListTimedOut`, and stderr
/// redacted before it becomes an error string.
async fn run_bounded(cmd: Command, program: &str, timeout: Duration) -> Result<()> {
    let outcome = bounded::run_bounded(cmd, program, timeout)
        .await
        .map_err(|e| GitError::OperationFailed(format!("failed to run {program}: {e}")))?;

    let Bounded::Finished { status, stderr, .. } = outcome else {
        return Err(GitError::CloneTimedOut {
            secs: timeout.as_secs(),
        }
        .into());
    };

    if !status.success() {
        // git suppresses its progress meter when stderr is not a terminal
        // ("Progress status is reported on the standard error stream by default
        // when it is attached to a terminal" — `git help clone`, `--progress`),
        // so what is left here is the actual diagnosis, short enough to pass on
        // whole.
        //
        // Redacted as the error is *built*, not merely kept out of the log: git
        // echoes the source in most failure messages, and a hand-typed URL may
        // carry `user:token@` userinfo. From here the string becomes a
        // `CloneStatus::Failed` message, crosses the wire and is rendered in a
        // UI, so the secret has to be gone at the one point all of those share.
        let stderr = String::from_utf8_lossy(&stderr);
        return Err(GitError::OperationFailed(format!(
            "{program} failed: {}",
            redact_credentials(stderr.trim())
        ))
        .into());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::fixture::fixture_git_std;

    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use claude_commander_protocol::hosting::CloneSource;
    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    use tempfile::TempDir;
    use tokio::process::Command;

    /// Run a git command to completion, asserting it succeeded.
    fn git(dir: &Path, args: &[&str]) {
        let out = fixture_git_std()
            .current_dir(dir)
            .args(args)
            .output()
            .expect("git invocation failed to spawn");
        assert!(out.status.success(), "git {args:?} failed: {out:?}");
    }

    /// Seed a bare repo with one commit on `main`, network-free.
    ///
    /// Modelled on `git::auto_pull`'s `setup_origin_and_local`, and like it the
    /// returned path is a **plain local path**, not a `file://` URL — which is
    /// what a user pasting a path into the clone box would give us.
    fn seed_bare_repo(tmp: &TempDir) -> PathBuf {
        let remote = tmp.path().join("remote.git");
        let seed = tmp.path().join("seed");
        git(tmp.path(), &["init", "--bare", "-b", "main", "remote.git"]);
        git(tmp.path(), &["init", "-b", "main", "seed"]);
        git(&seed, &["config", "user.email", "t@t"]);
        git(&seed, &["config", "user.name", "t"]);
        std::fs::write(seed.join("README"), "v1\n").unwrap();
        git(&seed, &["add", "README"]);
        git(&seed, &["commit", "-m", "initial"]);
        git(
            &seed,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git(&seed, &["push", "origin", "main"]);
        remote
    }

    #[tokio::test]
    async fn clones_from_a_local_bare_repo_without_network() {
        let tmp = TempDir::new().unwrap();
        let remote = seed_bare_repo(&tmp);
        let dest = tmp.path().join("out");

        run_clone(
            &CloneSource::Url {
                url: remote.to_string_lossy().into_owned(),
            },
            &dest,
            Duration::from_secs(60),
        )
        .await
        .unwrap();

        assert!(dest.join(".git").is_dir());
        assert_eq!(
            std::fs::read_to_string(dest.join("README")).unwrap(),
            "v1\n"
        );
    }

    #[tokio::test]
    async fn a_bad_source_fails_rather_than_hanging() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("out");

        let err = run_clone(
            &CloneSource::Url {
                url: tmp
                    .path()
                    .join("does-not-exist")
                    .to_string_lossy()
                    .into_owned(),
            },
            &dest,
            Duration::from_secs(60),
        )
        .await
        .unwrap_err();

        // git's own message, which is already phrased for a human.
        assert!(
            err.to_string().contains("does not exist"),
            "unhelpful error: {err}"
        );
        assert!(!dest.exists(), "a failed clone left a destination behind");
    }

    /// The cleanup removes only what this function created. A destination that
    /// was already there — someone else's checkout — must survive a failure,
    /// because `run_clone` cannot know it is disposable.
    #[tokio::test]
    async fn a_failed_clone_leaves_a_pre_existing_destination_alone() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("occupied");
        std::fs::create_dir(&dest).unwrap();
        std::fs::write(dest.join("precious"), "mine\n").unwrap();

        let err = run_clone(
            &CloneSource::Url {
                url: tmp
                    .path()
                    .join("does-not-exist")
                    .to_string_lossy()
                    .into_owned(),
            },
            &dest,
            Duration::from_secs(60),
        )
        .await
        .unwrap_err();

        assert!(!err.to_string().is_empty());
        assert_eq!(
            std::fs::read_to_string(dest.join("precious")).unwrap(),
            "mine\n",
            "cleanup deleted a destination it did not create"
        );
    }

    /// The timeout must actually *kill*, and kill the whole tree: `gh repo
    /// clone` spawns `git clone`, which spawns `ssh`/`git-remote-https`. Killing
    /// only the direct child would leave a live git writing into the very
    /// directory the failure path then tries to remove.
    ///
    /// Driven through the `run_bounded` seam with `sh` rather than `git`,
    /// because a clone that finished fast would prove nothing about either
    /// property.
    #[tokio::test]
    async fn a_timed_out_run_kills_the_child_and_its_descendants() {
        let tmp = TempDir::new().unwrap();
        let pidfile = tmp.path().join("grandchild.pid");

        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(format!("sleep 300 & echo $! > {}; wait", pidfile.display()));

        let started = Instant::now();
        let err = run_bounded(cmd, "sh", Duration::from_millis(300))
            .await
            .unwrap_err();
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_secs(10),
            "run_bounded waited {elapsed:?} on a 300ms timeout"
        );
        assert!(
            err.to_string().contains("timed out"),
            "not reported as a timeout: {err}"
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

    /// A failing git echoes the source back, and a hand-typed source may carry
    /// `user:token@`. That string becomes `CloneStatus::Failed { message }`,
    /// crosses the wire and is rendered in a UI, so the secret must not survive
    /// contact with the error type — "remember not to log this" does not
    /// survive four more tasks.
    ///
    /// Driven through `run_bounded` with a stub, because a genuine credentialed
    /// clone would need the network. The stub's wording is *modelled on* git's
    /// HTTPS auth failure from memory, not captured from a run — what this test
    /// pins is the redaction, not git's phrasing. Only the shape it depends on
    /// is verified: git echoes the source URL in failure messages (git 2.53.0,
    /// `git clone -- /nonexistent /tmp/x` → `fatal: repository
    /// '/nonexistent' does not exist`).
    #[tokio::test]
    async fn a_failure_message_carries_no_credentials() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(
            "echo \"fatal: Authentication failed for \
             'https://sizeak:ghp_s3cr3tt0ken@github.com/o/r.git/'\" >&2; exit 128",
        );

        let err = run_bounded(cmd, "git clone", Duration::from_secs(60))
            .await
            .unwrap_err()
            .to_string();

        assert!(!err.contains("ghp_s3cr3tt0ken"), "token leaked: {err}");
        assert!(err.contains("https://***@github.com/o/r.git/"), "{err}");
        // Still a usable diagnosis.
        assert!(err.contains("Authentication failed"), "{err}");
    }

    /// The `--` is load-bearing on both arms and lives right before the source.
    #[test]
    fn provider_clone_commands_keep_their_exact_argument_shapes() {
        let dest = Path::new("/projects/repo");

        let gh = clone_command(
            &CloneSource::Github {
                full_name: "o/r".to_string(),
            },
            dest,
        );
        let args: Vec<_> = gh.as_std().get_args().collect();
        assert_eq!(gh.as_std().get_program(), "gh");
        assert_eq!(args, ["repo", "clone", "--", "o/r", "/projects/repo"]);

        let glab = clone_command(
            &CloneSource::Gitlab {
                full_name: "group/sub/repo".to_string(),
                hostname: Some("gitlab.example.com".to_string()),
            },
            dest,
        );
        let args: Vec<_> = glab.as_std().get_args().collect();
        assert_eq!(glab.as_std().get_program(), "glab");
        assert_eq!(args, ["repo", "clone", "group/sub/repo", "/projects/repo"]);
        assert_eq!(
            glab.as_std()
                .get_envs()
                .find(|(key, _)| *key == "GITLAB_HOST")
                .unwrap()
                .1,
            Some(std::ffi::OsStr::new("gitlab.example.com"))
        );
        for (key, expected) in [
            ("GLAB_NO_PROMPT", "1"),
            ("GIT_TERMINAL_PROMPT", "0"),
            ("GIT_ASKPASS", ""),
        ] {
            assert_eq!(
                glab.as_std()
                    .get_envs()
                    .find(|(name, _)| *name == key)
                    .unwrap()
                    .1,
                Some(std::ffi::OsStr::new(expected))
            );
        }
        assert!(
            glab.as_std()
                .get_envs()
                .find(|(name, _)| *name == "GIT_SSH_COMMAND")
                .unwrap()
                .1
                .unwrap()
                .to_string_lossy()
                .contains("BatchMode=yes")
        );

        let git = clone_command(
            &CloneSource::Url {
                url: "https://example.com/o/r.git".to_string(),
            },
            dest,
        );
        let args: Vec<_> = git.as_std().get_args().collect();
        assert_eq!(git.as_std().get_program(), "git");
        assert_eq!(
            args,
            [
                "clone",
                "--",
                "https://example.com/o/r.git",
                "/projects/repo"
            ]
        );
    }

    /// The validators run again here, on the last stop before argv. A caller
    /// that forgot to validate must not be able to hand git a flag.
    #[tokio::test]
    async fn a_flag_shaped_source_is_refused_before_any_process_starts() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("out");

        for source in [
            CloneSource::Url {
                url: "--upload-pack=evil".to_string(),
            },
            CloneSource::Github {
                full_name: "-flag/name".to_string(),
            },
            CloneSource::Gitlab {
                full_name: "group/-flag".to_string(),
                hostname: None,
            },
        ] {
            let err = run_clone(&source, &dest, Duration::from_secs(60))
                .await
                .unwrap_err();
            assert!(!err.to_string().is_empty(), "source: {source:?}");
            assert!(!dest.exists());
        }
    }

    /// Rejecting a source must not quote the secret back.
    ///
    /// The `Github` arm is the sharp one: `validate_repo_slug` splits on `/`, so
    /// a URL pasted into the slug field always fails with
    /// `CloneRejection::MalformedSlug`, whose `Display` echoes the **whole raw
    /// input** — the entire credentialed URL. That rejection message is a `400`
    /// body and a `CloneStatus::Failed` message, so the same four-hop journey
    /// applies to it as to git's stderr.
    #[tokio::test]
    async fn a_rejection_message_carries_no_credentials() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("out");
        const SECRET: &str = "ghp_s3cr3tt0ken";

        // `echoes_source` records whether the rejection quotes the input back at
        // all. Only `MalformedSlug` does — no `validate_clone_url` rejection
        // carries more than a scheme or a directory name, which is why the `Url`
        // arm is here as a regression guard rather than as a reproduction.
        for (source, echoes_source) in [
            (
                // A URL pasted where a slug belongs — the leak path.
                CloneSource::Github {
                    full_name: format!("https://sizeak:{SECRET}@github.com/o/r"),
                },
                true,
            ),
            (
                CloneSource::Url {
                    url: format!("ftp://sizeak:{SECRET}@example.com/o/r"),
                },
                false,
            ),
        ] {
            let err = run_clone(&source, &dest, Duration::from_secs(60))
                .await
                .unwrap_err()
                .to_string();

            assert!(!err.contains(SECRET), "token leaked: {err}");
            assert!(!err.contains("sizeak:"), "userinfo leaked: {err}");
            if echoes_source {
                // Redacted, not swallowed: the user still has to be able to see
                // which source was refused.
                assert!(err.contains("***@github.com/o/r"), "unusable: {err}");
            }
            assert!(!dest.exists());
        }
    }
}
