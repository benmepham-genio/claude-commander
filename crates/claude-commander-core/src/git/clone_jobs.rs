//! In-memory registry of running and recently-finished clones.
//!
//! A clone takes minutes, so the request that starts one cannot wait for it: it
//! returns a [`CloneJobId`] and the frontend polls. This is the record those
//! polls read — the *only* thing that knows a clone is happening, which is why
//! every way a job can stop has to land here, including the ones nobody plans
//! for (see [`CloneJobs::spawn`] on panics).
//!
//! **In-memory only, deliberately.** A clone does not survive a server restart:
//! the subprocess is gone with the process that spawned it, so a persisted job
//! could only ever be a `Running` record that will never move again. A frontend
//! that loses its job to a restart retries, and the destination pre-flight in
//! `CommanderService::start_clone` tells it what actually happened on disk.
//!
//! The registry is deliberately ignorant of *how* a clone runs — [`spawn`] takes
//! a future and records what it resolves to. `gh`, `git`, timeouts and project
//! registration are [`crate::git::run_clone`]'s and the service's business. That
//! separation is what makes this testable without a subprocess.
//!
//! [`spawn`]: CloneJobs::spawn

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use claude_commander_protocol::github::{CloneJob, CloneJobId, CloneStatus, redact_credentials};
use claude_commander_protocol::session::ProjectId;
use tokio::sync::RwLock;
use tokio::task::JoinError;
use tracing::{debug, warn};

/// How long a finished job stays readable before [`CloneJobs::prune`] drops it.
///
/// Long enough that a frontend which was backgrounded, or a user who wandered
/// off mid-clone, still finds out how it went; short enough that a long-lived
/// server does not accumulate a job per clone for ever. A `Running` job is never
/// pruned at any age — it is bounded instead by the clone timeout the service
/// passes to [`run_clone`](crate::git::run_clone).
const TERMINAL_JOB_TTL: Duration = Duration::from_secs(10 * 60);

/// A clone that has *finished*, in one of the three ways it can.
///
/// A wrapper around [`CloneStatus`] with three constructors, rather than a
/// parallel enum: the shapes are already spelled out on the wire type, and a
/// second copy would need a mapping function that can drift from it. What the
/// wrapper buys is that [`CloneJobs::spawn`]'s future cannot resolve to
/// `Running`, the one value that would mean nothing — the same
/// unrepresentable-if-wrong reasoning as `protocol::paste::ImageFormat`.
///
/// [`CloneOutcome::failed`] redacts, so a message built from a subprocess's
/// stderr cannot carry a credential into the registry even if a future caller
/// forgets. `redact_credentials` is idempotent, so this second pass over a
/// message `git::clone` already redacted changes nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloneOutcome(CloneStatus);

impl CloneOutcome {
    /// The clone finished and the checkout was registered as a project.
    pub fn succeeded(project_id: ProjectId) -> Self {
        Self(CloneStatus::Succeeded { project_id })
    }

    /// The clone failed. `message` is redacted here, at the point it is built.
    pub fn failed(message: impl AsRef<str>) -> Self {
        Self(CloneStatus::Failed {
            message: redact_credentials(message.as_ref()),
        })
    }

    /// Nothing was cloned because the destination was already occupied.
    pub fn destination_exists(dest: impl Into<PathBuf>, is_git_repo: bool) -> Self {
        Self(CloneStatus::DestinationExists {
            dest: dest.into(),
            is_git_repo,
        })
    }

    /// The terminal status to record. Private: the wrapper's whole value is that
    /// only these three constructors can produce one.
    fn into_status(self) -> CloneStatus {
        self.0
    }
}

/// One registry slot: the job as a frontend sees it, plus when it finished.
struct Entry {
    job: CloneJob,
    /// When the job reached a terminal status; `None` while it is `Running`.
    /// This is what [`prune_stale`] ages out, and why a running job cannot be
    /// pruned by construction rather than by a check that could be forgotten.
    finished_at: Option<Instant>,
}

/// The clone jobs a server knows about.
///
/// A named type with methods rather than an `Arc<RwLock<HashMap<…>>>` threaded
/// through signatures: the locking, the destination dedupe and the pruning are
/// invariants of *this* map, and a caller holding the raw handle would have to
/// re-implement all three correctly at every call site.
///
/// Cheap to clone — every clone shares one map, which is what lets a spawned job
/// task hold the registry it will write its result back into.
#[derive(Clone, Default)]
pub struct CloneJobs {
    jobs: Arc<RwLock<HashMap<CloneJobId, Entry>>>,
    notify: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl CloneJobs {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) fn with_notifier(notify: Arc<dyn Fn() + Send + Sync>) -> Self {
        Self {
            notify: Some(notify),
            ..Self::new()
        }
    }

    /// Register a clone and drive `run` to completion in the background.
    ///
    /// Returns the new job's id — or, if a job that is **still running** is
    /// already cloning into `dest`, that job's id, leaving `run` unpolled. Two
    /// clones writing into one directory would race over the same files, and the
    /// loser's cleanup would delete the winner's checkout, so the dedupe is a
    /// correctness rule and not a nicety. It is keyed on `dest` rather than the
    /// source precisely because two different URLs pointed at one directory is
    /// the dangerous case. A *finished* job never blocks a new one: a clone that
    /// failed has to be retryable into the same place.
    ///
    /// Destinations compare as plain paths, with no canonicalisation. Every
    /// caller builds `dest` the same way (`projects_dir().join(name)`), and the
    /// directory does not exist yet at this point, which is exactly when
    /// `std::fs::canonicalize` fails.
    ///
    /// `source_label` is passed through
    /// [`redact_credentials`](claude_commander_protocol::github::redact_credentials)
    /// because it is built from `CloneRequest.source` — raw user input, which for
    /// a pasted `https://user:token@host/…` carries a secret into a value that is
    /// polled, rendered in a UI and logged. Redacting where the label is built is
    /// the only place that covers all three; the alternative is three
    /// implementers each remembering, which has already failed once in this
    /// feature.
    ///
    /// Every way `run` can stop is recorded. A panic in the future becomes
    /// `Failed` rather than leaving the job `Running` for ever: the registry is
    /// all a poller can see, so an unobserved panic would be a spinner that never
    /// stops. That is why `run` is driven by its own task whose `JoinHandle` this
    /// one awaits — a panic that unwound into the recording task would take the
    /// recording with it.
    pub async fn spawn<F>(&self, dest: PathBuf, source_label: impl AsRef<str>, run: F) -> CloneJobId
    where
        F: Future<Output = CloneOutcome> + Send + 'static,
    {
        let id = {
            // One lock for the prune, the dedupe read and the insert: checking
            // for a live job and then inserting under a second lock is a race in
            // which two callers both find nothing and both start a clone.
            let mut jobs = self.jobs.write().await;
            prune_stale(&mut jobs, Instant::now());
            if let Some(existing) = live_job_at(&jobs, &dest) {
                debug!(
                    "clone into {} already running as {existing}",
                    dest.display()
                );
                return existing;
            }
            let id = CloneJobId::new();
            jobs.insert(
                id,
                Entry {
                    job: CloneJob {
                        id,
                        source_label: redact_credentials(source_label.as_ref()),
                        dest,
                        status: CloneStatus::Running,
                    },
                    finished_at: None,
                },
            );
            id
        };

        if let Some(notify) = &self.notify {
            notify();
        }
        let task = tokio::spawn(run);
        let registry = self.clone();
        tokio::spawn(async move {
            let outcome = match task.await {
                Ok(outcome) => outcome,
                Err(err) => CloneOutcome::failed(interrupted_message(err)),
            };
            registry.finish(id, outcome).await;
        });

        id
    }

    /// The job with this id, as a frontend polls it.
    pub async fn get(&self, id: CloneJobId) -> Option<CloneJob> {
        self.jobs
            .read()
            .await
            .get(&id)
            .map(|entry| entry.job.clone())
    }

    /// Drop finished jobs older than [`TERMINAL_JOB_TTL`].
    ///
    /// Called on every [`spawn`](Self::spawn), so a server that keeps cloning
    /// keeps tidying itself and no caller has to remember. Exposed as well
    /// because "runs when something else happens" is not a schedule: a server
    /// that clones once and idles for a week should still be able to let that job
    /// go.
    pub async fn prune(&self) {
        let mut jobs = self.jobs.write().await;
        prune_stale(&mut jobs, Instant::now());
    }

    /// Record a terminal status against a job.
    async fn finish(&self, id: CloneJobId, outcome: CloneOutcome) {
        let status = outcome.into_status();
        if let CloneStatus::Failed { message } = &status {
            warn!("clone job {id} failed: {message}");
        }
        match self.jobs.write().await.get_mut(&id) {
            Some(entry) => {
                entry.job.status = status;
                entry.finished_at = Some(Instant::now());
            }
            // Unreachable in practice: only a terminal job is ever pruned, and a
            // job is only terminal once this has run. Logged rather than
            // asserted so a future `prune` that broke that invariant leaves a
            // trace instead of losing the outcome silently.
            None => debug!("clone job {id} vanished before its outcome was recorded"),
        }
        if let Some(notify) = &self.notify {
            notify();
        }
    }

    /// How many jobs are registered. Test-only: the count is not a contract, it
    /// is how a test proves a deduped `spawn` registered nothing.
    #[cfg(test)]
    async fn len(&self) -> usize {
        self.jobs.read().await.len()
    }

    /// Move a finished job's completion instant `by` further into the past.
    ///
    /// Test-only, and the reason no test here sleeps: "ten minutes have passed"
    /// means precisely this to the registry, so back-dating exercises the real
    /// [`prune`](Self::prune) against the real [`TERMINAL_JOB_TTL`] rather than a
    /// TTL shortened for the test. Panics on a job that has not finished — a
    /// running job has no completion instant to age, and a test that tried would
    /// otherwise silently assert nothing.
    ///
    /// Subtracting from an `Instant` is safe here rather than a fresh-boot flake:
    /// on Linux an `Instant` is a `Timespec` whose seconds field is signed, so an
    /// instant "before boot" is representable. Verified with this toolchain —
    /// `Instant::now().checked_sub(Duration::from_secs(10 * 365 * 86400))` is
    /// `Some`, and `duration_since` on the result reports the full ten years. On a
    /// platform where `Instant` wraps an unsigned counter this would need
    /// `checked_sub`.
    #[cfg(test)]
    async fn backdate_finish(&self, id: CloneJobId, by: Duration) {
        let mut jobs = self.jobs.write().await;
        let entry = jobs.get_mut(&id).expect("no such job");
        let finished_at = entry
            .finished_at
            .expect("cannot back-date a job that has not finished");
        entry.finished_at = Some(finished_at - by);
    }
}

/// The id of a *running* job cloning into `dest`, if there is one.
fn live_job_at(jobs: &HashMap<CloneJobId, Entry>, dest: &Path) -> Option<CloneJobId> {
    jobs.values()
        .find(|entry| entry.finished_at.is_none() && entry.job.dest == dest)
        .map(|entry| entry.job.id)
}

/// Drop every job that finished more than [`TERMINAL_JOB_TTL`] before `now`.
///
/// `now` is a parameter so the two callers share one definition of stale rather
/// than each reading the clock.
fn prune_stale(jobs: &mut HashMap<CloneJobId, Entry>, now: Instant) {
    jobs.retain(|id, entry| {
        let stale = entry
            .finished_at
            .is_some_and(|at| now.duration_since(at) >= TERMINAL_JOB_TTL);
        if stale {
            debug!("pruned finished clone job {id}");
        }
        !stale
    });
}

/// Why a job task stopped without producing an outcome.
///
/// A panic is the case that matters, and its payload is worth surfacing: the
/// message is what tells whoever reads the job *which* bug they hit.
/// `JoinError::is_panic`/`into_panic` and the `&'static str`/`String` payload
/// shapes are `std::panic` and tokio's documented API. The other arm is
/// cancellation, which cannot happen here — nothing holds the `JoinHandle` long
/// enough to abort it, and this registry has no cancellation API by design —
/// but it is a distinct message rather than a lie about a panic.
fn interrupted_message(err: JoinError) -> String {
    if !err.is_panic() {
        return format!("the clone task stopped unexpectedly: {err}");
    }
    let payload = err.into_panic();
    let detail = payload
        .downcast_ref::<&'static str>()
        .map(|s| (*s).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned());
    match detail {
        Some(detail) => format!("the clone task panicked: {detail}"),
        None => "the clone task panicked".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use claude_commander_protocol::session::ProjectId;
    use tempfile::TempDir;
    use tokio::sync::oneshot;
    use uuid::Uuid;

    /// Poll until `id` leaves `Running`, yielding rather than sleeping.
    ///
    /// A bounded loop, so a job that never finishes fails the test instead of
    /// hanging it. `yield_now` is what lets the spawned job task make progress on
    /// the single-threaded test runtime; no wall-clock time passes.
    async fn await_terminal(jobs: &CloneJobs, id: CloneJobId) -> CloneJob {
        for _ in 0..10_000 {
            let job = jobs.get(id).await.expect("job disappeared while running");
            if !matches!(job.status, CloneStatus::Running) {
                return job;
            }
            tokio::task::yield_now().await;
        }
        panic!("job {id} never reached a terminal status");
    }

    /// Let the runtime run every ready task, without advancing the clock.
    async fn settle() {
        for _ in 0..64 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn job_runs_then_reports_a_terminal_status() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("repo");
        let project_id = ProjectId::new();
        let jobs = CloneJobs::new();

        let id = jobs
            .spawn(dest.clone(), "owner/repo", async move {
                CloneOutcome::succeeded(project_id)
            })
            .await;

        // Readable immediately, with the source and destination the caller gave.
        let job = jobs.get(id).await.unwrap();
        assert_eq!(job.id, id);
        assert_eq!(job.source_label, "owner/repo");
        assert_eq!(job.dest, dest);

        let job = await_terminal(&jobs, id).await;
        assert_eq!(job.status, CloneStatus::Succeeded { project_id });
    }

    #[tokio::test]
    async fn a_failing_job_records_the_message() {
        let tmp = TempDir::new().unwrap();
        let jobs = CloneJobs::new();

        let id = jobs
            .spawn(tmp.path().join("repo"), "owner/repo", async {
                CloneOutcome::failed("git clone failed: repository not found")
            })
            .await;

        assert_eq!(
            await_terminal(&jobs, id).await.status,
            CloneStatus::Failed {
                message: "git clone failed: repository not found".to_string()
            }
        );
    }

    /// A terminal outcome the caller already knows (the destination is occupied)
    /// goes through the same door: an immediately-ready future.
    #[tokio::test]
    async fn a_job_can_be_born_terminal() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("repo");
        let jobs = CloneJobs::new();

        let id = jobs
            .spawn(dest.clone(), "owner/repo", {
                let dest = dest.clone();
                async move { CloneOutcome::destination_exists(dest, true) }
            })
            .await;

        assert_eq!(
            await_terminal(&jobs, id).await.status,
            CloneStatus::DestinationExists {
                dest,
                is_git_repo: true
            }
        );
    }

    /// Two clones must never write into one directory. The second call gets the
    /// first job's id back, and — the half that matters — its own future is never
    /// polled, so no second `git clone` starts.
    #[tokio::test]
    async fn spawning_for_a_live_destination_returns_the_existing_job() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("repo");
        let jobs = CloneJobs::new();
        let runs = Arc::new(AtomicUsize::new(0));

        // Genuinely unfinished: the future parks on a channel nobody has sent to,
        // so "still running" cannot hold for the wrong reason.
        let (release, parked) = oneshot::channel::<()>();
        let first = jobs
            .spawn(dest.clone(), "first", {
                let runs = Arc::clone(&runs);
                async move {
                    runs.fetch_add(1, Ordering::SeqCst);
                    let _ = parked.await;
                    CloneOutcome::failed("released")
                }
            })
            .await;
        settle().await;
        assert_eq!(jobs.get(first).await.unwrap().status, CloneStatus::Running);
        assert_eq!(runs.load(Ordering::SeqCst), 1);

        let second = jobs
            .spawn(dest.clone(), "second", {
                let runs = Arc::clone(&runs);
                async move {
                    runs.fetch_add(1, Ordering::SeqCst);
                    CloneOutcome::succeeded(ProjectId::new())
                }
            })
            .await;

        assert_eq!(first, second);
        settle().await;
        assert_eq!(
            runs.load(Ordering::SeqCst),
            1,
            "the deduped call started a second clone"
        );
        assert_eq!(jobs.len().await, 1, "a duplicate job was registered");
        // The first job's identity is what survives, not the second call's.
        assert_eq!(jobs.get(first).await.unwrap().source_label, "first");

        release.send(()).unwrap();
        assert!(matches!(
            await_terminal(&jobs, first).await.status,
            CloneStatus::Failed { .. }
        ));
    }

    /// Dedupe is about *live* jobs only. Once a clone has failed, the user must be
    /// able to retry it into the same directory.
    #[tokio::test]
    async fn a_finished_destination_can_be_cloned_again() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("repo");
        let jobs = CloneJobs::new();

        let first = jobs
            .spawn(dest.clone(), "first", async {
                CloneOutcome::failed("boom")
            })
            .await;
        await_terminal(&jobs, first).await;

        let second = jobs
            .spawn(dest.clone(), "second", async {
                CloneOutcome::succeeded(ProjectId::new())
            })
            .await;

        assert_ne!(first, second);
        assert!(matches!(
            await_terminal(&jobs, second).await.status,
            CloneStatus::Succeeded { .. }
        ));
    }

    #[tokio::test]
    async fn prune_drops_terminal_jobs_past_the_ttl_and_keeps_running_ones() {
        let tmp = TempDir::new().unwrap();
        let jobs = CloneJobs::new();

        let (_release, parked) = oneshot::channel::<()>();
        let running = jobs
            .spawn(tmp.path().join("slow"), "slow", async move {
                let _ = parked.await;
                CloneOutcome::failed("never")
            })
            .await;
        let fresh = jobs
            .spawn(tmp.path().join("fresh"), "fresh", async {
                CloneOutcome::failed("boom")
            })
            .await;
        let stale = jobs
            .spawn(tmp.path().join("stale"), "stale", async {
                CloneOutcome::failed("boom")
            })
            .await;
        await_terminal(&jobs, fresh).await;
        await_terminal(&jobs, stale).await;
        assert_eq!(
            jobs.get(running).await.unwrap().status,
            CloneStatus::Running
        );

        // No sleeping: back-date the stored completion instant instead, which is
        // exactly what "ten minutes have passed" means to the registry.
        jobs.backdate_finish(stale, TERMINAL_JOB_TTL + Duration::from_secs(1))
            .await;
        jobs.prune().await;

        assert!(jobs.get(stale).await.is_none(), "stale job survived prune");
        assert!(jobs.get(fresh).await.is_some(), "fresh job was pruned");
        assert!(
            jobs.get(running).await.is_some(),
            "a running job was pruned — it can never be, at any age"
        );
    }

    /// `prune` is not something a caller has to remember: every `spawn` runs it.
    #[tokio::test]
    async fn prune_runs_on_each_spawn() {
        let tmp = TempDir::new().unwrap();
        let jobs = CloneJobs::new();

        let stale = jobs
            .spawn(tmp.path().join("stale"), "stale", async {
                CloneOutcome::failed("boom")
            })
            .await;
        await_terminal(&jobs, stale).await;
        jobs.backdate_finish(stale, TERMINAL_JOB_TTL + Duration::from_secs(1))
            .await;

        jobs.spawn(tmp.path().join("other"), "other", async {
            CloneOutcome::failed("boom")
        })
        .await;

        assert!(jobs.get(stale).await.is_none());
    }

    /// A panicking job must not sit in `Running` for ever: the registry is the
    /// only thing a poller can see, so an unobserved panic is an unkillable
    /// spinner in the UI.
    #[tokio::test]
    async fn a_panicking_job_is_recorded_as_failed() {
        let tmp = TempDir::new().unwrap();
        let jobs = CloneJobs::new();

        let id = jobs
            .spawn(tmp.path().join("repo"), "owner/repo", async {
                panic!("clone task exploded");
                #[allow(unreachable_code)]
                CloneOutcome::succeeded(ProjectId::new())
            })
            .await;

        let CloneStatus::Failed { message } = await_terminal(&jobs, id).await.status else {
            panic!("a panicking job was not recorded as failed");
        };
        assert!(
            message.contains("panicked"),
            "message does not say what happened: {message}"
        );
    }

    /// The security property this registry owes the rest of the flow: a
    /// `CloneRequest.source` is raw user input, and `source_label` is polled,
    /// rendered and logged. A pasted `https://user:token@…` must be redacted
    /// where the label is *built*, not at each of those hops.
    #[tokio::test]
    async fn a_credentialed_source_never_reaches_a_stored_job() {
        const SECRET: &str = "ghp_s3cr3tt0ken";
        let tmp = TempDir::new().unwrap();
        let jobs = CloneJobs::new();

        let id = jobs
            .spawn(
                tmp.path().join("repo"),
                format!("https://sizeak:{SECRET}@github.com/o/r.git"),
                async {
                    CloneOutcome::failed(format!(
                        "git clone failed: fatal: Authentication failed for \
                         'https://sizeak:{SECRET}@github.com/o/r.git/'"
                    ))
                },
            )
            .await;

        let job = jobs.get(id).await.unwrap();
        assert_eq!(job.source_label, "https://***@github.com/o/r.git");
        assert!(
            !format!("{job:?}").contains(SECRET),
            "token leaked: {job:?}"
        );

        let job = await_terminal(&jobs, id).await;
        assert!(
            !format!("{job:?}").contains(SECRET),
            "token leaked: {job:?}"
        );
        let CloneStatus::Failed { message } = &job.status else {
            panic!("{:?}", job.status)
        };
        // Redacted, not swallowed — the user still has to be able to read it.
        assert!(message.contains("Authentication failed"), "{message}");
        assert!(
            message.contains("https://***@github.com/o/r.git/"),
            "{message}"
        );
    }

    #[tokio::test]
    async fn unknown_job_id_is_none() {
        assert!(
            CloneJobs::new()
                .get(CloneJobId::from_uuid(Uuid::new_v4()))
                .await
                .is_none()
        );
    }
}
