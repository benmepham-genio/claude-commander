# Claude Commander

A high-performance terminal UI for managing Claude coding sessions, written in Rust.

## Features

- **Async-first architecture** - Non-blocking tmux and git operations
- **Hierarchical session model** - Projects contain worktree sessions
- **Git worktree isolation** - Each session has its own worktree and branch
- **Live preview** - Real-time pane content capture with caching
- **Info pane** - Session metadata, PR details, CI status, and AI-generated change summaries
- **Agent state detection** - Detect if agent is waiting for input, processing, or errored
- **Persistent state** - Sessions survive restarts

## Requirements

- **Rust/Cargo** - Required to build from source ([install via rustup](https://rustup.rs/))
- **tmux** - Required for session management
- **git** - For worktree operations

## Installation

### Homebrew (macOS and Linux)

```bash
brew tap sizeak/tap
brew install claude-commander
```

### Arch Linux (AUR)

```bash
yay -S claude-commander
```

### Cargo

Install directly from GitHub:

```bash
cargo install --git https://github.com/sizeak/claude-commander.git
```

Or clone and install locally:

```bash
cargo install --path .
```

Or build from source without installing:

```bash
cargo build --release
./target/release/claude-commander
```

## Releasing

Releases are cut with [`cargo-release`](https://github.com/crate-ci/cargo-release):

```bash
cargo install cargo-release            # one-time

cargo release patch                    # X.Y.Z -> X.Y.(Z+1), dry-run
cargo release minor                    # X.Y.Z -> X.(Y+1).0, dry-run
cargo release major                    # X.Y.Z -> (X+1).0.0, dry-run
cargo release 1.2.3                    # explicit version, dry-run

cargo release <patch|minor|major|X.Y.Z> --execute   # actually release
```

Every invocation is a dry-run by default; add `--execute` once the printed plan looks right. The command bumps the version in `Cargo.toml`, refreshes `Cargo.lock`, creates a GPG-signed commit (`Bump version to X.Y.Z`) and a GPG-signed tag (`vX.Y.Z`), and pushes both to `origin/main`.

The tag push triggers `.github/workflows/publish-tap.yml`, which creates the GitHub release with auto-generated notes and bumps the formula in [`sizeak/homebrew-tap`](https://github.com/sizeak/homebrew-tap) so `brew upgrade claude-commander` sees the new version within ~60 seconds. In parallel, `.github/workflows/publish-aur.yml` rewrites the `sha256sums` line in `packaging/aur/PKGBUILD` against the new GitHub source tarball and pushes the result to the [`claude-commander`](https://aur.archlinux.org/packages/claude-commander) AUR package so `yay -Syu claude-commander` picks it up.

The AUR job depends on:

- A repo secret `AUR_SSH_PRIVATE_KEY` containing the private half of an SSH key registered against the maintainer's [aur.archlinux.org](https://aur.archlinux.org) account.
- The package already existing on AUR — the first publish must be done by hand (`git clone ssh://aur@aur.archlinux.org/claude-commander.git`, copy `packaging/aur/PKGBUILD`, run `makepkg --printsrcinfo > .SRCINFO`, commit and push). Every subsequent tag is handled by the workflow.

`cargo-release` keeps `pkgver` in `packaging/aur/PKGBUILD` in sync with `Cargo.toml` via the `pre-release-replacements` block in `release.toml`; the workflow fills in `sha256sums` at publish time.

## Usage

### Interactive TUI (default)

```bash
claude-commander
```

### Commands

```bash
# List active sessions (add --all to include stopped, --json for machine-readable output)
claude-commander list

# Create a new session
claude-commander new "feature-auth" --path /path/to/repo

# Create a session with an initial prompt (starts working immediately)
claude-commander new "fix-auth" --initial-prompt "Fix the auth bypass in login.rs" --effort high

# Create a session in plan mode, forking from a specific branch
claude-commander new "feature-api" --base-branch develop --mode plan

# Attach to a session
claude-commander attach feature-auth

# Dump recent terminal output from a session (default 100 lines, max 10000)
claude-commander log feature-auth --lines 200

# Show configuration
claude-commander config

# Initialize config file
claude-commander config --init

# Use a custom config file
claude-commander --config /path/to/config.toml
```

### Session List

The left pane shows projects and their worktree sessions in a tree view. Projects are sorted alphabetically. Sessions within a project are sorted newest first (by creation time).

Each session row shows the title and, in `[brackets]`, the branch name — but only when the branch differs from what the title would sanitize to. A session titled "Feature Auth" with branch `feature-auth` (or `prefix/feature-auth` when `branch_prefix` is set) renders as just `Feature Auth`; the bracket reappears only when the branch carries new information, e.g. you renamed it to `feature-auth-v2` outside the app.

#### PR Stacks

When a session's PR targets another session's branch (rather than `main`), the two form a stack. Stacked children render one indent deeper and sit directly beneath the session they're stacked on, in bottom-to-top stack order. The stack base keeps its normal position in the root session list sorted by creation time.

Press `t` on any session in a stack to create a new session on top of that stack — regardless of which member you have selected, the new branch is forked from the topmost session. When you launch Claude in the new session it is told to use `gh pr create --base <parent-branch>` so the PR targets the right place automatically.

Stacks are detected from the PR's `baseRefName` returned by the `gh` CLI, so they stay accurate across GitHub's auto-retargeting when a stack member is merged.

Stack grouping is only active in the default project-grouped view. When [Session List Sections](#session-list-sections) is configured, sessions are ordered by their section instead and stacked children render at the normal indent — the `t` hotkey and `stack_parent_session_id` still work, but a base and its child may land in different sections depending on their PR state.

##### Cascade merge main through a stack

When `main` moves forward, use **Cascade merge main** from the command palette to propagate it up the stack in one step: the command merges `main` into the stack base, then the base into its child, and so on to the leaf. Running it from any session in a stack works — the cascade always starts from the base.

Before touching any worktree, the cascade fetches `origin`, verifies no live agent in the stack is `Working` or `WaitingForInput` (racing a `git merge` against Claude writing files is unrecoverable), and refuses if any worktree has uncommitted changes. Each session shows a `⟳` spinner while its step is running.

On the first conflict the cascade pauses: the affected session gets a persistent `⏸` glyph (survives a restart of the TUI), and the worktree is left in the usual `git merge` in-progress state so you can resolve it however you like — typically by attaching to the session and asking the live Claude. Once you've committed the resolved merge, **Cascade resume** from the palette picks up where it stopped and propagates the new commit on up the chain. **Cascade abandon** clears the pause without continuing, if you decide to back out.

##### Push stack

**Push stack** (palette) runs `git push -u origin <branch>` across every session in the stack, base first then each child up the chain — pushing the base before its children keeps GitHub's PR base refs consistent. Each session shows the spinner glyph while its own push is in flight.

Pre-flight is the same as cascade merge: no live agent may be `Working` or `WaitingForInput`, and worktrees must have no uncommitted changes. On the first `git push` failure (rejection, auth, non-fast-forward, etc.) the chain stops and the toast shows git's stderr — no "resume" command is needed since `git push` is idempotent, so fix the root cause and re-run **Push stack** to continue.

### Status Symbols

Each session displays a status indicator to the left of its name:

| Symbol | Meaning |
|--------|---------|
| `⠋` (animated spinner) | Session is being created or mid-cascade-merge |
| `●` (rainbow cycling) | Agent is actively working |
| `?` | Agent is waiting for user input |
| `⏸` | Cascade merge paused here — resolve conflicts and resume from the palette |
| `◆` | Session has unread output |
| `●` | Running (agent idle) |
| `○` | Stopped |

Indicators are shown in priority order — for example, a running session with unread output shows `◆` rather than `●`.

### PR Badges

When a session has a GitHub PR, a badge appears next to the session name. The badge color indicates the PR state:

| Color | Meaning |
|-------|---------|
| Blue | Open |
| Green | Open and awaiting review |
| Grey | Draft |
| Red | Closed |
| Dark purple | Merged |

The info pane shows additional detail when a PR is present, including a CI checks indicator:

| Symbol | Meaning |
|--------|---------|
| `✓` (green) | All checks passing |
| `✗` (red) | Checks failing |
| `◌` (orange) | Checks pending |
| `—` (grey) | No checks configured |

### Keyboard Shortcuts

All keybindings below are defaults and can be customised via the `[keybindings]` config table (see [Configuration](#configuration)).

| Key | Action |
|-----|--------|
| `j/k` or `↑/↓` or `Ctrl-n/p` | Navigate session list |
| `Space` | Quick-switch palette (sessions and commands) |
| `Shift+Space` | Command palette (commands only) |
| `>` (as first char in palette) | Filter palette to commands only |
| `Enter` | Attach to selected session |
| `n` | New worktree session |
| `t` | New session stacked on top of the selected session's stack |
| `N` | Add new project |
| `c` | Checkout existing branch into a new worktree session (fetches `origin` in the background, filterable list) |
| `d` | Delete session |
| `r` | Rename session (UI title only; underlying worktree, branch, and tmux session are unchanged) |
| `R` | Restart session (kill tmux + recreate; adds `--resume` when `resume_session = true`) |
| `D` | Remove project |
| `.` or `Ctrl-.` | Open in editor/IDE |
| `o` | Open PR in browser (when the session has a PR) |
| `S` | Scan directory for git repos and add them as projects |
| `s` | Open shell in worktree |
| `v` | Toggle project/section view (requires `[[sections]]` config) |
| palette only | Collapse/expand section (press on any item in the section, or `Enter` on the section header) |
| palette only | Move session to section (manual override; see [Session List Sections](#session-list-sections)) |
| `Tab` / `Shift-Tab` | Switch between panes (forward / reverse) |
| `<` / `>` | Shrink / grow left pane |
| `Ctrl-u/d` or `PageUp/Down` | Page up/down in preview |
| `1`–`99` | Jump to session by number |
| `g` | Generate AI summary (Info pane only) |
| `,` | Open settings |
| `?` | Show help |
| `q` or `Ctrl-c` | Quit |

### Attached Session Shortcuts

When attached to a session (via `Enter` or `claude-commander attach`):

| Key | Action |
|-----|--------|
| `Ctrl-q` | Detach and return to session list |
| `Ctrl-\` | Switch between Claude and shell pane |
| `Ctrl-Space` | Open the in-session switcher popup to jump to another claude-commander session without detaching |
| `Ctrl-.` | Open the session worktree in your editor (requires a terminal that emits CSI-u or xterm modifyOtherKeys sequences for Ctrl-.) |

## Configuration

Configuration file location depends on your platform:

- **macOS**: `~/Library/Application Support/com.claude-commander.claude-commander/config.toml`
- **Linux**: `~/.config/claude-commander/config.toml`

All settings can also be set via environment variables with the `CC_` prefix (e.g. `CC_EDITOR=code`).

```toml
# Default program to run in new sessions
default_program = "claude"

# Branch name prefix for new sessions (empty = no prefix)
branch_prefix = ""

# Fetch latest changes from origin before creating a new session
fetch_before_create = true

# Pass `--resume` when restarting/recreating a session so the agent picks up
# where it left off. Set to false to start the program fresh each time.
resume_session = true

# Maximum concurrent tmux commands
max_concurrent_tmux = 16

# Content capture cache TTL in milliseconds
capture_cache_ttl_ms = 50

# Diff cache TTL in milliseconds
diff_cache_ttl_ms = 500

# UI refresh rate in FPS
ui_refresh_fps = 30

# Custom worktrees directory (default: platform-specific, see Data Storage below)
# worktrees_dir = "/path/to/worktrees"

# Organize worktrees into per-repository subdirectories (default: false)
# per_repo_worktree_dirs = true

# Shell program for shell sessions (default: $SHELL or "bash")
# shell_program = "zsh"

# Editor/IDE command for opening sessions (e.g. "code", "zed", "nvim")
# Falls back to $VISUAL, then $EDITOR if not set
# editor = "code"

# Whether the editor is a GUI application (true) or terminal-based (false)
# GUI editors are spawned in the background; terminal editors suspend the TUI
# Auto-detected from a known list if not set (code, zed, subl, JetBrains IDEs, etc.)
# editor_gui = true

# Interval in seconds between GitHub PR checks (0 = disabled)
pr_check_interval_secs = 120

# Use rounded border corners (╭╮╰╯) instead of square (┌┐└┘)
rounded_borders = false

# Dim the right pane (preview/diff/shell) when the session list is focused
dim_unfocused_preview = true

# How much to dim unfocused pane colors (0.0 = fully dimmed/black, 1.0 = no dimming)
# Uses a foreground color override for cross-terminal compatibility (no Modifier::DIM)
dim_unfocused_opacity = 0.4

# Leader key for quick-switch session search
# Supports: " ", "space", "ctrl+k", "f1", etc.
# leader_key = " "

# Render PR labels as colored text on the default background (the pre-pill
# style). Default false renders them as colored "pill" blocks that stand out
# more in the session list.
# invert_pr_label_color = false

# Show the running program as a "(program)" suffix on session rows. Only
# renders when sessions use more than one distinct program, so it's a no-op
# for a single-program setup. Set to false to always hide it.
# show_session_program = true

# Debounce delay in ms when typing multi-digit session numbers
# session_number_debounce_ms = 250

# Interval in milliseconds for syncing state file changes from other instances (0 = disabled)
state_sync_interval_ms = 2000

# Log file path (if set, logs to file; use with --debug)
# log_file = "/tmp/claude-commander.log"

# Enable AI-generated branch summaries in the Info pane (default: true)
# ai_summary_enabled = true

# Claude model used for AI summaries (default: Haiku for cost efficiency)
# ai_summary_model = "claude-haiku-4-5-20251001"

# Custom key bindings — override any default key with one or more alternatives
# [keybindings]
# navigate_up = ["k", "Up"]
# quit = ["q", "Ctrl-c"]
# toggle_pane = ["Tab"]
```

### Theme Presets

Set `preset` under `[theme]` in your config to switch the entire color palette:

```toml
[theme]
preset = "rose-pine"
```

Available presets:

| Preset | Description |
|--------|-------------|
| `basic` | 16-color ANSI (maximum compatibility) |
| `indexed` | 256-color palette |
| `truecolor` | 24-bit Catppuccin-inspired pastels (default on capable terminals) |
| `monokai-dimmed` | Muted/desaturated Monokai — dark grays with soft gold, green, and blue accents |
| `zedokai` | Vibrant Monokai variant inspired by the Zed editor — vivid pink, green, and orange |
| `rose-pine` | Soft pink/rose aesthetic — deep navy-rose backgrounds with warm rose, iris, and foam accents |

When `preset` is unset (or `"(auto)"`), the theme auto-detects your terminal's color capability.

Individual color overrides (e.g. `border_focused = "#ff6600"`) still apply on top of the chosen preset.

### Session List Sections

Group the session list under configurable headers based on GitHub PR state.

By default `[[sections]]` is empty and the list keeps its project-grouped view. Once you declare one or more sections, the list switches to a section-grouped layout: section headers at the top level, each repo nested beneath as a sub-header, and sessions indented below their repo.

Sections replace [PR-stack grouping](#pr-stacks) — when sections are configured, stacked children are no longer visually nested under their stack base. The underlying stack links are still tracked and the `t` hotkey still stacks new sessions onto the top of a stack, but ordering within the list follows the section rules.

An implicit **"In Progress"** section is always the first row and acts as the catch-all — any session whose PR state doesn't match a later section's predicate lands here. It also lists every repo that hasn't placed a session into a later section, so newly added projects remain visible.

#### Example

This mirrors a label-driven code review workflow: author adds `dev-review-required` when the PR is ready, a reviewer removes the label and picks it up, the PR gets approved and merged.

```toml
[[sections]]
name = "Needs Review"
has_label = "dev-review-required"

[[sections]]
name = "In Review"
pr_state = "open"          # scope to open PRs — otherwise merged PRs
has_reviewer = true        # with reviewers would also match here

[[sections]]
name = "Merged"
pr_state = ["merged", "closed"]

[[sections]]
name = "Stale"             # no predicates → manual-only waypoint
```

Visually:

```
In Progress (12)
   terraform [main] (3)
      session-a
      session-b
      session-c
   genio     [main] (0)

Needs Review (1)
   genio     [main] (1)
      fix-dns-spam

In Review (2)
   terraform [main] (1)
      new-metrics-port
   genio     [main] (1)
      claude/add-elasticsearch-readonly-creds

Merged (3)
   …

Stale (0)
```

#### Predicate fields

All fields are optional; a section matches when **every declared field** matches (AND). A section declared with no predicates is a **manual-only waypoint** — auto-matching never puts sessions there; you only move in via the palette.

| Field | Type | Notes |
|---|---|---|
| `pr_state` | `"open"` \| `"closed"` \| `"merged"` — scalar or array (any-of) | |
| `is_draft` | `bool` | |
| `has_pr` | `bool` | |
| `has_label` | string (literal) or array (any-of) | |
| `review_decision` | `"approved"` \| `"changes_requested"` \| `"review_required"` — scalar or array (any-of) | Mirrors GitHub's `reviewDecision` field |
| `has_reviewer` | `true` / `false`, a specific login, or an array of logins (any-of) | `true` excludes Copilot via case-insensitive `"copilot"` substring match; specific/array forms match literally |

#### Process order and forward-only

Config order is the pipeline. A session's section is re-evaluated on every PR refresh, but the scan **only considers sections at or after the session's current position** — auto never moves a session backwards. This keeps `"Needs Review"` sticky when a reviewer removes the label without leaving other signals; the session doesn't slide back to `"In Progress"`.

#### Moving sessions manually

Select a session, open the palette (`Space`, or `Shift+Space` for commands-only), run **Move session to section…**, then pick a target. An **Auto** entry clears an existing pin. The override is persisted to `state.json` and survives restarts; auto-moves are suppressed until the pin is released.

#### Reordering, adding, or removing sections

These are edit-`config.toml`-and-restart actions — there's no hot reload. The cached `current_section` on each session is reconciled against the new config on next startup; if the referenced section no longer exists, the session falls back to `"In Progress"` and continues forward-only from there.

### AI Summary

The Info pane can display an AI-generated summary of branch changes, powered
by the Claude CLI. Press `g` (configurable) while viewing the Info pane to
generate a summary. Summaries are cached per-session — once generated, they
display instantly when you revisit the session. Press `g` again to regenerate
after making changes.

Requires the `claude` CLI to be installed and authenticated. If unavailable,
the summary section shows a placeholder instead.

| Setting | Default | Description |
|---------|---------|-------------|
| `ai_summary_enabled` | `true` | Set to `false` to disable AI summaries entirely |
| `ai_summary_model` | `claude-haiku-4-5-20251001` | Claude model used for summaries (Haiku recommended for cost efficiency) |

Environment variable overrides: `CC_AI_SUMMARY_ENABLED`, `CC_AI_SUMMARY_MODEL`.

## Data Storage

Paths are platform-specific, determined by the `directories` crate:

| File | macOS | Linux |
|------|-------|-------|
| Config | `~/Library/Application Support/com.claude-commander.claude-commander/config.toml` | `~/.config/claude-commander/config.toml` |
| State | `~/Library/Application Support/com.claude-commander.claude-commander/state.json` | `~/.local/share/claude-commander/state.json` |
| Worktrees | `~/Library/Application Support/com.claude-commander.claude-commander/worktrees/` | `~/.local/share/claude-commander/worktrees/` |

## Development

```bash
# Run tests
cargo test

# Run with debug logging
claude-commander --debug

# Check for issues
cargo clippy
```

### Architecture

The TUI event loop (`App`) owns the terminal and render state. It sends user commands to a `SessionManager` which coordinates tmux and git operations via async channels. Git read operations use gitoxide (pure Rust); worktree mutations and tmux use CLI subprocesses with semaphore-based throttling.

```
┌───────────────────────────────────────────┐
│              TUI (ratatui)                │
│  Renders widgets, handles input           │
└─────────────────┬─────────────────────────┘
                  │ mpsc channels
┌─────────────────▼─────────────────────────┐
│           SessionManager                  │
│  Session lifecycle, state persistence     │
└──────┬────────────────────┬───────────────┘
       │                    │
┌──────▼──────┐      ┌──────▼──────┐
│ TmuxExecutor│      │ GitBackend  │
│ (async CLI) │      │ (gitoxide)  │
└─────────────┘      └─────────────┘
```

## License

MIT
