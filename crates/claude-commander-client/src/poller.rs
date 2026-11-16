//! Background poller: a [`RemoteClient`]'s change-feed and connection state
//! machine.
//!
//! Servers expose an authenticated change wait; older servers fall back to polling.
//! The poller waits on the server generation before reconciling snapshots;
//! unsupported servers use [`PollConfig::interval`]. Payload changes, generation
//! changes and recovery bump a [`watch`] generation counter; the consumer (the
//! remote adapter's change-feed task) waits on it and re-fetches the snapshots
//! itself. The poll's payloads are only hashed, never handed on, so a change
//! costs the poll's two GETs plus the consumer's own reads.
//!
//! Alongside the generation counter the poller drives a [`ConnectionState`]
//! watch: `Connecting` until the first successful poll, `Connected` while polls
//! succeed, and `Degraded { reason }` on failure, with exponential backoff
//! ([`backoff_delay`]) between retries so a downed server isn't hammered.
//!
//! The poll task holds no strong reference back to the adapter backend, so it
//! forms no cycle; [`Poller`]'s `Drop` aborts it when the backend goes away.

use std::sync::Arc;
use std::time::Duration;

use claude_commander_protocol::connection::ConnectionState;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::ClientError;
use crate::backoff::{BackoffConfig, backoff_delay};
use crate::client::RemoteClient;

/// Cadence + backoff for the background poller. Fields are public so a frontend
/// can wire them from config; the defaults match the local backend's agent-state
/// poll cadence.
#[derive(Clone, Copy, Debug)]
pub struct PollConfig {
    /// Fallback cadence for servers without the authenticated change endpoint.
    pub interval: Duration,
    /// Reconnect backoff applied while polls are failing.
    pub backoff: BackoffConfig,
}

impl Default for PollConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(2),
            backoff: BackoffConfig::default(),
        }
    }
}

/// A watch on a backend's [`ConnectionState`], for rendering health reactively.
#[derive(Clone)]
pub struct ConnectionFeed {
    rx: watch::Receiver<ConnectionState>,
}

impl ConnectionFeed {
    pub(crate) fn new(rx: watch::Receiver<ConnectionState>) -> Self {
        Self { rx }
    }

    /// The current connection state (cheap; no `.await`).
    pub fn current(&self) -> ConnectionState {
        self.rx.borrow().clone()
    }

    /// Wait until the connection state changes. Returns `false` once the poller
    /// is gone (the backend was dropped), so a render task can exit cleanly.
    pub async fn changed(&mut self) -> bool {
        self.rx.changed().await.is_ok()
    }
}

/// Owns the spawned poll task and the receiver ends of its two watches. Aborting
/// the task on `Drop` prevents an orphaned poll loop from outliving the backend.
pub struct Poller {
    // Receiver-version subtlety: a change-feed clones this receiver, and a cloned
    // `watch::Receiver` inherits the source's last-seen version. This stored
    // receiver is therefore never consumed (`borrow_and_update`) — it stays
    // pinned at the initial generation (0) so a clone's first `changed().await`
    // still fires on the poll loop's first bump. Consuming it here would mark
    // that bump as already-seen and reintroduce a startup race where the initial
    // remote snapshot is never fetched.
    generation: watch::Receiver<u64>,
    connection: watch::Receiver<ConnectionState>,
    handle: JoinHandle<()>,
}

impl Poller {
    /// A fresh clone of the change-feed generation watch (pinned at the initial
    /// version, so a first `changed().await` fires on the poll loop's first bump).
    pub fn generation_watch(&self) -> watch::Receiver<u64> {
        self.generation.clone()
    }

    /// A fresh clone of the connection-health watch.
    pub fn connection_watch(&self) -> watch::Receiver<ConnectionState> {
        self.connection.clone()
    }

    /// The current connection health (cheap; no `.await`).
    pub fn connection_state(&self) -> ConnectionState {
        self.connection.borrow().clone()
    }

    /// A reactive [`ConnectionFeed`] on the connection health.
    pub fn connection_feed(&self) -> ConnectionFeed {
        ConnectionFeed::new(self.connection.clone())
    }
}

impl Drop for Poller {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// Spawn the poll loop for `client` and return the [`Poller`] owning it. The
/// task holds `client` (an `Arc`) rather than the adapter backend, so it forms
/// no cycle; dropping the returned `Poller` aborts it.
pub fn spawn_poller(client: Arc<RemoteClient>, config: PollConfig) -> Poller {
    let (gen_tx, gen_rx) = watch::channel(0u64);
    let (conn_tx, conn_rx) = watch::channel(ConnectionState::Connecting);
    let handle = tokio::spawn(run(client, config, gen_tx, conn_tx));
    Poller {
        generation: gen_rx,
        connection: conn_rx,
        handle,
    }
}

/// The poll loop. Fetches + hashes on each tick, bumps `gen_tx` on a real change
/// or a recovery, and moves `conn_tx` through the connection state machine with
/// exponential backoff on failure.
async fn run(
    client: Arc<RemoteClient>,
    config: PollConfig,
    gen_tx: watch::Sender<u64>,
    conn_tx: watch::Sender<ConnectionState>,
) {
    let mut last_hash: Option<u64> = None;
    let mut generation = None;
    let mut push_supported = true;
    let mut consecutive_failures: u32 = 0;
    let mut last_poll: Option<tokio::time::Instant> = None;

    loop {
        let previous_generation = generation;
        if push_supported {
            match client.wait_for_change(generation).await {
                Ok(next) => generation = Some(next),
                Err(ClientError::NotFound) => push_supported = false,
                Err(err) => {
                    generation = None;
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    mark_degraded(&conn_tx, err.to_string());
                    tokio::time::sleep(backoff_delay(&config.backoff, consecutive_failures)).await;
                    continue;
                }
            }
        }
        if let Some(last) = last_poll {
            tokio::time::sleep_until(last + Duration::from_millis(250)).await;
        }
        last_poll = Some(tokio::time::Instant::now());
        match client.poll_hashes().await {
            Ok(hash) => {
                let content_changed = last_hash != Some(hash);
                let recovered = !matches!(&*conn_tx.borrow(), ConnectionState::Connected);
                last_hash = Some(hash);
                consecutive_failures = 0;

                if recovered {
                    // Only send on a real transition so idle polls don't spam
                    // the connection watch.
                    let _ = conn_tx.send(ConnectionState::Connected);
                }
                // Bump the change feed on the first successful poll (recovered
                // from `Connecting`), on any later recovery, or whenever the
                // observable state actually moved — so the consumer re-fetches.
                if content_changed || recovered || generation != previous_generation {
                    gen_tx.send_modify(|g| *g = g.wrapping_add(1));
                }

                if !push_supported {
                    tokio::time::sleep(config.interval).await;
                }
            }
            Err(err) => {
                // Retry payload failures after backoff without another change wait.
                generation = None;
                consecutive_failures += 1;
                let reason = err.to_string();
                tracing::debug!(server = %client.name(), %reason, "remote poll failed");
                mark_degraded(&conn_tx, reason);
                tokio::time::sleep(backoff_delay(&config.backoff, consecutive_failures)).await;
            }
        }
    }
}

/// Move the connection watch into `Degraded` on a failed poll, notifying
/// watchers on the transition into degraded *and* whenever the failure reason
/// changes while already degraded — so a shifting fault (refused → 503) surfaces
/// its current cause rather than being pinned to the first one. A repeated
/// failure with an unchanged reason is still deduped (mirroring the Connected
/// side's guard) so a run of identical failed polls doesn't spam the watch.
/// Returns whether a notification was sent.
fn mark_degraded(conn_tx: &watch::Sender<ConnectionState>, reason: String) -> bool {
    conn_tx.send_if_modified(|state| match state {
        ConnectionState::Degraded { reason: existing } if *existing == reason => false,
        ConnectionState::Degraded { reason: existing } => {
            *existing = reason;
            true
        }
        _ => {
            *state = ConnectionState::Degraded { reason };
            true
        }
    })
}

#[cfg(test)]
mod tests {
    fn fast_config() -> PollConfig {
        PollConfig {
            interval: Duration::from_millis(10),
            backoff: BackoffConfig {
                initial: Duration::from_millis(10),
                max: Duration::from_millis(10),
                factor: 1,
            },
        }
    }
    async fn next_change(rx: &mut watch::Receiver<u64>) {
        tokio::time::timeout(Duration::from_secs(2), rx.changed())
            .await
            .unwrap()
            .unwrap();
        rx.borrow_and_update();
    }
    #[tokio::test]
    async fn failed_wait_reconnects_without_waiting_for_another_change() {
        use std::sync::atomic::Ordering;
        let server = crate::test_server::Server::start(true).await;
        let poller = spawn_poller(server.client.clone(), fast_config());
        let mut rx = poller.generation_watch();
        next_change(&mut rx).await;
        server.wait_for_requests(2).await;
        // The parked wait has already passed the failure check, so this bump
        // wakes it normally; the *next* wait (since = the new generation) fails
        // with the generation unchanged.
        server.data.fail_wait.store(true, Ordering::SeqCst);
        server.data.generation.send_modify(|g| *g += 1);
        next_change(&mut rx).await;
        let mut conn = poller.connection_watch();
        wait_for_state(&mut conn, |s| matches!(s, ConnectionState::Degraded { .. })).await;
        // Recovery must not wait for another server change: a retry that kept
        // `since` would park for up to CHANGE_WAIT_SECS (forever, here).
        wait_for_state(&mut conn, |s| *s == ConnectionState::Connected).await;
        assert_eq!(server.data.workspace.load(Ordering::SeqCst), 3);
    }

    async fn wait_for_state(
        rx: &mut watch::Receiver<ConnectionState>,
        want: impl Fn(&ConnectionState) -> bool,
    ) {
        tokio::time::timeout(Duration::from_secs(2), rx.wait_for(|s| want(s)))
            .await
            .expect("connection state did not arrive in time")
            .unwrap();
    }

    #[tokio::test]
    async fn generation_changes_wake_even_when_payloads_are_identical() {
        use std::sync::atomic::Ordering;
        let server = crate::test_server::Server::start(true).await;
        let poller = spawn_poller(server.client.clone(), fast_config());
        let mut rx = poller.generation_watch();
        next_change(&mut rx).await;
        server.wait_for_requests(2).await;
        assert_eq!(server.data.workspace.load(Ordering::SeqCst), 1);
        assert!(!rx.has_changed().unwrap());
        server.data.generation.send_modify(|g| *g += 1);
        next_change(&mut rx).await;
        assert_eq!(server.data.workspace.load(Ordering::SeqCst), 2);
    }
    #[tokio::test]
    async fn legacy_servers_keep_polling_and_payload_failures_retry_without_waiting() {
        use std::sync::atomic::Ordering;
        let server = crate::test_server::Server::start(false).await;
        server.data.fail_workspace.store(true, Ordering::SeqCst);
        let poller = spawn_poller(server.client.clone(), fast_config());
        let mut rx = poller.generation_watch();
        next_change(&mut rx).await;
        assert!(server.data.workspace.load(Ordering::SeqCst) >= 2);
        assert_eq!(poller.connection_state(), ConnectionState::Connected);
        let server = crate::test_server::Server::start(true).await;
        server.data.fail_workspace.store(true, Ordering::SeqCst);
        let poller = spawn_poller(server.client.clone(), fast_config());
        let mut rx = poller.generation_watch();
        next_change(&mut rx).await;
        assert!(server.data.workspace.load(Ordering::SeqCst) >= 2);
        assert_eq!(poller.connection_state(), ConnectionState::Connected);
    }

    use super::*;

    #[test]
    fn consecutive_failures_with_same_reason_notify_once() {
        let (tx, mut rx) = watch::channel(ConnectionState::Connecting);

        // First failure transitions Connecting → Degraded and notifies.
        assert!(mark_degraded(&tx, "boom".to_string()));
        assert!(rx.has_changed().unwrap());
        rx.borrow_and_update();

        // Second failure with the *same* reason is a no-op — no further
        // notification (the every-poll spam guard).
        assert!(!mark_degraded(&tx, "boom".to_string()));
        assert!(!rx.has_changed().unwrap());
    }

    #[test]
    fn degraded_reason_change_notifies_with_new_reason() {
        let (tx, mut rx) = watch::channel(ConnectionState::Connecting);

        // First failure → Degraded { "connection refused" }.
        assert!(mark_degraded(&tx, "connection refused".to_string()));
        assert!(rx.has_changed().unwrap());
        rx.borrow_and_update();

        // A later poll fails differently (e.g. a 503 now, not a refused
        // connection). Still degraded, but the reason moved — watchers must be
        // notified with the *fresh* reason rather than being stuck on the old
        // one.
        assert!(mark_degraded(&tx, "server error 503".to_string()));
        assert!(rx.has_changed().unwrap());
        assert!(matches!(
            &*rx.borrow_and_update(),
            ConnectionState::Degraded { reason } if reason == "server error 503"
        ));
    }

    #[test]
    fn recovery_after_degraded_then_new_failure_notifies_again() {
        let (tx, mut rx) = watch::channel(ConnectionState::Connecting);
        assert!(mark_degraded(&tx, "down".to_string()));
        rx.borrow_and_update();
        // Recover, then fail again — a fresh transition notifies.
        let _ = tx.send(ConnectionState::Connected);
        rx.borrow_and_update();
        assert!(mark_degraded(&tx, "down again".to_string()));
        assert!(rx.has_changed().unwrap());
    }
}
