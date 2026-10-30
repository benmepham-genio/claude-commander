# Claude Commander

A high-performance terminal UI for managing Claude coding sessions, written in Rust.

![The session list grouped by section, with PR stacks nested and a live preview of the selected session's agent](docs/images/stacked.svg)

## Features

- **Async-first architecture** - Non-blocking tmux and git operations
- **Hierarchical session model** - Projects contain worktree sessions
- **Add a project from GitHub** - Add a project by picking one of your GitHub repos, or by URL, and it's cloned in ready to go
- **Git worktree isolation** - Each session has its own worktree and branch
- **Kanban board UI** - Full-screen board with sections as columns and sessions as project-coloured cards
- **Live preview pane** - In the list views, a right-hand pane with Preview / Info / Shell tabs: Preview and Shell tail the selected session's agent and shell output as it happens, Info shows its metadata and PR detail (`Tab` cycles, `<`/`>` resizes)
- **Info modal** - On-demand session metadata, PR details, CI status, and AI-generated change summaries (`i`)
- **Review & comment** - Full-screen diff of a session's changes (vs its PR base) where you select lines, attach comments, mark files as reviewed, and apply comments straight to the running agent
- **Agent state detection** - Detect if agent is waiting for input, processing, or errored
- **Persistent state** - Sessions survive restarts
- **Auto-pull project main** - Periodically fast-forwards each project's main branch from `origin` so it doesn't drift stale

![Session info modal](docs/images/info-modal.svg)

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

## Quick Start

```bash
claude-commander
```

In the TUI:

- `N` — add a project (a git repository to manage sessions for)
- `n` — create a new worktree session in the selected project
- `Enter` — attach to the selected session
- `Ctrl-q` — detach back to the board
- `?` — show help, `,` — open settings, `q` — quit

See the full [keyboard shortcuts](#keyboard-shortcuts) below, and the
[Usage guide](docs/usage.md) for CLI commands, PR stacks, and AI summaries.

## Reference

### Views

The session list can be shown four ways, cycled with `v`: three **list**
views — grouped by **project**, by **section**, or by **section with
PR stacks** — and the full-screen kanban **board**. A first run starts in the
section-with-stacks view when `[[sections]]` are configured and in the
project-grouped view when they aren't; press `v` to rotate project → sections →
stacks → board → project. When no `[[sections]]` are configured the two section
views render identically to the project view, so `v` skips them and simply toggles between
the project list and the board. The chosen view is remembered across restarts.

The three list views pair the list with a **right-hand pane** carrying
**Preview**, **Info** and **Shell** tabs — a live tail of the session's agent
pane, its details, and a live tail of its shell. `Tab`/`Shift-Tab` cycles them
and `<`/`>` moves the divider, whose position persists across restarts. The
board is a full-screen takeover with no side panel, so there `i` is the only
route to a session's details. In every view `i` opens the Info modal,
`Enter`/`s` a session's shell, and `r` its review diff. In the
section-grouped list views a section under the cursor can be collapsed or
expanded with the **Toggle section** command (unbound by default; bind a key in
Settings or run it from the command palette).

#### Board

![The kanban board: sections as columns, sessions as project-coloured cards, projects in the sidebar](docs/images/board.svg)

Each **column** is a section, and
every session is its own **card**: the border is coloured by project and its
title is the session's number and name (project identity lives in the colour and
the sidebar, not in card text). The card's single interior line shows the status
glyph, row markers, PR pill / `[branch]`, and — right aligned — three clickable
action buttons: `[>_]` open shell, `[±]` open review diff, and `[i]` open info.
Clicking a button selects that card and fires the action; clicking elsewhere on a
card selects it and double-clicking attaches. A stacked session renders as its
own card nested (indented) beneath its parent, and the whole stack moves between
columns as a unit.

Within a column, cards sort by how likely a session is to need you: a
**needs-you** band on top (waiting for input, paused cascade, or unread output),
an **active** band (working / idle), and **stopped** sessions at the bottom;
newer sessions float to the top of their band. The leftmost column is a narrow
**project sidebar** listing every project with its session count; selecting a
project there enables the project-scoped actions — new session, project shell
(`s`), and remove project.

**Selecting** a project in the sidebar (`Enter`, or double-click) **filters**
the board to just that project's cards (the sidebar still lists every project);
the top bar shows the active filter. Selecting the same project again — or
`Esc` — clears it; selecting a different project refilters. Just moving the
cursor over the sidebar does not filter. Jumping to a session via the
quick-switch palette clears the filter if needed so the jump always lands.

With no `[[sections]]` configured, three default columns are shown and sessions
are assigned to them automatically from PR state: **In Progress** (the catch-all
for anything without a matching PR), **In Review** (open PR), and **Merged**
(merged PR). Defining any `[[sections]]` in config replaces these defaults — see
[Session List Sections](docs/configuration.md#session-list-sections). Press `m`
to move a card to another column. Empty columns are hidden by default
(`hide_empty_sections`), so a board shows only sections that have work.

### Workspaces

A **workspace** is a named group of projects — say *Work* and *Personal* — so
the list, the board (and its project sidebar), the Recent block and the status
counts show one group at a time. It is only a label on each project: every
workspace shares the same sessions, server and background refresh, and
switching is instant. Projects start in the built-in **Main** workspace, and
none of the workspace UI appears until you create a second one.

`w` cycles workspaces and `W` opens a picker (type a name that doesn't exist to
create it). Each workspace can have its own theme, and switching re-themes the
whole TUI. The status bar then shows the active workspace as a chip in its
theme's accent, followed by a `Personal ●2` hint (in that workspace's accent)
for any other workspace with sessions waiting for input; the board header and an attached session's tmux status line name it
too. The palette and the in-session `Ctrl-Space` switcher still search **every**
workspace: the active one's sessions rank first, the rest are tagged with their
workspace, and picking one switches workspace before jumping. New projects,
clones and scans land in the active workspace. **Settings → Workspaces** creates
(`n`), renames (`r`), deletes (`d`, which moves its projects back to Main) and
reorders (`J`/`K`) them, moves projects between them (`m`), and chooses which
workspace opens at startup (`s`); `Enter` on a workspace's **Theme** row opens
**Settings → Theme** scoped to it, where you pick a preset or recolour any role
from the theme's swatches or a typed or pasted hex, and **Reset to usual theme**
drops it again. With remote servers,
workspaces merge by name and every change is sent to each server. See
[Workspaces](docs/configuration.md#workspaces) and
[Workspace themes](docs/configuration.md#workspace-themes).

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

A `*` after a session's title means it has pending [review comments](docs/usage.md#reviewing--commenting-on-changes) that haven't been applied to the agent yet.

### PR Badges

When a session has a GitHub PR, a badge appears next to the session name. The badge color indicates the PR state:

| Color | Meaning |
|-------|---------|
| Blue | Open |
| Green | Open and awaiting review |
| Grey | Draft |
| Red | Closed |
| Dark purple | Merged |

The Info modal (`i`) shows additional detail when a PR is present, including a CI checks indicator:

| Symbol | Meaning |
|--------|---------|
| `✓` (green) | All checks passing |
| `✗` (red) | Checks failing |
| `◌` (orange) | Checks pending |
| `—` (grey) | No checks configured |

### Project Badges

When automatic project-branch pulling is enabled (see `project_pull_enabled` in [Configuration](docs/configuration.md)), a `⚠` badge appears next to a project name (in the sidebar and on that project's cards) if its main branch could not be fast-forwarded. The badge is derived state — it clears automatically on the next successful or no-op pull. The pull is held back for one of these reasons:

| Reason | Meaning |
|--------|---------|
| `Working tree dirty` | Main is the active checkout but has uncommitted changes |
| `Branch diverged from origin` | Local main has commits not on `origin` |
| `Checked out in another worktree` | Main is checked out in a separate worktree |

### Keyboard Shortcuts

All keybindings below are defaults and can be customised via the `[keybindings]` config table (see [Configuration](docs/configuration.md)).

The status bar surfaces the most useful actions as clickable buttons, with the hotkey letter bracketed (`[n]ew session`, `[d]elete`); the review view's footer works the same way. Clicking a button fires the same action as its key, so the hotkeys below can also be discovered and triggered with the mouse.

| Key | Action |
|-----|--------|
| `v` | Cycle the view: project list → section list → section-stack list → board → (repeat); section views are skipped when no `[[sections]]` are configured |
| `j/k` or `↑/↓` or `Ctrl-n/p` | Move up / down (within a board column, or through the list) |
| `h/l` or `←/→` | Move between columns / groups (the project sidebar is the board's leftmost column) |
| `]` / `[` | Next / previous column or group |
| `PageUp` / `PageDown` | Move up / down a screenful (within the board column, or through the list; stops at the ends rather than wrapping) |
| `Home` / `End` or `Ctrl-u/d` | Jump to first / last item |
| `1`–`99` | Jump to session by number |
| palette only | Toggle section — collapse/expand the section under the cursor in the section list views (unbound by default) |
| `Space` | Quick-switch palette (sessions and commands) |
| `Ctrl-Space` | Quick-switch palette (the same shortcut, and the same palette, as the in-session switcher — and as the session switcher inside the review diff) |
| `Shift+Space` | Command palette (commands only) |
| `>` (as first char in palette) | Filter palette to commands only |
| `Enter` | Attach to selected session |
| `Esc` | Clear the active project filter (set by selecting a project in the sidebar) |
| `w` | Next [workspace](#workspaces) (wraps around; with only one workspace it says so and does nothing) |
| `W` | Workspace picker — switch workspace, or type a new name to create one |
| palette only | Previous workspace, New workspace…, and Move project to workspace… (moves the selected project; unbound by default) |
| `i` | Show session info in a modal — metadata, diffstat, PR details, stack chain, `g` for AI summary. Same content as the right pane's Info tab, and the only way to reach it from the board |
| `n` | New worktree session |
| `t` | New session stacked on top of the selected session's stack |
| `N` | Add new project |
| `c` | Checkout existing branch into a new worktree session (fetches `origin` in the background, filterable list) |
| `d` | Delete session |
| `R` | Restart session (kill tmux + recreate; adds `--resume` when `resume_session = true`) |
| `D` | Remove project |
| `.` or `Ctrl-.` | Open in editor/IDE (also works inside the review diff, opening that session's worktree) |
| `o` | Open PR in browser (when the session has a PR) |
| palette only | Refresh PR status (force an immediate re-check for all sessions instead of waiting for the `pr_check_interval_secs` cadence) |
| `C` | Open the commander session (a persistent, project-less Claude session that coordinates others; requires `commander_enabled = true`). While it is running, a `● Commander` chip in the footer status bar shows its live state (`· working` / `· waiting` / `· idle`) |
| `Alt-c` | Open/close the conversation overlay: a full-screen chat with a dedicated Claude session whose replies stream in and are spoken aloud via an OpenAI-compatible TTS engine. Enable it first in Settings ▸ Voice (off by default); see [Conversation mode](docs/configuration.md#conversation-mode-tts). The session keeps running when the overlay is closed |
| `Alt-v` | Voice input (push-to-talk by toggle): press once to start recording the microphone, press again to stop, transcribe via an OpenAI-compatible speech-to-text engine, and send the text to the conversation agent. Works whether the overlay is open or not. Enable it in Settings ▸ Voice (`stt_enabled`, off by default). Can also be triggered **system-wide** via a desktop global shortcut — see [Global voice hotkey](docs/configuration.md#global-voice-hotkey) |
| `Alt-t` | Dictation (same toggle, different destination): record the microphone and **type the transcript where you are**: the pane you're attached to — agent or shell, local or remote — or, outside a pane, the open dialog's text field (a review comment, the new-session name, the conversation input, a search box), rather than sending it to the conversation agent. Pressed in the bare session list, with nowhere to type, it says so and records nothing. The text is inserted without an Enter unless you set `dictation_submit`; see [Dictation (Alt-t)](docs/configuration.md#dictation-alt-t). Shares the `[stt]` engine and the `stt_enabled` switch with `Alt-v`, and either key stops a recording the other started |
| `S` | Scan directory for git repos and add them as projects |
| `s` | Open shell in worktree (or a project shell when a project is selected in the sidebar) |
| `m` | Move a card to another column (a stacked session moves with its whole stack; manual override — see [Session List Sections](docs/configuration.md#session-list-sections)) |
| `r` or `Alt-r` | Review & comment on a session's diff — see [Usage](docs/usage.md#reviewing--commenting-on-changes) |
| palette only | Reset session — restart it with **no** resume, so the agent starts a new conversation. Use it when `resume_session = true` but you want a clean slate, or when resuming is itself what breaks the relaunch. The worktree, branch and commits are untouched. Unbound by default so a mistyped `R` can't discard a conversation; bind `reset_session` under `[keybindings]` if you want a key |
| palette only | Rename session (UI title only; underlying worktree, branch, and tmux session are unchanged) |
| palette only | Change program (agent) — pick a different program (e.g. `claude`, `codex`, `opencode`, `omp`, `muse-code`) for the selected session and relaunch it with a fresh conversation |
| palette only | Set session base (restack) — re-point the selected session at a different stack parent, or unstack it onto the project's main branch |
| `g` | Generate AI summary (available while an Info surface is showing — the modal or the right pane's Info tab) |
| `Tab` / `Shift-Tab` | Cycle the right pane forward / back through Preview, Info and Shell (list views only; the board is full-screen). A project row has no agent pane, so it cycles Shell ↔ Info |
| `<` / `>` | Narrow / widen the session list, moving the divider between it and the right pane (list views only) |
| `,` | Open settings |
| palette only | Copy server token — puts the embedded server's bearer token on your clipboard for pairing a client. The URL is reported in the status bar rather than copied, so the secret never enters your scrollback. Only offered while a server is actually being served from this process |
| `?` | Show help |
| `q` or `Ctrl-c` | Quit |

### Attached Session Shortcuts

When attached to a session (via `Enter` or `claude-commander attach`):

| Key | Action |
|-----|--------|
| `Ctrl-q` | Detach and return to the board |
| `Ctrl-\` | Switch between the Claude and shell sessions |
| `Alt-r` | Switch to this session's review diff (and `Alt-r` in the diff switches back) — Claude sessions only. Uses `Alt-r` rather than `Ctrl-r` so the shell's `Ctrl-r` reverse-history-search is never shadowed |
| `Ctrl-Space` | Open the quick-switch palette over the session, to jump to another claude-commander session without detaching. It is the same palette as `Ctrl-Space` in the board, so it lists remote sessions and commands too; `Esc` returns you to the pane. Switching between two local sessions never detaches |
| `Ctrl-.` | Open the session worktree in your editor (requires a terminal that emits CSI-u or xterm modifyOtherKeys sequences for Ctrl-.) |
| `Alt-v` | Toggle voice input to the conversation agent — the same recording as `Alt-v` in the board, so the transcript goes to the conversation session and the reply is spoken, not typed into this pane. Claude sessions only (a shell pane forwards the key on) |
| `Alt-t` | Dictate into **this** pane: record, then type the transcript where the cursor is. Unlike `Alt-v` it is intercepted on agent **and** shell panes, so in a shell it shadows readline's `transpose-words`, and in a Claude pane it shadows Claude Code's own `Alt+T` (toggle extended thinking — [keyboard shortcuts](https://code.claude.com/docs/en/interactive-mode.md#keyboard-shortcuts)). Nothing is submitted unless `dictation_submit` says so; rebind it with `toggle_dictation` under `[keybindings]`. It follows the client, so `Ctrl-Space`ing to another session dictates into that one |
| `Ctrl-v` | **Remote sessions only:** paste an image from your local clipboard into the Claude prompt. The image is uploaded to the server, saved to a temp file, and its path is typed into the prompt. On a local session `Ctrl-v` is forwarded to Claude, which reads your clipboard directly. If the clipboard holds no image, `Ctrl-v` is forwarded unchanged |

### Serving Your Own Machine

The other direction: `claude-commander --serve` runs the HTTP API inside the TUI's
own process, so a client (the [Flutter app](client/README.md), a phone, another
desktop) can reach your sessions without you remembering to start a second
process. Set `auto_start = true` under `[server]` to make it the default, and it
comes up with the TUI and goes down when the TUI exits. A `⇅ 7878` chip in the
status bar confirms it, and the palette's **Copy server token** puts the bearer
token on your clipboard to pair a client with.

It binds loopback unless you say otherwise, and always requires a bearer token —
generated and saved on first serve. Bind address, port and token are editable in
**Settings → Server**; see [Configuration](docs/configuration.md#serving-this-machine-server).

`claude-commander-server` still exists as a standalone binary for headless hosts
where there is no TUI to attach it to.

### Remote Servers

The TUI can manage sessions on other machines running `claude-commander-server`.
Each configured server appears as its own node in the session tree, with that
server's projects and sessions underneath — create, delete, restart, review
diffs, and attach to remote terminals exactly as you would locally (attach
streams over WebSocket). Live agent-state dots and PR chips come from the
server's own background refresh, and an unreachable server degrades to a
greyed node with the error shown, retrying in the background without ever
blocking local work.

Add or remove servers from the command palette (**"Add remote server"** walks
name → URL → token with a connection test), or edit `[[remote_servers]]` in
the config file directly — see [Configuration](docs/configuration.md). Changes
hot-reload; no restart needed.

Each server's **program list** (the new-session picker options) lives in that
server's own config, so a fresh server offers only the built-in `claude` until
you configure it. Edit it without leaving the TUI: click the ⚙ on a server's
tree header, or run **"Edit server's program list…"** from the palette, to open
Settings → Programs targeting that server. In the Programs tab, `t` cycles which
backend (local or a remote server) you're editing; edits are saved to the chosen
backend as you make them.

## Web UI

The server also serves a browser UI, on the same port as its API: whichever of
`claude-commander --serve` or `claude-commander-server` is running, open

```
http://127.0.0.1:7878/#token=<bearer token>
```

(**Copy server token** in the palette gives you the token.) The page reads the
token from the URL fragment — which browsers never send to the server — stores it
in that browser's local storage, and strips it from the address bar. Without one
it asks. It lists projects and sessions, creates, restarts and deletes them,
attaches to the live terminal over WebSocket, and reviews diffs.

**Keep it on loopback, or behind a TLS-terminating reverse proxy.** The server
speaks plain HTTP, so on any other address the token and your terminal traffic
cross the network in the clear, and the bearer token is the only thing standing
between that network and a shell on your machine. Authentication fit for an
internet-facing deployment has not been designed yet; don't expose the port
directly.

Working on the page itself: its source is in [`web/`](web/), and
`scripts/dev-run.sh web` rebuilds it on save and serves it from an isolated
debug server, printing a ready-to-open URL.

## Flutter Client

For the times you're not at a terminal, [`client/`](client/README.md) is a
cross-platform GUI client for the same `claude-commander-server` — verified on
**Linux desktop** and **Android**. It connects to as many servers as you like at
once and groups their sessions together, and it carries the parts of the TUI that
matter away from the keyboard: live agent state, the attached terminal over
WebSocket, and the diff review with line comments.

Below the 900px breakpoint it's a stacked phone flow — fleet list, then session
detail, then the live agent terminal. It ships two themes, **Mission Control**
(the default) and **LCARS**, switchable from settings:

<p align="center">
  <img src="docs/images/client-sessions.png" alt="The client's fleet list on a phone: sessions grouped by project with agent-state glyphs and PR badges" width="31%">
  &nbsp;
  <img src="docs/images/client-terminal.png" alt="The client on a phone attached to a session's agent terminal" width="31%">
  &nbsp;
  <img src="docs/images/client-lcars.png" alt="The same fleet list on a phone in the LCARS theme: black background, amber and lilac elbow chrome" width="31%">
</p>

Above it, the same page bodies become a desktop layout: the fleet list beside a
workspace whose Overview / Agent / Shell / Changes tabs switch in place.

![The client on the desktop: fleet list on the left, the selected session's live agent terminal on the right](docs/images/client-desktop.png)

## Documentation

- **[Usage guide](docs/usage.md)** — CLI commands, the board, PR stacks (cascade merge / push stack), and AI summaries
- **[Configuration](docs/configuration.md)** — all config options, theme presets (including `appearance = "light"` for light terminals), session-list sections (with optional advisory WIP limits), and data-storage paths
- **[Contributing](CONTRIBUTING.md)** — releasing, the local dev loop, and architecture overview
- **[Flutter client](client/README.md)** — cross-platform GUI client (Linux desktop + Android) for `claude-commander-server`

## Telemetry & Privacy

Claude Commander reports anonymous **feature-usage** telemetry (on by default) so
we can learn which features are used and retire the ones that aren't. It sends
feature names, a coarse environment fingerprint (OS, terminal, shell), a
non-sensitive config snapshot (e.g. theme), and a random install id — **never**
typed text, prompts, session content, branch names, or paths. Opt out with
`telemetry.enabled = false` in your config or by exporting `DO_NOT_TRACK=1`. See
[Configuration → Usage Telemetry](docs/configuration.md#usage-telemetry) for the
full list and self-hosting options.

## License

MIT
