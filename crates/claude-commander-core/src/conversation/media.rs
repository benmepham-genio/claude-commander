//! Best-effort media playback control around a voice turn.
//!
//! Opening the microphone for voice input (Alt-V) pauses whatever the user was
//! listening to — on Bluetooth headsets PipeWire force-switches the card to the
//! HFP/HSP profile (which destroys the A2DP sink and pauses the player), and on
//! every device we'd rather the recording wasn't talking over music anyway. None
//! of that auto-resumes when the mic closes, so the music is left stranded.
//!
//! This module pauses the user's media players when recording starts and resumes
//! them once the assistant has finished its spoken reply — so the music is held
//! for the whole voice turn and comes back when the conversation goes quiet.
//!
//! It's entirely best-effort: every action shells out to `playerctl` (Linux) or
//! `osascript` (macOS) and silently no-ops when that tooling — or any media
//! player — is absent (e.g. a headless box, or Windows). It never blocks or
//! breaks voice input or TTS.
//!
//! The decision logic ([`MediaGate`]) and the output parsers are pure and
//! unit-tested; only [`spawn_media_gate`]'s task and the platform `backend`
//! touch the outside world.

use tokio::sync::mpsc;

/// Players to wait for the agent to start speaking before giving up and assuming
/// the turn produced no speech (TTS disabled, nothing speakable). Kept generous
/// because TTS for a short reply can start a beat after the text turn completes.
const GRACE: std::time::Duration = std::time::Duration::from_millis(2000);
/// Failsafe: resume no matter what this long after recording stopped, in case the
/// turn-complete / speaking-ended signals never arrive (e.g. a TTS server hang).
/// Reset on any speaking activity so a genuinely long spoken reply isn't cut off.
const SAFETY: std::time::Duration = std::time::Duration::from_secs(120);

/// A snapshot of which external media players were playing when recording began,
/// in the identifier form the platform backend understands (playerctl player
/// names on Linux, application names on macOS).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MediaSnapshot {
    players: Vec<String>,
}

impl MediaSnapshot {
    pub fn is_empty(&self) -> bool {
        self.players.is_empty()
    }
}

/// Playback status of a media player, as reported by `playerctl status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayerStatus {
    Playing,
    Paused,
    Stopped,
    Other,
}

/// Parse a single `playerctl status` line.
///
/// Only the Linux backend shells out to `playerctl` (macOS drives players via
/// AppleScript), so these three parsers are dead code on other targets; `test`
/// keeps their unit tests running everywhere.
#[cfg(any(target_os = "linux", test))]
fn parse_status(stdout: &str) -> PlayerStatus {
    match stdout.trim() {
        "Playing" => PlayerStatus::Playing,
        "Paused" => PlayerStatus::Paused,
        "Stopped" => PlayerStatus::Stopped,
        _ => PlayerStatus::Other,
    }
}

/// Parse `playerctl --list-all` output into player names, dropping the
/// "No players found" sentinel and blank lines.
#[cfg(any(target_os = "linux", test))]
fn parse_player_list(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && *l != "No players found")
        .map(str::to_owned)
        .collect()
}

/// Of the players we paused, which should we resume now: those still `Paused`.
/// Players the user manually resumed/stopped, or that have since disappeared, are
/// left alone.
#[cfg(any(target_os = "linux", test))]
fn resume_targets(snapshot: &MediaSnapshot, current: &[(String, PlayerStatus)]) -> Vec<String> {
    snapshot
        .players
        .iter()
        .filter(|p| {
            current
                .iter()
                .any(|(name, status)| name == *p && *status == PlayerStatus::Paused)
        })
        .cloned()
        .collect()
}

/// Signals that drive the [`MediaGate`]. Emitted from the listener (record +
/// silence), the conversation bridge (turn complete), and the speaker (speaking
/// start/end); the gate task itself feeds the two timeouts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaSignal {
    /// The microphone opened (Alt-V pressed to record).
    RecordStarted,
    /// The microphone closed (Alt-V pressed again / recording ended).
    RecordStopped,
    /// No reply is coming: a conversation transcription produced nothing, or a
    /// dictation recording stopped (its text is typed, never answered).
    Silence,
    /// The assistant finished the text of its turn.
    TurnComplete,
    /// TTS playback of the reply began.
    SpeakingStarted,
    /// TTS playback of the reply fully drained.
    SpeakingEnded,
    /// Grace period after a completed turn elapsed with no speech (internal).
    GraceTimeout,
    /// Failsafe deadline elapsed (internal).
    SafetyTimeout,
}

/// What the gate decided a signal should do to media playback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateAction {
    /// Snapshot the playing players and pause them.
    Pause,
    /// Resume the players we paused.
    Resume,
    /// Do nothing.
    None,
}

/// Pure state machine deciding when to pause/resume around a voice turn.
///
/// Media is paused on the first `RecordStarted` and stays paused — across
/// re-records within the same exchange — until the turn is both textually
/// complete and done being spoken (or it's clear nothing will be spoken). The
/// `armed` flag means "we are holding a pause we haven't resumed yet".
#[derive(Debug, Default)]
pub struct MediaGate {
    /// We paused media and have not resumed it yet.
    armed: bool,
    /// The mic is currently open.
    recording: bool,
    /// The assistant's text turn has completed.
    turn_complete: bool,
    /// TTS is currently playing.
    speaking: bool,
    /// TTS started at least once since recording began.
    spoke: bool,
}

impl MediaGate {
    /// Apply a signal, returning the action the driver should take.
    pub fn on(&mut self, signal: MediaSignal) -> GateAction {
        match signal {
            MediaSignal::RecordStarted => {
                self.recording = true;
                self.turn_complete = false;
                self.speaking = false;
                self.spoke = false;
                if self.armed {
                    GateAction::None
                } else {
                    self.armed = true;
                    GateAction::Pause
                }
            }
            MediaSignal::RecordStopped => {
                self.recording = false;
                self.maybe_resume()
            }
            MediaSignal::Silence => {
                // Nothing was said, so nothing will be spoken back.
                self.turn_complete = true;
                self.maybe_resume()
            }
            MediaSignal::TurnComplete => {
                // The text turn is done, but a spoken reply may still be starting
                // or playing — don't resume yet; the grace timeout or
                // `SpeakingEnded` will. (The driver arms the grace timer.)
                self.turn_complete = true;
                GateAction::None
            }
            MediaSignal::SpeakingStarted => {
                self.speaking = true;
                self.spoke = true;
                GateAction::None
            }
            MediaSignal::SpeakingEnded => {
                self.speaking = false;
                self.maybe_resume()
            }
            MediaSignal::GraceTimeout => {
                // The turn completed and nothing ever started speaking — treat it
                // as a silent turn and resume.
                if self.spoke {
                    GateAction::None
                } else {
                    self.maybe_resume()
                }
            }
            MediaSignal::SafetyTimeout => {
                if self.armed && !self.recording {
                    self.armed = false;
                    GateAction::Resume
                } else {
                    GateAction::None
                }
            }
        }
    }

    /// Abort a pause the driver couldn't act on (nothing was playing), so the
    /// gate doesn't keep waiting to resume a snapshot that doesn't exist.
    pub fn abort_pause(&mut self) {
        self.armed = false;
    }

    fn maybe_resume(&mut self) -> GateAction {
        if self.armed && !self.recording && self.turn_complete && !self.speaking {
            self.armed = false;
            GateAction::Resume
        } else {
            GateAction::None
        }
    }
}

/// Start the media-gate task, if media pausing is enabled. Returns a sender for
/// [`MediaSignal`]s; dropping it ends the task. Returns `None` when disabled, so
/// callers can cheaply skip emitting signals.
pub fn spawn_media_gate(enabled: bool) -> Option<mpsc::UnboundedSender<MediaSignal>> {
    if !enabled {
        return None;
    }
    let (tx, mut rx) = mpsc::unbounded_channel::<MediaSignal>();
    tokio::spawn(async move {
        let mut gate = MediaGate::default();
        let mut snapshot: Option<MediaSnapshot> = None;
        let mut grace: Option<tokio::time::Instant> = None;
        let mut safety: Option<tokio::time::Instant> = None;

        loop {
            let next = [grace, safety].into_iter().flatten().min();
            let signal = tokio::select! {
                biased;
                sig = rx.recv() => match sig {
                    Some(s) => s,
                    None => break, // all senders dropped
                },
                _ = async {
                    match next {
                        Some(d) => tokio::time::sleep_until(d).await,
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    let now = tokio::time::Instant::now();
                    // Fire whichever deadline elapsed (grace first; it's shorter).
                    if grace.is_some_and(|d| now >= d) {
                        grace = None;
                        MediaSignal::GraceTimeout
                    } else {
                        safety = None;
                        MediaSignal::SafetyTimeout
                    }
                }
            };

            // Timer policy (kept out of the pure gate): arm the failsafe when
            // recording stops, the grace timer when the turn completes, and
            // cancel grace as soon as speech starts.
            match signal {
                MediaSignal::RecordStopped => safety = Some(tokio::time::Instant::now() + SAFETY),
                MediaSignal::TurnComplete => grace = Some(tokio::time::Instant::now() + GRACE),
                MediaSignal::SpeakingStarted => grace = None,
                _ => {}
            }

            match gate.on(signal) {
                GateAction::Pause => {
                    let snap = backend::snapshot().await;
                    if snap.is_empty() {
                        gate.abort_pause();
                    } else {
                        backend::pause(&snap).await;
                        snapshot = Some(snap);
                    }
                }
                GateAction::Resume => {
                    if let Some(snap) = snapshot.take() {
                        backend::resume(&snap).await;
                    }
                    grace = None;
                    safety = None;
                }
                GateAction::None => {}
            }
        }
    });
    Some(tx)
}

/// Best-effort, fire-and-forget signal send: a closed/absent gate is a no-op.
pub fn signal(gate: &Option<mpsc::UnboundedSender<MediaSignal>>, sig: MediaSignal) {
    if let Some(tx) = gate {
        let _ = tx.send(sig);
    }
}

#[cfg(target_os = "linux")]
mod backend {
    use super::{MediaSnapshot, PlayerStatus, parse_player_list, parse_status, resume_targets};
    use tracing::debug;

    /// Run `playerctl` with `args`, returning trimmed stdout on success. Any
    /// failure (binary missing, no players, non-zero exit) is a quiet `None`.
    async fn playerctl(args: &[&str]) -> Option<String> {
        match tokio::process::Command::new("playerctl")
            .args(args)
            .output()
            .await
        {
            Ok(out) if out.status.success() => {
                Some(String::from_utf8_lossy(&out.stdout).into_owned())
            }
            Ok(_) => None,
            Err(e) => {
                debug!(target: "conversation", "playerctl unavailable: {e}");
                None
            }
        }
    }

    pub async fn snapshot() -> MediaSnapshot {
        let Some(list) = playerctl(&["--list-all"]).await else {
            return MediaSnapshot::default();
        };
        let mut players = Vec::new();
        for name in parse_player_list(&list) {
            if let Some(status) = playerctl(&["-p", &name, "status"]).await
                && parse_status(&status) == PlayerStatus::Playing
            {
                players.push(name);
            }
        }
        MediaSnapshot { players }
    }

    pub async fn pause(snapshot: &MediaSnapshot) {
        for player in &snapshot.players {
            let _ = playerctl(&["-p", player, "pause"]).await;
        }
    }

    pub async fn resume(snapshot: &MediaSnapshot) {
        let mut current = Vec::new();
        for player in &snapshot.players {
            if let Some(status) = playerctl(&["-p", player, "status"]).await {
                current.push((player.clone(), parse_status(&status)));
            }
        }
        for player in resume_targets(snapshot, &current) {
            let _ = playerctl(&["-p", &player, "play"]).await;
        }
    }
}

#[cfg(target_os = "macos")]
mod backend {
    use super::MediaSnapshot;
    use tracing::debug;

    /// The MPRIS-less macOS players we know how to drive via AppleScript. Each is
    /// guarded by `is running` so we never *launch* an app just to query it.
    const APPS: [&str; 2] = ["Spotify", "Music"];

    async fn osascript(script: &str) -> Option<String> {
        match tokio::process::Command::new("osascript")
            .arg("-e")
            .arg(script)
            .output()
            .await
        {
            Ok(out) if out.status.success() => {
                Some(String::from_utf8_lossy(&out.stdout).trim().to_owned())
            }
            Ok(_) => None,
            Err(e) => {
                debug!(target: "conversation", "osascript unavailable: {e}");
                None
            }
        }
    }

    /// `player state` of `app` ("playing"/"paused"/"stopped"), or `None` if the
    /// app isn't running.
    async fn player_state(app: &str) -> Option<String> {
        let script = format!(
            "if application \"{app}\" is running then return (player state of application \"{app}\") as text"
        );
        let out = osascript(&script).await?;
        (!out.is_empty()).then_some(out)
    }

    pub async fn snapshot() -> MediaSnapshot {
        let mut players = Vec::new();
        for app in APPS {
            if player_state(app).await.as_deref() == Some("playing") {
                players.push(app.to_string());
            }
        }
        MediaSnapshot { players }
    }

    pub async fn pause(snapshot: &MediaSnapshot) {
        for app in &snapshot.players {
            let _ = osascript(&format!(
                "if application \"{app}\" is running then tell application \"{app}\" to pause"
            ))
            .await;
        }
    }

    pub async fn resume(snapshot: &MediaSnapshot) {
        for app in &snapshot.players {
            // Only resume if we left it paused (don't fight the user).
            if player_state(app).await.as_deref() == Some("paused") {
                let _ = osascript(&format!("tell application \"{app}\" to play")).await;
            }
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod backend {
    use super::MediaSnapshot;

    pub async fn snapshot() -> MediaSnapshot {
        MediaSnapshot::default()
    }
    pub async fn pause(_snapshot: &MediaSnapshot) {}
    pub async fn resume(_snapshot: &MediaSnapshot) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(players: &[&str]) -> MediaSnapshot {
        MediaSnapshot {
            players: players.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn parses_player_statuses() {
        assert_eq!(parse_status("Playing\n"), PlayerStatus::Playing);
        assert_eq!(parse_status("Paused"), PlayerStatus::Paused);
        assert_eq!(parse_status("Stopped\n"), PlayerStatus::Stopped);
        assert_eq!(parse_status(""), PlayerStatus::Other);
    }

    #[test]
    fn parses_player_list_dropping_sentinel() {
        assert_eq!(
            parse_player_list("Plexamp\nspotify\n"),
            vec!["Plexamp".to_string(), "spotify".to_string()]
        );
        assert!(parse_player_list("No players found\n").is_empty());
        assert!(parse_player_list("\n\n").is_empty());
    }

    #[test]
    fn resume_targets_only_paused_snapshotted_players() {
        let snapshot = snap(&["Plexamp", "spotify"]);
        let current = [
            ("Plexamp".to_string(), PlayerStatus::Paused), // we paused it, still paused → resume
            ("spotify".to_string(), PlayerStatus::Playing), // user resumed it → leave it
            ("chromium".to_string(), PlayerStatus::Paused), // not ours → ignore
        ];
        assert_eq!(resume_targets(&snapshot, &current), vec!["Plexamp"]);
    }

    #[test]
    fn resume_targets_skips_vanished_players() {
        let snapshot = snap(&["Plexamp"]);
        // Player gone from the current list entirely.
        assert!(resume_targets(&snapshot, &[]).is_empty());
    }

    #[test]
    fn pauses_once_on_record_then_resumes_after_speaking() {
        let mut gate = MediaGate::default();
        assert_eq!(gate.on(MediaSignal::RecordStarted), GateAction::Pause);
        assert_eq!(gate.on(MediaSignal::RecordStopped), GateAction::None);
        // Reply streams + is spoken.
        assert_eq!(gate.on(MediaSignal::SpeakingStarted), GateAction::None);
        assert_eq!(gate.on(MediaSignal::TurnComplete), GateAction::None);
        // Still speaking the tail — no resume yet.
        assert_eq!(gate.on(MediaSignal::SpeakingEnded), GateAction::Resume);
    }

    #[test]
    fn turn_complete_before_speaking_ends_waits_for_speech() {
        let mut gate = MediaGate::default();
        gate.on(MediaSignal::RecordStarted);
        gate.on(MediaSignal::RecordStopped);
        gate.on(MediaSignal::SpeakingStarted);
        // Turn text done, but audio still playing.
        assert_eq!(gate.on(MediaSignal::TurnComplete), GateAction::None);
        assert_eq!(gate.on(MediaSignal::SpeakingEnded), GateAction::Resume);
    }

    #[test]
    fn empty_transcript_resumes_immediately() {
        let mut gate = MediaGate::default();
        gate.on(MediaSignal::RecordStarted);
        gate.on(MediaSignal::RecordStopped);
        assert_eq!(gate.on(MediaSignal::Silence), GateAction::Resume);
    }

    #[test]
    fn grace_timeout_resumes_when_nothing_spoke() {
        let mut gate = MediaGate::default();
        gate.on(MediaSignal::RecordStarted);
        gate.on(MediaSignal::RecordStopped);
        gate.on(MediaSignal::TurnComplete);
        // No SpeakingStarted ever (e.g. TTS off / nothing speakable).
        assert_eq!(gate.on(MediaSignal::GraceTimeout), GateAction::Resume);
    }

    #[test]
    fn grace_timeout_noop_once_speech_has_started() {
        let mut gate = MediaGate::default();
        gate.on(MediaSignal::RecordStarted);
        gate.on(MediaSignal::RecordStopped);
        gate.on(MediaSignal::TurnComplete);
        gate.on(MediaSignal::SpeakingStarted);
        // A stray grace tick must not resume mid-speech.
        assert_eq!(gate.on(MediaSignal::GraceTimeout), GateAction::None);
        assert_eq!(gate.on(MediaSignal::SpeakingEnded), GateAction::Resume);
    }

    #[test]
    fn does_not_resume_while_recording() {
        let mut gate = MediaGate::default();
        gate.on(MediaSignal::RecordStarted);
        // Speaking finished from a prior turn, but we're recording again.
        gate.on(MediaSignal::SpeakingStarted);
        gate.on(MediaSignal::TurnComplete);
        assert_eq!(gate.on(MediaSignal::SpeakingEnded), GateAction::None);
    }

    #[test]
    fn re_record_before_finish_does_not_re_pause_and_holds() {
        let mut gate = MediaGate::default();
        assert_eq!(gate.on(MediaSignal::RecordStarted), GateAction::Pause);
        gate.on(MediaSignal::RecordStopped);
        // User starts talking again before the agent replied — already paused.
        assert_eq!(gate.on(MediaSignal::RecordStarted), GateAction::None);
        gate.on(MediaSignal::RecordStopped);
        gate.on(MediaSignal::SpeakingStarted);
        gate.on(MediaSignal::TurnComplete);
        // Resume eventually, once, with the original pause still held.
        assert_eq!(gate.on(MediaSignal::SpeakingEnded), GateAction::Resume);
    }

    #[test]
    fn safety_timeout_is_a_failsafe_resume() {
        let mut gate = MediaGate::default();
        gate.on(MediaSignal::RecordStarted);
        gate.on(MediaSignal::RecordStopped);
        // Signals lost — failsafe still brings the music back.
        assert_eq!(gate.on(MediaSignal::SafetyTimeout), GateAction::Resume);
    }

    #[test]
    fn safety_timeout_noop_when_not_armed() {
        let mut gate = MediaGate::default();
        assert_eq!(gate.on(MediaSignal::SafetyTimeout), GateAction::None);
    }
}
