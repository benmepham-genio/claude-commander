//! Error types for claude-commander
//!
//! Uses `thiserror` for ergonomic error definitions with automatic `Display` and `Error` impls.

use std::path::PathBuf;

use thiserror::Error;

use crate::session::SessionId;

/// Top-level error type for claude-commander
#[derive(Error, Debug)]
pub enum Error {
    #[error("Session error: {0}")]
    Session(#[from] SessionError),

    #[error("Tmux error: {0}")]
    Tmux(#[from] TmuxError),

    #[error("Git error: {0}")]
    Git(#[from] GitError),

    #[error("Configuration error: {0}")]
    Config(#[from] ConfigError),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("TTS error: {0}")]
    Tts(#[from] TtsError),
}

/// Session management errors
#[derive(Error, Debug)]
pub enum SessionError {
    #[error("Session not found: {0}")]
    NotFound(SessionId),

    #[error("Session already exists: {0}")]
    AlreadyExists(String),

    #[error("Invalid session name '{name}': {reason}")]
    InvalidName { name: String, reason: String },

    #[error("Session {0} is in invalid state for this operation")]
    InvalidState(SessionId),

    #[error("Invalid program: {0}")]
    InvalidProgram(String),

    #[error("Failed to create session: {0}")]
    CreationFailed(String),

    #[error("Failed to persist session state: {0}")]
    PersistenceFailed(String),

    #[error("Project not found: {0}")]
    ProjectNotFound(String),

    #[error("Maximum sessions reached: {0}")]
    MaxSessionsReached(usize),

    #[error("Tmux session not found: {0} (session may have crashed or been killed)")]
    TmuxSessionNotFound(String),

    #[error("Cascade pre-flight failed for session {session}: {reason}")]
    CascadePreflightFailed { session: SessionId, reason: String },

    #[error("No cascade in progress")]
    NoCascadeInProgress,

    #[error(
        "Cascade resume blocked: session {0} is still in a merge state — commit the resolved merge first"
    )]
    CascadeMergeIncomplete(SessionId),

    #[error("Cascade merge failed in session {session}: {reason}")]
    CascadeMergeFailed { session: SessionId, reason: String },

    #[error("Push failed in session {session}: {reason}")]
    PushFailed { session: SessionId, reason: String },

    #[error(
        "Commander session is disabled. Enable it with `commander_enabled = true` in config.toml, or toggle it in the in-app settings."
    )]
    CommanderDisabled,

    #[error("File not in the current review diff: {0}")]
    FileNotInDiff(String),

    /// Some named files of a review-diff selection can't be used: `missing`
    /// aren't in the diff, `changed` were edited since their `@hash` was read.
    /// Both are reported so a caller can fix every problem in one pass.
    #[error("{}", file_selection_message(missing, changed))]
    FileSelectionRejected {
        missing: Vec<String>,
        changed: Vec<String>,
    },

    #[error("Invalid pasted image: {0}")]
    InvalidImage(String),

    /// A refused stack-base retarget. Transparent so the typed reason
    /// ([`crate::session::SetBaseRejection`]) is what the user reads.
    #[error(transparent)]
    InvalidBase(#[from] crate::session::SetBaseRejection),
}

/// A pasted-image rejection from the shared wire contract
/// ([`claude_commander_protocol::paste::validate`]) is an invalid-image error.
/// The contract owns the *rules* (accept allow-list, size cap) and their user-
/// facing wording; core owns how that surfaces in its own hierarchy — and the
/// server maps [`SessionError::InvalidImage`] to a 400, so the message reaches
/// the client verbatim.
impl From<claude_commander_protocol::paste::ImageRejection> for SessionError {
    fn from(e: claude_commander_protocol::paste::ImageRejection) -> Self {
        SessionError::InvalidImage(e.to_string())
    }
}

impl From<claude_commander_protocol::paste::ImageRejection> for Error {
    fn from(e: claude_commander_protocol::paste::ImageRejection) -> Self {
        Error::Session(SessionError::InvalidImage(e.to_string()))
    }
}

/// `Error::Session(#[from] SessionError)` does not chain through a second
/// `#[from]`, so — exactly as [`claude_commander_protocol::paste::ImageRejection`]
/// above — a rejection needs its own hop to the top-level error to keep call
/// sites on a plain `?`.
impl From<crate::session::SetBaseRejection> for Error {
    fn from(e: crate::session::SetBaseRejection) -> Self {
        Error::Session(SessionError::InvalidBase(e))
    }
}

/// Tmux integration errors
#[derive(Error, Debug)]
pub enum TmuxError {
    #[error("Tmux is not installed or not in PATH")]
    NotInstalled,

    #[error("Tmux server not running")]
    ServerNotRunning,

    #[error("Tmux command failed: {command} - {stderr}")]
    CommandFailed { command: String, stderr: String },

    /// tmux could not be launched at all (e.g. fork/exec failed because the
    /// process hit the open-file limit). Distinct from `CommandFailed`, which
    /// means tmux *ran* and exited non-zero — a difference that matters when
    /// interpreting `has-session`: a non-zero exit can mean "no such session",
    /// but a launch failure tells us nothing about whether the session exists.
    #[error("Failed to execute tmux command: {command} - {reason}")]
    ExecFailed { command: String, reason: String },

    #[error("Failed to capture pane content: {0}")]
    CaptureFailed(String),

    #[error("Session '{0}' not found in tmux")]
    SessionNotFound(String),

    #[error("Tmux command timed out after {0:?}")]
    Timeout(std::time::Duration),

    #[error("Failed to parse tmux output: {0}")]
    ParseError(String),

    #[error("Semaphore acquire failed")]
    SemaphoreError,

    #[error("PTY error: {0}")]
    PtyError(String),
}

impl From<pty_process::Error> for TmuxError {
    fn from(e: pty_process::Error) -> Self {
        TmuxError::PtyError(e.to_string())
    }
}

impl From<pty_process::Error> for Error {
    fn from(e: pty_process::Error) -> Self {
        Error::Tmux(TmuxError::PtyError(e.to_string()))
    }
}

/// Git operations errors
#[derive(Error, Debug)]
pub enum GitError {
    #[error("Not a git repository: {0}")]
    NotARepository(PathBuf),

    #[error("Git operation failed: {0}")]
    OperationFailed(String),

    #[error("Worktree error: {0}")]
    WorktreeError(String),

    #[error("Branch '{0}' already exists")]
    BranchExists(String),

    #[error("Branch '{0}' not found")]
    BranchNotFound(String),

    #[error("Failed to compute diff: {0}")]
    DiffFailed(String),

    #[error("Gitoxide error: {0}")]
    Gix(String),

    #[error("Invalid reference: {0}")]
    InvalidRef(String),

    /// The `gh` CLI is not installed or not runnable.
    ///
    /// Distinct from `OperationFailed` on purpose: the repo picker renders this
    /// as its own state ("install the GitHub CLI to browse your repos") rather
    /// than as a generic failure, and it is the one gh outcome a user can fix
    /// without seeing gh's own stderr.
    #[error("GitHub CLI (gh) is not installed or not runnable")]
    GhUnavailable,

    /// A clone ran past its time budget and was killed.
    ///
    /// Distinct from `OperationFailed` for the same reason as `GhUnavailable`:
    /// it is actionable (a huge repo on a slow link wants a larger
    /// `clone_timeout_secs`), and the process was killed, so there is no
    /// subprocess stderr worth surfacing.
    #[error("clone timed out after {secs}s")]
    CloneTimedOut { secs: u64 },

    /// A GitHub repo listing ran past its time budget and was killed.
    ///
    /// **Deliberately not folded into `GhUnavailable`.** That variant means "gh
    /// is missing or unauthenticated", and the picker answers it with "install /
    /// log in to the GitHub CLI" — advice that is actively wrong for a user whose
    /// working `gh` merely took too long over a large account. Nor is it
    /// `OperationFailed`: the process was killed, so there is no subprocess
    /// stderr to pass on, and the actionable answer is a larger
    /// `repo_list_timeout_secs`. Same reasoning as [`Self::CloneTimedOut`],
    /// separate variant because the two carry different budgets and different
    /// remedies.
    #[error("listing GitHub repos timed out after {secs}s")]
    RepoListTimedOut { secs: u64 },

    /// A clone source or destination name was refused by the
    /// [`claude_commander_protocol::github`] validators.
    ///
    /// Distinct from `OperationFailed` for the same reason as `GhUnavailable` and
    /// `CloneTimedOut`, plus one more that only applies here: nothing failed. The
    /// *request* is malformed, so a caller mapping errors onto a transport needs
    /// to answer "you sent something unusable" rather than "the server broke" —
    /// the server maps this to a 400 and every other `GitError` to a 500. A
    /// rejection folded into `OperationFailed` is indistinguishable from a real
    /// git failure, and the caller has no way to tell them apart.
    ///
    /// Built only by `clone_source_rejected`, which redacts the message, so a
    /// credentialed source cannot be quoted back through this variant.
    #[error("{0}")]
    CloneSourceRejected(String),
}

/// Configuration errors
#[derive(Error, Debug)]
pub enum ConfigError {
    #[error("Failed to load configuration: {0}")]
    LoadFailed(String),

    #[error("Failed to save configuration: {0}")]
    SaveFailed(String),

    #[error("Invalid configuration value for '{key}': {reason}")]
    InvalidValue { key: String, reason: String },

    #[error("Configuration file not found: {0}")]
    FileNotFound(PathBuf),

    #[error("Failed to create config directory: {0}")]
    DirectoryCreationFailed(PathBuf),
}

/// Text-to-speech / conversation-mode errors.
///
/// These never reach the UI event loop: the conversation worker logs them via
/// `tracing::warn!` and continues, so a flaky or absent TTS server degrades
/// gracefully rather than blocking the terminal.
#[derive(Error, Debug)]
pub enum TtsError {
    #[error("TTS request failed: {0}")]
    Request(String),

    #[error("TTS server returned {status}: {body}")]
    Status { status: u16, body: String },

    #[error("Audio playback error: {0}")]
    Audio(String),

    #[error("Conversation session error: {0}")]
    Session(String),
}

impl From<reqwest::Error> for TtsError {
    fn from(e: reqwest::Error) -> Self {
        TtsError::Request(e.to_string())
    }
}

/// Result type alias using our error type
pub type Result<T> = std::result::Result<T, Error>;

/// Convenience trait for converting gitoxide errors
impl From<gix::open::Error> for GitError {
    fn from(e: gix::open::Error) -> Self {
        GitError::Gix(e.to_string())
    }
}

impl From<gix::discover::Error> for GitError {
    fn from(e: gix::discover::Error) -> Self {
        GitError::Gix(e.to_string())
    }
}

/// [`SessionError::FileSelectionRejected`]'s message: one clause per
/// non-empty list.
fn file_selection_message(missing: &[String], changed: &[String]) -> String {
    let mut parts = Vec::new();
    if !missing.is_empty() {
        parts.push(format!(
            "Not in the current review diff: {}",
            missing.join(", ")
        ));
    }
    if !changed.is_empty() {
        parts.push(format!(
            "Changed since its hash was read (re-read its diff): {}",
            changed.join(", ")
        ));
    }
    parts.join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_display() {
        let err = SessionError::NotFound(SessionId::new());
        assert!(err.to_string().contains("Session not found"));

        let err = TmuxError::NotInstalled;
        assert!(err.to_string().contains("not installed"));

        let err = GitError::NotARepository(PathBuf::from("/tmp/foo"));
        assert!(err.to_string().contains("/tmp/foo"));
    }

    #[test]
    fn test_error_conversion() {
        let session_err = SessionError::NotFound(SessionId::new());
        let _top_err: Error = session_err.into();

        let tmux_err = TmuxError::NotInstalled;
        let _top_err: Error = tmux_err.into();
    }

    #[test]
    fn test_all_session_error_variants_display() {
        let variants: Vec<SessionError> = vec![
            SessionError::NotFound(SessionId::new()),
            SessionError::AlreadyExists("test".to_string()),
            SessionError::InvalidName {
                name: "x".to_string(),
                reason: "bad".to_string(),
            },
            SessionError::InvalidState(SessionId::new()),
            SessionError::CreationFailed("fail".to_string()),
            SessionError::PersistenceFailed("fail".to_string()),
            SessionError::ProjectNotFound("proj".to_string()),
            SessionError::MaxSessionsReached(10),
            SessionError::TmuxSessionNotFound("sess".to_string()),
            SessionError::CommanderDisabled,
            SessionError::FileNotInDiff("src/main.rs".to_string()),
            SessionError::FileSelectionRejected {
                missing: vec!["a.rs".to_string()],
                changed: vec![],
            },
        ];
        for err in variants {
            assert!(!err.to_string().is_empty(), "Empty display for {:?}", err);
        }
    }

    #[test]
    fn test_all_tmux_error_variants_display() {
        let variants: Vec<TmuxError> = vec![
            TmuxError::NotInstalled,
            TmuxError::ServerNotRunning,
            TmuxError::CommandFailed {
                command: "cmd".to_string(),
                stderr: "err".to_string(),
            },
            TmuxError::ExecFailed {
                command: "cmd".to_string(),
                reason: "io".to_string(),
            },
            TmuxError::CaptureFailed("fail".to_string()),
            TmuxError::SessionNotFound("sess".to_string()),
            TmuxError::Timeout(std::time::Duration::from_secs(5)),
            TmuxError::ParseError("parse".to_string()),
            TmuxError::SemaphoreError,
            TmuxError::PtyError("pty".to_string()),
        ];
        for err in variants {
            assert!(!err.to_string().is_empty(), "Empty display for {:?}", err);
        }
    }

    #[test]
    fn test_git_error_conversion() {
        let git_err = GitError::NotARepository(PathBuf::from("/tmp/foo"));
        let top_err: Error = git_err.into();
        assert!(matches!(top_err, Error::Git(_)));
    }

    #[test]
    fn test_config_error_conversion() {
        let config_err = ConfigError::LoadFailed("test".to_string());
        let top_err: Error = config_err.into();
        assert!(matches!(top_err, Error::Config(_)));
    }

    #[test]
    fn test_io_error_conversion() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "test");
        let top_err: Error = io_err.into();
        assert!(matches!(top_err, Error::Io(_)));
    }

    #[test]
    fn test_all_tts_error_variants_display() {
        let variants: Vec<TtsError> = vec![
            TtsError::Request("connection refused".to_string()),
            TtsError::Status {
                status: 500,
                body: "boom".to_string(),
            },
            TtsError::Audio("no device".to_string()),
            TtsError::Session("spawn failed".to_string()),
        ];
        for err in variants {
            assert!(!err.to_string().is_empty(), "Empty display for {:?}", err);
        }
    }

    #[test]
    fn test_tts_error_conversion() {
        let tts_err = TtsError::Audio("test".to_string());
        let top_err: Error = tts_err.into();
        assert!(matches!(top_err, Error::Tts(_)));
    }
}
