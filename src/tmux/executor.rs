//! Async tmux command executor with semaphore-controlled concurrency
//!
//! Provides non-blocking tmux command execution with:
//! - Semaphore to limit concurrent commands (default: 16)
//! - Timeout handling
//! - Structured output parsing

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::process::Command;
use tokio::sync::Semaphore;
use tokio::time::timeout;
use tracing::{debug, instrument, warn};

use crate::error::{Result, TmuxError};

/// Default maximum concurrent tmux commands
pub const DEFAULT_MAX_CONCURRENT: usize = 16;

/// Default command timeout
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

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
        }
    }

    /// Set the command timeout
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Check if tmux is installed and accessible
    pub async fn check_installed(&self) -> Result<()> {
        let output = Command::new("tmux")
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
        let mut cmd = Command::new("tmux");
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
                warn!("tmux command failed: {}", e);
                Err(TmuxError::CommandFailed {
                    command: format!("tmux {}", args.join(" ")),
                    stderr: e.to_string(),
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
            Err(crate::error::Error::Tmux(TmuxError::CommandFailed { .. })) => {
                // "has-session" returns non-zero if session doesn't exist
                Ok(false)
            }
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

        // Create session with remain-on-exit option so pane stays open if command exits
        let args: Vec<&str> = if let Some(cmd) = wrapped_cmd.as_deref() {
            vec![
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
                cmd,
            ]
        } else {
            vec![
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
            ]
        };

        self.execute(&args).await?;

        // Set remain-on-exit so pane stays open if the program exits/crashes
        self.execute(&["set-option", "-t", session_name, "remain-on-exit", "on"])
            .await?;

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

    /// Send keys to a tmux session
    pub async fn send_keys(&self, session_name: &str, keys: &str) -> Result<()> {
        self.execute(&["send-keys", "-t", session_name, keys])
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
        format!(
            " #[bold]{}#[nobold]{}{}{}{sep}#[bold]Ctrl-q#[nobold]: detach{sep}#[bold]Ctrl-\\#[nobold]: {}{sep}#[bold]Ctrl-o#[nobold]: switch ",
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

#[cfg(test)]
mod tests {
    use super::*;

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
        }
    }

    #[test]
    fn test_status_bar_format_left_basic() {
        let info = test_info("feature-auth", None, false);
        assert_eq!(
            info.format_left(),
            " #[bold]my-project#[nobold] \u{2502} feature-auth \u{2502} #[bold]Ctrl-q#[nobold]: detach \u{2502} #[bold]Ctrl-\\#[nobold]: shell \u{2502} #[bold]Ctrl-o#[nobold]: switch "
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
            " #[bold]my-project#[nobold] \u{2502} feature \u{2502} PR ##42 \u{2502} #[bold]Ctrl-q#[nobold]: detach \u{2502} #[bold]Ctrl-\\#[nobold]: shell \u{2502} #[bold]Ctrl-o#[nobold]: switch "
        );
    }

    #[test]
    fn test_status_bar_format_left_merged_pr() {
        let info = test_info("feature", Some(42), true);
        assert_eq!(
            info.format_left(),
            " #[bold]my-project#[nobold] \u{2502} feature \u{2502} PR ##42 merged \u{2502} #[bold]Ctrl-q#[nobold]: detach \u{2502} #[bold]Ctrl-\\#[nobold]: shell \u{2502} #[bold]Ctrl-o#[nobold]: switch "
        );
    }

    #[test]
    fn test_status_bar_format_left_shell_session() {
        let mut info = test_info("feature-auth", None, false);
        info.is_shell = true;
        assert_eq!(
            info.format_left(),
            " #[bold]my-project#[nobold] \u{2502} feature-auth \u{2502} #[bold]Ctrl-q#[nobold]: detach \u{2502} #[bold]Ctrl-\\#[nobold]: agent \u{2502} #[bold]Ctrl-o#[nobold]: switch "
        );
    }

    #[test]
    fn test_status_bar_format_right_empty() {
        let info = test_info("main", None, false);
        assert_eq!(info.format_right(), "");
    }
}
