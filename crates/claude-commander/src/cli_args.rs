//! Command-line argument definitions for this binary.
//!
//! These live in the binary crate because a clap derive takes its program name
//! and version from the package it compiles in: defined here, `--version`
//! reports `claude-commander <version>` with nothing hardcoded and nothing for
//! a rename or a version bump to get wrong. (In the library they resolved to
//! `claude-commander-core`, which also leaked into the generated commander CLI
//! reference as unrunnable `claude-commander-core <sub>` invocations.)
//!
//! Rendering the CLI reference for the commander and conversation `CLAUDE.md`
//! files lives here too, for the same reason: walking a clap tree means
//! depending on clap, and only a binary has a CLI to describe. Core is handed
//! the finished markdown ([`cli_reference`]) and stays clap-free, so embedders
//! of the library don't inherit a CLI parser they have no use for.

use clap::{Args, CommandFactory, Parser, Subcommand};

#[derive(Parser)]
#[command(version)]
#[command(about = "A high-performance terminal UI for managing Claude coding sessions")]
#[command(long_about = None)]
pub struct Cli {
    /// Enable debug logging
    #[arg(short, long)]
    pub debug: bool,

    /// Path to config file
    #[arg(short, long)]
    pub config: Option<std::path::PathBuf>,

    /// Run the HTTP server in this process for as long as the TUI is open, so
    /// clients on this machine (or the LAN, with `[server] bind`) can reach it
    /// without starting `claude-commander-server` separately. Overrides
    /// `[server] auto_start` for one run. TUI only.
    #[arg(long, conflicts_with = "no_serve")]
    pub serve: bool,

    /// Do not run the HTTP server, even if `[server] auto_start` is on.
    /// TUI only.
    #[arg(long)]
    pub no_serve: bool,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

// A clap subcommand enum is parsed once at startup and never stored in bulk, so
// the size gap between the arg-heavy `New` variant and the unit variants is
// immaterial — boxing a variant would only fight clap's derive for no runtime
// benefit.
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand)]
pub enum Commands {
    /// Start the interactive TUI (default)
    Tui,

    /// List all sessions
    List {
        /// Show all sessions including stopped ones
        #[arg(short, long)]
        all: bool,

        /// Output as JSON
        #[arg(long)]
        json: bool,

        /// Only list sessions in this workspace (by name, case-insensitive;
        /// `main` or Main's label for the built-in workspace).
        #[arg(short = 'w', long)]
        workspace: Option<String>,
    },

    /// Show detailed status of a session
    Status {
        /// Session name or ID prefix
        session: String,

        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// Delete a session: kill its tmux session and remove the git worktree.
    #[command(visible_alias = "rm")]
    Delete {
        /// Session name or ID prefix
        session: String,

        /// Delete without confirmation
        #[arg(short, long)]
        force: bool,
    },

    /// Toggle (or explicitly set) keep-alive on a session, exempting it from
    /// automatic idle-hibernation. With no flag, toggles the current value.
    KeepAlive {
        /// Session name or ID prefix
        session: String,

        /// Turn keep-alive on (never auto-hibernate this session)
        #[arg(long, conflicts_with = "off")]
        on: bool,

        /// Turn keep-alive off (allow auto-hibernation when idle)
        #[arg(long, conflicts_with = "on")]
        off: bool,
    },

    /// Dump recent terminal output from a session
    Log {
        /// Session name or ID prefix
        session: String,

        /// Number of scrollback lines to capture (default: 100, max: 10000)
        #[arg(short, long, default_value_t = 100)]
        lines: usize,
    },

    /// Create a new session
    New {
        /// Session name
        name: String,

        /// Program to run (default: claude)
        #[arg(short, long)]
        program: Option<String>,

        /// Project path (default: current directory)
        #[arg(short = 'd', long)]
        path: Option<std::path::PathBuf>,

        /// Existing project to create the session in, by name (case-insensitive).
        /// Resolves to the project's repo path via the backend, so a remote
        /// project needs no `--path`. If the name matches more than one project
        /// (names aren't unique), use `--path` to disambiguate. Mutually
        /// exclusive with `--path` (which seeds a new project).
        #[arg(long, conflicts_with = "path")]
        project: Option<String>,

        /// Initial prompt to send to the Claude agent
        #[arg(short = 'i', long)]
        initial_prompt: Option<String>,

        /// Claude effort level
        #[arg(short, long)]
        effort: Option<String>,

        /// Claude permission mode
        #[arg(short, long)]
        mode: Option<String>,

        /// Model to run the agent with (Claude, Codex, OpenCode, omp or Muse Code)
        #[arg(long)]
        model: Option<String>,

        /// Branch to fork from (default: origin/main)
        #[arg(short = 'b', long)]
        base_branch: Option<String>,

        /// Place session in a specific section
        #[arg(short = 's', long)]
        section: Option<String>,

        /// Create the session on a configured remote server (by name from
        /// `[[remote_servers]]`) instead of locally. Pair with `--project` to
        /// pick an existing server-side project by name, or `--path` to seed a
        /// new one (the path is resolved on the server, not this machine).
        #[arg(long)]
        remote: Option<String>,

        /// Workspace for the project when `--path` registers a new one (an
        /// existing project keeps its workspace). An unknown name creates the
        /// workspace; `main` is the built-in one.
        #[arg(short = 'w', long, requires = "path")]
        workspace: Option<String>,
    },

    /// Attach to an existing session
    Attach {
        /// Session name or ID
        session: String,

        /// Attach to a session on a configured remote server (by name from
        /// `[[remote_servers]]`) instead of a local one.
        #[arg(long)]
        remote: Option<String>,
    },

    /// Open the persistent commander session — a project-less Claude session
    /// that coordinates other sessions via this CLI. Creates it on first use.
    Commander,

    /// Inspect a session's review diff (the TUI's diff view) and mark files in
    /// it reviewed. Marks are pinned to each file's current diff and clear
    /// themselves when that file changes again. An open TUI review view picks
    /// up changes made here within about a second.
    Review {
        #[command(subcommand)]
        command: ReviewCommand,
    },

    /// Show configuration
    Config {
        /// Initialize config file with defaults
        #[arg(long)]
        init: bool,
    },

    /// Toggle voice input in the running TUI. Intended for binding to a desktop
    /// global keyboard shortcut (e.g. a KDE Plasma custom shortcut) so STT can be
    /// triggered system-wide, not just when the terminal is focused. Talks to the
    /// already-running instance over a local socket; with no flag it toggles.
    ListenToggle {
        /// Start recording (instead of toggling)
        #[arg(long, conflicts_with = "stop")]
        start: bool,

        /// Stop recording and transcribe (instead of toggling)
        #[arg(long, conflicts_with = "start")]
        stop: bool,
    },
}

/// `review` subcommands. Every one takes the session by exact title or full ID
/// (like `delete` and `keep-alive`), and files by the display path `files`
/// prints (the new path; the old path for a deleted file).
#[derive(Subcommand)]
pub enum ReviewCommand {
    /// List the files in a session's review diff with their change counts,
    /// reviewed state and (in `--json`) content hash.
    Files {
        /// Session title or full ID
        session: String,

        /// Only list files not yet marked reviewed
        #[arg(long)]
        unreviewed: bool,

        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// Print a session's review diff in unified format, optionally only some
    /// files. The output is exactly what the review view shows and marks.
    Diff {
        /// Session title or full ID
        session: String,

        /// Only these files: display paths as `review files` prints them, each
        /// optionally `PATH@HASH` (refused if the file changed since)
        files: Vec<String>,

        /// Only files not yet marked reviewed
        #[arg(long)]
        unreviewed: bool,
    },

    /// Mark files reviewed. Idempotent: re-marking a reviewed file is a no-op.
    /// All-or-nothing: if any file is not in the diff, or changed since the
    /// hash given for it, nothing is marked.
    Mark(SetReviewedArgs),

    /// Clear files' reviewed marks. Idempotent and all-or-nothing, like `mark`.
    Unmark(SetReviewedArgs),
}

/// The arguments `review mark` and `review unmark` share.
#[derive(Args)]
pub struct SetReviewedArgs {
    /// Session title or full ID
    pub session: String,

    /// Files: display paths as `review files` prints them, each optionally
    /// `PATH@HASH` with the hash from `review files --json` to refuse the
    /// change if the file changed since you read it
    #[arg(required_unless_present = "all", conflicts_with = "all")]
    pub files: Vec<String>,

    /// Every file in the diff
    #[arg(long)]
    pub all: bool,

    /// Output as JSON
    #[arg(long)]
    pub json: bool,
}

/// The clap command tree for the CLI. Single source of truth for this binary's
/// argument parsing and for the CLI reference generated from it.
pub fn cli_command() -> clap::Command {
    Cli::command()
}

/// Markdown documenting this CLI, for the commander and conversation `CLAUDE.md`
/// files. Regenerated from the live command tree on each write, so the docs an
/// agent reads can never drift from the commands it can actually run.
pub fn cli_reference() -> String {
    generate_cli_reference(&cli_command())
}

/// Render a markdown CLI reference by walking a clap command tree and emitting
/// each visible subcommand's long help verbatim, labelled with the tree's
/// program name. Hidden subcommands (e.g. internal popup helpers) are skipped.
///
/// Split from [`cli_reference`] so the walk is testable against a synthetic tree
/// with a known shape.
fn generate_cli_reference(cmd: &clap::Command) -> String {
    let bin = cmd.get_name().to_string();
    let mut out = String::new();
    for sub in cmd.get_subcommands() {
        if sub.is_hide_set() {
            continue;
        }
        out.push_str(&format!("### `{bin} {}`\n\n", sub.get_name()));
        let help = sub.clone().render_long_help();
        out.push_str("```\n");
        out.push_str(help.to_string().trim_end());
        out.push_str("\n```\n\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_flag_reports_this_packages_name_and_version() {
        // The string a user actually sees. Both halves come from *this*
        // package's metadata, which only holds while the derive lives in the
        // binary crate — from the library it read `claude-commander-core`.
        let err = cli_command()
            .try_get_matches_from(["claude-commander", "--version"])
            .expect_err("--version short-circuits parsing");
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayVersion);
        assert_eq!(
            err.to_string().trim(),
            format!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"))
        );
        // Pin the literal too: `CARGO_PKG_NAME` alone would still match if this
        // package were renamed, and `claude-commander` is the installed
        // command name that docs, the Homebrew formula, and the AUR package use.
        assert_eq!(
            err.to_string().trim(),
            format!("claude-commander {}", crate::VERSION)
        );
    }

    #[test]
    fn cli_reference_documents_runnable_invocations() {
        // The commander session reads this reference as instructions, so a wrong
        // program name is a documented command that doesn't exist — the second,
        // quieter symptom of the library-side derive.
        let reference = cli_reference();
        assert!(
            reference.contains("claude-commander list"),
            "reference must document runnable invocations; got:\n{reference}"
        );
        assert!(
            !reference.contains("claude-commander-core"),
            "the library crate name must never reach the commander's CLI reference"
        );
    }

    /// A miniature clap command tree mirroring the shape of the real CLI:
    /// a couple of visible subcommands (one with args) plus a hidden one.
    fn sample_cli() -> clap::Command {
        clap::Command::new("sample-bin")
            .subcommand(clap::Command::new("list").about("List all sessions"))
            .subcommand(
                clap::Command::new("new")
                    .about("Create a new session")
                    .arg(clap::Arg::new("name").help("Session name")),
            )
            .subcommand(clap::Command::new("hidden-helper").hide(true))
    }

    #[test]
    fn cli_reference_includes_visible_subcommands() {
        let reference = generate_cli_reference(&sample_cli());
        assert!(reference.contains("sample-bin list"));
        assert!(reference.contains("List all sessions"));
        assert!(reference.contains("sample-bin new"));
        assert!(reference.contains("Create a new session"));
    }

    #[test]
    fn cli_reference_skips_hidden_subcommands() {
        let reference = generate_cli_reference(&sample_cli());
        assert!(
            !reference.contains("hidden-helper"),
            "hidden subcommands must not leak into the CLI reference"
        );
    }

    #[test]
    fn cli_command_exposes_known_subcommands() {
        let cmd = cli_command();
        let names: Vec<&str> = cmd.get_subcommands().map(|s| s.get_name()).collect();
        for expected in [
            "list",
            "status",
            "log",
            "new",
            "attach",
            "config",
            "commander",
            "listen-toggle",
            "keep-alive",
        ] {
            assert!(
                names.contains(&expected),
                "cli_command() missing subcommand `{expected}`; got {names:?}"
            );
        }
    }

    #[test]
    fn cli_command_parses_without_panicking() {
        // `Cli::command()` panics at runtime if the derive is malformed
        // (duplicate args, bad short flags, etc.). Exercising debug_assert
        // here turns that into a test failure rather than a surprise at launch.
        cli_command().debug_assert();
    }

    /// `review mark`/`unmark` need files or `--all`, never both — an agent that
    /// forgets the paths must get an error, not a silent no-op.
    #[test]
    fn review_mark_requires_files_or_all_but_not_both() {
        let parse =
            |args: &[&str]| Cli::try_parse_from(["claude-commander", "review"].iter().chain(args));
        assert!(parse(&["mark", "s"]).is_err());
        assert!(parse(&["unmark", "s", "a.rs", "--all"]).is_err());

        let Some(Commands::Review {
            command: ReviewCommand::Mark(SetReviewedArgs { files, all, .. }),
        }) = parse(&["mark", "s", "a.rs", "b.rs"]).unwrap().command
        else {
            panic!("expected review mark");
        };
        assert_eq!(
            (files, all),
            (vec!["a.rs".to_string(), "b.rs".to_string()], false)
        );
        assert!(parse(&["unmark", "s", "--all"]).is_ok());
    }
}
