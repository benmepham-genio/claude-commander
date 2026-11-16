//! Wall-clock-bounded subprocess runs whose timeout reaches descendants.
//!
//! Two call sites need the same guarantee for the same reason, so the mechanism
//! is here rather than duplicated: [`clone`](super::clone) runs an unattended
//! `git`/`gh repo clone`, and [`github`](super::github) runs `gh api --paginate`
//! for the repo picker. Both spawn *trees* — `gh` shells out to `git`, which
//! shells out to `ssh` or `git-remote-https` — and in both cases abandoning the
//! wait without killing the tree leaves live processes behind. On the picker that
//! is a compounding leak: its retry button starts a fresh `gh` each press, and an
//! earlier request the caller stopped waiting on keeps paginating regardless.
//!
//! What this module does *not* do is decide what a timeout means. It reports
//! [`Bounded::TimedOut`] and lets the caller name it — a timed-out clone and a
//! timed-out listing carry different budgets, different remedies and different
//! error variants, and flattening them would cost a frontend the distinction.

use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

/// What became of a bounded run.
pub(crate) enum Bounded {
    /// The child exited on its own, within budget. Both output streams were
    /// drained to EOF.
    Finished {
        status: ExitStatus,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    },
    /// The child overran and its process group was killed. There is no output to
    /// report: whatever it had written is a truncated prefix, and treating a
    /// prefix as a result is precisely the failure mode a caller must not have
    /// (`github::parse_repo_stream` rejects a truncated stream for the same
    /// reason — a short list must never read as "these are all your repos").
    TimedOut,
}

/// Run `cmd` to completion within `timeout`, killing it and everything it
/// spawned if it overruns.
///
/// Both output streams are captured; `stdin` is `/dev/null`, so a child that
/// tries to read from it sees EOF rather than blocking on a terminal that may
/// not exist.
///
/// The `Err` case is *only* a failure to spawn, which is why it is an
/// [`std::io::Error`] rather than one of our own: the two callers disagree about
/// what an unspawnable program means (a missing hosting CLI is
/// [`CodeHostCliUnavailable`](crate::error::GitError::CodeHostCliUnavailable), an unspawnable
/// `git` is an operation failure), and that judgement is theirs to make.
pub(crate) async fn run_bounded(
    mut cmd: Command,
    program: &str,
    timeout: Duration,
) -> std::io::Result<Bounded> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // A new process group, so the timeout can take the whole tree down:
        // `gh` spawns `git`, which spawns `ssh` or `git-remote-https`. Killing
        // only the process we spawned would leave a live descendant — writing
        // into a clone destination the failure path is about to delete, or
        // continuing to paginate against the GitHub API. `process_group(0)` is
        // setpgid(0, 0), so the child's pgid equals its own pid.
        //
        // Pinned by `github`'s `a_timed_out_listing_kills_gh_and_its_descendants`
        // and `clone`'s `a_timed_out_run_kills_the_child_and_its_descendants`,
        // which both background a grandchild and assert its pid stops resolving.
        .process_group(0);

    let mut child = cmd.spawn()?;
    // Read once, before the wait: `Child::id` returns `None` after the child has
    // been reaped.
    let pgid = child.id().map(|id| Pid::from_raw(id as i32));

    // Drain both pipes concurrently, *before* waiting. Waiting on a child while
    // one of its output pipes fills would deadlock the pair: the child blocks
    // writing, we block waiting. Pinned by
    // `output_larger_than_a_pipe_buffer_does_not_deadlock`.
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());

    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(status) => status?,
        Err(_) => {
            warn!("{program} exceeded {}s; killing", timeout.as_secs());
            kill_tree(&mut child, pgid).await;
            // The drain tasks are left detached rather than awaited: they finish
            // on their own once the killed group's write ends close, and there is
            // nothing here that wants their partial output.
            return Ok(Bounded::TimedOut);
        }
    };

    Ok(Bounded::Finished {
        status,
        // The child has exited, so these awaits normally resolve at once. A
        // panicked drain task degrades to empty output rather than losing the
        // exit status.
        //
        // **Assumption, not a verified claim:** a pipe reaches EOF only when
        // *every* copy of its write end is closed, so a descendant that
        // inherited stdout and outlived its parent would stall these awaits past
        // the budget. Neither `gh api` nor `git clone` is known to leave such a
        // process behind, and this is the same assumption `kill_tree` already
        // rests on — but it is untested, and it is the reason to be suspicious
        // here first if a bounded run ever hangs *after* its child exits.
        stdout: stdout.await.unwrap_or_default(),
        stderr: stderr.await.unwrap_or_default(),
    })
}

/// Read a captured pipe to EOF on its own task.
fn drain<P: AsyncRead + Unpin + Send + 'static>(pipe: Option<P>) -> JoinHandle<Vec<u8>> {
    tokio::spawn(async move {
        let mut buf = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut buf).await;
        }
        buf
    })
}

/// SIGKILL the child's process group, then reap the child itself.
///
/// The group kill covers every descendant that did not put itself into a group
/// of its own; neither `git`, `gh` nor `ssh` is known to do so, but that is an
/// assumption, not a verified claim. `Child::kill` is still called afterwards:
/// it waits, which is what actually reaps the child rather than leaving a
/// zombie.
async fn kill_tree(child: &mut Child, pgid: Option<Pid>) {
    if let Some(pgid) = pgid
        && let Err(e) = killpg(pgid, Signal::SIGKILL)
    {
        // ESRCH just means it exited between the timeout firing and this call.
        debug!("killpg({pgid}) failed: {e}");
    }
    if let Err(e) = child.kill().await {
        debug!("failed to reap timed-out child: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hazard introduced by capturing stdout instead of discarding it: a
    /// child that writes more than a pipe buffer holds (64KiB on Linux by
    /// default, `pipe(7)`) blocks until someone reads, so a `wait()` that ran
    /// before the drain would hang until the timeout fired. This asserts the
    /// full output arrives well inside a generous budget.
    #[tokio::test]
    async fn output_larger_than_a_pipe_buffer_does_not_deadlock() {
        let mut cmd = Command::new("sh");
        // ~450_000 bytes down each stream, written concurrently so *both* pipes
        // fill: 50_000 lines of 9 bytes, comfortably past 64KiB either way. Plain
        // POSIX sh redirection rather than `tee /dev/stderr`, which would lean on
        // a `/dev/stderr` this test has no receipt for on every platform.
        cmd.arg("-c")
            .arg("{ yes abcdefgh | head -50000 & yes abcdefgh | head -50000 >&2; wait; }");

        let bounded = run_bounded(cmd, "sh", Duration::from_secs(30))
            .await
            .unwrap();
        let Bounded::Finished {
            status,
            stdout,
            stderr,
        } = bounded
        else {
            panic!("a fast command was reported as timed out");
        };
        assert!(status.success());
        assert!(
            stdout.len() > 400_000,
            "stdout truncated to {} bytes",
            stdout.len()
        );
        assert!(
            stderr.len() > 400_000,
            "stderr truncated to {} bytes",
            stderr.len()
        );
    }

    /// A spawn failure is an `Err`, distinct from a run that produced a bad exit
    /// status — the callers map the two to different error variants.
    #[tokio::test]
    async fn an_unspawnable_program_is_an_io_error_not_a_status() {
        let cmd = Command::new("claude-commander-no-such-program");
        assert!(
            run_bounded(cmd, "nope", Duration::from_secs(5))
                .await
                .is_err(),
            "a missing program should fail to spawn"
        );
    }
}
