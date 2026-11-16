//! Privacy-respecting usage telemetry.
//!
//! Records *which features are used* and a coarse environment/config snapshot,
//! so we can see what to keep and what to retire. It never records typed text,
//! prompts, Claude session content, comment bodies, branch/session names, repo
//! paths, or arbitrary environment variables — the event schema in [`event`] is
//! a fixed set of typed fields, so there is no path to leak content.
//!
//! Events are stamped with common fields, queued on a bounded channel, batched,
//! and shipped by a background task ([`EventSink`]). The whole thing degrades to
//! a cheap no-op when telemetry is disabled (config off, `DO_NOT_TRACK`, or no
//! ingest credential baked in / configured).
//!
//! ## Frontend identity is mandatory
//!
//! The library is consumed by more than one frontend (the `claude-commander`
//! binary and a separate GUI). Each MUST identify itself via [`FrontendInfo`] so
//! events can be attributed; [`FrontendInfo::new`] panics if name or version is
//! empty.

mod event;
mod sink;

use std::sync::Arc;

use serde_json::{Map, Value};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Duration, MissedTickBehavior, interval};
use tracing::{debug, warn};

pub use event::{ConfigSnapshot, EnvFingerprint};
/// Lets other modules' tests (e.g. `CommanderService`'s) assert which feature
/// events a domain method records.
#[cfg(test)]
pub(crate) use sink::MemorySink;
pub use sink::{EventPayload, EventSink, HttpSink};

use crate::config::TelemetryConfig;

/// Default ingest endpoint baked into official builds (isolated OpenObserve org
/// + a dedicated ingest-only credential). Overridable via `[telemetry] endpoint`.
const DEFAULT_ENDPOINT: &str =
    "https://o2.sljackson.co.uk/api/3FdRQPoDlXoEh6oPCW2CIao3g3a/commander_usage/_json";

/// Pre-encoded HTTP Basic credential (`base64("<email>:<token>")`) for the
/// shared OpenObserve ingest account.
///
/// Committed in source on purpose: the token is readable in any compiled binary
/// regardless, and baking it in means from-source builds report too (not just
/// official release artifacts), which is the point of usage telemetry. It only
/// grants append access to the isolated, non-sensitive telemetry stream.
///
/// A build-time `CC_TELEMETRY_TOKEN` overrides it (to rotate or redirect);
/// building with `CC_TELEMETRY_TOKEN=""` opts the build out entirely (for
/// distro packagers who don't want telemetry compiled in at all).
const BAKED_CREDENTIAL_RAW: &str = match option_env!("CC_TELEMETRY_TOKEN") {
    Some(token) => token,
    None => "dGVsZW1ldHJ5LWluZ2VzdEBzdmMuY29tbWFuZGVyOmlUTVY3elNSdWpSa1pUU0c=",
};

/// The baked ingest credential, or `None` when a build opted out by setting
/// `CC_TELEMETRY_TOKEN=""`.
fn baked_credential() -> Option<&'static str> {
    Some(BAKED_CREDENTIAL_RAW).filter(|t| !t.is_empty())
}

const CHANNEL_CAPACITY: usize = 256;
/// Flush once a batch reaches this size, regardless of the timer.
const BATCH_MAX: usize = 64;
/// Flush partial batches at least this often.
const FLUSH_INTERVAL: Duration = Duration::from_secs(5);

/// Identity of the application embedding this library. Required so events can be
/// attributed to a frontend + version (e.g. "is the GUI using feature X?").
#[derive(Debug, Clone)]
pub struct FrontendInfo {
    pub name: String,
    pub version: String,
}

impl FrontendInfo {
    /// Construct a frontend identity. **Panics** (like `expect`) if `name` or
    /// `version` is empty — the consuming application is required to identify
    /// itself, e.g. `FrontendInfo::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"))`.
    ///
    /// Pass the *frontend's* own package metadata, not
    /// [`crate::VERSION`]: that constant is this library's
    /// version, reported separately as `lib_version`, and a frontend that
    /// forwards it here reports the wrong version for itself.
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        let name = name.into();
        let version = version.into();
        assert!(
            !name.trim().is_empty(),
            "telemetry: FrontendInfo name must not be empty — the embedding \
             application must identify itself with its own package metadata, \
             e.g. FrontendInfo::new(env!(\"CARGO_PKG_NAME\"), env!(\"CARGO_PKG_VERSION\"))"
        );
        assert!(
            !version.trim().is_empty(),
            "telemetry: FrontendInfo version must not be empty"
        );
        Self { name, version }
    }
}

/// Control messages on the telemetry channel. Multiplexing flush onto the same
/// channel keeps event ordering and makes `flush().await` deterministic.
enum Msg {
    Event(EventPayload),
    /// Drain the current batch to the sink, then acknowledge.
    Flush(oneshot::Sender<()>),
}

struct Inner {
    /// Fields stamped onto every event (frontend, lib version, install id, …).
    common: EventPayload,
    /// `None` when telemetry is disabled — every method becomes a no-op.
    tx: Option<mpsc::Sender<Msg>>,
}

/// Cheaply cloneable telemetry handle. Clones share one background sender.
#[derive(Clone)]
pub struct Telemetry(Arc<Inner>);

impl Telemetry {
    /// A no-op handle that records nothing.
    pub fn disabled() -> Self {
        Telemetry(Arc::new(Inner {
            common: Map::new(),
            tx: None,
        }))
    }

    /// Build a handle from resolved config. Returns a [`disabled`](Self::disabled)
    /// handle when telemetry is off (config flag, `DO_NOT_TRACK`, or no
    /// credential). Spawns the background flush task otherwise.
    pub fn init(config: &TelemetryConfig, frontend: &FrontendInfo, install_id: &str) -> Self {
        if !would_be_enabled(config) {
            return Self::disabled();
        }
        let credential = config
            .token
            .clone()
            .or_else(|| baked_credential().map(str::to_string))
            .expect("credential present (checked by would_be_enabled)");
        let endpoint = config
            .endpoint
            .clone()
            .unwrap_or_else(|| DEFAULT_ENDPOINT.to_string());
        debug!("telemetry active (endpoint={endpoint})");
        let sink: Arc<dyn EventSink> = Arc::new(HttpSink::new(endpoint, credential));
        Self::with_sink(frontend, install_id, sink)
    }

    /// Build a live handle around an arbitrary sink and spawn its flush task.
    /// Requires a Tokio runtime. Used by [`init`](Self::init) and tests.
    pub fn with_sink(frontend: &FrontendInfo, install_id: &str, sink: Arc<dyn EventSink>) -> Self {
        // The flush task needs a Tokio runtime to live in. Outside one (e.g. a
        // sync context), degrade to a no-op rather than panic on spawn.
        if tokio::runtime::Handle::try_current().is_err() {
            warn!("telemetry disabled: no Tokio runtime to host the flush task");
            return Self::disabled();
        }
        let common = build_common(frontend, install_id);
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        spawn_flush_task(rx, sink);
        Telemetry(Arc::new(Inner {
            common,
            tx: Some(tx),
        }))
    }

    /// Whether this handle actually records (false for a disabled handle).
    pub fn is_active(&self) -> bool {
        self.0.tx.is_some()
    }

    /// Record use of a feature. `name` is always a compile-time constant
    /// (e.g. `"review.open"`), never user input.
    pub fn feature(&self, name: &'static str) {
        if let Some(payload) = self.build("feature", |m| {
            m.insert("feature".into(), Value::String(name.to_string()));
        }) {
            self.queue(payload);
        }
    }

    /// Record a once-per-launch startup event with the environment fingerprint
    /// and config snapshot.
    pub fn session_start(&self, env: &EnvFingerprint, config: &ConfigSnapshot) {
        if let Some(payload) = self.build("session_start", |m| {
            if let Ok(Value::Object(env_map)) = serde_json::to_value(env) {
                m.extend(env_map);
            }
            m.insert(
                "config".into(),
                serde_json::to_value(config).unwrap_or(Value::Null),
            );
        }) {
            self.queue(payload);
        }
    }

    /// Flush any queued events now and wait for the sink to drain them. Call on
    /// shutdown (TUI) or before a short-lived process exits (CLI) so events
    /// aren't lost to the flush interval. No-op when disabled.
    pub async fn flush(&self) {
        let Some(tx) = &self.0.tx else { return };
        let (ack_tx, ack_rx) = oneshot::channel();
        if tx.send(Msg::Flush(ack_tx)).await.is_ok() {
            let _ = ack_rx.await;
        }
    }

    /// Assemble an event from the common fields plus `event_type` and whatever
    /// `fill` adds. Returns `None` (and records nothing) when disabled.
    fn build(
        &self,
        event_type: &str,
        fill: impl FnOnce(&mut EventPayload),
    ) -> Option<EventPayload> {
        self.0.tx.as_ref()?;
        let mut payload = self.0.common.clone();
        payload.insert("event_type".into(), Value::String(event_type.to_string()));
        fill(&mut payload);
        Some(payload)
    }

    /// Non-blocking enqueue. Drops the event if the channel is full — telemetry
    /// must never block or back-pressure the application.
    fn queue(&self, payload: EventPayload) {
        if let Some(tx) = &self.0.tx {
            let _ = tx.try_send(Msg::Event(payload));
        }
    }
}

/// Whether the `DO_NOT_TRACK` convention opts the user out. Any non-empty,
/// non-`"0"` value counts (per <https://consoledonottrack.com/>).
fn do_not_track() -> bool {
    std::env::var_os("DO_NOT_TRACK").is_some_and(|v| !v.is_empty() && v != "0")
}

/// Why telemetry is off, for logging at the decision site. The variants are
/// checked in declaration order, so an earlier reason wins when several apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disabled {
    /// `[telemetry] enabled = false` in config.
    Config,
    /// The `DO_NOT_TRACK` environment convention is set.
    DoNotTrack,
    /// No ingest credential — none configured and none baked into the build.
    NoCredential,
}

impl Disabled {
    fn reason(self) -> &'static str {
        match self {
            Disabled::Config => "config flag off",
            Disabled::DoNotTrack => "DO_NOT_TRACK set",
            Disabled::NoCredential => "no ingest credential baked in or configured",
        }
    }
}

/// Whether telemetry would be active for this config — config flag on,
/// `DO_NOT_TRACK` unset, and a credential available (configured or baked in).
/// Callers use this to skip telemetry-only work (e.g. install-id generation and
/// its background persist) when telemetry is off. Logs the reason at `debug`
/// when off so a silent build is diagnosable from the log alone.
pub fn would_be_enabled(config: &TelemetryConfig) -> bool {
    // Never emit during a test run — the credential is committed, so without
    // this the suite would ship events to the live stream and the sync service
    // constructor would try to spawn without a runtime.
    //
    // `cfg!(test)` covers only THIS crate's own test binary. A downstream crate
    // testing against core compiles it as a normal dependency, where `cfg!(test)`
    // is false — so extracting `claude-commander-tui` silently re-enabled
    // telemetry for its whole suite (826 tests each firing `session_start` at the
    // live stream, and the added latency made timing-sensitive async tests flaky).
    // `test-support` is enabled only from a `[dev-dependencies]` entry, so it is
    // exactly the signal for "a test binary is being built against core" and
    // costs a release build nothing.
    if cfg!(test) || cfg!(feature = "test-support") {
        return false;
    }
    let has_credential = config.token.is_some() || baked_credential().is_some();
    match classify_enablement(config.enabled, do_not_track(), has_credential) {
        Ok(()) => true,
        Err(reason) => {
            debug!("telemetry disabled: {}", reason.reason());
            false
        }
    }
}

/// Pure enablement decision, factored out so it can be unit-tested without
/// touching the environment. `Ok(())` means active; `Err` carries why not.
fn classify_enablement(
    enabled_cfg: bool,
    do_not_track: bool,
    has_credential: bool,
) -> Result<(), Disabled> {
    if !enabled_cfg {
        Err(Disabled::Config)
    } else if do_not_track {
        Err(Disabled::DoNotTrack)
    } else if !has_credential {
        Err(Disabled::NoCredential)
    } else {
        Ok(())
    }
}

fn build_common(frontend: &FrontendInfo, install_id: &str) -> EventPayload {
    let mut m = Map::new();
    m.insert("frontend_name".into(), Value::String(frontend.name.clone()));
    m.insert(
        "frontend_version".into(),
        Value::String(frontend.version.clone()),
    );
    m.insert("lib_version".into(), Value::String(crate::VERSION.into()));
    m.insert("install_id".into(), Value::String(install_id.to_string()));
    m.insert("os".into(), Value::String(std::env::consts::OS.into()));
    m.insert("arch".into(), Value::String(std::env::consts::ARCH.into()));
    m
}

fn spawn_flush_task(mut rx: mpsc::Receiver<Msg>, sink: Arc<dyn EventSink>) {
    tokio::spawn(async move {
        let mut batch: Vec<EventPayload> = Vec::new();
        let mut ticker = interval(FLUSH_INTERVAL);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                msg = rx.recv() => match msg {
                    Some(Msg::Event(ev)) => {
                        batch.push(ev);
                        if batch.len() >= BATCH_MAX {
                            sink.send_batch(std::mem::take(&mut batch)).await;
                        }
                    }
                    Some(Msg::Flush(ack)) => {
                        if !batch.is_empty() {
                            sink.send_batch(std::mem::take(&mut batch)).await;
                        }
                        let _ = ack.send(());
                    }
                    None => {
                        // All handles dropped: final flush, then exit.
                        if !batch.is_empty() {
                            sink.send_batch(std::mem::take(&mut batch)).await;
                        }
                        break;
                    }
                },
                _ = ticker.tick() => {
                    if !batch.is_empty() {
                        sink.send_batch(std::mem::take(&mut batch)).await;
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::sink::MemorySink;
    use super::*;

    fn frontend() -> FrontendInfo {
        FrontendInfo::new("claude-commander", "9.9.9")
    }

    #[test]
    #[should_panic(expected = "FrontendInfo name must not be empty")]
    fn frontend_info_panics_on_empty_name() {
        FrontendInfo::new("", "1.0.0");
    }

    #[test]
    #[should_panic(expected = "FrontendInfo version must not be empty")]
    fn frontend_info_panics_on_empty_version() {
        FrontendInfo::new("claude-commander", "   ");
    }

    #[test]
    fn classify_enablement_truth_table() {
        assert_eq!(classify_enablement(true, false, true), Ok(()));
        assert_eq!(
            classify_enablement(false, false, true),
            Err(Disabled::Config),
            "config off"
        );
        assert_eq!(
            classify_enablement(true, true, true),
            Err(Disabled::DoNotTrack),
            "DO_NOT_TRACK"
        );
        assert_eq!(
            classify_enablement(true, false, false),
            Err(Disabled::NoCredential),
            "no credential"
        );
        // Config-off takes precedence over a missing credential when both apply.
        assert_eq!(
            classify_enablement(false, false, false),
            Err(Disabled::Config)
        );
    }

    #[tokio::test]
    async fn feature_event_carries_common_fields() {
        let sink = Arc::new(MemorySink::default());
        let t = Telemetry::with_sink(&frontend(), "install-abc", sink.clone());
        t.feature("review.open");
        t.flush().await;

        let events = sink.events();
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!(e["event_type"], "feature");
        assert_eq!(e["feature"], "review.open");
        assert_eq!(e["frontend_name"], "claude-commander");
        assert_eq!(e["frontend_version"], "9.9.9");
        assert_eq!(e["install_id"], "install-abc");
        assert_eq!(e["lib_version"], crate::VERSION);
        assert!(e.get("os").is_some());
        assert!(e.get("arch").is_some());
    }

    #[tokio::test]
    async fn session_start_includes_env_and_config() {
        let sink = Arc::new(MemorySink::default());
        let t = Telemetry::with_sink(&frontend(), "id-1", sink.clone());
        let env = EnvFingerprint {
            os: "linux".into(),
            arch: "x86_64".into(),
            terminal: Some("tmux".into()),
            shell: Some("zsh".into()),
            color_mode: Some("truecolor".into()),
        };
        let config = ConfigSnapshot::from_config(&crate::config::Config::default());
        t.session_start(&env, &config);
        t.flush().await;

        let events = sink.events();
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!(e["event_type"], "session_start");
        assert_eq!(e["terminal"], "tmux");
        assert_eq!(e["shell"], "zsh");
        assert!(e["config"].is_object());
    }

    #[tokio::test]
    async fn disabled_handle_records_nothing() {
        let sink = Arc::new(MemorySink::default());
        // A disabled handle has no sender, so events never reach any sink.
        let t = Telemetry::disabled();
        assert!(!t.is_active());
        t.feature("x");
        t.flush().await; // no-op, must not hang
        assert!(sink.events().is_empty());
    }
}
