//! Optional tmux socket-dir isolation for the commands commander spawns.

use std::path::Path;

/// A tmux command builder that can be pinned onto an isolated socket directory.
///
/// `TMUX_TMPDIR` selects the socket dir, but a tmux *client* prefers an
/// inherited live `$TMUX` server socket over it — so genuine isolation also
/// requires stripping `$TMUX`/`$TMUX_PANE`. Both command types commander spawns
/// tmux through (`tokio::process::Command` for [`TmuxExecutor`](super::TmuxExecutor),
/// `pty_process::Command` for [`HeadlessAttach`](super::HeadlessAttach)) implement
/// this so the env trio lives in exactly one place (CLAUDE.md: minimise
/// duplication).
///
/// Applied only when [`Config::tmux_tmpdir`](crate::config::Config::tmux_tmpdir)
/// is set — for hermetic tests and the e2e harness. A normal TUI/CLI run passes
/// `None`, and [`with_tmux_tmpdir`](Self::with_tmux_tmpdir) then touches the
/// environment not at all, keeping behaviour byte-identical to before the knob
/// existed.
pub(crate) trait TmuxTmpdir: Sized {
    /// Set an environment variable on the command.
    fn set_env(self, key: &str, val: &Path) -> Self;

    /// Remove an environment variable that would otherwise be inherited.
    fn remove_env(self, key: &str) -> Self;

    /// When `dir` is `Some`, pin this command onto that socket dir (set
    /// `TMUX_TMPDIR`, strip `$TMUX`/`$TMUX_PANE`); when `None`, leave the
    /// environment untouched.
    fn with_tmux_tmpdir(self, dir: Option<&Path>) -> Self {
        match dir {
            Some(dir) => self
                .set_env("TMUX_TMPDIR", dir)
                .remove_env("TMUX")
                .remove_env("TMUX_PANE"),
            None => self,
        }
    }
}

impl TmuxTmpdir for tokio::process::Command {
    fn set_env(mut self, key: &str, val: &Path) -> Self {
        self.env(key, val);
        self
    }

    fn remove_env(mut self, key: &str) -> Self {
        self.env_remove(key);
        self
    }
}

impl TmuxTmpdir for pty_process::Command {
    fn set_env(self, key: &str, val: &Path) -> Self {
        self.env(key, val)
    }

    fn remove_env(self, key: &str) -> Self {
        self.env_remove(key)
    }
}

impl TmuxTmpdir for std::process::Command {
    fn set_env(mut self, key: &str, val: &Path) -> Self {
        self.env(key, val);
        self
    }

    fn remove_env(mut self, key: &str) -> Self {
        self.env_remove(key);
        self
    }
}
