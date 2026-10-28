//! Error type for the [`CommanderBackend`](super::CommanderBackend) seam.
//!
//! A backend is either the in-process [`LocalBackend`](super::local::LocalBackend)
//! or (Phase F) a remote HTTP/WS client. `BackendError` is the common failure
//! type both surface, so the TUI can drive either through one `Result` without
//! caring which transport produced the error. Local failures classify the core
//! [`Error`](crate::error::Error) into transport-neutral categories (mirroring
//! the server's status-code mapping); remote failures construct the same
//! categories from HTTP status / transport state directly.
//!
//! No construction path takes a bearer token, so `Display` can never leak one —
//! keep it that way when Phase F adds remote error mapping.

use thiserror::Error;

use crate::error::{Error as CoreError, GitError, SessionError, TmuxError};

use super::run_local::RunLocalError;

/// A failure from any [`CommanderBackend`](super::CommanderBackend) method.
#[derive(Debug, Error)]
pub enum BackendError {
    /// A local core error that didn't map onto a more specific category
    /// (git/IO/persistence/cascade failures, etc.).
    #[error("{0}")]
    Local(CoreError),

    /// The backing service is unavailable — tmux not installed, the remote
    /// server unreachable, etc. Distinct from a request-level failure.
    #[error("backend unavailable: {reason}")]
    Unavailable { reason: String },

    /// Authentication was rejected (remote backends). Deliberately carries no
    /// detail so a token can never appear in the message.
    #[error("authentication failed")]
    Auth,

    /// The requested resource (session, project, file in diff) does not exist.
    #[error("not found")]
    NotFound,

    /// The request was malformed or semantically invalid (bad name, program
    /// flags that don't apply, etc.).
    #[error("invalid request: {0}")]
    InvalidRequest(String),

    /// The backend hit an internal error producing a response.
    #[error("server error: {0}")]
    Server(String),

    /// A wire-protocol violation (unexpected/undecodable response). Reserved for
    /// remote backends; the local backend never produces it.
    #[error("protocol error: {0}")]
    Protocol(String),
}

/// Result alias for backend methods.
pub type BResult<T> = Result<T, BackendError>;

impl From<CoreError> for BackendError {
    /// Classify a core error into a transport-neutral backend category, so the
    /// local backend and a future remote backend surface the same shapes. The
    /// buckets mirror the server's HTTP status mapping (`server/src/error.rs`):
    /// missing → [`NotFound`](BackendError::NotFound), bad input →
    /// [`InvalidRequest`](BackendError::InvalidRequest), tmux absent →
    /// [`Unavailable`](BackendError::Unavailable), everything else stays
    /// [`Local`](BackendError::Local).
    fn from(err: CoreError) -> Self {
        match &err {
            CoreError::Session(
                SessionError::NotFound(_)
                | SessionError::ProjectNotFound(_)
                | SessionError::TmuxSessionNotFound(_)
                | SessionError::FileNotInDiff(_),
            ) => BackendError::NotFound,

            // As the server splits it (404 / 409), so a session behaves the
            // same on either backend: a named file missing from the diff is
            // not-found; only stale hashes are an invalid request (a remote
            // 409 arrives as one).
            CoreError::Session(SessionError::FileSelectionRejected { missing, .. })
                if !missing.is_empty() =>
            {
                BackendError::NotFound
            }

            CoreError::Session(
                SessionError::InvalidName { .. }
                | SessionError::InvalidProgram(_)
                | SessionError::FileSelectionRejected { .. },
            ) => BackendError::InvalidRequest(err.to_string()),

            // A refused clone source/destination name: nothing failed, the
            // *request* is unusable. Its message is redacted at construction
            // (`clone_source_rejected`), so surfacing it cannot echo a credential.
            CoreError::Git(GitError::CloneSourceRejected(_)) => {
                BackendError::InvalidRequest(err.to_string())
            }

            // A missing backing tool, joining tmux: `gh` is installable, which is
            // why core carved this out of `OperationFailed`.
            CoreError::Tmux(TmuxError::NotInstalled | TmuxError::ServerNotRunning)
            | CoreError::Git(GitError::GhUnavailable) => BackendError::Unavailable {
                reason: err.to_string(),
            },

            _ => BackendError::Local(err),
        }
    }
}

impl From<RunLocalError<CoreError>> for BackendError {
    /// A `!Send` core call routed through [`run_local`](super::run_local::run_local):
    /// the inner error classifies as usual; a lost worker thread is an internal
    /// server error.
    fn from(err: RunLocalError<CoreError>) -> Self {
        match err {
            RunLocalError::Inner(e) => e.into(),
            RunLocalError::WorkerLost => BackendError::Server(err_worker_lost()),
        }
    }
}

fn err_worker_lost() -> String {
    "internal worker failed to produce a response".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::GitError;
    use crate::session::SessionId;

    #[test]
    fn not_found_variants_map_to_not_found() {
        for e in [
            CoreError::Session(SessionError::NotFound(SessionId::new())),
            CoreError::Session(SessionError::ProjectNotFound("p".into())),
            CoreError::Session(SessionError::TmuxSessionNotFound("s".into())),
            CoreError::Session(SessionError::FileNotInDiff("a.rs".into())),
        ] {
            assert!(
                matches!(BackendError::from(e), BackendError::NotFound),
                "expected NotFound"
            );
        }
    }

    #[test]
    fn bad_input_variants_map_to_invalid_request() {
        let e = CoreError::Session(SessionError::InvalidName {
            name: "x".into(),
            reason: "bad".into(),
        });
        assert!(matches!(
            BackendError::from(e),
            BackendError::InvalidRequest(_)
        ));
        let e = CoreError::Session(SessionError::InvalidProgram("vim".into()));
        assert!(matches!(
            BackendError::from(e),
            BackendError::InvalidRequest(_)
        ));
        // A selection rejected only for stale hashes keeps its message
        // (which files, and why); one naming a missing file is not-found, as
        // the server's 404 arrives through a remote backend.
        let e = CoreError::Session(SessionError::FileSelectionRejected {
            missing: vec![],
            changed: vec!["b.rs".into()],
        });
        assert!(matches!(
            BackendError::from(e),
            BackendError::InvalidRequest(m) if m.contains("b.rs")
        ));
        let e = CoreError::Session(SessionError::FileSelectionRejected {
            missing: vec!["a.rs".into()],
            changed: vec!["b.rs".into()],
        });
        assert!(matches!(BackendError::from(e), BackendError::NotFound));
    }

    #[test]
    fn tmux_absence_maps_to_unavailable() {
        assert!(matches!(
            BackendError::from(CoreError::Tmux(TmuxError::NotInstalled)),
            BackendError::Unavailable { .. }
        ));
        assert!(matches!(
            BackendError::from(CoreError::Tmux(TmuxError::ServerNotRunning)),
            BackendError::Unavailable { .. }
        ));
    }

    #[test]
    fn other_errors_stay_local() {
        let e = CoreError::Git(GitError::OperationFailed("boom".into()));
        assert!(matches!(BackendError::from(e), BackendError::Local(_)));
    }

    /// A refused clone source is the *caller's* mistake, and the server already
    /// answers it with a 400 — which a remote backend turns into
    /// `InvalidRequest`. The local backend has to reach the same category from
    /// the same core error, or one frontend message can't cover both transports.
    /// Asserted next to `OperationFailed` because the pair is the point: same
    /// shape, and only the variant separates a bad request from a broken backend.
    #[test]
    fn a_refused_clone_source_maps_to_invalid_request() {
        let e = CoreError::Git(GitError::CloneSourceRejected("nope".into()));
        // The exact string a remote backend would carry: the server's 400 body is
        // `CoreError::to_string()` too (`server/src/error.rs`'s `IntoResponse`),
        // so both transports hand a frontend the same message.
        assert!(matches!(
            BackendError::from(e),
            BackendError::InvalidRequest(m) if m == "Git error: nope"
        ));
        assert!(matches!(
            BackendError::from(CoreError::Git(GitError::OperationFailed("nope".into()))),
            BackendError::Local(_)
        ));
    }

    /// A missing `gh` joins missing tmux in `Unavailable`: it is a backing tool
    /// the user can install, which is why core carved it out of
    /// `OperationFailed`, and the server maps it to a 503 the remote backend
    /// already turns into `Unavailable`.
    #[test]
    fn missing_gh_maps_to_unavailable() {
        assert!(matches!(
            BackendError::from(CoreError::Git(GitError::GhUnavailable)),
            BackendError::Unavailable { .. }
        ));
    }

    /// A repo listing that overran must **not** join it there. `Unavailable` is
    /// what frontends word as "install gh", so a timeout landing in that bucket
    /// would tell a user to install a `gh` they demonstrably have. It falls to
    /// `Local`, carrying its own message, exactly as `CloneTimedOut` does.
    #[test]
    fn a_timed_out_repo_listing_is_not_unavailable() {
        let err = BackendError::from(CoreError::Git(GitError::RepoListTimedOut { secs: 90 }));
        assert!(
            matches!(err, BackendError::Local(_)),
            "a timeout is a failure with a reason, not a missing tool: {err:?}"
        );
        assert!(err.to_string().contains("timed out"), "{err}");
    }

    #[test]
    fn run_local_inner_classifies_and_worker_lost_is_server() {
        let inner =
            RunLocalError::Inner(CoreError::Session(SessionError::NotFound(SessionId::new())));
        assert!(matches!(BackendError::from(inner), BackendError::NotFound));

        let lost: RunLocalError<CoreError> = RunLocalError::WorkerLost;
        assert!(matches!(BackendError::from(lost), BackendError::Server(_)));
    }

    /// Every variant's `Display` is non-empty and — since no path takes a token
    /// — free of anything token-shaped. Guards the "never leak a bearer token"
    /// invariant against future edits.
    ///
    /// This covers the *local* construction paths only. The end-to-end guarantee
    /// (an actual bearer token threaded through a failing remote call never
    /// surfaces in the resulting `BackendError`) is exercised by
    /// `token_never_appears_in_errors` in `claude-commander-remote`'s
    /// `backend.rs`.
    #[test]
    fn display_is_populated_and_tokenless() {
        // A recognisable sentinel standing in for a bearer token: no variant is
        // constructed with it, so it must never appear in any `Display`.
        const TOKEN_SENTINEL: &str = "s3cr3t-bearer-token-value";
        let variants = [
            BackendError::Local(CoreError::Tmux(TmuxError::NotInstalled)),
            BackendError::Unavailable {
                reason: "server down".into(),
            },
            BackendError::Auth,
            BackendError::NotFound,
            BackendError::InvalidRequest("bad".into()),
            BackendError::Server("oops".into()),
            BackendError::Protocol("garbage".into()),
        ];
        for v in variants {
            let s = v.to_string();
            assert!(!s.is_empty());
            assert!(!s.to_lowercase().contains("bearer"));
            assert!(!s.contains(TOKEN_SENTINEL));
        }
    }
}
