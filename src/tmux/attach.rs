//! Async PTY-based tmux session attachment
//!
//! Provides fully async terminal attachment that runs within the tokio runtime,
//! avoiding the need to drop and recreate the runtime for each attach operation.

use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crossterm::terminal::{self, disable_raw_mode, enable_raw_mode};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Mutex, mpsc};
use tracing::{debug, info, warn};

use crate::error::Result;

/// Result of a session attachment attempt
#[derive(Debug)]
pub enum AttachResult {
    /// User detached with Ctrl+Q or tmux detach (Ctrl+B D)
    Detached,
    /// User pressed Ctrl+\ to toggle between Claude and shell sessions
    SwitchToShell,
    /// User pressed Ctrl+. to open the editor for the session's worktree
    OpenEditor,
    /// The session/process ended
    SessionEnded,
    /// An error occurred during attachment
    Error(String),
}

/// Outcome of an attach. `final_session` is the tmux session the client
/// was attached to when the attach loop exited — usually the same as the
/// session passed in, but updated when the user picks a different
/// session via the in-session switcher (Ctrl+O), which runs `tmux
/// switch-client` mid-attach.
#[derive(Debug)]
pub struct AttachOutcome {
    pub result: AttachResult,
    pub final_session: String,
}

/// Async PTY attachment - runs entirely within tokio
///
/// Spawns `tmux attach-session` in a PTY and bridges stdin/stdout asynchronously.
/// Returns when the user detaches (Ctrl+Q or Ctrl+B D) or the session ends.
///
/// `editor_triggers` is a list of byte patterns that, when seen on stdin,
/// cause the attach loop to exit with [`AttachResult::OpenEditor`]. Callers
/// compute these from the user's `OpenInEditor` keybindings — typically a
/// single control byte for `Ctrl-<letter>` bindings, or CSI-u/modifyOtherKeys
/// sequences for `Ctrl-<non-letter>` bindings. Bindings that cannot be
/// detected in raw stdin (e.g. a bare letter) should simply be omitted.
///
/// When `intercept_ctrl_z` is true, Ctrl+Z (`0x1A`) bytes are stripped from
/// stdin before reaching the pane. Use this for Claude sessions where SIGTSTP
/// would freeze the pane with no shell to recover from. Leave it false for
/// shell sessions, where Ctrl+Z is genuinely useful for job control.
pub async fn attach_to_session(
    session_name: &str,
    editor_triggers: Vec<Vec<u8>>,
    intercept_ctrl_z: bool,
) -> Result<AttachOutcome> {
    // Get terminal size
    let (cols, rows) = terminal::size().unwrap_or((80, 24));

    // Open PTY (async)
    let (pty, pts) = pty_process::open()?;
    pty.resize(pty_process::Size::new(rows, cols))?;

    // Get the raw fd for resize operations (before we split the pty)
    let pty_fd = pty.as_raw_fd();

    // Spawn tmux attach-session
    let cmd = pty_process::Command::new("tmux").args(["attach-session", "-t", session_name]);
    let mut child = cmd.spawn(pts)?;

    info!("Spawned tmux attach-session for {}", session_name);

    // Enter raw mode
    info!("Enabling raw mode for PTY session");
    enable_raw_mode()?;

    // Shared state for the in-session switcher: the popup task updates
    // `current_session` after a successful `tmux switch-client`, and the
    // attach outcome reports it back to the caller so subsequent state
    // (shell-toggle pair, editor open) uses the right session.
    let current_session = Arc::new(Mutex::new(session_name.to_string()));
    let popup_open = Arc::new(AtomicBool::new(false));

    // Run the async I/O loop
    info!("Starting async I/O loop");
    let result = run_async_loop(
        pty,
        pty_fd,
        &mut child,
        editor_triggers,
        intercept_ctrl_z,
        current_session.clone(),
        popup_open,
    )
    .await;
    info!("Async I/O loop ended with result: {:?}", result);

    // Restore terminal
    info!("Disabling raw mode");
    let _ = disable_raw_mode();
    let _ = std::io::stdout().flush();

    // Flush any leftover input at the kernel level BEFORE child cleanup
    info!("Flushing stdin with tcflush (before child wait)");
    flush_stdin();
    log_pending_stdin("after first tcflush");

    // Ensure child is cleaned up
    info!("Waiting for child process");
    let _ = child.wait().await;
    info!("Child process finished");

    // Flush again after child exits
    info!("Flushing stdin with tcflush (after child wait)");
    flush_stdin();
    log_pending_stdin("after second tcflush");

    let final_session = current_session.lock().await.clone();
    info!(
        "Attach complete, result: {:?}, final session: {}",
        result, final_session
    );

    Ok(AttachOutcome {
        result,
        final_session,
    })
}

/// Return true if `haystack` contains `needle` as a contiguous subsequence.
fn contains_subsequence(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || needle.len() > haystack.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Strip Ctrl+Z (0x1A) bytes from `data`. Returns `Some(filtered)` when any
/// were removed, `None` otherwise so callers can keep using the original
/// borrow without an allocation.
///
/// Ctrl+Z reaches the foreground process inside the tmux pane as SIGTSTP and
/// suspends it. Since tmux launches Claude directly with no shell wrapper,
/// there's no `fg` to recover with — the pane just freezes. Users hit it by
/// accident; Claude doesn't read it.
fn strip_ctrl_z(data: &[u8]) -> Option<Vec<u8>> {
    data.contains(&0x1A)
        .then(|| data.iter().copied().filter(|b| *b != 0x1A).collect())
}

/// Log any pending bytes in stdin for debugging
fn log_pending_stdin(context: &str) {
    use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
    use std::io::Read;
    use std::os::unix::io::{AsFd, AsRawFd};

    let stdin = std::io::stdin();
    let fd = stdin.as_fd();
    let mut poll_fds = [PollFd::new(fd, PollFlags::POLLIN)];

    // Check if there's data available (non-blocking)
    match poll(&mut poll_fds, PollTimeout::ZERO) {
        Ok(n) if n > 0 => {
            // There's data - try to read it
            let flags = unsafe { nix::libc::fcntl(stdin.as_raw_fd(), nix::libc::F_GETFL) };
            unsafe {
                nix::libc::fcntl(
                    stdin.as_raw_fd(),
                    nix::libc::F_SETFL,
                    flags | nix::libc::O_NONBLOCK,
                )
            };

            let mut buf = [0u8; 256];
            let mut stdin_lock = stdin.lock();
            match stdin_lock.read(&mut buf) {
                Ok(n) if n > 0 => {
                    let bytes = &buf[..n];
                    let as_str: String = bytes
                        .iter()
                        .map(|b| {
                            if *b >= 32 && *b < 127 {
                                format!("{}", *b as char)
                            } else {
                                format!("\\x{:02x}", b)
                            }
                        })
                        .collect();
                    warn!("STDIN {} - JUNK FOUND ({} bytes): {}", context, n, as_str);
                }
                Ok(_) => info!(
                    "STDIN {} - empty (poll said data but read got none)",
                    context
                ),
                Err(e) => info!("STDIN {} - read error: {}", context, e),
            }
            drop(stdin_lock);

            unsafe { nix::libc::fcntl(stdin.as_raw_fd(), nix::libc::F_SETFL, flags) };
        }
        Ok(_) => info!("STDIN {} - empty", context),
        Err(e) => info!("STDIN {} - poll error: {}", context, e),
    }
}

/// Flush any pending input from stdin at the kernel level
pub fn flush_stdin() {
    use nix::sys::termios::{FlushArg, tcflush};

    let _ = tcflush(std::io::stdin(), FlushArg::TCIFLUSH);
}

/// Resize PTY using ioctl
fn resize_pty(fd: i32, rows: u16, cols: u16) {
    use nix::libc::{TIOCSWINSZ, ioctl, winsize};

    let ws = winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };

    // SAFETY: fd is valid (from pty.as_raw_fd()), ws is valid stack pointer
    unsafe {
        ioctl(fd, TIOCSWINSZ, &ws);
    }
}

async fn run_async_loop(
    pty: pty_process::Pty,
    pty_fd: i32,
    child: &mut tokio::process::Child,
    editor_triggers: Vec<Vec<u8>>,
    intercept_ctrl_z: bool,
    current_session: Arc<Mutex<String>>,
    popup_open: Arc<AtomicBool>,
) -> AttachResult {
    // Channel for shutdown signal
    let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<AttachResult>(1);

    // Split PTY into read and write halves for concurrent access
    let (mut pty_reader, mut pty_writer) = tokio::io::split(pty);

    // Task 1: PTY output -> stdout
    let stdout_shutdown = shutdown_tx.clone();
    let stdout_task = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        let mut buf = [0u8; 4096];

        loop {
            match pty_reader.read(&mut buf).await {
                Ok(0) => {
                    let _ = stdout_shutdown.send(AttachResult::SessionEnded).await;
                    break;
                }
                Ok(n) => {
                    if stdout.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                    let _ = stdout.flush().await;
                }
                Err(e) => {
                    // EIO is expected when PTY closes
                    if e.raw_os_error() != Some(5) {
                        warn!("PTY read error: {}", e);
                    }
                    let _ = stdout_shutdown.send(AttachResult::SessionEnded).await;
                    break;
                }
            }
        }
    });

    // Task 2: stdin -> PTY (raw byte forwarding, no crossterm EventStream)
    // We use raw stdin to avoid conflicting with TUI's EventStream
    let stdin_shutdown = shutdown_tx.clone();
    let stdin_task = tokio::spawn(async move {
        let mut stdin = tokio::io::stdin();
        let mut buf = [0u8; 1024];

        loop {
            match stdin.read(&mut buf).await {
                Ok(0) => break, // EOF
                Ok(n) => {
                    let data = &buf[..n];

                    // While the in-session switcher popup is open, forward
                    // every byte to tmux verbatim. tmux routes keystrokes to
                    // the popup (which has its own PTY), and our hotkeys
                    // (Ctrl+Q etc.) shouldn't fire while the user is in the
                    // picker.
                    if popup_open.load(Ordering::Acquire) {
                        if pty_writer.write_all(data).await.is_err() {
                            break;
                        }
                        let _ = pty_writer.flush().await;
                        continue;
                    }

                    // Check for Ctrl+Q (0x11) anywhere in the input
                    if data.contains(&0x11) {
                        debug!("Ctrl+Q detected, detaching");
                        let _ = stdin_shutdown.send(AttachResult::Detached).await;
                        break;
                    }

                    // Check for Ctrl+\ (0x1C) to toggle shell
                    if data.contains(&0x1C) {
                        debug!("Ctrl+\\ detected, switching to shell");
                        let _ = stdin_shutdown.send(AttachResult::SwitchToShell).await;
                        break;
                    }

                    // Check for Ctrl+O (0x0F): open the switcher popup over
                    // the attached pane. We swallow the byte (don't forward
                    // it) and spawn a task that runs `tmux display-popup`
                    // followed by `tmux switch-client` on selection. The
                    // attach loop keeps running so the user stays "in" the
                    // pane the whole time.
                    if data.contains(&0x0F) {
                        if popup_open
                            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                            .is_ok()
                        {
                            debug!("Ctrl+O detected, spawning switcher popup");
                            let popup_open = popup_open.clone();
                            let current_session = current_session.clone();
                            tokio::spawn(async move {
                                run_switcher_popup(current_session, popup_open).await;
                            });
                        }
                        // Skip the 0x0F byte; never forward it.
                        let filtered: Vec<u8> =
                            data.iter().copied().filter(|b| *b != 0x0F).collect();
                        if filtered.is_empty() {
                            continue;
                        }
                        if pty_writer.write_all(&filtered).await.is_err() {
                            break;
                        }
                        let _ = pty_writer.flush().await;
                        continue;
                    }

                    // Check for any user-configured editor trigger bytes.
                    // Empty `editor_triggers` (the default) disables this
                    // feature entirely — the user's OpenInEditor binding has
                    // no encoding that makes sense inside a tmux attach.
                    if editor_triggers
                        .iter()
                        .any(|pat| contains_subsequence(data, pat))
                    {
                        debug!("Editor trigger detected, opening editor");
                        let _ = stdin_shutdown.send(AttachResult::OpenEditor).await;
                        break;
                    }

                    let stripped = if intercept_ctrl_z {
                        strip_ctrl_z(data)
                    } else {
                        None
                    };
                    if stripped.is_some() {
                        debug!("Ctrl+Z stripped from input");
                    }
                    let data: &[u8] = stripped.as_deref().unwrap_or(data);
                    if data.is_empty() {
                        continue;
                    }

                    // Forward raw bytes to PTY
                    if pty_writer.write_all(data).await.is_err() {
                        break;
                    }
                    let _ = pty_writer.flush().await;
                }
                Err(e) => {
                    warn!("stdin read error: {}", e);
                    break;
                }
            }
        }
    });

    // Task 3: SIGWINCH handling (Unix only, as backup for resize events)
    #[cfg(unix)]
    let resize_task = tokio::spawn(async move {
        use tokio::signal::unix::{SignalKind, signal};

        if let Ok(mut sigwinch) = signal(SignalKind::window_change()) {
            loop {
                sigwinch.recv().await;
                if let Ok((cols, rows)) = terminal::size() {
                    resize_pty(pty_fd, rows, cols);
                }
            }
        }
    });

    #[cfg(not(unix))]
    let resize_task = tokio::spawn(async {});

    // Wait for shutdown signal or child exit
    let result = tokio::select! {
        result = shutdown_rx.recv() => {
            result.unwrap_or(AttachResult::Detached)
        }
        status = child.wait() => {
            match status {
                Ok(s) if s.success() => AttachResult::Detached,
                Ok(_) => AttachResult::SessionEnded,
                Err(_) => AttachResult::SessionEnded,
            }
        }
    };

    // Abort spawned tasks
    stdout_task.abort();
    stdin_task.abort();
    resize_task.abort();

    result
}

/// Single-quote shell-escape `s` for embedding in a tmux `display-popup`
/// shell command. Wraps in `'…'` and escapes any embedded single quotes
/// as `'\''` (close-quote, literal quote, re-open-quote).
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Run the in-session switcher: spawn a `tmux display-popup` showing
/// the picker subcommand, then on a non-empty result `tmux
/// switch-client` to the chosen session and record it in
/// `current_session`. Always clears `popup_open` before returning.
async fn run_switcher_popup(current_session: Arc<Mutex<String>>, popup_open: Arc<AtomicBool>) {
    let current_name = current_session.lock().await.clone();
    let new_session = run_switcher_popup_inner(&current_name).await;
    if let Some(name) = new_session {
        info!("Switcher picked session: {}", name);
        let switch_status = tokio::process::Command::new("tmux")
            .args(["switch-client", "-t", &name])
            .status()
            .await;
        match switch_status {
            Ok(s) if s.success() => {
                *current_session.lock().await = name;
            }
            Ok(s) => warn!("tmux switch-client exited with {:?}", s.code()),
            Err(e) => warn!("Failed to spawn tmux switch-client: {}", e),
        }
    }
    popup_open.store(false, Ordering::Release);
}

/// Spawn the popup and return the chosen session name, if any.
async fn run_switcher_popup_inner(current_session: &str) -> Option<String> {
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(|s| s.to_string()))
        .unwrap_or_else(|| "claude-commander".to_string());
    let tmp = std::env::temp_dir().join(format!(
        "cc-pick-{}-{}.txt",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));

    let popup_cmd = format!(
        "{} pick-session --out {} --current {}",
        shell_quote(&exe),
        shell_quote(&tmp.to_string_lossy()),
        shell_quote(current_session),
    );

    let status = tokio::process::Command::new("tmux")
        .args(["display-popup", "-E", "-h", "70%", "-w", "70%", &popup_cmd])
        .status()
        .await;

    match status {
        Ok(s) if !s.success() => {
            warn!("tmux display-popup exited with {:?}", s.code());
        }
        Err(e) => {
            warn!("Failed to spawn tmux display-popup: {}", e);
        }
        _ => {}
    }

    let pick = tokio::fs::read_to_string(&tmp).await.ok();
    let _ = tokio::fs::remove_file(&tmp).await;
    pick.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_contains_subsequence_finds_needle() {
        assert!(contains_subsequence(b"hello world", b"world"));
        assert!(contains_subsequence(b"\x1b[46;5u", b"\x1b[46;5u"));
        assert!(contains_subsequence(b"abc\x1b[46;5udef", b"\x1b[46;5u"));
    }

    #[test]
    fn test_contains_subsequence_rejects_missing() {
        assert!(!contains_subsequence(b"hello", b"world"));
        assert!(!contains_subsequence(b"\x1b[45;5u", b"\x1b[46;5u"));
    }

    #[test]
    fn test_contains_subsequence_empty_cases() {
        assert!(!contains_subsequence(b"", b"x"));
        assert!(!contains_subsequence(b"x", b""));
        assert!(!contains_subsequence(b"ab", b"abc"));
    }

    #[test]
    fn test_strip_ctrl_z_removes_byte() {
        assert_eq!(strip_ctrl_z(b"\x1a"), Some(vec![]));
        assert_eq!(strip_ctrl_z(b"a\x1ab"), Some(b"ab".to_vec()));
        assert_eq!(strip_ctrl_z(b"\x1a\x1a\x1a"), Some(vec![]));
        assert_eq!(strip_ctrl_z(b"hi\x1a"), Some(b"hi".to_vec()));
    }

    #[test]
    fn test_strip_ctrl_z_passthrough_when_absent() {
        assert_eq!(strip_ctrl_z(b""), None);
        assert_eq!(strip_ctrl_z(b"hello"), None);
        // Other control bytes must not be stripped.
        assert_eq!(strip_ctrl_z(b"\x03\x11\x1c"), None);
    }
}
