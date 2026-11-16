//! [`ClientError`]: the transport-neutral failure categories every
//! [`RemoteClient`](crate::RemoteClient) method surfaces, plus the HTTP/transport
//! classification that produces them.
//!
//! The categories are the inverse of the server's status mapping
//! (`server/src/error.rs`): a `404` becomes [`ClientError::NotFound`],
//! `401`/`403` → [`ClientError::Auth`], `400`/`422`/`409` →
//! [`ClientError::InvalidRequest`], `503` → [`ClientError::Unavailable`], other
//! `5xx` → [`ClientError::Server`], and a JSON decode failure →
//! [`ClientError::Protocol`]. A transport failure (no response — connection
//! refused, timeout, DNS) is [`ClientError::Unavailable`]. These map 1:1 onto
//! core's `BackendError` categories in the thin `claude-commander-remote`
//! adapter, so the TUI sees exactly the same shapes as before.
//!
//! **Token safety:** no path here reads the bearer token (it lives only in a
//! request header), and every reason string is derived from the HTTP status or
//! the server's own `{"error":{"message"}}` body — never from request headers —
//! so a token can't reach an error's `Display`/`Debug`.

use claude_commander_protocol::api::ApiErrorBody;
use reqwest::{Response, StatusCode};
use thiserror::Error;

/// A failure from any [`RemoteClient`](crate::RemoteClient) method. Categories
/// mirror the server's HTTP status mapping so the remote adapter can convert them
/// to core's `BackendError` one-for-one.
#[derive(Debug, Error)]
pub enum ClientError {
    /// The backing service is unavailable — the server unreachable, a transport
    /// timeout, or the server reporting its own backing service (tmux) down.
    #[error("backend unavailable: {reason}")]
    Unavailable { reason: String },

    /// Authentication was rejected. Deliberately carries no detail so a token can
    /// never appear in the message.
    #[error("authentication failed")]
    Auth,

    /// The requested resource (session, project, file in diff) does not exist.
    #[error("not found")]
    NotFound,

    /// The request was malformed or semantically invalid.
    #[error("invalid request: {0}")]
    InvalidRequest(String),

    /// The server hit an internal error producing a response.
    #[error("server error: {0}")]
    Server(String),

    /// A wire-protocol violation (unexpected/undecodable response).
    #[error("protocol error: {0}")]
    Protocol(String),
}

/// Result alias for client methods.
pub type ClientResult<T> = Result<T, ClientError>;

/// Map a transport-level reqwest error (the request never produced a response)
/// onto a category. Connection refused / timeout / DNS failures mean the server
/// is unreachable → [`ClientError::Unavailable`]; a stray decode error is a
/// protocol violation.
pub(crate) fn transport_error(err: reqwest::Error) -> ClientError {
    if err.is_decode() {
        ClientError::Protocol(short_transport_reason(&err))
    } else {
        ClientError::Unavailable {
            reason: short_transport_reason(&err),
        }
    }
}

/// Map an error reading/decoding a response body onto a category.
pub(crate) fn body_error(err: reqwest::Error) -> ClientError {
    if err.is_decode() {
        ClientError::Protocol(short_transport_reason(&err))
    } else {
        ClientError::Unavailable {
            reason: short_transport_reason(&err),
        }
    }
}

/// A short, user-facing reason for a transport failure. Deliberately does NOT
/// embed `err.to_string()` (which can be verbose and URL-bearing); the fixed
/// phrasing keeps toasts tidy and the reason deterministic for tests. The full
/// error is logged at `debug` by the caller.
fn short_transport_reason(err: &reqwest::Error) -> String {
    if err.is_timeout() {
        "connection timed out".to_string()
    } else if err.is_connect() {
        "could not connect to server".to_string()
    } else if err.is_decode() {
        "malformed response from server".to_string()
    } else {
        "network error".to_string()
    }
}

/// Map a non-success HTTP status + an extracted message onto a category.
pub(crate) fn status_error(status: StatusCode, message: String) -> ClientError {
    match status {
        // Deliberately message-free so nothing sensitive can ride along.
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => ClientError::Auth,
        StatusCode::NOT_FOUND => ClientError::NotFound,
        // The backing service (tmux) is down, not the request's fault.
        StatusCode::SERVICE_UNAVAILABLE => ClientError::Unavailable {
            reason: if message.is_empty() {
                "remote service unavailable".to_string()
            } else {
                message
            },
        },
        _ if status.is_server_error() => ClientError::Server(message),
        // Any other 4xx (bad input, conflict, unsupported method, …) is a
        // request the server rejected.
        _ if status.is_client_error() => ClientError::InvalidRequest(message),
        // Anything else non-2xx (e.g. an unexpected 3xx we didn't follow).
        _ => ClientError::Server(format!("unexpected status {}", status.as_u16())),
    }
}

/// Consume an error response and produce a reason string, preferring the
/// server's `{"error":{"message":…}}` body and falling back to the status'
/// canonical phrase. The body is the server's own error text (a core error
/// string) — never anything token-bearing.
pub(crate) async fn error_message(resp: Response) -> String {
    let status = resp.status();
    let fallback = || {
        status
            .canonical_reason()
            .unwrap_or("request failed")
            .to_string()
    };
    match resp.text().await {
        Ok(text) => parse_error_message(&text)
            .or_else(|| {
                // Old axum Json<PatchSession> rejects a new op before its handler.
                // Preserve only this fixed capability marker, never arbitrary text
                // (which can quote a credential-bearing value). Pinned by
                // client::session_edit_tests' real legacy Json extractor.
                (status == StatusCode::UNPROCESSABLE_ENTITY && text.starts_with(
                    "Failed to deserialize the JSON body into the target type: op: unknown variant `edit`"
            )).then(|| "unknown variant `edit`".into())
            })
            .unwrap_or_else(fallback),
        Err(_) => fallback(),
    }
}

/// Pull `error.message` out of the server's uniform error body, if present.
fn parse_error_message(text: &str) -> Option<String> {
    serde_json::from_str::<ApiErrorBody>(text)
        .ok()
        .map(|body| body.error.message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_maps_to_categories() {
        assert!(matches!(
            status_error(StatusCode::UNAUTHORIZED, "x".into()),
            ClientError::Auth
        ));
        assert!(matches!(
            status_error(StatusCode::FORBIDDEN, "x".into()),
            ClientError::Auth
        ));
        assert!(matches!(
            status_error(StatusCode::NOT_FOUND, "x".into()),
            ClientError::NotFound
        ));
        assert!(matches!(
            status_error(StatusCode::BAD_REQUEST, "bad".into()),
            ClientError::InvalidRequest(_)
        ));
        assert!(matches!(
            status_error(StatusCode::UNPROCESSABLE_ENTITY, "bad".into()),
            ClientError::InvalidRequest(_)
        ));
        assert!(matches!(
            status_error(StatusCode::CONFLICT, "conflict".into()),
            ClientError::InvalidRequest(_)
        ));
        assert!(matches!(
            status_error(StatusCode::SERVICE_UNAVAILABLE, "tmux".into()),
            ClientError::Unavailable { .. }
        ));
        assert!(matches!(
            status_error(StatusCode::INTERNAL_SERVER_ERROR, "boom".into()),
            ClientError::Server(_)
        ));
        assert!(matches!(
            status_error(StatusCode::BAD_GATEWAY, "boom".into()),
            ClientError::Server(_)
        ));
    }

    #[test]
    fn parses_server_error_body() {
        let body = r#"{"error":{"kind":"session","message":"no such session"}}"#;
        assert_eq!(
            parse_error_message(body).as_deref(),
            Some("no such session")
        );
    }

    #[test]
    fn parse_ignores_non_error_shapes() {
        assert!(parse_error_message("not json").is_none());
        assert!(parse_error_message(r#"{"other":1}"#).is_none());
        assert!(parse_error_message(r#"{"error":{}}"#).is_none());
    }

    /// Every variant's `Display` is non-empty and free of anything token-shaped.
    #[test]
    fn display_is_populated_and_tokenless() {
        const TOKEN_SENTINEL: &str = "s3cr3t-bearer-token-value";
        let variants = [
            ClientError::Unavailable {
                reason: "server down".into(),
            },
            ClientError::Auth,
            ClientError::NotFound,
            ClientError::InvalidRequest("bad".into()),
            ClientError::Server("oops".into()),
            ClientError::Protocol("garbage".into()),
        ];
        for v in variants {
            let s = v.to_string();
            assert!(!s.is_empty());
            assert!(!s.to_lowercase().contains("bearer"));
            assert!(!s.contains(TOKEN_SENTINEL));
        }
    }
}
