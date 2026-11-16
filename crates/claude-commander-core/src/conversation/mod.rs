//! Conversation mode: a dedicated headless `claude` session whose replies are
//! streamed to a TTS engine (and rendered in a full-screen overlay).
//!
//! The clean, supported way to get the assistant's text incrementally is the
//! stream-json headless protocol (`claude -p --input-format stream-json
//! --output-format stream-json --include-partial-messages`) — the interactive
//! TUI is render-only. [`session`] drives that subprocess; [`extract`] turns the
//! streamed text into spoken-ready sentences; [`tts`] synthesizes them and
//! [`audio`] plays them. All the text logic is pure and unit-tested; only
//! [`audio`] and [`session`] touch the outside world.
//!
//! # Why headless streaming, not hooks / MCP / transcript-tail
//!
//! The overriding requirement is *reliable, low-latency* capture: start speaking
//! the first sentence before the whole reply finishes. Only the stream-json
//! protocol delivers that — it is the one mechanism that yields text *mid-reply*.
//! The alternatives were evaluated and set aside; don't "simplify" to one of them
//! without re-checking against this requirement:
//!
//! - **Stop hook** — fires only at end-of-turn and hands over `transcript_path`,
//!   not the text. Whole-reply latency; defeats sentence-streamed TTS.
//! - **Transcript-tail** (`*.jsonl`) — written at block/turn boundaries, not per
//!   token; same end-of-reply latency, plus file races on fast turns.
//! - **MCP `speak(text)` tool** — an *instructed* call (the agent must choose to
//!   call it), not observation. It could speak the real commander and reuse its
//!   tmux UI — the one thing streaming forfeits — but latency is ~end-of-reply
//!   (the model composes, then calls the tool) and reliability is prompt-dependent.
//!   Rejected because the conversation overlay is a *secondary* interface, so that
//!   UI-reuse win doesn't justify losing guaranteed mid-reply latency.
//!
//! The interactive commander TUI does not emit stream-json, so "reuse the
//! commander's UI" and "sub-reply latency" are mutually exclusive (short of
//! fragile pane-scraping). See [`session`] for the actual protocol handling.

/// Whether this build has core's `audio` feature (off in the server, cdylib and -novoice TUI).
pub const AUDIO_SUPPORTED: bool = cfg!(feature = "audio");

pub mod audio;
pub mod dictation;
pub mod extract;
pub mod ipc;
pub mod listener;
pub mod media;
pub mod recorder;
pub mod session;
pub mod speaker;
pub mod stt;
pub mod tts;

pub use dictation::{
    DictationPlan, DictationSubmit, SubmitPlan, normalise_dictation, plan_dictation,
};
pub use extract::{SpeakScope, split_sentences, spoken_text};
pub use listener::{
    ListenAction, ListenerCommand, ListenerHandle, Transcript, VoiceMode, apply_listen_action,
    spawn_listener,
};
pub use media::{MediaSignal, signal as media_signal, spawn_media_gate};
pub use session::{ConversationEvent, ConversationSession, parse_event, user_message_line};
pub use speaker::{
    SentenceAccumulator, SpeakerCommand, SpeakerHandle, spawn_speaker, speaker_command_for,
};
pub use stt::SttClient;
pub use tts::{SpeechRequest, TtsClient, build_speech_body};
