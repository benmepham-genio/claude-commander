//! Transport-agnostic tmux attach bridge.
//!
//! Spawns `tmux attach-session -t <name>` inside a PTY and exposes the **raw**
//! PTY reader/writer halves, a [`ResizeHandle`], and a [`ChildGuard`] that reaps
//! the `tmux attach-session` child when dropped (or killed explicitly). It knows
//! nothing about *where* bytes flow: no stdin/stdout, no SIGWINCH, no hotkeys,
//! and no intermediate channel on the data path.
//!
//! Two adapters consume this bridge:
//! - the local TUI/CLI ([`super::attach_to_session`]), which pumps the PTY
//!   halves directly to the process stdin/stdout with raw-mode + hotkey
//!   interception layered on top;
//! - the remote server's WebSocket handler, which bridges socket frames to the
//!   PTY halves.
//!
//! Keeping the spawn/resize/reaping in one place means both transports share the
//! same lifecycle and there is exactly one copy of the PTY plumbing.

use std::os::unix::io::{AsRawFd, RawFd};
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::io::{ReadHalf, WriteHalf};
use tracing::{info, warn};

use crate::backend::{AttachEnd, AttachRefresher, AttachResizer, AttachStreams, AttachTerminator};
use crate::error::Result;
use crate::tmux::isolation::TmuxTmpdir;

/// The slave (`/dev/pts/N`, `/dev/ttysN`) name of a PTY, from its master fd.
///
/// The reentrant `ptsname_r` form is required here; the classic `ptsname`
/// returns a pointer into a shared static buffer, which is not safe to call
/// from an async runtime with several PTYs in flight. The libc crate only
/// declares `ptsname_r` for Linux/BSD/illumos targets, not Apple ones, so on
/// macOS this calls the `TIOCPTYGNAME` ioctl instead — which is all Apple's own
/// `ptsname_r` is: `ioctl(fd, TIOCPTYGNAME, buf)` into a `PTSNAME_MAX_SIZE`
/// (128) buffer (apple-oss-distributions/Libc, `stdlib/grantpt.c`) — so both
/// arms have the same reentrancy: the caller's buffer, no shared state.
fn pts_name(master_fd: RawFd) -> Option<String> {
    // 128 bytes is TIOCPTYGNAME's contract (Apple Libc's PTSNAME_MAX_SIZE, the
    // 0x80 out-length baked into the request value) and comfortably holds any
    // Linux `/dev/pts/N` path.
    let mut buf = [0 as nix::libc::c_char; 128];

    // SAFETY: `master_fd` is a live PTY master (`Pty::as_raw_fd`), and `buf` is
    // a valid writable buffer of at least the length the call requires.
    #[cfg(target_os = "macos")]
    let rc = unsafe {
        nix::libc::ioctl(
            master_fd,
            nix::libc::TIOCPTYGNAME as nix::libc::c_ulong,
            buf.as_mut_ptr(),
        )
    };
    // SAFETY: as above; `buf.len()` is the buffer's real length.
    #[cfg(not(target_os = "macos"))]
    let rc = unsafe { nix::libc::ptsname_r(master_fd, buf.as_mut_ptr(), buf.len()) };

    if rc != 0 {
        warn!(
            "pts name lookup failed: {}; in-session switcher repaint disabled",
            std::io::Error::last_os_error()
        );
        return None;
    }
    // SAFETY: the call returned 0, so `buf` holds a NUL-terminated string.
    let name = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) };
    Some(name.to_string_lossy().into_owned())
}

/// Fit the target session's window to its attached clients via
/// `tmux resize-window -A`.
///
/// Makes a `window-size manual` session (the birth default, 200×50) follow the
/// attaching client — see the call site in [`HeadlessAttach::spawn`] for why the
/// pty's SIGWINCH alone doesn't. `-A` ("adjust to the attached clients") is what
/// makes this correct rather than a fixed `-x`/`-y`: tmux computes the window
/// from the client itself, so it reserves the status-bar row the client's own
/// resize would (an explicit `-y <rows>` overshoots by that one row), *without*
/// flipping `window-size` off `manual` — so a later detached capture keeps its
/// width.
///
/// Because `-A` reads the *attached clients*, it is a no-op if it runs before the
/// `tmux attach-session` child has registered as one — a real race at spawn time,
/// where the child was only just forked (measured ~16ms to register). So this
/// retries on a short bounded backoff until a client is present, then adjusts.
/// The [`ResizeHandle::resize`] caller already has a live client, so its first
/// attempt succeeds. Best-effort throughout: a failure or a giving-up leaves the
/// previous size, no worse than not trying, and the attach itself is already
/// live. Blocks on the (fast, local-socket) `tmux` calls and reaps them rather
/// than leaking zombies; the total wait is capped well under the attach's own
/// process-spawn cost.
fn resize_window(session_name: &str, tmux_tmpdir: Option<&Path>) {
    // ~16ms observed for a client to register; cap the wait an order of
    // magnitude above that so a loaded host still lands it, but the sync path
    // never stalls for long.
    const ATTEMPTS: u32 = 12;
    const BACKOFF: std::time::Duration = std::time::Duration::from_millis(15);

    for _ in 0..ATTEMPTS {
        // `-A` reads the attached clients, so it is a silent no-op with none
        // attached — wait for one rather than "succeeding" without effect.
        if !session_has_client(session_name, tmux_tmpdir) {
            std::thread::sleep(BACKOFF);
            continue;
        }
        let mut cmd = std::process::Command::new("tmux");
        cmd.args(["resize-window", "-t", session_name, "-A"]);
        match cmd.with_tmux_tmpdir(tmux_tmpdir).status() {
            Ok(s) if s.success() => return,
            Ok(s) => {
                warn!(
                    "tmux resize-window -t {session_name} -A exited with {:?}",
                    s.code()
                );
                return;
            }
            Err(e) => {
                warn!("failed to spawn tmux resize-window: {e}");
                return;
            }
        }
    }
    warn!("tmux resize-window -A gave up: no client registered for {session_name}");
}

/// Whether the session has at least one attached client. Used to avoid firing
/// `resize-window -A` (which reads the attached clients) before the attach child
/// has registered. A spawn failure is treated as "no client" so the caller
/// retries rather than adjusting against a stale view.
fn session_has_client(session_name: &str, tmux_tmpdir: Option<&Path>) -> bool {
    let mut cmd = std::process::Command::new("tmux");
    cmd.args(["list-clients", "-t", session_name, "-F", "x"]);
    match cmd.with_tmux_tmpdir(tmux_tmpdir).output() {
        Ok(out) => out.status.success() && !out.stdout.is_empty(),
        Err(_) => false,
    }
}

/// A live `tmux attach-session` running inside a PTY.
///
/// Holds the PTY (for I/O + resize) and the child process. Use [`Self::split`]
/// to break it into the independently-ownable halves an async I/O loop needs.
pub struct HeadlessAttach {
    pty: pty_process::Pty,
    child: tokio::process::Child,
    /// The PTY *slave* path (`/dev/pts/N`) — which is exactly the name tmux
    /// knows this client by, so it can be used as `refresh-client -t`. Verified
    /// against a live server: `ptsname_r` on the master and
    /// `tmux list-clients -F '#{client_tty}'` return the same string. `None` if
    /// the lookup failed, which only disables the repaint.
    client_tty: Option<String>,
    /// The tmux session this attach is on. Shared into the [`ResizeHandle`] so a
    /// resize can drive `resize-window` as well as the pty ioctl.
    session_name: Arc<str>,
    /// Kept so the refresher's/resizer's `tmux` subprocess lands on the same
    /// socket dir as the client it acts on (the server bridge runs isolated).
    tmux_tmpdir: Option<Arc<Path>>,
}

impl HeadlessAttach {
    /// Spawn `tmux attach-session -t <session_name>` in a fresh PTY sized to
    /// `cols`×`rows`.
    ///
    /// `tmux_tmpdir` isolates the attach client onto a throwaway socket dir (for
    /// hermetic tests/e2e — see [`Config::tmux_tmpdir`](crate::config::Config::tmux_tmpdir));
    /// pass `None` for normal use, which leaves the environment untouched. It
    /// must match the socket dir the target session was created on, or the
    /// client attaches to the wrong server.
    pub fn spawn(
        session_name: &str,
        cols: u16,
        rows: u16,
        tmux_tmpdir: Option<&Path>,
    ) -> Result<Self> {
        let (pty, pts) = pty_process::open()?;
        pty.resize(pty_process::Size::new(rows, cols))?;

        // `-d` detaches any other client from the session as this one attaches.
        // A tmux window is one size shared by all its clients, and under
        // `window-size manual` `resize-window -A` fits it to the *largest* — so a
        // single stale wide client (a browser tab that never cleanly detached, a
        // left-open TUI) pins the window wide and re-scrambles every narrower
        // client, defeating the fit below. Found live: a session with a lingering
        // 205-col client kept a phone's 80-col attach at 205, wrapping every line.
        // Making each attach the sole client is also the behaviour a remote
        // terminal wants — whoever attaches last drives the size.
        let mut cmd = pty_process::Command::new("tmux")
            .args(["attach-session", "-d", "-t", session_name])
            .with_tmux_tmpdir(tmux_tmpdir);
        // `tmux attach` refuses to start (or degrades to no IO) when the
        // inherited TERM is missing or "dumb" — the norm for headless hosts
        // (systemd units, CI runners). The bridge's pty does no rendering of
        // its own — bytes stream raw to the far client, whose real terminal
        // is what matters — so pin a capable TERM in that case. An inherited
        // real TERM (the local TUI attach path) is left untouched so tmux
        // emits sequences for the terminal actually displaying them.
        if let Some(term) = fallback_term(std::env::var("TERM").ok().as_deref()) {
            cmd = cmd.env("TERM", term);
        }
        // Read the slave name off the master *before* spawning consumes `pts`.
        let client_tty = pts_name(pty.as_raw_fd());
        let child = cmd.spawn(pts)?;

        info!(
            "Spawned tmux attach-session for {} (client tty {:?})",
            session_name, client_tty
        );

        let session: Arc<str> = session_name.into();
        let tmpdir: Option<Arc<Path>> = tmux_tmpdir.map(Arc::from);

        // Fit the window to this new client. Sessions are born at 200x50 with
        // `window-size manual` (so a *detached* session holds a wide pane for the
        // TUI's capture), and a `manual` window does **not** shrink to a client
        // that attaches narrower than it — the pty's SIGWINCH is ignored. Without
        // this, a phone/browser attach to a wide session renders 200-column lines
        // wrapped into its ~80-column grid: words run together and glyphs overlap.
        //
        // Run on a detached thread, not inline: `spawn` is called from the async
        // WS/attach handlers, and `resize_window` polls-and-sleeps for the attach
        // child to register as a tmux client (tens of ms) — blocking a tokio
        // worker for that would stall other tasks. Best-effort: a failure leaves
        // the previous (wrong) size, no worse than not trying.
        {
            let session = session.clone();
            let tmpdir = tmpdir.clone();
            std::thread::spawn(move || resize_window(&session, tmpdir.as_deref()));
        }

        Ok(Self {
            pty,
            child,
            client_tty,
            session_name: session,
            tmux_tmpdir: tmpdir,
        })
    }

    /// A handle that repaints this client's screen, for restoring the region an
    /// overlay covered. See [`AttachRefresher`] for why this is `refresh-client`
    /// and not a resize.
    pub fn refresh_handle(&self) -> AttachRefresher {
        let tty = self.client_tty.clone();
        let tmpdir = self.tmux_tmpdir.clone();
        AttachRefresher::new(move || {
            let tty = tty.clone();
            let tmpdir = tmpdir.clone();
            async move {
                let Some(tty) = tty else {
                    return;
                };
                let mut cmd = tokio::process::Command::new("tmux");
                cmd.args(["refresh-client", "-t", &tty]);
                let status = cmd.with_tmux_tmpdir(tmpdir.as_deref()).status().await;
                match status {
                    Ok(s) if !s.success() => {
                        warn!("tmux refresh-client -t {tty} exited with {:?}", s.code())
                    }
                    Err(e) => warn!("failed to spawn tmux refresh-client: {e}"),
                    _ => {}
                }
            }
        })
    }

    /// A handle that can resize the PTY without owning it. Cloneable; safe to
    /// move into a separate task (e.g. one driven by SIGWINCH or a `resize`
    /// control frame).
    pub fn resize_handle(&self) -> ResizeHandle {
        ResizeHandle {
            fd: self.pty.as_raw_fd(),
            session_name: self.session_name.clone(),
            tmux_tmpdir: self.tmux_tmpdir.clone(),
        }
    }

    /// Break the bridge into its independently-ownable parts:
    /// - the raw PTY reader half (PTY output → transport),
    /// - the raw PTY writer half (transport → PTY input),
    /// - a [`ResizeHandle`] for out-of-band resizes,
    /// - a [`ChildGuard`] that reaps the `tmux attach-session` child.
    ///
    /// No channel or extra copy sits between the halves and the PTY, so a
    /// consumer that pumps `reader.read() → out` and `in → writer` has the same
    /// latency/throughput as touching the PTY directly.
    pub fn split(
        self,
    ) -> (
        ReadHalf<pty_process::Pty>,
        WriteHalf<pty_process::Pty>,
        ResizeHandle,
        ChildGuard,
    ) {
        let resize = self.resize_handle();
        let (reader, writer) = tokio::io::split(self.pty);
        let guard = ChildGuard { child: self.child };
        (reader, writer, resize, guard)
    }

    /// Consume the bridge into the transport-agnostic [`AttachStreams`] the
    /// generalized attach loop drives: boxed PTY halves, an [`AttachResizer`]
    /// wrapping the `TIOCSWINSZ` ioctl, and a [`PtyTerminator`] owning the
    /// `tmux attach-session` child. Both the local backend's `attach` and the
    /// CLI's [`attach_to_session`](super::attach_to_session) build their streams
    /// this way, so there is exactly one PTY→streams adapter.
    pub fn into_streams(self) -> AttachStreams {
        let refresher = self.refresh_handle();
        // This bridge *is* a local tmux client, so the switcher may move it with
        // `switch-client` rather than re-attaching. Only the two local-attach
        // paths reach `into_streams`; the server's WS bridge uses `split`, and
        // its client is on the far side of a socket.
        let local_client_tty = self.client_tty.clone();
        let (reader, writer, resize, child) = self.split();
        let resizer = AttachResizer::new(move |cols, rows| resize.resize(cols, rows));
        AttachStreams {
            reader: Box::new(reader),
            writer: Box::new(writer),
            resizer,
            refresher,
            terminator: Box::new(PtyTerminator { child }),
            local_client_tty,
        }
    }
}

/// [`AttachTerminator`] for a local PTY attach: owns the `tmux attach-session`
/// [`ChildGuard`]. `detach` kills the attach client (leaving the tmux session +
/// its program running); `wait` reports how the client exited.
pub struct PtyTerminator {
    child: ChildGuard,
}

#[async_trait]
impl AttachTerminator for PtyTerminator {
    async fn detach(&mut self) {
        self.child.kill().await;
    }

    async fn wait(&mut self) -> AttachEnd {
        match self.child.wait().await {
            // A clean exit is a tmux detach (Ctrl+B D / our own kill).
            Ok(status) if status.success() => AttachEnd::Detached,
            // A non-clean exit means the pane's process/session ended.
            Ok(_) => AttachEnd::SessionEnded,
            Err(e) => AttachEnd::Error(e.to_string()),
        }
    }
}

/// Resizes an attach to `cols`×`rows`. Holds the PTY fd (for the `TIOCSWINSZ`
/// ioctl) plus the tmux session name and socket dir, so it can also issue a
/// `resize-window` — the ioctl alone is silently ignored by a `window-size
/// manual` session (see [`resize_window`] and [`HeadlessAttach::spawn`]). The
/// `Arc`-shared strings keep it cheap to clone across tasks.
#[derive(Debug, Clone)]
pub struct ResizeHandle {
    fd: RawFd,
    session_name: Arc<str>,
    tmux_tmpdir: Option<Arc<Path>>,
}

impl ResizeHandle {
    /// Resize the attach to `cols`×`rows`: the PTY (so the client sees the new
    /// size) and, because a `manual` window ignores that, the tmux window too.
    pub fn resize(&self, cols: u16, rows: u16) {
        use nix::libc::{TIOCSWINSZ, ioctl, winsize};

        let ws = winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };

        // SAFETY: fd comes from a live PTY (`Pty::as_raw_fd`) that outlives this
        // handle in practice; `ws` is a valid stack pointer for the call.
        let rc = unsafe { ioctl(self.fd, TIOCSWINSZ, &ws) };

        // Fit the tmux window to the client too: a `window-size manual` session
        // (the birth default) won't follow the pty's SIGWINCH, so a browser resize
        // would otherwise leave the pane at its old width and mis-wrap every line.
        // On a detached thread for the same reason as at spawn — `resize` is
        // called from the async attach pump, and `resize_window` spawns tmux
        // subprocesses. The client is already attached here, so it lands on the
        // first try; the ioctl above has already run, so `-A` reads the new size.
        {
            let session = self.session_name.clone();
            let tmpdir = self.tmux_tmpdir.clone();
            std::thread::spawn(move || resize_window(&session, tmpdir.as_deref()));
        }

        if rc != 0 {
            // A resize failure is non-fatal (the PTY keeps its previous size),
            // but worth surfacing rather than silently swallowing.
            warn!(
                "PTY resize ioctl(TIOCSWINSZ) to {cols}x{rows} failed: {}",
                std::io::Error::last_os_error()
            );
        }
    }
}

/// Owns the `tmux attach-session` child and reaps it.
///
/// Dropping the guard kills the child (best-effort, non-blocking) so a consumer
/// that simply drops the bridge — e.g. a closed browser tab — never leaks an
/// attach process. **This detaches, not kills**: only the `tmux attach-session`
/// client process dies; the tmux *session* and the program inside it keep
/// running, exactly like pressing the tmux detach key. For deterministic
/// teardown in an async context, prefer [`Self::kill`] before dropping.
pub struct ChildGuard {
    child: tokio::process::Child,
}

impl ChildGuard {
    /// Kill the `tmux attach-session` child and await its exit. Detaches the
    /// client; leaves the tmux session + its program running.
    pub async fn kill(&mut self) {
        let _ = self.child.kill().await;
    }

    /// Wait for the child to exit on its own (e.g. the user pressed tmux's
    /// detach key, or the session ended) and return its exit status. Idempotent
    /// once the child has exited.
    pub async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.child.wait().await
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        // Best-effort synchronous reap. `start_kill` only signals; the child is
        // reaped by tokio's background machinery. This is the safety net for
        // ungraceful drops — graceful paths should call `kill().await`.
        let _ = self.child.start_kill();
    }
}

/// The TERM to force on the bridge child when the inherited one can't drive
/// `tmux attach`: `None` leaves a genuine terminal's TERM untouched.
fn fallback_term(current: Option<&str>) -> Option<&'static str> {
    match current {
        Some(t) if !t.is_empty() && t != "dumb" && t != "unknown" => None,
        _ => Some("xterm-256color"),
    }
}

#[cfg(test)]
mod pts_name_tests {
    use std::os::unix::io::AsRawFd;

    use super::pts_name;

    // Regression coverage for macOS: `pts_name` originally called
    // `nix::libc::ptsname_r`, which the libc crate does not declare for Apple
    // targets, so core failed to *compile* on aarch64-apple-darwin. Any test
    // exercising `pts_name` pins the fix — this one also pins the runtime
    // contract on every platform.
    #[tokio::test]
    async fn resolves_the_slave_path_of_a_live_pty() {
        let (pty, pts) = pty_process::open().expect("failed to open a PTY");
        let name = pts_name(pty.as_raw_fd()).expect("pts_name returned None for a live master");
        assert!(
            name.starts_with("/dev/"),
            "expected a device path, got {name:?}"
        );
        drop(pts);
    }

    #[tokio::test]
    async fn returns_none_for_an_invalid_fd() {
        assert_eq!(pts_name(-1), None);
    }
}

#[cfg(test)]
mod term_tests {
    use super::fallback_term;

    #[test]
    fn headless_terms_get_a_capable_fallback() {
        // Unset/dumb/unknown/empty: the states found on CI runners and
        // systemd units, where tmux attach exits immediately without this.
        assert_eq!(fallback_term(None), Some("xterm-256color"));
        assert_eq!(fallback_term(Some("dumb")), Some("xterm-256color"));
        assert_eq!(fallback_term(Some("unknown")), Some("xterm-256color"));
        assert_eq!(fallback_term(Some("")), Some("xterm-256color"));
    }

    #[test]
    fn real_terminals_are_left_untouched() {
        assert_eq!(fallback_term(Some("xterm-256color")), None);
        assert_eq!(fallback_term(Some("tmux-256color")), None);
        assert_eq!(fallback_term(Some("screen")), None);
    }
}
