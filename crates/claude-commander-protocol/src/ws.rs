//! WebSocket terminal protocol: framing + control-message types.
//!
//! Two kinds of frame travel over the `/ws/attach` socket, and the split is
//! deliberate:
//!
//! - **Raw PTY bytes use WebSocket *binary* frames.** Terminal output and
//!   keystrokes are arbitrary byte streams — escape sequences and partial
//!   multibyte UTF-8 are routine — so routing them through *text* frames would
//!   corrupt them (text frames must be valid UTF-8). The bridge never sees JSON;
//!   it sees bytes.
//! - **Control messages use WebSocket *text* frames carrying JSON.** These are
//!   small, structured, and human-debuggable: the handshake (`auth`, `attach`),
//!   out-of-band resize, explicit detach, and the server's replies.
//!
//! The server discriminates purely on frame *kind*: a binary frame is always
//! PTY data, a text frame is always a control message. There is no in-band
//! tagging mixing the two, so the discipline can't be violated by a malformed
//! payload.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// How often the server pings an attached socket. On each tick it also checks
/// that *some* inbound frame (a pong, or real traffic) arrived since the
/// previous one, which is how a half-open socket that never sends a close is
/// detected.
pub const ATTACH_PING_INTERVAL: Duration = Duration::from_secs(20);

/// How many consecutive ping ticks may pass with no intervening inbound frame
/// before the peer is declared dead and the attach is torn down. At 2 a peer
/// tolerates a single dropped pong / scheduling hiccup.
pub const ATTACH_MISSED_PONG_LIMIT: u32 = 2;

/// How long a *silent* peer can survive before the server is guaranteed to have
/// torn its attach down — the client's half of the heartbeat contract.
///
/// This lives here, rather than as a constant in each client, because it is a
/// rule both ends must agree on: a mobile client whose process was frozen in the
/// background cannot pong, so on resume it uses this to know whether its attach
/// is definitely gone (reconnect) or merely *might* be (leave the live socket
/// alone, so a scrolled tmux copy-mode view keeps its place). Duplicating the
/// number client-side would let it drift the moment the heartbeat is retuned.
///
/// The derivation follows the server pump. Each tick first tears down if
/// [`ATTACH_MISSED_PONG_LIMIT`] pings already went unanswered, otherwise sends a
/// ping and counts it; any inbound frame zeroes the count. Reaching the limit
/// therefore takes `LIMIT` ticks and detecting it takes one more, so a peer that
/// falls silent survives at most `LIMIT + 1` intervals.
///
/// That is deliberately the **worst** case, so this is a one-way test: longer
/// than it means the attach is certainly gone, but shorter does *not* mean it
/// lives. Measured from the peer's last inbound frame — which zeroes the count —
/// the next tick falls somewhere in `(0, ATTACH_PING_INTERVAL]` and teardown
/// follows `LIMIT` intervals after it, so silence starting just *before* a tick is
/// punished soonest: a floor of a shade over `LIMIT` intervals, 40s against
/// today's 60s deadline. A client wanting certainty inside that window has to wait
/// for the server's `detached` frame — exactly what a half-open socket never
/// delivers.
pub const fn attach_dead_after() -> Duration {
    // Saturating rather than `checked_mul`, which is const but returns an Option
    // a const fn can't unwrap ergonomically. Both operands are compile-time
    // constants in the tens of seconds, so saturation is unreachable.
    ATTACH_PING_INTERVAL.saturating_mul(ATTACH_MISSED_PONG_LIMIT + 1)
}

/// A control message sent by the *client* (browser/native UI) as a JSON text
/// frame. The `auth` then `attach` messages form the mandatory handshake;
/// `resize` and `detach` are valid in steady state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientControl {
    /// First frame: authenticate the socket. Browsers can't set headers on the
    /// WS upgrade, so the token travels in-band here. **Never logged.**
    Auth { token: String },
    /// Second frame: attach to a session. `session_id` is resolved exactly like
    /// the HTTP API's `find_session` (full UUID, ID prefix, or exact title).
    /// `kind` selects the agent pane (default) or the paired shell pane; it is
    /// omitted on the wire for an agent attach, so an old client's
    /// `{"type":"attach","session_id":…}` frame parses unchanged.
    ///
    /// `cols`/`rows` are the client's terminal size. They are carried *here*,
    /// in the handshake, rather than left to the first [`Resize`](Self::Resize)
    /// because a `resize` cannot arrive until a round trip after `ready` — and
    /// tmux paints a full screen into the socket the moment the attach spawns.
    /// A client that announced its size only afterwards therefore always
    /// received one paint at the server's fallback geometry, which its emulator
    /// wrapped at its own (narrower) width; tmux's follow-up repaint is
    /// incremental and carries no full-screen clear, so that mis-wrapped
    /// content was never corrected and the two screens stayed desynchronised
    /// for the life of the attach. Sizing the PTY before `tmux attach-session`
    /// is spawned removes the window entirely instead of racing it.
    ///
    /// The no-full-screen-clear half is a claim about tmux, so here is its
    /// receipt: capture the raw WS byte stream across an attach, send a
    /// `resize`, and grep the bytes that follow — the post-resize repaint
    /// contains no `ESC[2J` or `ESC[3J`. Measured at ~1.9 KB of purely
    /// incremental redraw against a pane holding a full screen of wide text.
    ///
    /// Both are optional so an old client's frame still parses (the server then
    /// falls back to its default geometry) and a new client still works against
    /// an old server, which ignores the fields and gets the pre-existing
    /// behaviour from the follow-up `resize`.
    Attach {
        session_id: String,
        #[serde(default, skip_serializing_if = "AttachKind::is_agent")]
        kind: AttachKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cols: Option<u16>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rows: Option<u16>,
    },
    /// Resize the remote PTY. Sent whenever the client's terminal viewport
    /// changes.
    Resize { cols: u16, rows: u16 },
    /// Force the remote tmux client to repaint its whole visible screen.
    ///
    /// Sent when a client has drawn something *over* the terminal — the TUI's
    /// in-session switcher paints the palette on top of the live pane — and
    /// needs the region it covered restored. The server answers it with
    /// `tmux refresh-client`, which repaints without disturbing a scrolled
    /// copy-mode view (a resize round-trip would lose that anchor, which is why
    /// this is its own message rather than a resize to the same size).
    ///
    /// Purely additive: a server that predates it takes the `Err` arm of its
    /// control-frame match and logs the frame as malformed, leaving the attach
    /// up. A new client against an old server therefore just keeps a stale
    /// rectangle until the pane next emits output.
    Refresh,
    /// Explicitly detach: kill the `tmux attach-session` child but leave the
    /// tmux session (and the program inside it) running.
    Detach,
}

/// A control message sent by the *server* as a JSON text frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerControl {
    /// Handshake succeeded and the bridge is attached. Echoes the resolved tmux
    /// session name so the client can label the terminal.
    Ready { session: String },
    /// The attach ended. `reason` distinguishes a client-requested detach from a
    /// session that ended or a transport error, for client-side UX.
    Detached { reason: DetachReason },
    /// A handshake or steady-state error. `message` is safe to surface to the
    /// user; it never contains the auth token.
    Error { message: String },
}

/// Fixed [`ServerControl::Error`] handshake message for a rejected auth token.
/// Pinned as a constant so the server's wording and the client's error
/// classifier reference the same string and can't drift out of sync.
pub const WS_ERR_AUTH: &str = "authentication failed";

/// Fixed [`ServerControl::Error`] handshake message for an attach to a session
/// that doesn't exist. Shared by the server (which sends it) and the client
/// (which classifies it), so the wording is a single source of truth.
pub const WS_ERR_NO_SESSION: &str = "no such session";

/// Which pane of a session to attach to. Mirrors core's `backend::AttachKind`
/// but lives here so the wire shape has one source of truth. Serialized inside
/// [`ClientControl::Attach`]; [`Agent`](Self::Agent) is the default and is
/// omitted on the wire (see the `skip_serializing_if` on the field), so the
/// frame an old client sends is unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum AttachKind {
    /// The agent (e.g. Claude) pane — the session's primary tmux session.
    #[default]
    Agent,
    /// The paired shell pane (Ctrl+\ toggles here), created on demand.
    Shell,
}

impl AttachKind {
    /// Whether this is the default agent pane. Used to skip serializing the
    /// field for an agent attach so the wire form matches the pre-`kind` frame.
    pub fn is_agent(&self) -> bool {
        matches!(self, AttachKind::Agent)
    }
}

/// Why an attach ended. Serialized as part of [`ServerControl::Detached`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum DetachReason {
    /// The client sent a `detach` control frame.
    ClientRequest,
    /// The tmux session is gone (killed, deleted, or killed and recreated by a
    /// restart). Taken from the attach client's own exit line, never inferred
    /// from the client merely exiting.
    SessionEnded,
    /// The attach's tmux client was detached while the session lives on:
    /// another viewer opened the session (each attach detaches the others, so
    /// the window fits whoever attached last) or the user pressed tmux's detach
    /// key. A client should stop here rather than reconnect — reattaching
    /// would displace the other viewer in turn.
    ClientDetached,
    /// The transport dropped (socket closed, heartbeat timed out).
    Transport,
}

/// Map the HTTP(S) base URL to the `/ws/attach` WebSocket URL: `http`→`ws`,
/// `https`→`wss`, path prefix preserved. A base with any other scheme is left
/// as-is (the endpoint path is still appended) — a client that accepts only
/// http(s) bases rejects the others before it gets here.
///
/// Here rather than in a client because the endpoint's location relative to
/// the HTTP base is part of the contract every client (Rust, Flutter via the
/// client crate, the browser page) must agree on.
pub fn ws_attach_url(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    let ws = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        base.to_string()
    };
    format!("{ws}/ws/attach")
}

impl ClientControl {
    /// Parse a control message from a text-frame payload.
    pub fn from_text(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(text)
    }

    /// Render this control message as a JSON string for a text frame.
    pub fn to_text(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }
}

impl ServerControl {
    /// Parse a control message from a text-frame payload (client side).
    pub fn from_text(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(text)
    }

    /// Render this control message as a JSON string for a text frame.
    pub fn to_text(&self) -> String {
        // The variants are simple, infallible-to-serialize structs; fall back to
        // a generic error frame on the (impossible) failure rather than panic.
        serde_json::to_string(self).unwrap_or_else(|_| {
            r#"{"type":"error","message":"failed to serialize control message"}"#.to_string()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_url_maps_scheme_and_appends_path() {
        assert_eq!(
            ws_attach_url("http://host:8080"),
            "ws://host:8080/ws/attach"
        );
        assert_eq!(
            ws_attach_url("https://host:8080/"),
            "wss://host:8080/ws/attach"
        );
        // A path prefix is preserved ahead of the endpoint.
        assert_eq!(
            ws_attach_url("https://host/prefix"),
            "wss://host/prefix/ws/attach"
        );
        // Unknown scheme is left as-is, path still appended.
        assert_eq!(ws_attach_url("host:8080"), "host:8080/ws/attach");
    }

    #[test]
    fn client_auth_round_trip() {
        let msg = ClientControl::Auth {
            token: "s3cret".into(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(json, r#"{"type":"auth","token":"s3cret"}"#);
        assert_eq!(ClientControl::from_text(&json).unwrap(), msg);
    }

    #[test]
    fn client_attach_round_trip() {
        // An agent attach with no size omits both `kind` and the dimensions on
        // the wire (byte-identical to the pre-`kind` frame), so old and new
        // peers agree.
        let msg = ClientControl::Attach {
            session_id: "abc123".into(),
            kind: AttachKind::Agent,
            cols: None,
            rows: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(json, r#"{"type":"attach","session_id":"abc123"}"#);
        assert_eq!(ClientControl::from_text(&json).unwrap(), msg);
    }

    #[test]
    fn client_attach_shell_round_trip() {
        // A shell attach carries `kind` explicitly.
        let msg = ClientControl::Attach {
            session_id: "abc123".into(),
            kind: AttachKind::Shell,
            cols: None,
            rows: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(
            json,
            r#"{"type":"attach","session_id":"abc123","kind":"shell"}"#
        );
        assert_eq!(ClientControl::from_text(&json).unwrap(), msg);
    }

    /// The handshake must be able to carry the client's geometry, so the server
    /// can size the PTY *before* spawning `tmux attach-session` and tmux never
    /// paints a screen at a width the client will re-wrap.
    #[test]
    fn client_attach_with_size_round_trip() {
        let msg = ClientControl::Attach {
            session_id: "abc123".into(),
            kind: AttachKind::Agent,
            cols: Some(39),
            rows: Some(40),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(
            json,
            r#"{"type":"attach","session_id":"abc123","cols":39,"rows":40}"#
        );
        assert_eq!(ClientControl::from_text(&json).unwrap(), msg);
    }

    #[test]
    fn old_attach_frame_without_kind_parses_as_agent() {
        // Backward compatibility: a frame from a client that predates the `kind`
        // field must still parse, defaulting to the agent pane.
        let parsed =
            ClientControl::from_text(r#"{"type":"attach","session_id":"abc123"}"#).unwrap();
        assert_eq!(
            parsed,
            ClientControl::Attach {
                session_id: "abc123".into(),
                kind: AttachKind::Agent,
                cols: None,
                rows: None,
            }
        );
    }

    /// A client that predates the size fields must still parse, and must be
    /// distinguishable from one that sent a size — `None` is what tells the
    /// server to fall back to its own default geometry.
    #[test]
    fn old_attach_frame_without_size_parses_as_no_size() {
        let parsed =
            ClientControl::from_text(r#"{"type":"attach","session_id":"abc123","kind":"shell"}"#)
                .unwrap();
        assert_eq!(
            parsed,
            ClientControl::Attach {
                session_id: "abc123".into(),
                kind: AttachKind::Shell,
                cols: None,
                rows: None,
            }
        );
    }

    #[test]
    fn client_resize_round_trip() {
        let msg = ClientControl::Resize {
            cols: 120,
            rows: 40,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(json, r#"{"type":"resize","cols":120,"rows":40}"#);
        assert_eq!(ClientControl::from_text(&json).unwrap(), msg);
    }

    #[test]
    fn client_refresh_round_trip() {
        let msg = ClientControl::Refresh;
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(json, r#"{"type":"refresh"}"#);
        assert_eq!(ClientControl::from_text(&json).unwrap(), msg);
    }

    #[test]
    fn an_unknown_control_tag_is_an_error_not_a_panic() {
        // `refresh` was added after the first servers shipped. Its safety as an
        // additive change rests on the receiver treating an unrecognised frame
        // as a *parse error* it can log and ignore (see the server's steady-state
        // match) rather than something that tears the attach down. Pin that:
        // a new client against an old server must only lose the repaint.
        let err = ClientControl::from_text(r#"{"type":"not_a_real_control"}"#);
        assert!(err.is_err(), "unknown tags must not deserialize");
    }

    #[test]
    fn client_detach_round_trip() {
        let msg = ClientControl::Detach;
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(json, r#"{"type":"detach"}"#);
        assert_eq!(ClientControl::from_text(&json).unwrap(), msg);
    }

    #[test]
    fn client_to_text_round_trips() {
        // The client serializes its own control frames to send them; verify the
        // helper matches the canonical wire form.
        let msg = ClientControl::Resize { cols: 80, rows: 24 };
        let text = msg.to_text().unwrap();
        assert_eq!(text, r#"{"type":"resize","cols":80,"rows":24}"#);
        assert_eq!(ClientControl::from_text(&text).unwrap(), msg);
    }

    #[test]
    fn server_ready_round_trip() {
        let msg = ServerControl::Ready {
            session: "cc-1234abcd".into(),
        };
        let json = msg.to_text();
        assert_eq!(json, r#"{"type":"ready","session":"cc-1234abcd"}"#);
        assert_eq!(ServerControl::from_text(&json).unwrap(), msg);
    }

    #[test]
    fn server_detached_round_trip() {
        for (reason, tag) in [
            (DetachReason::ClientRequest, "client_request"),
            (DetachReason::SessionEnded, "session_ended"),
            (DetachReason::ClientDetached, "client_detached"),
            (DetachReason::Transport, "transport"),
        ] {
            let msg = ServerControl::Detached { reason };
            let json = msg.to_text();
            assert_eq!(json, format!(r#"{{"type":"detached","reason":"{tag}"}}"#));
            assert_eq!(ServerControl::from_text(&json).unwrap(), msg);
        }
    }

    #[test]
    fn server_error_round_trip() {
        let msg = ServerControl::Error {
            message: "no such session".into(),
        };
        let json = msg.to_text();
        assert_eq!(json, r#"{"type":"error","message":"no such session"}"#);
        assert_eq!(ServerControl::from_text(&json).unwrap(), msg);
    }

    /// The heartbeat deadline is a *wire* contract: a client that was away
    /// longer than this knows its attach is gone without probing for it. Assert
    /// the derivation, so retuning the interval or the tolerance can't silently
    /// leave a client reconnecting on a stale threshold.
    #[test]
    fn attach_dead_after_covers_the_worst_case_heartbeat() {
        assert_eq!(
            attach_dead_after(),
            ATTACH_PING_INTERVAL * (ATTACH_MISSED_PONG_LIMIT + 1)
        );
        // Concretely, with today's values: a silent peer is torn down by 60s.
        assert_eq!(attach_dead_after(), Duration::from_secs(60));
    }

    #[test]
    fn unknown_control_type_is_rejected() {
        assert!(ClientControl::from_text(r#"{"type":"bogus"}"#).is_err());
        // A binary-only payload (not JSON) must not parse as a control message —
        // the handler relies on this so binary frames are never misread as text.
        assert!(ClientControl::from_text("\x1b[2J not json").is_err());
    }

    #[test]
    fn missing_required_field_is_rejected() {
        // `auth` without a token is invalid.
        assert!(ClientControl::from_text(r#"{"type":"auth"}"#).is_err());
        // `attach` without a session_id is invalid.
        assert!(ClientControl::from_text(r#"{"type":"attach"}"#).is_err());
    }
}
