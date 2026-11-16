//! `GET /ws/attach` — interactive terminal over a WebSocket.
//!
//! Bridges a browser/native WebSocket to a tmux attach via core's
//! transport-agnostic [`HeadlessAttach`] bridge. The handshake authenticates
//! in-band (browsers can't set headers on the WS upgrade), resolves the target
//! session, and spawns the bridge; steady state pumps raw bytes both ways and
//! honours `resize`/`detach` control frames.
//!
//! Detach semantics: a client `detach`, a closed socket, or a heartbeat-ping
//! timeout kills the `tmux attach-session` child **only** — the tmux session and
//! the program inside it keep running. The bridge's [`ChildGuard`] guarantees
//! the child is reaped even on an ungraceful drop, so a closed browser tab never
//! leaks an attach process.

use std::time::Duration;

use axum::{
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    response::Response,
};
use claude_commander_core::backend::AttachEnd;
use claude_commander_core::tmux::{ClientTail, HeadlessAttach};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{debug, info, warn};

use claude_commander_protocol::ws::{
    ATTACH_MISSED_PONG_LIMIT, ATTACH_PING_INTERVAL, AttachKind, ClientControl, DetachReason,
    ServerControl, WS_ERR_AUTH, WS_ERR_NO_SESSION,
};

use crate::state::AppState;

/// How long to wait for the mandatory `auth` then `attach` handshake frames
/// before giving up. Keeps a connecting-but-silent socket from holding the
/// upgrade open indefinitely.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// Fallback PTY size, used only when the `attach` frame carries no geometry —
/// i.e. for a client that predates the handshake's `cols`/`rows` fields.
///
/// A client that *does* send its size gets the PTY opened at it before
/// `tmux attach-session` is spawned, which matters more than it looks: tmux
/// paints a full screen into the socket as soon as the attach starts, and a
/// client that could only announce its size afterwards (a round trip after
/// `ready`) always received that first paint at this fallback geometry. Its
/// emulator then wrapped those over-wide lines at its own width, and because
/// tmux's post-resize repaint is incremental — no full-screen clear — the
/// mis-wrapped content was never corrected. On a phone-width viewport that
/// desynchronised the pane for the life of the attach.
const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;

/// Axum handler for `GET /ws/attach`. Performs the protocol upgrade; all real
/// work happens in [`handle_socket`] once the socket is established.
pub async fn attach(ws: WebSocketUpgrade, State(state): State<AppState>) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

/// Send a control message as a JSON text frame. Returns `Err` if the socket is
/// gone.
async fn send_control(socket: &mut WebSocket, msg: &ServerControl) -> Result<(), axum::Error> {
    socket.send(Message::Text(msg.to_text().into())).await
}

/// Drive a single attached socket through handshake → steady state → teardown.
async fn handle_socket(mut socket: WebSocket, state: AppState) {
    // -- Handshake: auth frame --
    if !authenticate(&mut socket, &state).await {
        // `authenticate` has already sent an error frame where appropriate.
        return;
    }

    // -- Handshake: attach frame → resolve session → spawn bridge --
    let (session_name, bridge) = match attach_session(&mut socket, &state).await {
        Some(pair) => pair,
        None => return,
    };

    info!("WS attached to tmux session {}", session_name);
    if send_control(
        &mut socket,
        &ServerControl::Ready {
            session: session_name,
        },
    )
    .await
    .is_err()
    {
        return;
    }

    // -- Steady state --
    let reason = pump(socket, bridge).await;
    debug!("WS attach loop ended: {:?}", reason);
}

/// Read the mandatory first `auth` frame and validate the token. The token is
/// **never logged**. Returns true on success; on failure sends an `error` frame
/// and returns false.
async fn authenticate(socket: &mut WebSocket, state: &AppState) -> bool {
    match next_control(socket).await {
        Some(ClientControl::Auth { token }) => {
            if state.auth.authorizes_token(&token) {
                true
            } else {
                warn!("WS auth rejected: invalid token");
                let _ = send_control(
                    socket,
                    &ServerControl::Error {
                        message: WS_ERR_AUTH.into(),
                    },
                )
                .await;
                false
            }
        }
        Some(_) => {
            let _ = send_control(
                socket,
                &ServerControl::Error {
                    message: "expected auth frame first".into(),
                },
            )
            .await;
            false
        }
        None => false,
    }
}

/// Read the `attach` frame, resolve the session to its tmux name, and spawn the
/// bridge. Returns `(tmux_session_name, bridge)` on success; on failure sends an
/// `error` frame and returns `None`.
async fn attach_session(
    socket: &mut WebSocket,
    state: &AppState,
) -> Option<(String, HeadlessAttach)> {
    let (session_query, kind, cols, rows) = match next_control(socket).await {
        Some(ClientControl::Attach {
            session_id,
            kind,
            cols,
            rows,
        }) => (
            session_id,
            kind,
            cols.unwrap_or(DEFAULT_COLS),
            rows.unwrap_or(DEFAULT_ROWS),
        ),
        Some(_) => {
            let _ = send_control(
                socket,
                &ServerControl::Error {
                    message: "expected attach frame".into(),
                },
            )
            .await;
            return None;
        }
        None => return None,
    };

    // Resolve the requested pane to a tmux session name through the same service
    // method `LocalBackend::attach` uses, so both transports get identical
    // revive-on-attach (a dead agent tmux session is recreated) and MRU-stamp
    // (`last_attached_at`) behaviour. The agent pane is the session's primary
    // tmux session; the shell pane (`Ctrl+\` partner) is created on demand.
    let core_kind = match kind {
        AttachKind::Agent => claude_commander_core::backend::AttachKind::Agent,
        AttachKind::Shell => claude_commander_core::backend::AttachKind::Shell,
    };
    let resolved = state
        .service
        .resolve_attach_session(&session_query, core_kind)
        .await;
    let tmux_name = match resolved {
        Ok(Some(name)) => name,
        Ok(None) => {
            let _ = send_control(
                socket,
                &ServerControl::Error {
                    message: WS_ERR_NO_SESSION.into(),
                },
            )
            .await;
            return None;
        }
        Err(e) => {
            let _ = send_control(
                socket,
                &ServerControl::Error {
                    message: format!("failed to resolve session: {e}"),
                },
            )
            .await;
            return None;
        }
    };

    // Honour the socket-dir isolation knob so a hermetic test attaches to the
    // same throwaway tmux server its session was created on, not the real one.
    let tmux_tmpdir = state.service.read_config().tmux_tmpdir;
    match HeadlessAttach::spawn(&tmux_name, cols, rows, tmux_tmpdir.as_deref()) {
        Ok(bridge) => Some((tmux_name, bridge)),
        Err(e) => {
            let _ = send_control(
                socket,
                &ServerControl::Error {
                    message: format!("failed to attach: {e}"),
                },
            )
            .await;
            None
        }
    }
}

/// Steady-state pump: WS binary → PTY, PTY → WS binary, `resize`/`detach`
/// control frames, and a pong-tracked heartbeat. Each interval sends a ping and
/// counts it as outstanding; any inbound frame (a pong, or real traffic) clears
/// the count. After [`ATTACH_MISSED_PONG_LIMIT`] un-answered intervals the peer is
/// declared dead and the loop tears down — so a half-open socket whose sends
/// still nominally succeed is still detected, not just one where `send` errors.
/// Returns once any teardown condition fires; the bridge's `ChildGuard` reaps
/// the attach child on the way out.
async fn pump(mut socket: WebSocket, bridge: HeadlessAttach) -> DetachReason {
    // Take the repaint handle before `split` consumes the bridge. It answers the
    // client's `refresh` frame, which is how a client that drew over its own
    // terminal (the TUI's in-session switcher) gets the covered region back.
    let refresh = bridge.refresh_handle();
    // The PTY closing doesn't say why the attach client went: another viewer's
    // `attach-session -d` displacing it looks exactly like its session dying,
    // and a client told `session_ended` attaches again, which would displace
    // that viewer in turn. The client's own exit line does say, so keep the
    // tail of its output.
    let mut tail = ClientTail::default();
    let (mut pty_reader, mut pty_writer, resize, mut child) = bridge.split();
    let mut pty_buf = [0u8; 4096];
    let mut ping = tokio::time::interval(ATTACH_PING_INTERVAL);
    // Skip the immediate first tick so we don't ping before any traffic.
    ping.tick().await;
    // Liveness: a pong (or any inbound frame) resets this; each ping tick
    // increments it. Past [`ATTACH_MISSED_PONG_LIMIT`] consecutive un-ponged ticks the
    // peer is declared dead even if the socket send still appears to succeed.
    let mut missed_pongs: u32 = 0;

    let reason = loop {
        tokio::select! {
            // PTY output → WS binary frame. `send().await` applies backpressure:
            // a slow client pauses this branch, which pauses PTY reads, so memory
            // never grows unbounded (no intermediate channel).
            read = pty_reader.read(&mut pty_buf) => match read {
                Ok(0) => break detach_reason(&tail),
                Ok(n) => {
                    tail.record(&pty_buf[..n]);
                    if socket
                        .send(Message::Binary(pty_buf[..n].to_vec().into()))
                        .await
                        .is_err()
                    {
                        break DetachReason::Transport;
                    }
                }
                Err(e) => {
                    // EIO (raw os error 5) is the expected signal that the PTY
                    // closed; anything else is worth a warning.
                    if e.raw_os_error() != Some(5) {
                        warn!("WS PTY read error: {e}");
                    }
                    break detach_reason(&tail);
                }
            },

            // WS frame → PTY or control handling. Any inbound frame is evidence
            // the peer is alive, so reset the missed-pong counter.
            frame = socket.recv() => {
                missed_pongs = 0;
                match frame {
                    Some(Ok(Message::Binary(bytes))) => {
                        // A failed write means the client is going; don't end
                        // here, before the PTY branch has read its exit line —
                        // that branch reaches EOF and reports why.
                        if pty_writer.write_all(&bytes).await.is_ok() {
                            let _ = pty_writer.flush().await;
                        }
                    }
                    Some(Ok(Message::Text(text))) => match ClientControl::from_text(&text) {
                        Ok(ClientControl::Resize { cols, rows }) => resize.resize(cols, rows),
                        Ok(ClientControl::Refresh) => refresh.refresh().await,
                        Ok(ClientControl::Detach) => break DetachReason::ClientRequest,
                        // `auth`/`attach` are handshake-only; ignore once attached.
                        Ok(_) => debug!("ignoring unexpected control frame in steady state"),
                        Err(e) => debug!("ignoring malformed control frame: {e}"),
                    },
                    Some(Ok(Message::Close(_))) | None => break DetachReason::Transport,
                    // `Pong` (and `Ping`, auto-answered by axum) just refresh
                    // liveness, already handled by the counter reset above.
                    Some(Ok(_)) => {}
                    Some(Err(_)) => break DetachReason::Transport,
                }
            }

            // Heartbeat: ping the peer to detect a half-open socket. A failed
            // send means the transport is gone; too many un-ponged ticks means a
            // half-open socket where sends still nominally succeed.
            _ = ping.tick() => {
                if missed_pongs >= ATTACH_MISSED_PONG_LIMIT {
                    warn!("WS peer missed {missed_pongs} heartbeat pongs; treating as dead");
                    break DetachReason::Transport;
                }
                if socket.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break DetachReason::Transport;
                }
                missed_pongs += 1;
            }
        }
    };

    // Kill the attach child explicitly (deterministic teardown) and notify the
    // client. Killing the `tmux attach-session` client detaches; the session and
    // its program keep running.
    child.kill().await;
    let _ = send_control(&mut socket, &ServerControl::Detached { reason }).await;
    let _ = socket.send(Message::Close(None)).await;
    reason
}

/// Why the attach ended once its `tmux attach-session` client has gone: the
/// session was destroyed, or the client was detached (displaced by another
/// viewer, or tmux's detach key) while it lives on.
fn detach_reason(tail: &ClientTail) -> DetachReason {
    if tail.end() == AttachEnd::SessionEnded {
        DetachReason::SessionEnded
    } else {
        DetachReason::ClientDetached
    }
}

/// Read frames until the next control (text) frame arrives, parsing it. Binary
/// frames during the handshake are unexpected and ignored. Returns `None` on
/// close, transport error, parse failure, or handshake timeout.
async fn next_control(socket: &mut WebSocket) -> Option<ClientControl> {
    let recv = async {
        loop {
            match socket.recv().await {
                Some(Ok(Message::Text(text))) => match ClientControl::from_text(&text) {
                    Ok(msg) => return Some(msg),
                    Err(e) => {
                        debug!("malformed handshake control frame: {e}");
                        return None;
                    }
                },
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return None,
                // Binary before the handshake completes is unexpected; skip it.
                // Ping/pong are handled by axum; skip them too.
                Some(Ok(_)) => continue,
            }
        }
    };

    match tokio::time::timeout(HANDSHAKE_TIMEOUT, recv).await {
        Ok(result) => result,
        Err(_) => {
            debug!("WS handshake timed out");
            None
        }
    }
}
