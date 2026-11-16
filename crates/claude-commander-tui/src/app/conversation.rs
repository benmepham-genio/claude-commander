//! TUI side of conversation mode.
//!
//! The conversation's content (history, in-flight reply, status) lives in a
//! shared [`ConversationView`] behind an `Arc<Mutex<…>>`. The off-loop bridge
//! task updates that model directly from the session stream (and feeds the
//! speaker), so it advances whether or not the overlay is open and never
//! depends on the UI event loop. The overlay is a *pure view*: it locks the
//! model and renders it. This satisfies the "runs fully headless; the window is
//! just a log + input" requirement.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use super::*;
use claude_commander_core::agent::AgentKind;
use claude_commander_core::conversation::{
    AUDIO_SUPPORTED, ConversationEvent, ConversationSession, DictationSubmit, ListenAction,
    ListenerHandle, MediaSignal, SpeakerCommand, SpeakerHandle, SubmitPlan, Transcript, VoiceMode,
    apply_listen_action, media_signal, plan_dictation, spawn_listener, spawn_media_gate,
    spawn_speaker,
};
use claude_commander_core::tmux::{PaneInfo, PaneInjector};

/// Where dictation works, for when it was asked for somewhere it doesn't.
const NOWHERE_TO_DICTATE: &str = "Open a text field or attach to a session to dictate";

/// The status shown while a dictation records. Spelt as the attach loop's
/// pane notice spells it (core `tmux/attach.rs`), so both surfaces agree.
const DICTATING: &str = "● Dictating… (Alt-t to type)";

/// Status when no microphone opened; a build without `audio` says so instead of blaming the device.
fn voice_unavailable_message(audio_supported: bool) -> &'static str {
    if audio_supported {
        "Voice input unavailable — no microphone?"
    } else {
        "Voice isn't available in this build (built without audio support)"
    }
}

/// Canonical project spinner frames (advanced every 3 render ticks).
const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Preamble seeded into the conversation session's `CLAUDE.md` (`{name}` is
/// replaced with the configured assistant name). Followed by the generated
/// `claude-commander` CLI reference so the agent can inspect live session/
/// project state. Tuned for spoken replies.
const CONVERSATION_PRIME: &str = "\
# {name}

You are {name}, a voice assistant for Claude Commander (the `claude-commander`
CLI), which manages the user's Claude coding sessions across their projects. The
user is *talking* to you, and your replies are read aloud by text-to-speech.

## Tone
You are *speaking*, not writing. Talk like a person would out loud.
- Lead with the answer. Give the conclusion or result directly — don't narrate
  your reasoning, your plan, or the steps you took to find it.
- Keep it short: a sentence or two for most things. Be succinct, but don't drop
  details the user actually needs — brevity, not vagueness.
- Plain spoken English prose only. No markdown, headings, bullet lists, tables,
  or code blocks — they sound terrible read aloud.
- Don't read out code, long file paths, IDs, or long lists verbatim; summarise
  them in words (\"three sessions, all running\" — not each name and hash).
- It's fine to ask a quick follow-up question instead of guessing.

## Checking current state
Don't guess about the user's sessions or projects — inspect the live state with
the CLI (it needs no approval). Good first commands:
- `claude-commander list` — all current sessions, their projects and status.
- `claude-commander status <name>` — detail on one session.
- `claude-commander log <name>` — recent output from a session.
Run `claude-commander list` early when the user asks anything about what's
going on. You can read anything on the filesystem the user can.

## Working directory
This directory is your own scratch space; nothing else in it matters.
";

/// File (in the conversation scratch dir) holding the last headless session id,
/// so the next launch resumes the same conversation via `--resume`.
const SESSION_ID_FILE: &str = "session-id";

/// Read the persisted session id to resume, if any. A missing/blank file means
/// "start fresh" (e.g. the very first launch). Async (`tokio::fs`) so it never
/// blocks the runtime from the spawn path.
async fn read_resume_id(dir: &Path) -> Option<String> {
    let id = tokio::fs::read_to_string(dir.join(SESSION_ID_FILE))
        .await
        .ok()?;
    let id = id.trim();
    (!id.is_empty()).then(|| id.to_string())
}

/// Persist the current session id so the next launch can resume it. Best-effort:
/// a write failure just means the next launch starts a fresh conversation.
/// Async (`tokio::fs`) so it never blocks the bridge task.
async fn write_resume_id(dir: &Path, id: &str) {
    if let Err(e) = tokio::fs::write(dir.join(SESSION_ID_FILE), id).await {
        warn!(target: "conversation", "failed to persist session id: {e}");
    }
}

/// Who authored a conversation message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConvRole {
    User,
    Assistant,
}

/// A finalized turn in the conversation history.
#[derive(Debug, Clone)]
pub struct ConvMessage {
    pub role: ConvRole,
    pub text: String,
}

/// Lifecycle status of the conversation, shown in the overlay.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ConvStatus {
    #[default]
    Idle,
    Thinking,
    Error(String),
}

/// The shared conversation model. Updated off-loop by the bridge task (and by
/// `submit`); read by the renderer. The single source of truth for the overlay.
#[derive(Default)]
pub struct ConversationView {
    /// Finalized turns.
    pub messages: Vec<ConvMessage>,
    /// In-progress assistant text (streaming).
    pub streaming: String,
    pub status: ConvStatus,
    pub session_id: Option<String>,
    /// When the last text delta arrived — drives the "working…" spinner when a
    /// reply stalls (the agent is running tools, producing no text).
    pub last_delta_at: Option<Instant>,
    /// User messages sent while a turn was still in progress. The session queues
    /// them and answers them in order; we defer *displaying* each until the
    /// preceding reply completes, so history stays correctly ordered.
    pub pending_user: std::collections::VecDeque<String>,
    /// When the current agent turn began (user message handed to the session).
    /// Drives the stage-2 latency traces; `None` between turns.
    turn_started_at: Option<Instant>,
    /// Whether time-to-first-token has already been logged for this turn (so we
    /// emit it once, at the first delta).
    first_token_logged: bool,
}

impl ConversationView {
    /// Apply a session event to the display model (audio is handled separately
    /// by the bridge). Pure + unit-tested.
    pub fn apply(&mut self, ev: &ConversationEvent) {
        match ev {
            ConversationEvent::Started { session_id } => {
                self.session_id = Some(session_id.clone());
                if self.status != ConvStatus::Thinking {
                    self.status = ConvStatus::Idle;
                }
            }
            ConversationEvent::Delta(text) => {
                self.last_delta_at = Some(Instant::now());
                // Stage 2a: time-to-first-token — how long the agent spent
                // thinking / running tools before it started speaking.
                if let Some(t0) = self.turn_started_at
                    && !self.first_token_logged
                {
                    self.first_token_logged = true;
                    tracing::debug!(
                        target: "conversation",
                        "timing [agent] first token after {} ms (think/tool latency)",
                        t0.elapsed().as_millis()
                    );
                }
                self.streaming.push_str(text);
            }
            ConversationEvent::Break => {
                // A new text segment: separate it from the previous one, which
                // streamed with no trailing separator.
                if !self.streaming.is_empty() && !self.streaming.ends_with(char::is_whitespace) {
                    self.streaming.push_str("\n\n");
                }
            }
            ConversationEvent::TurnComplete => {
                // Stage 2b: total agent turn — submission to last token.
                if let Some(t0) = self.turn_started_at.take() {
                    tracing::debug!(
                        target: "conversation",
                        "timing [agent] turn complete in {} ms total",
                        t0.elapsed().as_millis()
                    );
                }
                self.finalize_streaming();
                // If the user queued a message while this reply streamed, show it
                // now (after the reply) and stay Thinking — it's answered next.
                if let Some(next) = self.pending_user.pop_front() {
                    self.messages.push(ConvMessage {
                        role: ConvRole::User,
                        text: next,
                    });
                    self.status = ConvStatus::Thinking;
                    // The queued turn starts being answered now — restart the clock.
                    self.turn_started_at = Some(Instant::now());
                    self.first_token_logged = false;
                } else {
                    self.status = ConvStatus::Idle;
                }
            }
            ConversationEvent::Error(e) => {
                self.turn_started_at = None;
                self.finalize_streaming();
                self.status = ConvStatus::Error(e.clone());
            }
            ConversationEvent::Exited => {
                self.turn_started_at = None;
                self.finalize_streaming();
                self.status = ConvStatus::Error("session ended".to_string());
            }
        }
    }

    fn finalize_streaming(&mut self) {
        let text = std::mem::take(&mut self.streaming);
        let text = text.trim();
        if !text.is_empty() {
            self.messages.push(ConvMessage {
                role: ConvRole::Assistant,
                text: text.to_string(),
            });
        }
    }

    /// Record a user turn. Shown immediately when idle; when a reply is still in
    /// progress it's queued for display until that reply completes (the session
    /// has already received it and will answer it next).
    fn push_user(&mut self, text: String) {
        if self.status == ConvStatus::Thinking {
            self.pending_user.push_back(text);
        } else {
            self.messages.push(ConvMessage {
                role: ConvRole::User,
                text,
            });
            self.status = ConvStatus::Thinking;
            self.last_delta_at = None;
            // Start the stage-2 clock: this turn is now in the agent's hands.
            self.turn_started_at = Some(Instant::now());
            self.first_token_logged = false;
        }
    }

    fn set_error(&mut self, msg: impl Into<String>) {
        self.status = ConvStatus::Error(msg.into());
    }
}

/// Conversation runtime held on `App`. The model is shared with the bridge; the
/// session handle stays here (only `submit` writes to its stdin).
#[derive(Default)]
pub struct ConversationRuntime {
    /// The headless `claude` subprocess; `None` until first opened / after exit.
    /// Shared behind an async mutex so a transcript can be submitted off the UI
    /// loop (e.g. while the main loop is parked in a tmux attach) as well as from
    /// the typed-input path on the loop.
    pub session: Arc<tokio::sync::Mutex<Option<ConversationSession>>>,
    /// Shared display model — updated off-loop, read by the renderer.
    pub view: Arc<Mutex<ConversationView>>,
    /// Respawn-stable handle to the voice-input listener's command channel.
    /// Empty until the listener comes up (STT off / no mic keeps it empty).
    /// Shared with the IPC task and the in-attach interceptor so swapping the
    /// listener (e.g. on a microphone change) is visible to all of them at once.
    pub listener: ListenerHandle,
    /// Whether the microphone is currently capturing. Shared (`Arc`) so every
    /// trigger — the Alt-V key path, the in-attach byte interceptor, and the
    /// external Unix-socket toggle — observes and flips the one flag without
    /// copying state in and out of the attach loop.
    pub recording: Arc<AtomicBool>,
    /// Media-gate signal channel: pauses other players for the voice turn and
    /// resumes them after the reply. `None` when the feature is disabled.
    pub gate: Option<tokio::sync::mpsc::UnboundedSender<MediaSignal>>,
    /// Respawn-stable handle to the current session's TTS speaker. Held here so
    /// the long-lived listener (interrupt on record-start) and the submit path
    /// (resume for the new reply) reach the speaker even as it's recreated on
    /// each session respawn.
    pub speaker: SpeakerHandle,
    /// The way in to whatever pane is attached, for dictation. Long-lived like
    /// the listener and for the same reason: the transcript consumer task is
    /// spawned once and outlives every individual attach, so it cannot be handed
    /// a per-attach writer. Each attach installs its own channel into this one
    /// handle, and a [`send`](PaneInjector::send) that returns `false` *is* the
    /// "nothing is attached" answer ([`is_attached`](PaneInjector::is_attached)
    /// is only the advisory pre-check for starting a recording).
    pub injector: PaneInjector,
    /// Where the transcript consumer leaves a dictation that no pane took,
    /// for the UI loop to place (see [`DictationMailbox`]).
    pub dictations: DictationMailbox,
    /// Whether the recording in progress, if any, was started by the UI's own
    /// Alt-T. Display only — it picks the conversation overlay's recording
    /// indicator. What the *transcript* may do rides with the recording itself
    /// ([`VoiceMode::UiDictation`]), never with this.
    pub ui_dictating: bool,
}

/// What a dictation produced, for the UI loop to deliver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DictationOutcome {
    /// The transcript, already normalised to one line
    /// ([`normalise_dictation`](claude_commander_core::conversation::normalise_dictation)),
    /// so every text field can take it as-is.
    Text(String),
    /// The recording transcribed to nothing.
    NothingHeard,
    /// Transcription failed; carries the engine's error.
    Failed(String),
}

/// The hand-off from the transcript consumer task to the UI loop, for a
/// dictation with no attached pane to go to.
///
/// Its own unbounded channel rather than a [`StateUpdate`] on the event loop's,
/// for two reasons. That channel is bounded, and nothing drains it while the
/// loop is parked in an attach or the editor (the render ticks fill it), so a
/// hand-off there would stall the consumer — and every dictation queued behind
/// it. And [`EventLoop::restart_input`] discards whatever it holds when an
/// attach returns, which would lose the transcript without a word. The UI loop
/// drains this on every tick instead ([`App::drain_dictations`]).
pub struct DictationMailbox {
    tx: tokio::sync::mpsc::UnboundedSender<DictationOutcome>,
    rx: tokio::sync::mpsc::UnboundedReceiver<DictationOutcome>,
}

impl Default for DictationMailbox {
    fn default() -> Self {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        Self { tx, rx }
    }
}

impl DictationMailbox {
    /// The consumer task's end.
    pub fn sender(&self) -> tokio::sync::mpsc::UnboundedSender<DictationOutcome> {
        self.tx.clone()
    }
}

impl ConversationRuntime {
    /// Apply a voice [`ListenAction`] if a listener is up, tagging the recording
    /// with the [`VoiceMode`] that decides where its transcript goes. Returns the
    /// new recording state, or `None` when there's no listener (STT off / no
    /// mic).
    ///
    /// The mode rides with the *start*, not the stop: one microphone serves both
    /// modes, so a recording started by Alt-V stays a conversation turn even if
    /// Alt-T stops it.
    pub fn apply_listen(&self, action: ListenAction, mode: VoiceMode) -> Option<bool> {
        if !self.listener.is_present() {
            return None;
        }
        Some(apply_listen_action(
            &self.listener,
            &self.recording,
            action,
            mode,
        ))
    }

    /// Whether the microphone is currently capturing.
    pub fn is_recording(&self) -> bool {
        self.recording.load(Ordering::Acquire)
    }
}

/// Write one user turn to the session and record it in the shared view. Returns
/// `false` if there's no live session (caller may respawn and retry). Lives free
/// of `&App` so it can run off the UI loop — from the typed-input path *and* from
/// the off-loop voice-submit task (which keeps working during a tmux attach).
async fn submit_to_session(
    session: &Arc<tokio::sync::Mutex<Option<ConversationSession>>>,
    view: &Arc<Mutex<ConversationView>>,
    speaker: &SpeakerHandle,
    text: String,
) -> bool {
    let mut guard = session.lock().await;
    let Some(s) = guard.as_mut() else {
        return false;
    };
    match s.send_user_message(&text).await {
        Ok(()) => {
            drop(guard);
            view.lock().unwrap().push_user(text);
            // The new query is in — lift any mute set when the user started
            // recording, so its reply (not the interrupted one) is spoken.
            speaker.send(SpeakerCommand::Resume);
            true
        }
        Err(e) => {
            tracing::warn!(target: "conversation", "send failed: {e}");
            false
        }
    }
}

/// Spawn the headless `claude` session, the TTS speaker, and the off-loop bridge
/// that feeds the speaker + updates the shared model. Idempotent (a no-op if a
/// session is already running). Free of `&App` so it can run off the UI loop —
/// the listener's submit task calls it to lazily heal the session when a voice
/// transcript arrives before the conversation was ever opened.
async fn spawn_session_runtime(
    conv: &claude_commander_core::config::ConversationConfig,
    gate: Option<tokio::sync::mpsc::UnboundedSender<MediaSignal>>,
    session: &Arc<tokio::sync::Mutex<Option<ConversationSession>>>,
    view: &Arc<Mutex<ConversationView>>,
    speaker_handle: &SpeakerHandle,
    cli_reference: &str,
) {
    if session.lock().await.is_some() {
        return;
    }
    let dir = match Config::data_dir() {
        Ok(d) => d.join("conversation"),
        Err(e) => {
            view.lock().unwrap().set_error(format!("no data dir: {e}"));
            return;
        }
    };
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        view.lock().unwrap().set_error(format!("mkdir failed: {e}"));
        return;
    }

    // Seed CLAUDE.md so the agent knows it's a (spoken) Claude Commander
    // assistant and how to inspect live session/project state. Rewritten on
    // each (re)spawn so the embedded CLI reference stays current.
    let prime = CONVERSATION_PRIME.replace("{name}", &conv.name);
    let claude_md = format!("{}\n{}", prime.trim_end(), cli_reference);
    if let Err(e) = tokio::fs::write(dir.join("CLAUDE.md"), claude_md).await {
        warn!(target: "conversation", "failed to write CLAUDE.md: {e}");
    }

    // Streaming-TTS speaker (fed directly by the bridge, off the UI loop).
    // Failure (e.g. no audio device) is non-fatal: chat still works, silent.
    let speaker = if conv.enabled {
        match spawn_speaker(conv.clone(), gate.clone()) {
            Ok(tx) => Some(tx),
            Err(e) => {
                warn!(target: "conversation", "TTS unavailable: {e}");
                None
            }
        }
    } else {
        None
    };
    // Publish the (possibly None) speaker so the long-lived listener/submit paths
    // reach this session's speaker; overwrites any prior speaker on respawn.
    speaker_handle.set(speaker.clone());

    let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel::<ConversationEvent>();
    // Resume the previous conversation if we have a stored session id, so the
    // agent keeps its history (and memory of the user) across restarts.
    let resume = read_resume_id(&dir).await;
    match ConversationSession::spawn(
        &conv.command,
        &conv.permission_mode,
        &dir,
        resume.as_deref(),
        ev_tx,
    ) {
        Ok(s) => *session.lock().await = Some(s),
        Err(e) => {
            view.lock().unwrap().set_error(e.to_string());
            return;
        }
    }

    // Bridge: entirely off the UI loop. Feeds the speaker AND updates the
    // shared model, so audio and the conversation log both advance while the
    // overlay is closed / the main loop is busy. The renderer just reads the
    // model when visible.
    let bridge_view = view.clone();
    let bridge_dir = dir.clone();
    let bridge_gate = gate.clone();
    tokio::spawn(async move {
        while let Some(ev) = ev_rx.recv().await {
            // Let the media gate know the text turn finished, so it can resume
            // media once the spoken reply (if any) has drained.
            if matches!(ev, ConversationEvent::TurnComplete) {
                media_signal(&bridge_gate, MediaSignal::TurnComplete);
            }
            if matches!(ev, ConversationEvent::Delta(_)) {
                tracing::trace!(target: "conversation", "bridge delta");
            } else {
                tracing::debug!(target: "conversation", "bridge event: {ev:?}");
            }
            // Persist the session id so the next launch resumes this
            // conversation. Claude may fork a fresh id on resume, so record
            // whatever the latest init reports.
            if let ConversationEvent::Started { session_id } = &ev {
                write_resume_id(&bridge_dir, session_id).await;
            }
            if let Some(sp) = &speaker
                && let Some(cmd) = claude_commander_core::conversation::speaker_command_for(&ev)
            {
                let _ = sp.send(cmd);
            }
            bridge_view.lock().unwrap().apply(&ev);
        }
        tracing::debug!(target: "conversation", "bridge task ended");
    });

    view.lock().unwrap().status = ConvStatus::Idle;
}

/// Type one dictated transcript into whatever pane is attached, then submit it
/// if the policy says so — never for a [`VoiceMode::UiDictation`], which was
/// meant for a dialog. With nothing attached, the outcome goes to the UI loop
/// instead (a [`DictationMailbox`]), which types it into the open modal's text
/// field — so dictation reaches whatever is on screen, pane or dialog.
///
/// Free of `&App` for the same reason [`submit_to_session`] is: it runs on the
/// long-lived transcript consumer task, and while a session is attached the UI
/// loop is parked inside `drive_attach` — so anything that needed the app would
/// never run at exactly the moment dictation is used.
async fn dictate(
    injector: &PaneInjector,
    policy: DictationSubmit,
    to_ui: &tokio::sync::mpsc::UnboundedSender<DictationOutcome>,
    transcript: &Transcript,
) {
    let hand_to_ui = |outcome| {
        if to_ui.send(outcome).is_err() {
            warn!(target: "conversation", "UI loop gone; dropped a dictation");
        }
    };
    let policy = match transcript.mode {
        VoiceMode::UiDictation => DictationSubmit::Never,
        _ => policy,
    };
    // The attach loop is holding a "● Transcribing…" notice in the pane's
    // status line (see `PaneInput::Notice`); every branch below replaces it, so
    // the operator is never left looking at a state that has already ended.
    if let Some(err) = &transcript.error {
        warn!(target: "conversation", "dictation transcription failed: {err}");
        if !injector.notice(format!("✗ Transcription failed: {err}"), false) {
            hand_to_ui(DictationOutcome::Failed(err.clone()));
        }
        return;
    }
    // With no pane ever recorded, fall back to the most restrictive descriptor
    // the planner accepts: a shell running an unknown harness. `Agent` never
    // submits into one, and `Always` was an explicit choice to submit anywhere,
    // so the fallback cannot turn a transcript into a command someone's shell
    // runs. It matters even though the `send` below would fail with nothing
    // attached, because the pane is read before the send and an attach can end
    // in between.
    let pane = injector.pane().unwrap_or(PaneInfo {
        kind: AttachKind::Shell,
        agent: AgentKind::Unknown,
    });
    let Some(plan) = plan_dictation(&transcript.text, policy, pane) else {
        debug!(target: "conversation", "dictated transcript was empty; nothing to type");
        if !injector.notice("✗ Nothing heard", false) {
            hand_to_ui(DictationOutcome::NothingHeard);
        }
        return;
    };
    // The transcript is the user's speech, so its length is logged and its text
    // is not — the same rule the typed-content telemetry schema follows.
    debug!(
        target: "conversation",
        chars = plan.text.chars().count(),
        ?pane,
        "typing a dictated transcript into the attached pane"
    );
    if !injector.send(plan.text.as_bytes()) {
        // `send` returning false *is* the "nothing is attached" signal (see
        // `PaneInjector`), so the UI loop — which owns `&mut App`, and with it
        // the open modal — gets the text instead. The plan's text, not the
        // raw transcript: it is already one line, and the submit half of the
        // plan is deliberately dropped, because a dialog's Enter commits it.
        debug!(target: "conversation", "no attached pane; handing the transcript to the UI loop");
        hand_to_ui(DictationOutcome::Text(plan.text));
        return;
    }
    if let SubmitPlan::Submit { delay } = plan.submit {
        // Some harnesses need a beat to read the typed text as its own
        // keystrokes before an Enter means "send this" rather than "send what
        // you had".
        if let Some(delay) = delay {
            tokio::time::sleep(delay).await;
        }
        if !injector.send(b"\r") {
            warn!(target: "conversation", "the pane went away before the dictated submit");
            return;
        }
    }
    // The text on screen is the real confirmation; this only retires the held
    // "Transcribing…" so it does not outlive the work it described.
    injector.notice("✓ Typed", false);
}

impl App {
    /// Alt-c: open the overlay (starting the session on first use) or close it
    /// (leaving the session running). The single `conversation.enabled` setting
    /// gates the whole feature — when off, the overlay doesn't open at all.
    pub(super) async fn toggle_conversation_overlay(&mut self) {
        if matches!(self.ui_state.modal, Modal::Conversation { .. }) {
            self.ui_state.modal = Modal::None;
            return;
        }
        if !self.config.conversation.enabled {
            self.set_status_message(
                "Conversation mode is disabled — enable it in Settings ▸ Voice",
                4,
            );
            return;
        }
        self.ensure_conversation_started().await;
        self.ui_state.modal = Modal::Conversation {
            input: Input::default(),
            scroll: 0,
        };
    }

    /// Bring up the full conversation runtime: the mic listener (eager, via
    /// [`ensure_listener_started`](Self::ensure_listener_started)) plus the
    /// headless `claude` session, TTS speaker, and off-loop bridge. Idempotent.
    pub(super) async fn ensure_conversation_started(&mut self) {
        self.ensure_listener_started().await;
        let conv = self.config.conversation.clone();
        let gate = self.conversation.gate.clone();
        spawn_session_runtime(
            &conv,
            gate,
            &self.conversation.session,
            &self.conversation.view,
            &self.conversation.speaker,
            &self.cli_reference,
        )
        .await;
    }

    /// Bring up *just* the microphone listener (and its off-loop submit task),
    /// without the heavy headless session. Idempotent and gated on `stt.enabled`.
    ///
    /// Called eagerly at startup so an external global hotkey (the Unix-socket
    /// toggle) — or in-app Alt-V — can toggle recording even before the
    /// conversation overlay is ever opened. The submit task spawns the session
    /// lazily on the first transcript, so the mic costs nothing until used.
    pub(super) async fn ensure_listener_started(&mut self) {
        if !self.config.stt.enabled || self.conversation.listener.is_present() {
            return;
        }

        // Media gate: pause other players while recording voice and until the
        // assistant finishes its spoken reply. Shared by the listener and the
        // (later) speaker; created once and reused across session respawns.
        if self.conversation.gate.is_none() && self.config.stt.pause_media {
            self.conversation.gate = spawn_media_gate(true);
        }
        self.build_and_store_listener();
    }

    /// Whether the saved `pause_media` differs from what the running app does.
    ///
    /// The media gate is spawned once, with the listener, and never respawned:
    /// toggling the setting afterwards changes nothing until a restart, in
    /// either direction. Before the listener starts nothing has read the
    /// setting yet, so there is nothing pending.
    pub(super) fn pause_media_needs_restart(&self) -> bool {
        self.conversation.listener.is_present()
            && self.config.stt.pause_media != self.conversation.gate.is_some()
    }

    /// Rebuild the voice listener with the current STT config after the selected
    /// microphone changes, so the new device takes effect on the next recording
    /// without an app restart. A no-op when STT is off or no listener is running
    /// (enabling STT still comes up at boot). Replacing the sender in the shared
    /// [`ListenerHandle`] drops the old one, which ends the old listener task —
    /// and with it the old recorder thread, releasing the previous device.
    pub(super) fn respawn_listener(&mut self) {
        if !self.config.stt.enabled || !self.conversation.listener.is_present() {
            return;
        }
        // A device swap cancels any in-flight capture (the old recorder thread
        // dies with its buffer); reset the flag so state stays consistent.
        // (Narrow race: an external socket toggle firing during the rebuild
        // below could set the flag and Start the *old*, about-to-be-dropped
        // recorder. It self-heals on the next toggle, so we don't lock here.)
        self.conversation.recording.store(false, Ordering::Release);
        self.conversation.ui_dictating = false;
        self.build_and_store_listener();
    }

    /// Build a listener from the current STT config and its off-loop transcript
    /// submit task, installing the command sender in the shared handle. The
    /// listener returns its sender immediately and opens the mic device inside
    /// its own task, so this never blocks the UI. Installing the new sender drops
    /// the previous one (if any), ending the old listener and releasing its
    /// device as that task unwinds; the new device opens asynchronously (the two
    /// can briefly overlap), and any commands sent before it's ready queue on the
    /// channel and run once it is. Assumes the media gate is set up.
    fn build_and_store_listener(&mut self) {
        let gate = self.conversation.gate.clone();
        let speaker = self.conversation.speaker.clone();
        let (tx_text, mut rx_text) = tokio::sync::mpsc::unbounded_channel::<Transcript>();
        let tx = spawn_listener(
            self.config.stt.clone(),
            tx_text,
            gate.clone(),
            speaker.clone(),
        );
        self.conversation.listener.replace(tx);
        let session = self.conversation.session.clone();
        let view = self.conversation.view.clone();
        let conv = self.config.conversation.clone();
        // The lazy-heal path below writes the conversation's CLAUDE.md, so this
        // off-loop task needs its own copy of the injected CLI reference.
        let cli_reference = self.cli_reference.clone();
        // Dictation's two halves: where to type, and whether an Enter follows.
        // The policy is captured here rather than read per transcript because
        // this task holds no `&App` — a settings change calls `respawn_listener`,
        // which rebuilds this task with the new value.
        let injector = self.conversation.injector.clone();
        let policy = self.config.stt.dictation_submit;
        let to_ui = self.conversation.dictations.sender();
        tokio::spawn(async move {
            while let Some(transcript) = rx_text.recv().await {
                match transcript.mode {
                    VoiceMode::Dictation | VoiceMode::UiDictation => {
                        dictate(&injector, policy, &to_ui, &transcript).await;
                    }
                    VoiceMode::Conversation => {
                        let text = transcript.text.trim().to_string();
                        if text.is_empty() {
                            continue;
                        }
                        // Submit directly (off the UI loop) so voice input works
                        // even while the main loop is parked in a tmux attach. If
                        // no session is up yet (transcript arrived before the
                        // overlay was opened), spawn one and retry once.
                        if !submit_to_session(&session, &view, &speaker, text.clone()).await {
                            spawn_session_runtime(
                                &conv,
                                gate.clone(),
                                &session,
                                &view,
                                &speaker,
                                &cli_reference,
                            )
                            .await;
                            submit_to_session(&session, &view, &speaker, text).await;
                        }
                        // If the user spoke from inside an attach, the pane's
                        // status line is holding "● Transcribing…"; retire it.
                        // Best-effort — false (no attach) just means the TUI's
                        // own status bar was visible all along.
                        injector.notice(format!("✓ Sent to {}", conv.name), false);
                    }
                }
            }
        });
    }

    /// Start the external voice-toggle trigger so the hotkey works from anywhere
    /// on the desktop: a portable Unix-domain socket backing `claude-commander
    /// listen-toggle`, which a desktop global shortcut (e.g. a KDE Plasma custom
    /// *command* shortcut) runs to toggle recording — feeding the same listener +
    /// recording flag as in-app Alt-V. No-op when the mic listener didn't come up
    /// (STT off / no microphone).
    pub(super) fn spawn_listen_ipc(&self) {
        if !self.conversation.listener.is_present() {
            debug!(target: "conversation", "voice IPC not started: no mic listener");
            return;
        }
        // The IPC task holds a clone of the respawn-stable handle, so a later
        // microphone change (which swaps the sender inside the handle) keeps the
        // desktop hotkey driving the *current* listener.
        let listener = self.conversation.listener.clone();
        let recording = self.conversation.recording.clone();
        let path = claude_commander_core::conversation::ipc::default_socket_path();
        if let Err(e) = claude_commander_core::conversation::ipc::serve(path, listener, recording) {
            warn!(target: "conversation", "voice IPC socket unavailable: {e}");
        }
    }

    /// Send a typed user turn to the session. The session serializes turns, so a
    /// message sent mid-reply is queued and answered after — we never interrupt.
    /// Self-heals a dead session by respawning and retrying once.
    pub(super) async fn submit_conversation_input(&mut self, text: String) {
        self.ensure_conversation_started().await;
        if submit_to_session(
            &self.conversation.session,
            &self.conversation.view,
            &self.conversation.speaker,
            text.clone(),
        )
        .await
        {
            return;
        }
        // Session likely exited (idle timeout / crash) — respawn and retry once.
        *self.conversation.session.lock().await = None;
        self.ensure_conversation_started().await;
        if !submit_to_session(
            &self.conversation.session,
            &self.conversation.view,
            &self.conversation.speaker,
            text,
        )
        .await
        {
            self.conversation
                .view
                .lock()
                .unwrap()
                .set_error("session not running".to_string());
        }
    }

    /// Alt-V: toggle voice input. First press starts recording the microphone;
    /// the next press stops it and submits the transcript to the conversation
    /// agent. Works whether the overlay is open or not, mirroring spoken replies.
    pub(super) async fn toggle_voice_input(&mut self) {
        if !self.config.stt.enabled {
            self.set_status_message(
                "Voice input is disabled — enable STT in Settings ▸ Voice",
                4,
            );
            return;
        }
        // Bring the session (and listener) up on first use, just like typing.
        self.ensure_conversation_started().await;
        match self
            .conversation
            .apply_listen(ListenAction::Toggle, VoiceMode::Conversation)
        {
            Some(true) => {
                self.conversation.ui_dictating = false;
                self.set_status_message("● Listening… (Alt-V to send)", 60);
            }
            Some(false) => self.set_status_message("● Transcribing…", 4),
            None => self.set_status_message(voice_unavailable_message(AUDIO_SUPPORTED), 4),
        }
    }

    /// Alt-T with the TUI's own UI on screen: start or stop dictating into the
    /// open modal's text field (a review comment, the new-session name, the
    /// conversation overlay's input…).
    ///
    /// The mirror of [`toggle_voice_input`](Self::toggle_voice_input), except
    /// that dictation needs somewhere to type, so it only *starts* a recording
    /// when the open modal has a text field for it. Reaching this method at all
    /// means the UI loop is running — while a session is attached the loop is
    /// parked inside `drive_attach`, and Alt-T is recognised there by the
    /// attach loop's byte interceptor instead. A *stop* is always honoured,
    /// whatever is open: the transcript is placed wherever is on screen when it
    /// comes back (see [`apply_dictation`](Self::apply_dictation)).
    ///
    /// Only the listener is brought up, never the headless conversation session:
    /// dictation never speaks to the agent, so paying for one would be a
    /// subprocess the feature has no use for.
    pub(super) async fn toggle_dictation(&mut self) {
        if !self.config.stt.enabled {
            self.set_status_message(
                "Voice input is disabled — enable STT in Settings ▸ Voice",
                4,
            );
            return;
        }
        self.ensure_listener_started().await;
        if !self.conversation.is_recording() {
            // A live attach means the modal is the in-session switcher, drawn
            // over a parked pane: its transcript would go to that pane (or be
            // lost with the attach), not into the search box on screen.
            if self.conversation.injector.is_attached() {
                self.set_status_message("Close the switcher to dictate into the session", 4);
                return;
            }
            if !self.modal_accepts_dictation() {
                self.set_status_message(NOWHERE_TO_DICTATE, 4);
                return;
            }
        }
        match self
            .conversation
            .apply_listen(ListenAction::Toggle, VoiceMode::UiDictation)
        {
            Some(true) => {
                self.conversation.ui_dictating = true;
                self.set_status_message(DICTATING, 60);
            }
            Some(false) => self.set_status_message("● Transcribing…", 4),
            None => self.set_status_message(voice_unavailable_message(AUDIO_SUPPORTED), 4),
        }
    }

    /// Place every dictation the transcript consumer has left in the mailbox.
    /// Called on each render tick, so a hand-off waits at most one tick.
    pub(super) fn drain_dictations(&mut self) {
        while let Ok(outcome) = self.conversation.dictations.rx.try_recv() {
            self.apply_dictation(outcome);
        }
    }

    /// Deliver a dictation that had no attached pane to go to: type it into
    /// the open modal's text field, or say why nothing happened.
    pub(super) fn apply_dictation(&mut self, outcome: DictationOutcome) {
        match outcome {
            DictationOutcome::Text(text) => {
                if self.dictate_into_modal(&text) {
                    self.set_status_message("✓ Typed", 2);
                } else {
                    // The dialog closed (or the user detached) while they
                    // spoke. Say where dictation does work rather than
                    // reporting a failure they can't act on.
                    self.set_status_message(NOWHERE_TO_DICTATE, 4);
                }
            }
            DictationOutcome::NothingHeard => self.set_status_message("✗ Nothing heard", 4),
            DictationOutcome::Failed(err) => {
                self.set_status_message(format!("✗ Transcription failed: {err}"), 6);
            }
        }
    }

    /// Show a transient status-bar message for `secs` seconds.
    pub(super) fn set_status_message(&mut self, msg: impl Into<String>, secs: u64) {
        self.ui_state.status_message = Some((
            msg.into(),
            Instant::now() + std::time::Duration::from_secs(secs),
        ));
    }

    /// Render the full-screen conversation overlay (a pure view of the model).
    pub(super) fn render_conversation_modal(
        &self,
        frame: &mut Frame,
        area: Rect,
        input: &Input,
        scroll: u16,
    ) {
        frame.render_widget(Clear, area);
        let block = Block::default()
            .title(" Conversation ")
            .borders(Borders::ALL)
            .border_type(self.border_type())
            .border_style(Style::default().fg(self.theme.modal_info));
        // Inset by one column so the text lines up with the title (which renders
        // two cells in: corner + line char), plus a top/bottom gap.
        let inner = block.inner(area).inner(Margin {
            horizontal: 1,
            vertical: 1,
        });
        frame.render_widget(block, area);

        // Layout: history (fills), then the input row bracketed by rules.
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(3)])
            .split(inner);

        // Build the history lines from the shared model.
        let width = chunks[0].width.max(1) as usize;
        let mut lines: Vec<Line> = Vec::new();
        {
            let view = self.conversation.view.lock().unwrap();
            for msg in &view.messages {
                self.push_message_lines(&mut lines, msg.role, &msg.text, width);
            }
            if !view.streaming.is_empty() {
                // The reply is arriving — the text itself is the progress indicator.
                self.push_message_lines(&mut lines, ConvRole::Assistant, &view.streaming, width);
            }
            // Progress / error indicator below the (partial) reply.
            match &view.status {
                ConvStatus::Thinking => {
                    // While tokens actively stream the text is the indicator;
                    // once it stalls (agent thinking / running a tool) show a
                    // spinner so the silence doesn't read as a hang.
                    let streaming_now = view
                        .last_delta_at
                        .is_some_and(|t| t.elapsed() < std::time::Duration::from_millis(700));
                    if !streaming_now {
                        let frame_glyph = SPINNER_FRAMES
                            [(self.ui_state.tick_count as usize / 3) % SPINNER_FRAMES.len()];
                        let label = if view.streaming.is_empty() {
                            "thinking…"
                        } else {
                            "working…"
                        };
                        lines.push(Line::from(Span::styled(
                            format!("{frame_glyph} {label}"),
                            Style::default().fg(self.theme.conversation_accent),
                        )));
                    }
                }
                ConvStatus::Error(e) => {
                    lines.push(Line::from(Span::styled(
                        format!("⚠ {e}"),
                        Style::default().fg(self.theme.modal_error),
                    )));
                }
                ConvStatus::Idle => {}
            }
        }
        if lines.is_empty() {
            lines.push(Line::from(Span::styled(
                "Type a message and press Enter. Replies stream in and are spoken aloud.",
                Style::default().fg(self.theme.text_secondary),
            )));
        }
        let view_h = chunks[0].height as usize;
        let total = lines.len();
        let bottom_start = total.saturating_sub(view_h);
        let start = bottom_start.saturating_sub(scroll as usize);
        let end = (start + view_h).min(total);
        let visible: Vec<Line> = lines[start..end].to_vec();
        frame.render_widget(Paragraph::new(visible), chunks[0]);

        // Input: a single prompt line bracketed by top/bottom rules. The
        // overlay covers the status bar, so a live status message (a
        // dictation's "✓ Typed" or "✗ Nothing heard", say) is drawn into the
        // bottom rule instead — except while recording, when the input row
        // already says so.
        let mut input_block = Block::default()
            .borders(Borders::TOP | Borders::BOTTOM)
            .border_type(self.border_type())
            .border_style(Style::default().fg(self.theme.conversation_accent));
        if !self.conversation.is_recording()
            && let Some((msg, expires)) = &self.ui_state.status_message
            && Instant::now() < *expires
        {
            input_block = input_block.title_bottom(Line::from(Span::styled(
                format!(" {msg} "),
                Style::default().fg(self.theme.text_secondary),
            )));
        }
        let input_inner = input_block.inner(chunks[1]);
        frame.render_widget(input_block, chunks[1]);

        const PROMPT: &str = "› ";
        let prompt_w = PROMPT.chars().count() as u16;
        frame.render_widget(
            Paragraph::new(PROMPT).style(Style::default().fg(self.theme.text_secondary)),
            Rect {
                width: prompt_w.min(input_inner.width),
                ..input_inner
            },
        );
        let text_area = Rect {
            x: input_inner.x + prompt_w,
            width: input_inner.width.saturating_sub(prompt_w),
            ..input_inner
        };
        let text_width = text_area.width.max(1);
        let view_scroll = input.visual_scroll(text_width as usize);
        if self.conversation.is_recording() {
            // Recording takes over the input row — show a live indicator instead
            // of the typing placeholder. One microphone serves both keys, so
            // the indicator says which one this recording will answer to.
            let indicator = if self.conversation.ui_dictating {
                DICTATING
            } else {
                "● Listening… (Alt-V to send)"
            };
            frame.render_widget(
                Paragraph::new(indicator).style(
                    Style::default()
                        .fg(self.theme.modal_error)
                        .add_modifier(Modifier::BOLD),
                ),
                text_area,
            );
        } else if input.value().is_empty() {
            frame.render_widget(
                Paragraph::new("Type a message…")
                    .style(Style::default().fg(self.theme.text_secondary)),
                text_area,
            );
        } else {
            frame.render_widget(
                Paragraph::new(input.value())
                    .scroll((0, view_scroll as u16))
                    .style(Style::default().fg(self.theme.text_primary)),
                text_area,
            );
        }
        // Real cursor in the text field.
        let col = (input.visual_cursor().saturating_sub(view_scroll)) as u16;
        frame.set_cursor_position((
            text_area.x + col.min(text_width.saturating_sub(1)),
            text_area.y,
        ));
    }

    /// Append a message's wrapped lines (role header + body) to `lines`.
    fn push_message_lines(
        &self,
        lines: &mut Vec<Line<'static>>,
        role: ConvRole,
        text: &str,
        width: usize,
    ) {
        let (label, color) = match role {
            ConvRole::User => ("You", self.theme.text_accent),
            ConvRole::Assistant => (
                self.config.conversation.name.as_str(),
                self.theme.conversation_accent,
            ),
        };
        lines.push(Line::from(Span::styled(
            format!("{label}:"),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )));
        for raw in text.split('\n') {
            if raw.is_empty() {
                lines.push(Line::from(String::new()));
                continue;
            }
            for chunk in wrap_text(raw, width) {
                lines.push(Line::from(chunk));
            }
        }
        lines.push(Line::from(String::new())); // blank between messages
    }

    /// Conversation overlay key handling. Returns `true` if the key was consumed.
    pub(super) async fn handle_conversation_key(
        &mut self,
        key: crossterm::event::KeyEvent,
    ) -> bool {
        use crossterm::event::{KeyCode, KeyModifiers};

        let Modal::Conversation { input, scroll } = &mut self.ui_state.modal else {
            return false;
        };
        match key.code {
            // Esc, Alt-c again, or Ctrl-q close the overlay (session keeps running).
            KeyCode::Esc => {
                self.ui_state.modal = Modal::None;
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::ALT) => {
                self.ui_state.modal = Modal::None;
            }
            KeyCode::Char('q') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.ui_state.modal = Modal::None;
            }
            KeyCode::Enter => {
                let text = input.value().trim().to_string();
                *input = Input::default();
                *scroll = 0; // snap back to the live bottom
                if !text.is_empty() {
                    self.submit_conversation_input(text).await;
                }
            }
            KeyCode::PageUp => {
                *scroll = scroll.saturating_add(10);
            }
            KeyCode::PageDown => {
                *scroll = scroll.saturating_sub(10);
            }
            _ => {
                super::edit_text_input(input, key);
            }
        }
        true
    }
}

/// Greedy word-wrap to `width` columns (by char count). Always returns at least
/// one segment for non-empty input.
fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_string()];
    }
    let mut out = Vec::new();
    let mut line = String::new();
    for word in text.split(' ') {
        // A single word longer than the width is hard-split across rows.
        if word.chars().count() > width {
            if !line.is_empty() {
                out.push(std::mem::take(&mut line));
            }
            for ch in word.chars() {
                if line.chars().count() == width {
                    out.push(std::mem::take(&mut line));
                }
                line.push(ch);
            }
            continue;
        }
        if line.is_empty() {
            line.push_str(word);
        } else if line.chars().count() + 1 + word.chars().count() <= width {
            line.push(' ');
            line.push_str(word);
        } else {
            out.push(std::mem::take(&mut line));
            line.push_str(word);
        }
    }
    if !line.is_empty() {
        out.push(line);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use claude_commander_core::tmux::PaneInput;

    #[test]
    fn voice_unavailable_names_a_build_without_audio() {
        let without = voice_unavailable_message(false);
        assert!(without.contains("this build"), "{without}");
        assert!(!without.contains("microphone"), "{without}");
        assert!(voice_unavailable_message(true).contains("microphone"));
    }

    /// An installed injector plus the pump's end of its channel, and an event
    /// channel standing in for the UI loop.
    fn wired() -> (
        PaneInjector,
        tokio::sync::mpsc::UnboundedReceiver<PaneInput>,
        tokio::sync::mpsc::UnboundedSender<DictationOutcome>,
        tokio::sync::mpsc::UnboundedReceiver<DictationOutcome>,
    ) {
        let injector = PaneInjector::default();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        injector.install(tx);
        let (ev_tx, ev_rx) = tokio::sync::mpsc::unbounded_channel();
        (injector, rx, ev_tx, ev_rx)
    }

    fn heard(text: &str) -> Transcript {
        Transcript {
            mode: VoiceMode::Dictation,
            text: text.into(),
            error: None,
        }
    }

    fn drain(rx: &mut tokio::sync::mpsc::UnboundedReceiver<PaneInput>) -> Vec<PaneInput> {
        let mut out = Vec::new();
        while let Ok(i) = rx.try_recv() {
            out.push(i);
        }
        out
    }

    #[tokio::test]
    async fn dictate_types_then_retires_the_held_notice() {
        let (injector, mut rx, ev_tx, _ev_rx) = wired();
        dictate(
            &injector,
            DictationSubmit::Never,
            &ev_tx,
            &heard("ls -la\n"),
        )
        .await;
        assert_eq!(
            drain(&mut rx),
            vec![
                PaneInput::Bytes(b"ls -la".to_vec()),
                PaneInput::Notice {
                    text: "✓ Typed".into(),
                    hold: false
                },
            ]
        );
    }

    #[tokio::test]
    async fn dictate_submit_sends_enter_before_the_notice() {
        let (injector, mut rx, ev_tx, _ev_rx) = wired();
        injector.set_pane(PaneInfo {
            kind: AttachKind::Agent,
            agent: AgentKind::Claude,
        });
        dictate(
            &injector,
            DictationSubmit::Agent,
            &ev_tx,
            &heard("run tests"),
        )
        .await;
        assert_eq!(
            drain(&mut rx),
            vec![
                PaneInput::Bytes(b"run tests".to_vec()),
                PaneInput::Bytes(b"\r".to_vec()),
                PaneInput::Notice {
                    text: "✓ Typed".into(),
                    hold: false
                },
            ]
        );
    }

    #[tokio::test]
    async fn dictate_empty_says_nothing_heard_and_types_nothing() {
        let (injector, mut rx, ev_tx, _ev_rx) = wired();
        dictate(&injector, DictationSubmit::Always, &ev_tx, &heard("  \n ")).await;
        assert_eq!(
            drain(&mut rx),
            vec![PaneInput::Notice {
                text: "✗ Nothing heard".into(),
                hold: false
            }]
        );
    }

    #[tokio::test]
    async fn dictate_failure_reports_it_in_the_pane() {
        let (injector, mut rx, ev_tx, _ev_rx) = wired();
        let failed = Transcript {
            mode: VoiceMode::Dictation,
            text: String::new(),
            error: Some("STT down".into()),
        };
        dictate(&injector, DictationSubmit::Never, &ev_tx, &failed).await;
        let out = drain(&mut rx);
        assert_eq!(out.len(), 1);
        assert!(matches!(
            &out[0],
            PaneInput::Notice { text, hold: false } if text.contains("STT down")
        ));
    }

    /// The one dictation the UI loop was handed, if any.
    fn handed_to_ui(
        ev_rx: &mut tokio::sync::mpsc::UnboundedReceiver<DictationOutcome>,
    ) -> Option<DictationOutcome> {
        ev_rx.try_recv().ok()
    }

    #[tokio::test]
    async fn dictate_with_no_attach_hands_the_text_to_the_ui_loop() {
        // Nothing installed: the UI loop gets the text, so the open modal's
        // text field (a review comment, say) can take it.
        let injector = PaneInjector::default();
        let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel();
        dictate(&injector, DictationSubmit::Never, &ev_tx, &heard("hello")).await;
        assert_eq!(
            handed_to_ui(&mut ev_rx),
            Some(DictationOutcome::Text("hello".into()))
        );
    }

    #[tokio::test]
    async fn dictate_with_no_attach_hands_over_one_line_and_never_a_submit() {
        // A dialog's Enter commits it, so the hand-off is the normalised text
        // only — no newline from the engine, and no submit even under `Always`.
        let injector = PaneInjector::default();
        let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel();
        dictate(
            &injector,
            DictationSubmit::Always,
            &ev_tx,
            &heard("  use a\nhelper here\n"),
        )
        .await;
        assert_eq!(
            handed_to_ui(&mut ev_rx),
            Some(DictationOutcome::Text("use a helper here".into()))
        );
        assert_eq!(handed_to_ui(&mut ev_rx), None, "exactly one hand-off");
    }

    #[tokio::test]
    async fn dictate_with_no_attach_reports_nothing_heard_to_the_ui_loop() {
        let injector = PaneInjector::default();
        let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel();
        dictate(&injector, DictationSubmit::Never, &ev_tx, &heard(" \n ")).await;
        assert_eq!(
            handed_to_ui(&mut ev_rx),
            Some(DictationOutcome::NothingHeard)
        );
    }

    #[tokio::test]
    async fn dictate_with_no_attach_reports_a_failure_to_the_ui_loop() {
        // Without this the error went to a pane notice nobody could see.
        let injector = PaneInjector::default();
        let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel();
        let failed = Transcript {
            mode: VoiceMode::Dictation,
            text: String::new(),
            error: Some("STT down".into()),
        };
        dictate(&injector, DictationSubmit::Never, &ev_tx, &failed).await;
        assert_eq!(
            handed_to_ui(&mut ev_rx),
            Some(DictationOutcome::Failed("STT down".into()))
        );
    }

    #[tokio::test]
    async fn a_ui_started_dictation_is_never_submitted_even_into_a_pane() {
        // Started in a dialog, it can still land in a pane attached while it
        // transcribed — insert-only there, whatever `dictation_submit` says.
        // The mode rides with that one recording, so the *next* transcript
        // (a pane-started `Dictation`) is still submitted as configured.
        let (injector, mut rx, ev_tx, _ev_rx) = wired();
        injector.set_pane(PaneInfo {
            kind: AttachKind::Agent,
            agent: AgentKind::Unknown,
        });
        let ui_started = Transcript {
            mode: VoiceMode::UiDictation,
            ..heard("run tests")
        };
        dictate(&injector, DictationSubmit::Always, &ev_tx, &ui_started).await;
        assert!(
            !drain(&mut rx).contains(&PaneInput::Bytes(b"\r".to_vec())),
            "a UI-started dictation must never submit"
        );
        dictate(
            &injector,
            DictationSubmit::Always,
            &ev_tx,
            &heard("run tests"),
        )
        .await;
        assert!(drain(&mut rx).contains(&PaneInput::Bytes(b"\r".to_vec())));
    }

    #[tokio::test]
    async fn the_mailbox_hands_over_without_blocking_and_in_order() {
        // Unbounded so the consumer can never stall behind a UI loop parked
        // in an attach (the event channel it replaced was bounded and full).
        let mailbox = DictationMailbox::default();
        let injector = PaneInjector::default();
        let tx = mailbox.sender();
        for n in 0..300 {
            dictate(
                &injector,
                DictationSubmit::Never,
                &tx,
                &heard(&format!("w{n}")),
            )
            .await;
        }
        let mut rx = mailbox.rx;
        assert_eq!(
            rx.try_recv().ok(),
            Some(DictationOutcome::Text("w0".into()))
        );
        let mut last = None;
        while let Ok(o) = rx.try_recv() {
            last = Some(o);
        }
        assert_eq!(last, Some(DictationOutcome::Text("w299".into())));
    }

    #[tokio::test]
    async fn dictate_into_an_attached_pane_tells_the_ui_loop_nothing() {
        // The pane is the destination whenever one is attached: its notices
        // are the feedback, and the UI loop must not also type the text into
        // a modal hidden behind the attach.
        let (injector, _rx, ev_tx, mut ev_rx) = wired();
        dictate(&injector, DictationSubmit::Never, &ev_tx, &heard("hello")).await;
        dictate(&injector, DictationSubmit::Never, &ev_tx, &heard("  ")).await;
        assert_eq!(handed_to_ui(&mut ev_rx), None);
    }

    #[test]
    fn wrap_basic() {
        assert_eq!(wrap_text("hello world foo", 11), vec!["hello world", "foo"]);
    }

    #[tokio::test]
    async fn resume_id_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        // No file yet → start fresh.
        assert_eq!(read_resume_id(dir.path()).await, None);
        // After persisting, the same id comes back for the next launch.
        write_resume_id(dir.path(), "abc-123").await;
        assert_eq!(
            read_resume_id(dir.path()).await,
            Some("abc-123".to_string())
        );
        // A blank/whitespace file is treated as "no id" (start fresh).
        write_resume_id(dir.path(), "  \n").await;
        assert_eq!(read_resume_id(dir.path()).await, None);
    }

    #[test]
    fn wrap_long_word_hard_splits() {
        assert_eq!(wrap_text("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
    }

    #[test]
    fn wrap_short_fits_on_one_line() {
        assert_eq!(wrap_text("hi there", 80), vec!["hi there"]);
    }

    #[test]
    fn view_streams_and_finalizes_a_turn() {
        let mut v = ConversationView::default();
        v.push_user("hi".into());
        assert_eq!(v.status, ConvStatus::Thinking);
        assert_eq!(v.messages.len(), 1); // the user message
        v.apply(&ConversationEvent::Started {
            session_id: "s".into(),
        });
        v.apply(&ConversationEvent::Delta("Hello ".into()));
        v.apply(&ConversationEvent::Delta("there.".into()));
        assert_eq!(v.streaming, "Hello there.");
        v.apply(&ConversationEvent::TurnComplete);
        assert_eq!(v.status, ConvStatus::Idle);
        assert_eq!(v.streaming, "");
        assert_eq!(v.messages.last().unwrap().role, ConvRole::Assistant);
        assert_eq!(v.messages.last().unwrap().text, "Hello there.");
    }

    #[test]
    fn agent_turn_timer_tracks_turn_boundaries() {
        let mut v = ConversationView::default();
        // No turn in flight initially.
        assert!(v.turn_started_at.is_none());

        // Submitting a turn starts the stage-2 clock.
        v.push_user("hi".into());
        assert!(v.turn_started_at.is_some());
        assert!(!v.first_token_logged);

        // First delta marks time-to-first-token (logged once).
        v.apply(&ConversationEvent::Delta("Hel".into()));
        assert!(v.first_token_logged);
        assert!(v.turn_started_at.is_some()); // still timing the total turn
        v.apply(&ConversationEvent::Delta("lo.".into()));
        assert!(v.first_token_logged); // not re-armed by later deltas

        // Completing the turn (nothing queued) stops the clock.
        v.apply(&ConversationEvent::TurnComplete);
        assert!(v.turn_started_at.is_none());

        // A queued message promoted at TurnComplete restarts the clock.
        v.push_user("one".into());
        v.apply(&ConversationEvent::Delta("a".into()));
        v.push_user("two".into()); // queued while Thinking
        v.apply(&ConversationEvent::TurnComplete);
        assert!(v.turn_started_at.is_some()); // re-armed for the queued turn
        assert!(!v.first_token_logged);
    }

    #[test]
    fn agent_turn_timer_cleared_on_error() {
        let mut v = ConversationView::default();
        v.push_user("hi".into());
        v.apply(&ConversationEvent::Error("boom".into()));
        assert!(v.turn_started_at.is_none());
    }

    #[test]
    fn view_queues_a_message_sent_mid_reply() {
        let mut v = ConversationView::default();
        v.push_user("first".into()); // Thinking
        v.apply(&ConversationEvent::Delta("answering".into()));
        // A second message while Thinking is queued, not shown.
        v.push_user("second".into());
        assert_eq!(v.pending_user.len(), 1);
        let user_msgs = v
            .messages
            .iter()
            .filter(|m| m.role == ConvRole::User)
            .count();
        assert_eq!(user_msgs, 1, "second message is deferred");
        // When the reply completes, the queued message is shown and we stay Thinking.
        v.apply(&ConversationEvent::TurnComplete);
        assert!(v.pending_user.is_empty());
        assert_eq!(v.status, ConvStatus::Thinking);
        assert_eq!(v.messages.last().unwrap().text, "second");
    }

    #[test]
    fn view_break_inserts_separator() {
        let mut v = ConversationView::default();
        v.apply(&ConversationEvent::Delta("First part.".into()));
        v.apply(&ConversationEvent::Break);
        v.apply(&ConversationEvent::Delta("Second part.".into()));
        assert_eq!(v.streaming, "First part.\n\nSecond part.");
    }
}
