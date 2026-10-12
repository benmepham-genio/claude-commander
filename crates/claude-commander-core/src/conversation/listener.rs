//! Voice-input listener: owns the mic [`Recorder`] and [`SttClient`].
//!
//! Mirrors [`speaker::spawn_speaker`](crate::conversation::speaker::spawn_speaker),
//! but in the opposite direction — instead of turning text into audio, it turns
//! a finished recording into text. The app toggles capture with
//! [`ListenerCommand`]s; when a recording stops, the captured WAV is transcribed
//! and the resulting [`Transcript`] is sent on `transcript_tx` for the app to
//! route to whichever destination the recording was started for — see
//! [`VoiceMode`].

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::config::SttConfig;
use crate::conversation::media::{MediaSignal, signal as media_signal};
use crate::conversation::recorder::Recorder;
use crate::conversation::speaker::{SpeakerCommand, SpeakerHandle};
use crate::conversation::stt::SttClient;
use crate::error::TtsError;

/// Where a recording's transcript is headed. Fixed when the recording *starts*
/// (the hotkey that opened the mic picks it) and carried on that recording's
/// [`ListenerCommand::Start`], so a transcript can never be delivered to the
/// destination the user didn't ask for — including when the *other* voice hotkey
/// is the one that stops the capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceMode {
    /// The transcript is submitted to the headless conversation agent, which
    /// replies in the overlay and speaks it back.
    Conversation,
    /// The transcript is typed into the pane the attached tmux client is
    /// showing, as if the operator had typed it themselves.
    Dictation,
    /// Dictation started from the frontend's own UI, for the text field open
    /// there. Delivered exactly as [`Dictation`](Self::Dictation) is — to a
    /// pane if one is attached by the time it arrives, else to the frontend —
    /// except that it is never submitted: it was meant for a dialog, where
    /// nothing submits. A mode rather than frontend-side state because it has
    /// to stay paired with its own recording (see `PendingModes`), not with
    /// whichever transcript happens to come back next.
    UiDictation,
}

impl VoiceMode {
    /// Whether the transcript is typed somewhere, rather than spoken to the
    /// conversation agent.
    pub fn is_dictation(self) -> bool {
        matches!(self, Self::Dictation | Self::UiDictation)
    }
}

/// Commands to the listener task.
#[derive(Debug, Clone)]
pub enum ListenerCommand {
    /// Begin recording the microphone, for the given destination.
    Start(VoiceMode),
    /// Stop recording and transcribe what was captured. The destination is not
    /// repeated here: it was decided at `Start` and the listener remembers it
    /// (see `PendingModes`).
    Stop,
}

/// The result of one recording, tagged with the destination it was started
/// for.
///
/// For [`VoiceMode::Conversation`] one is sent only when there is text to
/// submit — silence and failures are absorbed here, where the speaker and media
/// gate they affect live. For [`VoiceMode::Dictation`] one is sent for **every**
/// recording, text or not: the frontend is holding a "Transcribing…" notice in
/// the attached pane's status line and needs the outcome to replace it, and it
/// is the only party that can tell the operator "nothing heard" where they are
/// looking. `error` carries the STT failure, if that is what ended it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transcript {
    pub mode: VoiceMode,
    pub text: String,
    pub error: Option<String>,
}

/// What, if anything, the frontend is told about a finished recording — see
/// [`Transcript`] for the asymmetry between the modes. Pure so it is tested
/// without a microphone.
fn transcript_for(mode: VoiceMode, outcome: &Result<String, TtsError>) -> Option<Transcript> {
    match (mode, outcome) {
        (_, Ok(text)) if !text.is_empty() => Some(Transcript {
            mode,
            text: text.clone(),
            error: None,
        }),
        (VoiceMode::Conversation, _) => None,
        (VoiceMode::Dictation | VoiceMode::UiDictation, Ok(_)) => Some(Transcript {
            mode,
            text: String::new(),
            error: None,
        }),
        (VoiceMode::Dictation | VoiceMode::UiDictation, Err(e)) => Some(Transcript {
            mode,
            text: String::new(),
            error: Some(e.to_string()),
        }),
    }
}

/// The modes of the recordings the listener has started but not yet paired with
/// a WAV, oldest first.
///
/// Recordings are strictly sequential — the flag in [`apply_listen_action`]
/// makes a second `Start` a no-op while one is live — and the recorder emits
/// their WAVs in that same order, so a plain FIFO pairs each WAV with the mode
/// its `Start` carried. The pop happens per **WAV**, not per transcript: a
/// silent or failed recording yields no transcript at all, and popping on the
/// transcript would leave that recording's mode queued to mis-tag the next one.
///
/// One known desync path remains: [`Recorder`] logs and emits *nothing* when its
/// in-memory `encode_wav` fails (`recorder.rs`, the `Command::Stop` arm), which
/// would strand a mode here and shift every later pairing by one. Practically
/// unreachable — the encode writes to a `Cursor<Vec<u8>>` with a fixed, valid
/// spec — so it is accepted rather than defended against with an id threaded
/// through the recorder.
#[derive(Debug, Default)]
struct PendingModes(VecDeque<VoiceMode>);

impl PendingModes {
    /// Remember the mode of a recording that has just been started.
    fn push(&mut self, mode: VoiceMode) {
        self.0.push_back(mode);
    }

    /// The mode of the recording that is live right now: the most recent
    /// `Start`. Earlier entries are recordings already stopped whose WAVs are
    /// still on their way, so a `Stop` always belongs to the last one.
    fn live(&self) -> Option<VoiceMode> {
        self.0.back().copied()
    }

    /// Claim the mode belonging to the WAV that just arrived. An empty queue
    /// means the pairing has desynced (see the type docs), so this falls back to
    /// [`VoiceMode::Conversation`] — the destination that existed before
    /// dictation, and the one that cannot type stray text into a live pane.
    fn pop_for_wav(&mut self) -> VoiceMode {
        self.0.pop_front().unwrap_or_else(|| {
            warn!(
                target: "conversation",
                "no pending voice mode for a finished recording; treating it as conversation"
            );
            VoiceMode::Conversation
        })
    }
}

/// The media-gate signals to send when the mic closes on a recording of mode
/// `live`.
///
/// Dictation also signals [`MediaSignal::Silence`] here: its transcript is typed
/// somewhere, never submitted to the conversation, so no `TurnComplete` or
/// spoken reply will ever arrive to release the gate — and nothing about the
/// transcription outcome changes that. Signalling at stop rather than once the
/// transcript is back resumes paused players the moment the user stops talking
/// instead of after the STT round-trip. A conversation recording holds the gate
/// until its reply has been spoken (`media.rs`).
fn stop_signals(live: Option<VoiceMode>) -> &'static [MediaSignal] {
    if live.is_some_and(VoiceMode::is_dictation) {
        &[MediaSignal::RecordStopped, MediaSignal::Silence]
    } else {
        &[MediaSignal::RecordStopped]
    }
}

/// What an external trigger wants the microphone to do. `Toggle` flips the
/// current state; `Start`/`Stop` request an absolute state (a no-op if already
/// there).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListenAction {
    Toggle,
    Start,
    Stop,
}

/// A respawn-stable handle to the current voice listener's command channel.
///
/// The listener's [`Recorder`] binds its microphone once at spawn time, so
/// changing the selected mic means tearing the listener down and building a
/// fresh one. Every voice trigger (the Alt-V key path, the in-attach byte
/// interceptor, and the Unix-socket toggle) reaches the listener *through* this
/// handle rather than holding a raw `Sender` clone, so a [`replace`] swaps the
/// device under all of them at once. Dropping the previous `Sender` (there is
/// only ever the one, held here) ends the old listener task — which in turn ends
/// its recorder thread, releasing the old device.
///
/// [`replace`]: ListenerHandle::replace
#[derive(Clone, Default)]
pub struct ListenerHandle(Arc<Mutex<Option<mpsc::UnboundedSender<ListenerCommand>>>>);

impl ListenerHandle {
    /// Install (or swap in) the current listener's command sender, dropping any
    /// previous one.
    pub fn replace(&self, tx: mpsc::UnboundedSender<ListenerCommand>) {
        *self.0.lock().unwrap() = Some(tx);
    }

    /// Whether a listener sender is currently installed. Note this means
    /// "a listener has been spawned", not "the microphone opened successfully":
    /// the sender is installed synchronously while the device opens in the
    /// background, so on a device-open failure this stays `true` but the sender
    /// is inert (sends are dropped).
    pub fn is_present(&self) -> bool {
        self.0.lock().unwrap().is_some()
    }

    /// Send a command to the current listener. Returns `false` if none is
    /// installed or the listener task has gone away.
    pub fn send(&self, cmd: ListenerCommand) -> bool {
        matches!(&*self.0.lock().unwrap(), Some(tx) if tx.send(cmd).is_ok())
    }
}

impl From<mpsc::UnboundedSender<ListenerCommand>> for ListenerHandle {
    fn from(tx: mpsc::UnboundedSender<ListenerCommand>) -> Self {
        Self(Arc::new(Mutex::new(Some(tx))))
    }
}

/// Apply a [`ListenAction`] against the shared recording flag and listener.
///
/// This is the single shared entry point for *every* voice trigger — the in-app
/// Alt-V key path, the in-attach byte interceptor, and the external Unix-socket
/// toggle — so the recording state machine stays consistent no matter where the
/// toggle originates. Returns the new recording state. Sends a
/// [`ListenerCommand`] only when the state actually changes (so a redundant
/// `Start`/`Stop` doesn't restart capture or transcribe silence).
///
/// `mode` is consulted **only** when the transition is *to* recording, because
/// that is the only transition that sends a `Start` to carry it. A trigger that
/// stops an in-progress recording passes whatever mode its own hotkey implies
/// and it is ignored — which is what lets either voice hotkey stop a recording
/// the other one started without redirecting its transcript.
pub fn apply_listen_action(
    listener: &ListenerHandle,
    recording: &AtomicBool,
    action: ListenAction,
    mode: VoiceMode,
) -> bool {
    let now = match action {
        // Atomic flip so concurrent triggers can't both observe the old value.
        ListenAction::Toggle => !recording.fetch_xor(true, Ordering::AcqRel),
        ListenAction::Start | ListenAction::Stop => {
            let want = matches!(action, ListenAction::Start);
            if recording.swap(want, Ordering::AcqRel) == want {
                return want; // already in the desired state — nothing to send
            }
            want
        }
    };
    let _ = listener.send(if now {
        ListenerCommand::Start(mode)
    } else {
        ListenerCommand::Stop
    });
    now
}

/// Start the listener task and return its [`ListenerCommand`] sender
/// *immediately* — the microphone device is opened inside the task (awaiting the
/// recorder's readiness), so a slow device never blocks the caller. Commands
/// sent before the device finishes opening queue on the channel and run once it
/// is ready. If no input device is available the task logs and exits, leaving
/// the returned sender inert. Recognized transcripts are sent on
/// `transcript_tx`, each tagged with the [`VoiceMode`] its recording was started
/// for; dropping the sender ends the task and releases the mic.
pub fn spawn_listener(
    cfg: SttConfig,
    transcript_tx: mpsc::UnboundedSender<Transcript>,
    gate: Option<mpsc::UnboundedSender<MediaSignal>>,
    speaker: SpeakerHandle,
) -> mpsc::UnboundedSender<ListenerCommand> {
    let (tx, mut rx) = mpsc::unbounded_channel::<ListenerCommand>();

    tokio::spawn(async move {
        let (wav_tx, mut wav_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        // Open the mic off the caller's task; on failure the sender goes inert.
        let recorder = match Recorder::new(wav_tx, cfg.input_device.clone()).await {
            Ok(r) => r,
            Err(e) => {
                warn!(target: "conversation", "STT unavailable: {e}");
                return;
            }
        };
        let client = SttClient::new(&cfg);
        let mut pending = PendingModes::default();
        loop {
            tokio::select! {
                // A finished recording arrived from the recorder thread.
                Some(wav) = wav_rx.recv() => {
                    // Claim this recording's destination before anything else can
                    // go wrong: the pairing is per WAV, and the branches below do
                    // not all produce a transcript. See `PendingModes`.
                    let mode = pending.pop_for_wav();
                    // Stage 1 timing: WAV bytes in → transcript out.
                    let wav_bytes = wav.len();
                    let t0 = Instant::now();
                    // Dictation released the media gate when its recording
                    // stopped (`stop_signals`), and sends no `SpeakerCommand`
                    // here either: start sent no Interrupt, so there is no mute
                    // to lift, and a Resume would un-mute a reply the user
                    // deliberately let keep playing.
                    let outcome = client.transcribe(wav).await;
                    // Dictation hears about every outcome (see `Transcript`);
                    // conversation only about text. Sent before the per-outcome
                    // bookkeeping below so the frontend's held notice is replaced
                    // as early as possible.
                    if let Some(transcript) = transcript_for(mode, &outcome)
                        && transcript_tx.send(transcript).is_err()
                    {
                        break; // app gone
                    }
                    match outcome {
                        Ok(text) if !text.is_empty() => {
                            debug!(
                                target: "conversation",
                                "timing [stt] transcribed {wav_bytes} byte WAV in {} ms ({} chars)",
                                t0.elapsed().as_millis(),
                                text.len()
                            );
                        }
                        // Empty transcript (silence) — nothing to send, but still
                        // worth timing so a slow "no speech" round-trip is visible.
                        // No reply is coming, so let any paused media resume now.
                        Ok(_) => {
                            debug!(
                                target: "conversation",
                                "timing [stt] transcribed {wav_bytes} byte WAV in {} ms (empty — silence)",
                                t0.elapsed().as_millis()
                            );
                            if matches!(mode, VoiceMode::Conversation) {
                                media_signal(&gate, MediaSignal::Silence);
                                // Nothing was said, so no new message will be submitted
                                // to lift the mute set on record-start — clear it here.
                                speaker.send(SpeakerCommand::Resume);
                            }
                        }
                        Err(e) => {
                            warn!("STT transcription failed: {e}");
                            if matches!(mode, VoiceMode::Conversation) {
                                // Failed round-trip → no reply either; don't strand media.
                                media_signal(&gate, MediaSignal::Silence);
                                // No submit will follow to unmute the speaker — do it here.
                                speaker.send(SpeakerCommand::Resume);
                            }
                        }
                    }
                },
                cmd = rx.recv() => match cmd {
                    Some(ListenerCommand::Start(mode)) => {
                        pending.push(mode);
                        if matches!(mode, VoiceMode::Conversation) {
                            // The user is starting a new message — stop speaking the
                            // current reply at once and stay muted until the new query
                            // is submitted (a no-op when nothing is speaking).
                            //
                            // Dictation is not part of the conversation, so it must
                            // not cut the reply off: someone typing a shell command
                            // by voice is still listening to what is being said.
                            speaker.send(SpeakerCommand::Interrupt);
                        }
                        // Signal *before* opening the mic: the gate snapshots the
                        // playing players concurrently, and on Bluetooth the mic
                        // opening only pauses playback ~300ms later — so the
                        // snapshot reads "Playing" before the device pause lands.
                        media_signal(&gate, MediaSignal::RecordStarted);
                        recorder.start();
                    }
                    Some(ListenerCommand::Stop) => {
                        recorder.stop();
                        for &sig in stop_signals(pending.live()) {
                            media_signal(&gate, sig);
                        }
                    }
                    None => break, // sender dropped → end the task (releases mic)
                },
            }
        }
    });

    tx
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation::media::{GateAction, MediaGate};

    /// Drain whatever commands are queued on the listener channel.
    fn drain(rx: &mut mpsc::UnboundedReceiver<ListenerCommand>) -> Vec<ListenerCommand> {
        let mut out = Vec::new();
        while let Ok(cmd) = rx.try_recv() {
            out.push(cmd);
        }
        out
    }

    #[test]
    fn toggle_alternates_start_and_stop() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let handle = ListenerHandle::from(tx);
        let recording = AtomicBool::new(false);

        // First toggle starts recording.
        assert!(apply_listen_action(
            &handle,
            &recording,
            ListenAction::Toggle,
            VoiceMode::Conversation
        ));
        assert!(recording.load(Ordering::Acquire));
        assert!(matches!(
            drain(&mut rx).as_slice(),
            [ListenerCommand::Start(VoiceMode::Conversation)]
        ));

        // Second toggle stops it.
        assert!(!apply_listen_action(
            &handle,
            &recording,
            ListenAction::Toggle,
            VoiceMode::Conversation
        ));
        assert!(!recording.load(Ordering::Acquire));
        assert!(matches!(drain(&mut rx).as_slice(), [ListenerCommand::Stop]));
    }

    #[test]
    fn explicit_start_stop_are_idempotent() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let handle = ListenerHandle::from(tx);
        let recording = AtomicBool::new(false);

        // Start from idle records and sends Start.
        assert!(apply_listen_action(
            &handle,
            &recording,
            ListenAction::Start,
            VoiceMode::Conversation
        ));
        assert!(matches!(
            drain(&mut rx).as_slice(),
            [ListenerCommand::Start(VoiceMode::Conversation)]
        ));

        // A second Start is a no-op — no command, no restart.
        assert!(apply_listen_action(
            &handle,
            &recording,
            ListenAction::Start,
            VoiceMode::Conversation
        ));
        assert!(drain(&mut rx).is_empty());

        // Stop sends Stop; a second Stop is a no-op.
        assert!(!apply_listen_action(
            &handle,
            &recording,
            ListenAction::Stop,
            VoiceMode::Conversation
        ));
        assert!(matches!(drain(&mut rx).as_slice(), [ListenerCommand::Stop]));
        assert!(!apply_listen_action(
            &handle,
            &recording,
            ListenAction::Stop,
            VoiceMode::Conversation
        ));
        assert!(drain(&mut rx).is_empty());
    }

    #[test]
    fn replace_swaps_the_delivered_to_sender() {
        // The core respawn guarantee: after `replace`, triggers routed through
        // the (cloned) handle reach the *new* listener, not the old one — this
        // is what lets a mic change take effect live under the IPC/attach
        // clones that still hold their original handle clone.
        let (tx1, mut rx1) = mpsc::unbounded_channel();
        let handle = ListenerHandle::from(tx1);
        let shared = handle.clone(); // as the IPC/attach tasks would hold it
        let recording = AtomicBool::new(false);

        let (tx2, mut rx2) = mpsc::unbounded_channel();
        handle.replace(tx2);

        // A trigger via the pre-existing clone now lands on the new receiver.
        assert!(apply_listen_action(
            &shared,
            &recording,
            ListenAction::Start,
            VoiceMode::Conversation
        ));
        assert!(drain(&mut rx1).is_empty());
        assert!(matches!(
            drain(&mut rx2).as_slice(),
            [ListenerCommand::Start(VoiceMode::Conversation)]
        ));
    }

    #[test]
    fn toggle_start_carries_the_requested_mode() {
        // The mode is fixed when the recording *starts*, so the `Start` the
        // toggle sends is what carries it to the listener.
        let (tx, mut rx) = mpsc::unbounded_channel();
        let handle = ListenerHandle::from(tx);
        let recording = AtomicBool::new(false);

        assert!(apply_listen_action(
            &handle,
            &recording,
            ListenAction::Toggle,
            VoiceMode::Dictation
        ));
        assert!(matches!(
            drain(&mut rx).as_slice(),
            [ListenerCommand::Start(VoiceMode::Dictation)]
        ));
    }

    #[test]
    fn stop_ignores_the_requested_mode() {
        // Either hotkey stops an in-progress recording, so the mode passed on
        // the stopping toggle is irrelevant — the WAV is already claimed by the
        // mode its `Start` carried.
        let (tx, mut rx) = mpsc::unbounded_channel();
        let handle = ListenerHandle::from(tx);
        let recording = AtomicBool::new(false);

        apply_listen_action(
            &handle,
            &recording,
            ListenAction::Toggle,
            VoiceMode::Dictation,
        );
        let _ = drain(&mut rx);

        assert!(!apply_listen_action(
            &handle,
            &recording,
            ListenAction::Toggle,
            VoiceMode::Conversation
        ));
        assert!(matches!(drain(&mut rx).as_slice(), [ListenerCommand::Stop]));
    }

    #[test]
    fn pending_modes_pair_fifo_across_recordings() {
        // Recordings finish in the order they started, so the queue is FIFO: a
        // dictation started first claims the first WAV even if a conversation
        // recording was queued behind it.
        let mut modes = PendingModes::default();
        modes.push(VoiceMode::Dictation);
        modes.push(VoiceMode::Conversation);
        assert_eq!(modes.pop_for_wav(), VoiceMode::Dictation);
        assert_eq!(modes.pop_for_wav(), VoiceMode::Conversation);
    }

    #[test]
    fn pending_modes_empty_pop_falls_back_to_conversation() {
        let mut modes = PendingModes::default();
        assert_eq!(modes.pop_for_wav(), VoiceMode::Conversation);
    }

    #[test]
    fn pending_modes_live_is_the_most_recent_start() {
        // `Stop` belongs to the recording that is live, which is the last one
        // started — even with an earlier recording's WAV still being transcribed.
        let mut modes = PendingModes::default();
        assert_eq!(modes.live(), None);
        modes.push(VoiceMode::Conversation);
        modes.push(VoiceMode::Dictation);
        assert_eq!(modes.live(), Some(VoiceMode::Dictation));
    }

    /// Feed a recording's start and its stop-time signals through a real gate,
    /// returning what the last stop-time signal decided.
    fn gate_action_at_stop(mode: VoiceMode) -> GateAction {
        let mut gate = MediaGate::default();
        assert_eq!(gate.on(MediaSignal::RecordStarted), GateAction::Pause);
        stop_signals(Some(mode))
            .iter()
            .map(|&s| gate.on(s))
            .last()
            .expect("a stop always signals RecordStopped")
    }

    #[test]
    fn dictation_releases_media_as_soon_as_recording_stops() {
        // No reply follows a dictation, so the music must not wait out the STT
        // round-trip — it comes back the moment the mic closes.
        assert_eq!(
            gate_action_at_stop(VoiceMode::Dictation),
            GateAction::Resume
        );
        assert_eq!(
            gate_action_at_stop(VoiceMode::UiDictation),
            GateAction::Resume
        );
    }

    #[test]
    fn conversation_holds_media_past_recording_stop() {
        // A conversation turn is still to be answered (and spoken), so stopping
        // the mic alone must not resume playback.
        assert_eq!(
            gate_action_at_stop(VoiceMode::Conversation),
            GateAction::None
        );
        assert_eq!(stop_signals(None), [MediaSignal::RecordStopped]);
    }

    #[test]
    fn conversation_hears_only_text() {
        // Silence and failures are handled inside the listener for the
        // conversation (speaker resume, media gate), so nothing is forwarded.
        assert_eq!(
            transcript_for(VoiceMode::Conversation, &Ok(String::new())),
            None
        );
        assert_eq!(
            transcript_for(
                VoiceMode::Conversation,
                &Err(TtsError::Audio("boom".into()))
            ),
            None
        );
        assert_eq!(
            transcript_for(VoiceMode::Conversation, &Ok("hi".into())),
            Some(Transcript {
                mode: VoiceMode::Conversation,
                text: "hi".into(),
                error: None
            })
        );
    }

    #[test]
    fn dictation_hears_every_outcome() {
        // The frontend holds a "Transcribing…" notice in the pane; it needs
        // the empty and failed outcomes to replace it, not just the text.
        assert_eq!(
            transcript_for(VoiceMode::Dictation, &Ok(String::new())),
            Some(Transcript {
                mode: VoiceMode::Dictation,
                text: String::new(),
                error: None
            })
        );
        let failed = transcript_for(
            VoiceMode::Dictation,
            &Err(TtsError::Audio("mic gone".into())),
        )
        .expect("failure is reported");
        assert!(failed.text.is_empty());
        assert!(
            failed
                .error
                .as_deref()
                .is_some_and(|e| e.contains("mic gone"))
        );
    }

    #[test]
    fn ui_dictation_is_dictation_and_hears_every_outcome() {
        // Only the submit decision differs; everything else treats it as
        // dictation — including the empty and failed outcomes, which the
        // frontend reports where the user is looking.
        assert!(VoiceMode::UiDictation.is_dictation());
        assert!(VoiceMode::Dictation.is_dictation());
        assert!(!VoiceMode::Conversation.is_dictation());
        assert_eq!(
            transcript_for(VoiceMode::UiDictation, &Ok(String::new())),
            Some(Transcript {
                mode: VoiceMode::UiDictation,
                text: String::new(),
                error: None
            })
        );
        assert!(
            transcript_for(VoiceMode::UiDictation, &Err(TtsError::Audio("x".into())))
                .and_then(|t| t.error)
                .is_some()
        );
    }
}
