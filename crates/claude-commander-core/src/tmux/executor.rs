//! Async tmux command executor with semaphore-controlled concurrency
//!
//! Provides non-blocking tmux command execution with:
//! - Semaphore to limit concurrent commands (default: 16)
//! - Timeout handling
//! - Structured output parsing

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::process::Command;
use tokio::sync::Semaphore;
use tokio::time::timeout;
use tracing::{debug, instrument, warn};

use crate::error::{Result, TmuxError};
use crate::tmux::isolation::TmuxTmpdir;

/// Default maximum concurrent tmux commands
pub const DEFAULT_MAX_CONCURRENT: usize = 16;

/// Default command timeout
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// Tries `create_session` makes when it catches the tmux server mid-shutdown,
/// and the pause between them. A server exits within milliseconds of losing
/// its last session, so 10 × 50ms is ample.
const CREATE_SESSION_ATTEMPTS: u32 = 10;
const CREATE_SESSION_RETRY_DELAY: Duration = Duration::from_millis(50);

/// Async tmux command executor
///
/// Uses a semaphore to limit concurrent tmux commands, preventing
/// resource exhaustion when managing many sessions.
#[derive(Clone)]
pub struct TmuxExecutor {
    /// Semaphore for concurrency control
    semaphore: Arc<Semaphore>,
    /// Command timeout
    timeout: Duration,
    /// When set, every spawned tmux command is pinned onto this socket dir
    /// (`TMUX_TMPDIR=<dir>`, `$TMUX`/`$TMUX_PANE` stripped) so it hits a
    /// throwaway per-test server rather than the real one. `None` in normal use
    /// leaves the environment untouched. See [`Config::tmux_tmpdir`](crate::config::Config::tmux_tmpdir).
    tmux_tmpdir: Option<PathBuf>,
}

impl TmuxExecutor {
    /// Create a new executor with default settings
    pub fn new() -> Self {
        Self::with_max_concurrent(DEFAULT_MAX_CONCURRENT)
    }

    /// Create an executor with custom concurrency limit
    pub fn with_max_concurrent(max_concurrent: usize) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(max_concurrent)),
            timeout: DEFAULT_TIMEOUT,
            tmux_tmpdir: None,
        }
    }

    /// Set the command timeout
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Isolate every spawned tmux command onto `dir` (for hermetic tests/e2e).
    /// `None` (the default) leaves the environment untouched.
    pub fn with_tmux_tmpdir(mut self, dir: Option<PathBuf>) -> Self {
        self.tmux_tmpdir = dir;
        self
    }

    /// Build a `tmux` command, applying socket-dir isolation when configured.
    /// The single construction seam every spawn goes through, so the isolation
    /// env trio can never be applied inconsistently (CLAUDE.md: minimise
    /// duplication).
    fn command(&self) -> Command {
        Command::new("tmux").with_tmux_tmpdir(self.tmux_tmpdir.as_deref())
    }

    /// Check if tmux is installed and accessible
    pub async fn check_installed(&self) -> Result<()> {
        let output = self
            .command()
            .arg("-V")
            .output()
            .await
            .map_err(|_| TmuxError::NotInstalled)?;

        if output.status.success() {
            let version = String::from_utf8_lossy(&output.stdout);
            debug!("tmux version: {}", version.trim());
            Ok(())
        } else {
            Err(TmuxError::NotInstalled.into())
        }
    }

    /// Execute a tmux command and return its output
    #[instrument(skip(self), fields(args = ?args))]
    pub async fn execute(&self, args: &[&str]) -> Result<String> {
        // Acquire semaphore permit
        let _permit = self
            .semaphore
            .acquire()
            .await
            .map_err(|_| TmuxError::SemaphoreError)?;

        // Build command
        let mut cmd = self.command();
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // Execute with timeout
        let result = timeout(self.timeout, cmd.output()).await;

        match result {
            Ok(Ok(output)) => {
                if output.status.success() {
                    Ok(String::from_utf8_lossy(&output.stdout).to_string())
                } else {
                    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                    Err(TmuxError::CommandFailed {
                        command: format!("tmux {}", args.join(" ")),
                        stderr,
                    }
                    .into())
                }
            }
            Ok(Err(e)) => {
                warn!("tmux command failed to execute: {}", e);
                Err(TmuxError::ExecFailed {
                    command: format!("tmux {}", args.join(" ")),
                    reason: e.to_string(),
                }
                .into())
            }
            Err(_) => Err(TmuxError::Timeout(self.timeout).into()),
        }
    }

    /// Check if a tmux session exists
    pub async fn session_exists(&self, session_name: &str) -> Result<bool> {
        let result = self.execute(&["has-session", "-t", session_name]).await;
        match result {
            Ok(_) => Ok(true),
            // `has-session` exits non-zero both when the session is genuinely
            // gone AND when tmux itself hits a transient failure (e.g. the
            // server crashes, or a sibling command exhausted file descriptors
            // and left the server in a bad state). Only the former means the
            // session doesn't exist. Treating the latter as "absent" makes the
            // reconciler mark live sessions Stopped — see `stderr_means_session_absent`.
            Err(crate::error::Error::Tmux(TmuxError::CommandFailed { stderr, .. }))
                if stderr_means_session_absent(&stderr) =>
            {
                Ok(false)
            }
            // Launch failures (`ExecFailed`), timeouts, and unrecognised
            // command failures tell us nothing about the session's existence,
            // so we propagate them rather than guessing it's gone.
            Err(e) => Err(e),
        }
    }

    /// Create a new tmux session
    ///
    /// Sets `remain-on-exit on` so that if the command exits or crashes,
    /// the pane stays open showing the exit status rather than disappearing.
    pub async fn create_session(
        &self,
        session_name: &str,
        working_dir: &std::path::Path,
        command: Option<&str>,
    ) -> Result<()> {
        let working_dir_str = working_dir.to_str().unwrap_or(".");

        // tmux unconditionally sets `TERM_PROGRAM=tmux` for processes inside a
        // pane (and `-e` cannot override it), which causes Claude Code to
        // report `terminal.type=tmux` in its OpenTelemetry metrics. Prefix the
        // launched command with explicit env assignments so child processes
        // (the shell, then `claude` itself) see Claude Commander instead.
        let version = env!("CARGO_PKG_VERSION");
        let wrapped_cmd = command.map(|cmd| {
            format!("TERM_PROGRAM=claude-commander TERM_PROGRAM_VERSION={version} {cmd}")
        });

        // Enable remain-on-exit globally BEFORE the new session's pane is
        // born, in the SAME tmux invocation as new-session. If we set the
        // option in a separate tmux call, two failure modes appear in
        // environments without a pre-existing tmux server (e.g. CI):
        //   1. `set-option -g` doesn't auto-start the server, so it fails
        //      with "error connecting to /tmp/tmux-1001/default".
        //   2. If we instead set remain-on-exit per-session AFTER
        //      new-session, a fast-exiting command (e.g. non-interactive
        //      `bash` with no controlling tty) can close the only pane,
        //      end the session, and shut the server down before the
        //      follow-up set-option call runs.
        // Chaining `start-server`, `set-option -g`, and `new-session` into
        // a single tmux invocation sidesteps both: the server is alive
        // when set-option runs, and the new pane inherits the option from
        // the moment it's created.
        let mut args: Vec<&str> = vec![
            "start-server",
            ";",
            "set-option",
            "-g",
            "remain-on-exit",
            "on",
            ";",
            "new-session",
            "-d",
            "-s",
            session_name,
            "-c",
            working_dir_str,
            "-x",
            "200",
            "-y",
            "50",
        ];
        if let Some(cmd) = wrapped_cmd.as_deref() {
            args.push(cmd);
        }

        // `kill-session` on a server's last session returns while that server
        // is still shutting down, so a recreate straight after it (every
        // restart path) can connect to the dying server and fail with "server
        // exited unexpectedly". Once it has gone, `start-server` starts a fresh
        // one, so retry briefly. Pinned by the integration test
        // `recreating_the_only_session_survives_server_shutdown`.
        let mut attempt = 1;
        loop {
            match self.execute(&args).await {
                Err(crate::error::Error::Tmux(TmuxError::CommandFailed { stderr, .. }))
                    if stderr_means_server_exiting(&stderr)
                        && attempt < CREATE_SESSION_ATTEMPTS =>
                {
                    debug!(
                        attempt,
                        "tmux server was exiting during new-session; retrying"
                    );
                    tokio::time::sleep(CREATE_SESSION_RETRY_DELAY).await;
                    attempt += 1;
                }
                result => {
                    result?;
                    break;
                }
            }
        }

        // Enable mouse support so scroll wheel enters copy mode for scrollback
        self.execute(&["set-option", "-t", session_name, "mouse", "on"])
            .await?;

        // Increase scrollback buffer for long Claude sessions
        self.execute(&["set-option", "-t", session_name, "history-limit", "50000"])
            .await?;

        Ok(())
    }

    /// Kill a tmux session
    pub async fn kill_session(&self, session_name: &str) -> Result<()> {
        self.execute(&["kill-session", "-t", session_name]).await?;
        Ok(())
    }

    /// List all tmux sessions
    pub async fn list_sessions(&self) -> Result<Vec<String>> {
        let output = self
            .execute(&["list-sessions", "-F", "#{session_name}"])
            .await?;

        Ok(output.lines().map(String::from).collect())
    }

    /// Check if a pane is dead (program has exited)
    pub async fn is_pane_dead(&self, session_name: &str) -> Result<bool> {
        let output = self
            .execute(&["list-panes", "-t", session_name, "-F", "#{pane_dead}"])
            .await?;

        // Returns "1" if pane is dead, "0" if alive
        Ok(output.trim() == "1")
    }

    /// Whether any client is currently attached to the session. Used by the
    /// hibernation policy to avoid stopping a session the user is viewing.
    /// `#{session_attached}` is the count of attached clients, so any non-zero
    /// value means attached. Callers should treat an `Err` conservatively (as
    /// attached) so a detection failure never triggers hibernation.
    pub async fn is_session_attached(&self, session_name: &str) -> Result<bool> {
        let output = self
            .execute(&[
                "display-message",
                "-p",
                "-t",
                session_name,
                "#{session_attached}",
            ])
            .await?;
        Ok(parse_session_attached(&output))
    }

    /// Send keys to a tmux session
    pub async fn send_keys(&self, session_name: &str, keys: &str) -> Result<()> {
        self.execute(&["send-keys", "-t", session_name, keys])
            .await?;
        Ok(())
    }

    /// Send `text` to a session's pane as **literal** input (`send-keys -l`), so
    /// no character is interpreted as a tmux key name — the text lands in the
    /// pane's input verbatim. `--` guards against any leading `-` in `text`
    /// being parsed as an option. Used to inject a pasted-image file path into
    /// the Claude prompt; deliberately sends no Enter.
    pub async fn send_keys_literal(&self, session_name: &str, text: &str) -> Result<()> {
        self.execute(&["send-keys", "-t", session_name, "-l", "--", text])
            .await?;
        Ok(())
    }

    /// Configure the status bar for a CC tmux session.
    ///
    /// Shows branch name, optional PR badge, and key hints. Style is set by
    /// `StatusBarInfo::status_style`. Errors are logged but not propagated.
    pub async fn configure_status_bar(&self, session_name: &str, info: &StatusBarInfo) {
        let left = info.format_left();
        let right = info.format_right();

        let options: &[(&str, &str)] = &[
            ("status-style", &info.status_style),
            ("status-left", &left),
            ("status-left-length", "200"),
            ("status-right", &right),
            // Suppress the default window list so only our left/right content shows
            ("window-status-format", ""),
            ("window-status-current-format", ""),
        ];

        for (key, value) in options {
            if let Err(e) = self
                .execute(&["set-option", "-t", session_name, key, value])
                .await
            {
                warn!(
                    "Failed to set tmux {} for session {}: {}",
                    key, session_name, e
                );
            }
        }
    }

    /// Capture the content of a tmux pane
    pub async fn capture_pane(
        &self,
        session_name: &str,
        start_line: Option<i32>,
        end_line: Option<i32>,
    ) -> Result<String> {
        // -p: output to stdout, -e: include escape sequences (ANSI colors)
        let mut args = vec!["capture-pane", "-t", session_name, "-p", "-e"];

        let start_str;
        let end_str;

        if let Some(start) = start_line {
            start_str = start.to_string();
            args.push("-S");
            args.push(&start_str);
        }

        if let Some(end) = end_line {
            end_str = end.to_string();
            args.push("-E");
            args.push(&end_str);
        }

        self.execute(&args).await
    }
}

/// Info used to render a per-session tmux status bar.
#[derive(Debug, Clone)]
pub struct StatusBarInfo {
    /// Branch name for this session
    pub branch: String,
    /// GitHub PR number, if one exists
    pub pr_number: Option<u32>,
    /// Whether the PR has been merged
    pub pr_merged: bool,
    /// tmux status-style value (e.g. "bg=colour236,fg=colour252")
    pub status_style: String,
    /// Whether this status bar is for a shell session (changes Ctrl-\ hint)
    pub is_shell: bool,
    /// Project name (shown as a prefix)
    pub project_name: String,
    /// Label of the project's workspace, shown before the project name.
    /// `None` hides it — the host has only Main, so there is nothing to tell
    /// apart (workspace UI stays hidden until a second workspace exists).
    pub workspace: Option<String>,
}

impl StatusBarInfo {
    const SEP: &'static str = " \u{2502} ";

    /// Format the left side of the status bar.
    ///
    /// Uses bold labels and box-drawing separators (`│`) for a polished look.
    /// `#` is escaped to `##` for tmux format safety.
    pub fn format_left(&self) -> String {
        let safe_branch = self.branch.replace('#', "##");
        let pr = match self.pr_number {
            Some(n) if self.pr_merged => format!("{}PR ##{} merged", Self::SEP, n),
            Some(n) => format!("{}PR ##{}", Self::SEP, n),
            None => String::new(),
        };
        let toggle_hint = if self.is_shell { "agent" } else { "shell" };
        let workspace = self
            .workspace
            .as_deref()
            .map(|w| format!(" {}{}", w.replace('#', "##"), Self::SEP.trim_end()))
            .unwrap_or_default();
        format!(
            "{workspace} #[bold]{}#[nobold]{}{}{}{sep}#[bold]Ctrl-q#[nobold]: detach{sep}#[bold]Ctrl-\\#[nobold]: {}{sep}#[bold]Ctrl-Space#[nobold]: switch ",
            self.project_name,
            Self::SEP,
            safe_branch,
            pr,
            toggle_hint,
            sep = Self::SEP,
        )
    }

    /// Format the right side of the status bar (currently empty).
    pub fn format_right(&self) -> String {
        String::new()
    }
}

impl Default for TmuxExecutor {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether a non-zero `tmux has-session` stderr means the session genuinely
/// does not exist (as opposed to a transient tmux failure).
///
/// tmux prints `can't find session: NAME` when the target session is absent,
/// and `no server running on PATH` when there's no server at all (so, no
/// sessions). When the server is running but has *no sessions left* (e.g. the
/// only session was just killed), `has-session -t NAME` can't anchor the target
/// and fails with `no current target` / `no current session` instead — which
/// still means the named session doesn't exist. Anything else — `server exited
/// unexpectedly`, `lost server`, resource errors — is a failure we must NOT
/// mistake for absence, or the state reconciler will wrongly mark a live
/// session as Stopped.
fn stderr_means_session_absent(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    if s.contains("can't find session")
        || s.contains("no server running")
        || s.contains("no current target")
        || s.contains("no current session")
    {
        return true;
    }
    // No tmux server has started at all: the socket file is missing, so
    // `has-session` fails with `error connecting to <socket> (No such file or
    // directory)`. No server means no session can exist — distinct from a
    // transient connection failure (e.g. "connection refused"), which we still
    // propagate rather than mistake for absence.
    s.contains("error connecting to") && s.contains("no such file")
}

/// Whether a failed tmux command's stderr says the server it connected to
/// went away mid-command — what a client sees when it reaches a server that is
/// shutting down after losing its last session.
fn stderr_means_server_exiting(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    s.contains("server exited unexpectedly") || s.contains("lost server")
}

/// Parse tmux's `#{session_attached}` client count into "is a client attached".
///
/// Conservative on unexpected output: an empty or unparsable value (seen on
/// some tmux versions / client-handoff edge states) counts as **attached**, so
/// a glitch never lets the hibernation loop kill a session out from under a
/// user. The `execute` error path is likewise treated as attached by callers
/// (`unwrap_or(true)`); this keeps the Ok-but-garbage path equally safe.
fn parse_session_attached(output: &str) -> bool {
    output.trim().parse::<u32>().map(|n| n > 0).unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_session_attached_counts() {
        assert!(parse_session_attached("1"));
        assert!(parse_session_attached("2"));
        assert!(!parse_session_attached("0"));
        assert!(!parse_session_attached(" 0\n"));
        assert!(parse_session_attached(" 1 "));
    }

    #[test]
    fn parse_session_attached_unparsable_is_conservatively_attached() {
        // Empty / non-numeric output must NOT read as unattached, or a
        // detection glitch could hibernate a session under an attached user.
        assert!(parse_session_attached(""));
        assert!(parse_session_attached("   "));
        assert!(parse_session_attached("garbage"));
    }

    #[tokio::test]
    async fn test_executor_creation() {
        let executor = TmuxExecutor::new();
        assert_eq!(executor.timeout, DEFAULT_TIMEOUT);
    }

    #[tokio::test]
    async fn test_executor_with_custom_settings() {
        let executor = TmuxExecutor::with_max_concurrent(8).with_timeout(Duration::from_secs(10));

        assert_eq!(executor.timeout, Duration::from_secs(10));
    }

    #[test]
    fn command_isolates_tmux_env_when_socket_dir_set() {
        use std::ffi::OsStr;

        let dir = PathBuf::from("/socket/dir");
        let executor = TmuxExecutor::new().with_tmux_tmpdir(Some(dir.clone()));
        let cmd = executor.command();
        let envs: Vec<(&OsStr, Option<&OsStr>)> = cmd.as_std().get_envs().collect();

        // TMUX_TMPDIR points the client at the isolated socket dir.
        assert!(
            envs.contains(&(OsStr::new("TMUX_TMPDIR"), Some(dir.as_os_str()))),
            "TMUX_TMPDIR must be set to the socket dir, got {envs:?}"
        );
        // $TMUX / $TMUX_PANE must be stripped (recorded as a removal → `None`),
        // or an inherited live server would win over TMUX_TMPDIR.
        assert!(
            envs.contains(&(OsStr::new("TMUX"), None)),
            "TMUX must be removed, got {envs:?}"
        );
        assert!(
            envs.contains(&(OsStr::new("TMUX_PANE"), None)),
            "TMUX_PANE must be removed, got {envs:?}"
        );
    }

    #[test]
    fn command_leaves_env_untouched_without_socket_dir() {
        let executor = TmuxExecutor::new();
        let cmd = executor.command();
        assert_eq!(
            cmd.as_std().get_envs().count(),
            0,
            "normal (TUI/CLI) mode must not tamper with the tmux environment"
        );
    }

    #[test]
    fn stderr_no_running_server_reads_as_session_absent() {
        // tmux's explicit phrasings.
        assert!(stderr_means_session_absent(
            "can't find session: cc-commander"
        ));
        assert!(stderr_means_session_absent(
            "no server running on /tmp/tmux-1001/default"
        ));
        // No server socket at all (fresh CI runner with no tmux server): this
        // must read as absent so `session_exists` returns Ok(false) and
        // `ensure_session` can create the session instead of erroring.
        assert!(stderr_means_session_absent(
            "error connecting to /tmp/tmux-1001/default (No such file or directory)"
        ));
        // Server up but no sessions left (the only one was just killed): tmux
        // can't resolve the target and says "no current target" rather than
        // "can't find session". Still means the session is absent — must read
        // as such so `ensure_session` recreates instead of erroring.
        assert!(stderr_means_session_absent("no current target"));
        assert!(stderr_means_session_absent("no current session"));
        // A transient connection failure tells us nothing about existence —
        // never mistake it for absence.
        assert!(!stderr_means_session_absent(
            "error connecting to /tmp/tmux-1001/default (Connection refused)"
        ));
        assert!(!stderr_means_session_absent("some unrelated tmux error"));
    }

    // Integration tests would require tmux to be installed
    // They should be marked with #[ignore] and run separately

    fn test_info(branch: &str, pr_number: Option<u32>, pr_merged: bool) -> StatusBarInfo {
        StatusBarInfo {
            branch: branch.to_string(),
            pr_number,
            pr_merged,
            status_style: "bg=colour236,fg=colour252".to_string(),
            is_shell: false,
            project_name: "my-project".to_string(),
            workspace: None,
        }
    }

    #[test]
    fn test_status_bar_format_left_names_the_workspace() {
        let mut info = test_info("feature", None, false);
        info.workspace = Some("Work".to_string());
        assert_eq!(
            info.format_left(),
            " Work \u{2502} #[bold]my-project#[nobold] \u{2502} feature \u{2502} #[bold]Ctrl-q#[nobold]: detach \u{2502} #[bold]Ctrl-\\#[nobold]: shell \u{2502} #[bold]Ctrl-Space#[nobold]: switch "
        );
    }

    #[test]
    fn test_status_bar_format_left_escapes_hash_in_workspace() {
        let mut info = test_info("feature", None, false);
        info.workspace = Some("#1".to_string());
        assert!(info.format_left().starts_with(" ##1 \u{2502} "));
    }

    #[test]
    fn test_status_bar_format_left_basic() {
        let info = test_info("feature-auth", None, false);
        assert_eq!(
            info.format_left(),
            " #[bold]my-project#[nobold] \u{2502} feature-auth \u{2502} #[bold]Ctrl-q#[nobold]: detach \u{2502} #[bold]Ctrl-\\#[nobold]: shell \u{2502} #[bold]Ctrl-Space#[nobold]: switch "
        );
    }

    #[test]
    fn test_status_bar_format_left_escapes_hash() {
        let info = test_info("fix-#123-bug", None, false);
        assert!(info.format_left().contains("fix-##123-bug"));
    }

    #[test]
    fn test_status_bar_format_left_open_pr() {
        let info = test_info("feature", Some(42), false);
        assert_eq!(
            info.format_left(),
            " #[bold]my-project#[nobold] \u{2502} feature \u{2502} PR ##42 \u{2502} #[bold]Ctrl-q#[nobold]: detach \u{2502} #[bold]Ctrl-\\#[nobold]: shell \u{2502} #[bold]Ctrl-Space#[nobold]: switch "
        );
    }

    #[test]
    fn test_status_bar_format_left_merged_pr() {
        let info = test_info("feature", Some(42), true);
        assert_eq!(
            info.format_left(),
            " #[bold]my-project#[nobold] \u{2502} feature \u{2502} PR ##42 merged \u{2502} #[bold]Ctrl-q#[nobold]: detach \u{2502} #[bold]Ctrl-\\#[nobold]: shell \u{2502} #[bold]Ctrl-Space#[nobold]: switch "
        );
    }

    #[test]
    fn test_status_bar_format_left_shell_session() {
        let mut info = test_info("feature-auth", None, false);
        info.is_shell = true;
        assert_eq!(
            info.format_left(),
            " #[bold]my-project#[nobold] \u{2502} feature-auth \u{2502} #[bold]Ctrl-q#[nobold]: detach \u{2502} #[bold]Ctrl-\\#[nobold]: agent \u{2502} #[bold]Ctrl-Space#[nobold]: switch "
        );
    }

    #[test]
    fn test_status_bar_format_right_empty() {
        let info = test_info("main", None, false);
        assert_eq!(info.format_right(), "");
    }

    #[test]
    fn stderr_absent_recognises_missing_session() {
        // The two messages tmux emits when the session is genuinely gone.
        assert!(stderr_means_session_absent("can't find session: cc-abc123"));
        assert!(stderr_means_session_absent(
            "no server running on /tmp/tmux-501/default"
        ));
        // Case-insensitive, since wording can vary slightly across versions.
        assert!(stderr_means_session_absent("Can't find session cc-abc123"));
    }

    #[test]
    fn server_exiting_is_recognised_and_nothing_else() {
        assert!(stderr_means_server_exiting("server exited unexpectedly\n"));
        assert!(stderr_means_server_exiting("lost server"));
        assert!(!stderr_means_server_exiting("duplicate session: cc-abc"));
        assert!(!stderr_means_server_exiting(
            "no server running on /tmp/tmux-501/default"
        ));
        assert!(!stderr_means_server_exiting(""));
    }

    #[test]
    fn stderr_absent_rejects_transient_failures() {
        // These are the failures that previously got misread as "session
        // gone", causing the reconciler to mark live sessions Stopped.
        assert!(!stderr_means_session_absent("server exited unexpectedly"));
        assert!(!stderr_means_session_absent("lost server"));
        assert!(!stderr_means_session_absent(
            "Too many open files (os error 24)"
        ));
        assert!(!stderr_means_session_absent(""));
    }
}
