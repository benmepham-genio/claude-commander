//! [`AttachConnection`]: a live attach over the server's `/ws/attach` WebSocket.
//!
//! These are the client-local, transport-concrete attach types. The thin
//! `claude-commander-remote` adapter wraps them into core's transport-neutral
//! `AttachConnection`/`AttachStreams`/`AttachTerminator` seam (a local PTY and
//! this WebSocket then present the same shape to the TUI).
//!
//! # Shape
//!
//! [`connect`] performs the synchronous handshake (`auth` → `attach` → await
//! `ready`) and, on success, spawns a **pump task** that owns the split socket
//! for the rest of the attach. The pump is the only thing that touches the
//! socket; the caller reaches it through three seams that [`AttachConnection::split`]
//! hands out:
//!
//! - **reader / writer** — raw PTY bytes both ways. Each direction is a
//!   [`tokio::io::duplex`] pipe: the pump writes inbound binary frames into the
//!   caller's reader and reads the caller's writer to emit outbound binary
//!   frames. A duplex gives bounded buffering — and thus natural backpressure —
//!   for free.
//! - **resizer / terminator** — out-of-band control. These aren't byte streams,
//!   so they ride a separate [`WsControl`] channel wrapped in the [`ControlTx`]
//!   newtype (house style: the raw sender never appears in a signature).
//!   `detach()` sends [`WsControl::Detach`]; the resizer sends
//!   [`WsControl::Resize`].
//!
//! # Shutdown
//!
//! The pump ends — and closes the socket — on any of: a server `detached`/
//! `error` control frame, a transport close/error, an explicit
//! [`WsControl::Detach`], or the caller dropping the streams. It publishes the
//! resulting [`AttachEnd`] through a [`watch`] channel that
//! [`AttachTerminator::wait`] reads. As a belt-and-suspenders against a leaked
//! task, dropping the terminator aborts the pump handle.
//!
//! # Token safety
//!
//! The bearer token reaches the wire only inside the `auth` frame. Every error
//! this module produces is either a fixed transport string or the server's own
//! `error` message — never the token — so it can't leak into a toast or log.

use claude_commander_protocol::ws::{AttachKind, ClientControl, DetachReason, ServerControl};

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

use crate::error::{ClientError, ClientResult};

/// Bounded buffer for each direction's duplex pipe. Large enough that a burst of
/// PTY output doesn't stall on tiny writes, small enough to bound memory and
/// keep backpressure meaningful.
const PIPE_BUF: usize = 64 * 1024;

/// Read chunk for draining the caller's writer duplex before framing it.
const READ_CHUNK: usize = 8 * 1024;

/// The concrete socket type `connect_async` yields (loopback TCP, optionally
/// TLS-wrapped for `wss://`).
type WsStream = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// Why an [`AttachConnection`] ended, so a driving loop can decide whether to
/// re-attach, return to the tree, etc. Mirrors core's `AttachEnd`; the remote
/// adapter maps between them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachEnd {
    /// The tmux session was destroyed (the server sent `session_ended`).
    SessionEnded,
    /// The attach ended while the session lives on: a requested detach, or
    /// another viewer taking the pane over (`client_detached`).
    Detached,
    /// The transport failed.
    Error(String),
}

/// An out-of-band control action from the resizer/terminator to the pump. Byte
/// streams travel over the duplex pipes; only these structured actions use the
/// channel.
enum WsControl {
    Resize { cols: u16, rows: u16 },
    Refresh,
    Detach,
}

/// The pump's control inlet. Wraps the raw unbounded sender (house style: hide
/// the cursed channel type behind a named newtype with intent-named methods)
/// and is cheap to clone so the resizer and terminator can each hold one.
#[derive(Clone)]
struct ControlTx(mpsc::UnboundedSender<WsControl>);

impl ControlTx {
    /// Ask the pump to send a `resize` control frame. Fire-and-forget: a closed
    /// channel means the pump already ended, so the resize is moot.
    fn resize(&self, cols: u16, rows: u16) {
        let _ = self.0.send(WsControl::Resize { cols, rows });
    }

    /// Ask the pump to send a `refresh` control frame, so the remote tmux client
    /// repaints a region the caller drew over. Fire-and-forget for the same
    /// reason as [`Self::resize`].
    fn refresh(&self) {
        let _ = self.0.send(WsControl::Refresh);
    }

    /// Ask the pump to detach (send a `detach` frame, close the socket).
    /// Idempotent and fire-and-forget for the same reason as [`Self::resize`].
    fn detach(&self) {
        let _ = self.0.send(WsControl::Detach);
    }
}

/// The terminator's view of the pump's final [`AttachEnd`]. Wraps a [`watch`]
/// receiver so `wait()` can be polled, dropped, and re-polled without consuming
/// a one-shot.
struct EndWatch(watch::Receiver<Option<AttachEnd>>);

impl EndWatch {
    /// The end reason if the pump has already published one.
    fn current(&self) -> Option<AttachEnd> {
        self.0.borrow().clone()
    }

    /// Wait for the next change; `false` if the pump dropped its sender without
    /// publishing (an aborted/panicked task).
    async fn changed(&mut self) -> bool {
        self.0.changed().await.is_ok()
    }
}

/// A live remote attach. Opaque until [`Self::split`] distributes its seams.
pub struct AttachConnection {
    reader: DuplexStream,
    writer: DuplexStream,
    control: ControlTx,
    end: EndWatch,
    pump: JoinHandle<()>,
}

impl AttachConnection {
    /// Break the connection into its byte streams, resize handle, and
    /// terminator. Consumes the connection.
    pub fn split(self) -> AttachStreams {
        AttachStreams {
            reader: self.reader,
            writer: self.writer,
            resizer: AttachResizer {
                control: self.control.clone(),
            },
            refresher: AttachRefresher {
                control: self.control.clone(),
            },
            terminator: AttachTerminator {
                control: self.control,
                end: self.end,
                pump: Some(self.pump),
            },
        }
    }
}

/// The independently-ownable parts of a live attach, ready for an interactive
/// I/O loop: raw byte streams both ways, a resize handle, and a terminator.
pub struct AttachStreams {
    pub reader: DuplexStream,
    pub writer: DuplexStream,
    pub resizer: AttachResizer,
    pub refresher: AttachRefresher,
    pub terminator: AttachTerminator,
}

/// Resizes an attached terminal. Cheaply cloneable and `Send + Sync`, so a
/// SIGWINCH task can hold a clone while the main loop owns the streams.
#[derive(Clone)]
pub struct AttachResizer {
    control: ControlTx,
}

impl AttachResizer {
    /// Resize to `cols`×`rows`. Fire-and-forget: a failed resize is non-fatal.
    pub fn resize(&self, cols: u16, rows: u16) {
        self.control.resize(cols, rows);
    }
}

/// Asks the remote tmux client to repaint its whole visible screen, restoring a
/// region the caller drew over (the TUI's in-session switcher paints the palette
/// on top of the live pane). Cheaply cloneable, like [`AttachResizer`].
#[derive(Clone)]
pub struct AttachRefresher {
    control: ControlTx,
}

impl AttachRefresher {
    /// Repaint. Fire-and-forget: a dropped refresh leaves a stale rectangle,
    /// which the pane's next output overwrites anyway.
    pub fn refresh(&self) {
        self.control.refresh();
    }
}

/// Teardown + termination-signal half of an attach. `detach` nudges the pump to
/// send a `detach` frame and tear down; `wait` resolves from the pump's
/// published [`AttachEnd`]. Dropping it aborts the pump task as a leak safety net.
pub struct AttachTerminator {
    control: ControlTx,
    end: EndWatch,
    pump: Option<JoinHandle<()>>,
}

impl AttachTerminator {
    /// Explicitly end the attach: send a `detach` frame, killing the attach
    /// child while the tmux session keeps running. Idempotent.
    pub async fn detach(&mut self) {
        self.control.detach();
    }

    /// Wait for the attach to end and report why. After [`Self::detach`] this
    /// resolves promptly.
    pub async fn wait(&mut self) -> AttachEnd {
        loop {
            if let Some(end) = self.end.current() {
                return end;
            }
            if !self.end.changed().await {
                // The pump dropped its sender without publishing an end — it was
                // aborted or panicked. Report a transport error rather than hang.
                return AttachEnd::Error("attach pump ended unexpectedly".to_string());
            }
        }
    }
}

impl Drop for AttachTerminator {
    fn drop(&mut self) {
        // Safety net: if the whole attach is dropped without a graceful detach,
        // abort the pump so its socket + task don't leak. The graceful paths have
        // already let the pump exit, so this is usually a no-op.
        if let Some(pump) = self.pump.take() {
            pump.abort();
        }
    }
}

/// Open a remote attach: connect, handshake (`auth` → `attach` → `resize` →
/// await `ready`), then spawn the pump. On any handshake failure the socket is
/// dropped and a classified [`ClientError`] returned; the token never appears in
/// that error.
pub(crate) async fn connect(
    url: &str,
    token: Option<&str>,
    session_id: String,
    cols: u16,
    rows: u16,
    kind: AttachKind,
) -> ClientResult<AttachConnection> {
    let (ws, _resp) = connect_async(url)
        .await
        .map_err(|_| ClientError::Unavailable {
            reason: "could not connect to server".to_string(),
        })?;
    let (mut write, mut read) = ws.split();

    // Handshake frames. The token rides only in this `auth` frame.
    send_control(
        &mut write,
        ClientControl::Auth {
            token: token.unwrap_or_default().to_string(),
        },
    )
    .await?;
    // The size rides in the handshake so the server opens the PTY at it before
    // spawning `tmux attach-session` — tmux's very first paint is then already
    // at our width. Announcing it only afterwards leaves one paint at the
    // server's fallback geometry, which this end wraps at its own width and
    // tmux's incremental repaint never clears.
    send_control(
        &mut write,
        ClientControl::Attach {
            session_id,
            kind,
            cols: Some(cols),
            rows: Some(rows),
        },
    )
    .await?;

    await_ready(&mut read, &mut write).await?;

    // Belt and braces for an older server, which ignores the handshake's
    // dimensions and starts the PTY at its own default. Against a current
    // server the PTY already has this size, so the ioctl changes nothing and no
    // SIGWINCH is raised — on Linux `tty_do_resize` (drivers/tty/tty_ioctl.c)
    // returns early when the winsize is unchanged, before it signals. It costs
    // one frame and nothing else.
    send_control(&mut write, ClientControl::Resize { cols, rows }).await?;

    // Steady-state plumbing: one duplex per direction + the control channel.
    let (pump_out, caller_reader) = tokio::io::duplex(PIPE_BUF);
    let (caller_writer, pump_in) = tokio::io::duplex(PIPE_BUF);
    let (control_tx, control_rx) = mpsc::unbounded_channel();
    let (end_tx, end_rx) = watch::channel(None);

    let pump = tokio::spawn(run_pump(write, read, pump_out, pump_in, control_rx, end_tx));

    Ok(AttachConnection {
        reader: caller_reader,
        writer: caller_writer,
        control: ControlTx(control_tx),
        end: EndWatch(end_rx),
        pump,
    })
}

/// Serialize and send a client control frame, mapping a serialize failure to
/// [`ClientError::Protocol`] and a transport failure to
/// [`ClientError::Unavailable`]. Neither reason carries the token.
async fn send_control(
    write: &mut SplitSink<WsStream, Message>,
    msg: ClientControl,
) -> ClientResult<()> {
    let text = msg
        .to_text()
        .map_err(|_| ClientError::Protocol("could not encode control frame".to_string()))?;
    write
        .send(Message::Text(text.into()))
        .await
        .map_err(|_| ClientError::Unavailable {
            reason: "connection to server lost during handshake".to_string(),
        })
}

/// Read frames until the server's `ready` arrives (success), classifying an
/// `error` frame or a premature close into a [`ClientError`]. Answers pings
/// while waiting.
async fn await_ready(
    read: &mut SplitStream<WsStream>,
    write: &mut SplitSink<WsStream, Message>,
) -> ClientResult<()> {
    loop {
        match read.next().await {
            Some(Ok(Message::Text(text))) => match ServerControl::from_text(&text) {
                Ok(ServerControl::Ready { .. }) => return Ok(()),
                Ok(ServerControl::Error { message }) => return Err(handshake_error(&message)),
                // A `detached` before `ready` is unexpected; treat as unavailable.
                Ok(ServerControl::Detached { .. }) => {
                    return Err(ClientError::Unavailable {
                        reason: "server detached before the attach was ready".to_string(),
                    });
                }
                // Ignore anything we don't recognise and keep waiting.
                Err(_) => continue,
            },
            Some(Ok(Message::Ping(payload))) => {
                let _ = write.send(Message::Pong(payload)).await;
            }
            // Binary/pong before `ready` is unexpected; skip and keep waiting.
            Some(Ok(_)) => continue,
            Some(Err(_)) | None => {
                return Err(ClientError::Unavailable {
                    reason: "server closed the connection during handshake".to_string(),
                });
            }
        }
    }
}

/// Classify a server `error` frame received *before* `ready`. The message is the
/// server's own text (never token-bearing): the fixed auth-rejection phrase maps
/// to [`ClientError::Auth`], the no-session frame to [`ClientError::NotFound`],
/// anything else to [`ClientError::Server`].
///
/// Both pre-ready sentinels are matched *exactly* (or, for the no-session case,
/// as a `"{constant}: detail"` prefix should the server ever attach detail) —
/// never by substring. A generic `"failed to attach: {tmux error}"` frame can
/// embed tmux's own "no such session" wording, and a `contains` check would
/// misclassify it as [`ClientError::NotFound`].
fn handshake_error(message: &str) -> ClientError {
    use claude_commander_protocol::ws::{WS_ERR_AUTH, WS_ERR_NO_SESSION};
    if message == WS_ERR_AUTH {
        ClientError::Auth
    } else if message == WS_ERR_NO_SESSION || message.starts_with(&format!("{WS_ERR_NO_SESSION}: "))
    {
        ClientError::NotFound
    } else {
        ClientError::Server(message.to_string())
    }
}

/// The steady-state pump: owns the split socket and both duplex ends for the
/// life of the attach. Bridges binary frames ↔ the caller's byte streams,
/// honours `resize`/`detach` control actions, answers pings, and publishes the
/// terminal [`AttachEnd`]. Returns (and closes the socket) on the first teardown
/// condition.
///
/// Wedge case: the server→caller `write_all` applies backpressure by design, so
/// a caller that stops *reading* its `to_caller` end without *dropping* it can
/// stall this branch — a `detach`/`resize` from `control_rx` still lands (that's
/// a separate select branch), but a server-side end can't be surfaced until the
/// stalled write unblocks or the caller drops the stream. In practice the caller
/// is the [`AttachTerminator`], whose `Drop` aborts the pump task, so the stall
/// is bounded by the caller releasing its handle rather than being unbounded.
async fn run_pump(
    mut ws_write: SplitSink<WsStream, Message>,
    mut ws_read: SplitStream<WsStream>,
    mut to_caller: DuplexStream,
    mut from_caller: DuplexStream,
    mut control_rx: mpsc::UnboundedReceiver<WsControl>,
    end_tx: watch::Sender<Option<AttachEnd>>,
) {
    let mut in_buf = [0u8; READ_CHUNK];

    // `we_detached` distinguishes a client-initiated teardown (send a `detach`
    // frame on the way out) from a server/transport-driven end (just close).
    let (end, we_detached) = loop {
        tokio::select! {
            // Server → caller. `write_all` applies backpressure: a slow caller
            // pauses this branch, pausing further socket reads, so memory stays
            // bounded (the duplex buffer is the only slack).
            frame = ws_read.next() => match frame {
                Some(Ok(Message::Binary(bytes))) => {
                    if to_caller.write_all(&bytes).await.is_err() {
                        // Caller dropped its reader — treat as a client detach.
                        break (AttachEnd::Detached, true);
                    }
                }
                Some(Ok(Message::Text(text))) => match ServerControl::from_text(&text) {
                    Ok(ServerControl::Detached { reason }) => break (end_for(reason), false),
                    Ok(ServerControl::Error { message }) => break (AttachEnd::Error(message), false),
                    // `ready` only appears in the handshake; ignore if repeated.
                    Ok(ServerControl::Ready { .. }) | Err(_) => {}
                },
                Some(Ok(Message::Ping(payload))) => {
                    if ws_write.send(Message::Pong(payload)).await.is_err() {
                        break (transport_lost(), false);
                    }
                }
                Some(Ok(Message::Close(_))) | None => break (transport_lost(), false),
                // Pong / raw frame: liveness only, nothing to do.
                Some(Ok(_)) => {}
                Some(Err(_)) => break (transport_lost(), false),
            },

            // Caller → server (raw PTY input as binary frames).
            read = from_caller.read(&mut in_buf) => match read {
                // EOF: the caller dropped its writer — a clean client teardown.
                Ok(0) => break (AttachEnd::Detached, true),
                Ok(n) => {
                    if ws_write
                        .send(Message::Binary(in_buf[..n].to_vec().into()))
                        .await
                        .is_err()
                    {
                        break (transport_lost(), false);
                    }
                }
                Err(_) => break (AttachEnd::Detached, true),
            },

            // Out-of-band control.
            control = control_rx.recv() => match control {
                Some(WsControl::Resize { cols, rows }) => {
                    if let Ok(text) = (ClientControl::Resize { cols, rows }).to_text() {
                        // A failed resize is non-fatal; keep pumping.
                        let _ = ws_write.send(Message::Text(text.into())).await;
                    }
                }
                Some(WsControl::Refresh) => {
                    if let Ok(text) = ClientControl::Refresh.to_text() {
                        // A failed refresh is non-fatal; keep pumping.
                        let _ = ws_write.send(Message::Text(text.into())).await;
                    }
                }
                Some(WsControl::Detach) => break (AttachEnd::Detached, true),
                // All senders dropped (streams dropped without an explicit
                // detach): tear down as a client detach.
                None => break (AttachEnd::Detached, true),
            },
        }
    };

    // Teardown. On a client-initiated end, tell the server to detach (kill the
    // attach child, leave the session running) before closing.
    if we_detached && let Ok(text) = ClientControl::Detach.to_text() {
        let _ = ws_write.send(Message::Text(text.into())).await;
    }
    let _ = ws_write.close().await;
    let _ = end_tx.send(Some(end));
}

/// Fixed transport-loss end reason (post-`ready` socket drop). The interactive
/// loop turns this into a toast and returns to the tree — never a panic.
fn transport_lost() -> AttachEnd {
    AttachEnd::Error("connection to server lost".to_string())
}

/// Map the server's [`DetachReason`] onto the [`AttachEnd`].
fn end_for(reason: DetachReason) -> AttachEnd {
    match reason {
        DetachReason::ClientRequest => AttachEnd::Detached,
        DetachReason::SessionEnded => AttachEnd::SessionEnded,
        // Displaced by another viewer, or tmux's detach key: the session is
        // alive, so a quiet detach — never a restart, never a reconnect.
        DetachReason::ClientDetached => AttachEnd::Detached,
        DetachReason::Transport => transport_lost(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handshake_error_classifies_by_message() {
        use claude_commander_protocol::ws::{WS_ERR_AUTH, WS_ERR_NO_SESSION};
        // Build the messages from the SAME constants the server formats its
        // error frames with, so the classifier and the server can't drift.
        assert!(matches!(handshake_error(WS_ERR_AUTH), ClientError::Auth));
        assert!(matches!(
            handshake_error(WS_ERR_NO_SESSION),
            ClientError::NotFound
        ));
        match handshake_error("failed to attach: boom") {
            ClientError::Server(m) => assert_eq!(m, "failed to attach: boom"),
            other => panic!("expected Server, got {other:?}"),
        }
        // A generic attach failure whose *detail* happens to embed tmux's own
        // "no such session" wording must NOT be misclassified as NotFound — the
        // pre-ready no-session frame is the constant verbatim, not a substring.
        match handshake_error("failed to attach: no such session: foo") {
            ClientError::Server(_) => {}
            other => panic!("embedded 'no such session' must stay Server, got {other:?}"),
        }
    }

    #[test]
    fn detach_reason_maps_to_attach_end() {
        assert_eq!(end_for(DetachReason::ClientRequest), AttachEnd::Detached);
        assert_eq!(end_for(DetachReason::SessionEnded), AttachEnd::SessionEnded);
        assert_eq!(end_for(DetachReason::ClientDetached), AttachEnd::Detached);
        // A server-side transport detach surfaces as an error end.
        assert!(matches!(
            end_for(DetachReason::Transport),
            AttachEnd::Error(_)
        ));
    }

    use std::net::SocketAddr;
    use std::time::Duration;

    use tokio::net::TcpListener;
    use tokio::sync::oneshot;
    use tokio_tungstenite::accept_async;

    /// A minimal loopback WebSocket server that speaks just enough of the attach
    /// protocol to bring a client to `ready`: it accepts one connection, reads
    /// the `auth` + `attach` handshake frames, replies `ready`, then drains until
    /// the client closes the socket — firing `closed` when it does. Hermetic: a
    /// plain loopback TCP listener, no tmux, no real server.
    async fn spawn_ready_ws_server() -> (SocketAddr, oneshot::Receiver<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (closed_tx, closed_rx) = oneshot::channel();

        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(tcp).await.unwrap();

            // Handshake: consume `auth` then `attach`, reply `ready`.
            let _auth = ws.next().await;
            let _attach = ws.next().await;
            ws.send(Message::Text(
                ServerControl::Ready {
                    session: "cc-test".to_string(),
                }
                .to_text()
                .into(),
            ))
            .await
            .unwrap();

            // Drain until the client closes; then report it.
            while let Some(frame) = ws.next().await {
                if matches!(frame, Ok(Message::Close(_)) | Err(_)) {
                    break;
                }
            }
            let _ = closed_tx.send(());
        });

        (addr, closed_rx)
    }

    /// Dropping the byte streams (writer EOF) makes the pump self-tear-down and
    /// close the socket — no leaked task — even with the terminator still held
    /// (so this proves the *self*-exit, not just the drop-abort safety net). The
    /// terminator then reports a clean `Detached`.
    #[tokio::test]
    async fn pump_task_stops_when_streams_dropped() {
        let (addr, closed_rx) = spawn_ready_ws_server().await;
        let url = format!("ws://{addr}/ws/attach");

        let conn = connect(&url, None, "sid".to_string(), 80, 24, AttachKind::Agent)
            .await
            .expect("handshake should reach ready");
        let AttachStreams {
            reader,
            writer,
            resizer,
            refresher,
            mut terminator,
        } = conn.split();

        // Drop the byte streams + control handles, but KEEP the terminator (so
        // its Drop-abort can't be what stops the pump). The writer's EOF must
        // drive the pump to close the socket on its own.
        drop(reader);
        drop(writer);
        drop(resizer);
        drop(refresher);

        // The server must observe the socket close promptly.
        tokio::time::timeout(Duration::from_secs(5), closed_rx)
            .await
            .expect("pump should close the socket after the streams drop")
            .expect("server task should signal closure");

        // And the terminator reports the clean client detach.
        let end = tokio::time::timeout(Duration::from_secs(5), terminator.wait())
            .await
            .expect("wait should resolve once the pump published its end");
        assert_eq!(end, AttachEnd::Detached);
    }

    /// A connect failure (nothing listening) maps to `Unavailable`, and the
    /// token never rides in the error.
    #[tokio::test]
    async fn connect_refused_is_unavailable_without_token() {
        // Grab a free port then drop the listener so the connect is refused.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let url = format!("ws://{addr}/ws/attach");

        // `AttachConnection` isn't `Debug`, so match rather than `expect_err`.
        let err = match connect(
            &url,
            Some("TOKEN_DO_NOT_LEAK_attach"),
            "sid".to_string(),
            80,
            24,
            AttachKind::Agent,
        )
        .await
        {
            Err(e) => e,
            Ok(_) => panic!("connect to a dead port must fail"),
        };
        assert!(
            matches!(err, ClientError::Unavailable { .. }),
            "got {err:?}"
        );
        assert!(!format!("{err:?}").contains("TOKEN_DO_NOT_LEAK_attach"));
    }
}
