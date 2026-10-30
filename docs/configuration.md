# Configuration

Configuration, theming, section grouping, and data-storage paths for Claude
Commander. For installation and the keyboard / status-symbol reference, see the
[README](../README.md). For the usage guide, see [Usage](usage.md).

Configuration file location depends on your platform:

- **macOS**: `~/Library/Application Support/com.claude-commander.claude-commander/config.toml`
- **Linux**: `~/.config/claude-commander/config.toml`

```toml
# Selectable agent harnesses for the New Session dialog's program picker. Each
# entry pairs a display `label` with the `command` launched (program plus any
# flags); the command's first token determines the harness, so Claude Code
# (`claude`), OpenAI Codex (`codex`), OpenCode (`opencode`) and Oh My Pi (`omp`)
# are all recognised and get the right launch, resume, and working/waiting
# detection. Anything else is launched as-is, with no such handling — a bare
# shell is a valid entry. The first entry is the default
# for new sessions. When `programs` is omitted, the picker offers a single
# built-in `claude` entry.
#
# Legacy configs using `default_program = "..."` are migrated automatically:
# the legacy command is moved or inserted as the first `[[programs]]` entry and
# `default_program` is removed from the file.
#
# In the New Session dialog, press Tab (or Shift+Tab to go back) to cycle focus
# between the name field, the project picker, and the program picker, then ↑/↓
# to choose. The project picker defaults to the currently-selected project and
# can be typed into to filter the list — so the usual "type a name, hit Enter"
# flow is unchanged.
#
# Program entries can be managed in-app from Settings ▸ Programs (settings
# modal, `,` key) — add (`n`), rename (`r`), edit the label/command fields, and
# remove (`d`) entries, or reorder them with `J`/`K`. Reordering changes the
# default, since the first entry is the default for new sessions.
#
# [[programs]]
# label = "Claude"
# command = "claude"
#
# [[programs]]
# label = "Claude (Opus, plan mode)"
# command = "claude --model opus --permission-mode plan"
#
# [[programs]]
# label = "Codex"
# command = "codex"
#
# [[programs]]
# label = "Codex (full auto)"
# command = "codex --full-auto"

# Branch name prefix for new sessions (empty = no prefix)
branch_prefix = ""

# Fetch latest changes from origin before creating a new session
fetch_before_create = true

# Skip git-LFS smudging during `git worktree add` so session creation is fast on
# LFS repos (the checkout leaves cheap pointer files). The real LFS content is
# fetched afterwards with `git lfs pull` — asynchronously in the TUI (with a
# `⇣ LFS` indicator on the session row) or synchronously on the CLI. While the
# background pull runs, the agent briefly sees pointer files rather than real
# content. Set to false to smudge during checkout as before.
skip_lfs_smudge = true

# Pass `--resume` when restarting/recreating a session so the agent picks up
# where it left off. Set to false to start the program fresh each time.
# This is the default for every restart; to override it for one session without
# touching config, run **Reset session** from the command palette — that
# relaunches the pane with no resume, whatever this says.
resume_session = true

# Automatically hibernate idle sessions to free memory (see "Idle-session
# hibernation" below). A background loop stops the tmux process of sessions
# that have sat idle-and-unattended past the timeout, keeping the worktree and
# metadata; the session transparently resumes on next attach. Disabled by
# default. Enabling (and the check interval) is restart-required; the idle
# timeout is read live.
# hibernate_enabled = false
# Idle duration (seconds) before an eligible session is hibernated. Default
# 86400 (1 day). The in-app settings editor enforces a minimum of 60;
# hand-edited values here are used as-is.
# hibernate_idle_timeout_secs = 86400
# Interval (seconds) between hibernation policy checks. Default 600 (10 min).
# The in-app settings editor enforces a minimum of 10; hand-edited values here
# are used as-is, and 0 disables the loop entirely.
# hibernate_check_interval_secs = 600

# Launch sessions inside `nix develop` when the project has a `flake.nix` at
# its root or in `nix/`, and `nix` is on PATH, so the agent and shell get the project's dev
# environment. Applies to Claude sessions and shell sessions. Projects without
# a flake are unaffected.
nix_develop = true

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

# Directory that cloned repositories land in (default: "~/Projects"). Unlike the
# worktrees directory this lives under your home, not the app's data dir — these
# are ordinary checkouts you work in directly.
# projects_dir = "/path/to/projects"

# Timeout (seconds) for a single repository clone before it is abandoned.
# Default 1800 (30 min).
# clone_timeout_secs = 1800

# Timeout (seconds) for listing your GitHub repos in the clone picker, which
# runs `gh api --paginate`. Default 90. Raise it if a very large account times
# out — the repo list is never truncated to fit, so a timeout means an empty
# picker rather than a short one.
#
# Note for remote (server) use: a client gives this request 120 seconds. Raising
# repo_list_timeout_secs past that means the client gives up first and reports a
# connection error instead of the server's real reason.
# repo_list_timeout_secs = 90

# Isolate every tmux command onto a throwaway socket dir (default: unset).
# For hermetic tests and the e2e harness ONLY — leave unset for normal use.
# When set, tmux commands run with TMUX_TMPDIR=<dir> and $TMUX/$TMUX_PANE
# stripped, so they hit a per-run tmux server instead of your real one.
# tmux_tmpdir = "/path/to/throwaway/tmux"

# Base directory for the temp files handed to the agent by path — pasted images
# (remote image paste) and comment-apply briefs (default: the OS temp dir).
# For hermetic tests ONLY — leave unset for normal use; when set, those writes
# and the paste store's pruning go here instead of the OS temp dir.
# (Accepted under its former name `paste_images_dir` too.)
# agent_temp_dir = "/path/to/throwaway/agent-temp"

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
# Run "Refresh PR status" from the command palette to force an immediate
# re-check without waiting for this interval to elapse.
pr_check_interval_secs = 120

# Periodically fast-forward each project's main branch from origin.
# When enabled, runs `git fetch origin <main>` and advances the local
# `<main>` ref whenever a fast-forward is possible. If `<main>` is the
# currently checked-out branch in the project repo, `git merge --ff-only`
# is used when the working tree is clean. The pull is held back (and the
# project row shows a ⚠ badge) when the working tree is dirty, the branch
# has diverged from origin, or `<main>` is checked out in another worktree.
# Default: enabled.
project_pull_enabled = true

# Interval in seconds between project-branch pulls. Minimum 60.
# Default: 3600 (one hour).
project_pull_interval_secs = 3600

# Use rounded border corners (╭╮╰╯) instead of square (┌┐└┘)
rounded_borders = false

# Dim the live-capture tabs (Preview / Shell) of the list views' right-hand
# pane. They are passive tails — keys always drive the session list — so they
# render dimmed by default to keep the list visually dominant. Set to false to
# render them at full brightness. The pane's Info tab is never dimmed (it is
# static text, not a tail), and the board has no right pane at all.
dim_unfocused_preview = true

# How much to dim the right pane's colours (0.0 = fully dimmed/black, 1.0 = no
# dimming). Uses a foreground colour override rather than the terminal DIM
# modifier, for cross-terminal consistency. Only applies when
# dim_unfocused_preview is true.
dim_unfocused_opacity = 0.4

# When opening the review view, precompute every file's render caches
# (word-diff segments + syntax highlighting) up front behind a loading spinner,
# instead of building each file's cache lazily on first navigation. Trades a
# one-off wait when opening for instant file switching afterwards. Default true;
# set to false for lazy, instant-open behaviour.
# precompute_review_caches = true

# Leader key for quick-switch session search
# Supports: " ", "space", "ctrl+k", "f1", etc.
# leader_key = " "

# Render PR labels as colored text on the default background (the pre-pill
# style). Default false renders them as colored "pill" blocks that stand out
# more on the board.
# invert_pr_label_color = false

# Show the running program as a "(program)" suffix on session rows. Only
# renders when sessions use more than one distinct program, so it's a no-op
# for a single-program setup. Disabled by default; set to true to show it.
# show_session_program = false

# Hide empty section columns on the board. When enabled, a section with no
# cards (including the implicit "In Progress" catch-all) is dropped from the
# columns, so a board with many configured sections shows only those with work.
# The sidebar still lists every project. Enabled by default; set to false to
# always show every section column.
# hide_empty_sections = true

# Debounce delay in ms when typing multi-digit session numbers
# session_number_debounce_ms = 250

# Number of most-recently-attached sessions to surface in the "Recent" block at
# the top of the session list. The block spans all servers, colours each row by
# its project, and reuses the same number the session has in its normal position
# below (so the block acts as a view, not a duplicate). Set to 0 to hide it.
# recent_sessions_limit = 5

# Workspaces: named groups of projects (see "Workspaces" below). Array order is
# display order; the built-in Main workspace (untagged projects) is implicit and
# never listed.
#
# [[workspaces]]
# name = "Work"
#
# [[workspaces]]
# name = "Personal"
#
# Label for the built-in Main workspace (default label "Main").
#
# [main_workspace]
# name = "Home"
#
# Which workspace to open on: "last" (default — whichever this client last had
# active), "main", or a workspace name (falls back to Main if it no longer
# exists).
# startup_workspace = "last"
#
# Per-workspace TUI themes, keyed by workspace name; the built-in Main
# workspace's key is always "main", whatever its label (see "Workspace themes"
# below). Each table takes the same keys as [theme]. Without `preset` it layers
# its colours over your usual [theme]; with `preset` it starts from that preset
# and only its own colours apply.
#
# [workspace_themes."Work"]
# text_accent = "#e5a50a"
#
# [workspace_themes.main]
# preset = "basic"

# Interval in milliseconds for syncing state file changes from other instances (0 = disabled)
state_sync_interval_ms = 2000

# Log file path (if set, logs to file; use with --debug)
# log_file = "/tmp/claude-commander.log"

# Enable AI-generated branch summaries in the Info modal (default: true)
# ai_summary_enabled = true

# Claude model used for AI summaries (default: Haiku for cost efficiency)
# ai_summary_model = "claude-haiku-4-5-20251001"

# Persistent "commander" session — a project-less Claude session (opened with
# `C` or `claude-commander commander`) that coordinates other sessions via the
# CLI. Disabled by default; enabling it is restart-required (the open path and
# the footer chip both key off the value read at launch). While running, a
# `● Commander` chip in the footer status bar shows its live state.
# commander_enabled = false
# Program (with flags) for the commander; defaults to the first `[[programs]]`
# entry.
# commander_program = "claude --model opus-4-7"
# Working directory for the commander; defaults to <data dir>/commander.
# commander_dir = "/path/to/commander"

# Conversation mode: a full-screen chat (open with `Alt-c`) backed by a
# dedicated headless Claude session, whose replies stream in and are spoken
# aloud via an OpenAI-compatible TTS engine. See "Conversation mode" below.
# [conversation]
# enabled = true                          # master switch: Alt-c overlay + spoken replies (off by default)
# name = "Claudette"                       # assistant's display name / nickname
# command = "claude"                       # binary for the conversation session
# permission_mode = "auto"                 # --permission-mode for the agent (acts without approval prompts)
# base_url = "http://127.0.0.1:8002/v1"   # OpenAI-compatible TTS endpoint (include /v1)
# model = "kokoro"                         # TTS model name (engines serving one model ignore it)
# voice = "af_sky"                         # omit to use the server's default voice
# response_format = "wav"                  # wav | mp3 | opus | flac (wav = lowest local latency)
# speed = 1.0                              # 0.25–4.0
# speak_scope = "prose_only"               # prose_only | verbatim (per-sentence, streamed)
# volume = 1.0                             # 0.0–2.0

# Voice input (speech-to-text): talk instead of typing. Toggle recording with
# `Alt-v` to send the transcript to the conversation agent, or with `Alt-t` to
# type it into the pane you're attached to. Either way it's transcribed via an
# OpenAI-compatible STT engine. See "Voice input (STT)" and "Dictation (Alt-t)".
# [stt]
# enabled = true                          # master switch for Alt-v / Alt-t voice input (off by default)
# base_url = "http://127.0.0.1:8000/v1"   # OpenAI-compatible transcription endpoint (include /v1)
# model = "Systran/faster-whisper-base"    # transcription model name
# language = "en"                          # ISO-639-1 hint; omit to auto-detect
# prompt = "..."                           # optional decoding prompt (domain vocab / spelling)
# api_key = "..."                          # sent as a Bearer header; omit for local servers
# input_device = "..."                     # microphone to capture from; omit for the system default
# pause_media = true                       # pause other players while recording, resume after the
#                                          # reply (best-effort via playerctl/osascript; on by default)
# dictation_submit = "never"               # never | agent | always — press Enter after a dictated transcript?

# Custom key bindings — override any default key with one or more alternatives
# [keybindings]
# navigate_up = ["k", "Up"]
# next_group = ["]"]
# previous_group = ["["]
# navigate_first = ["Home"]
# navigate_last = ["End"]
# navigate_left = ["h", "Left"]
# navigate_right = ["l", "Right"]
# list_page_up = ["PageUp"]                # page the list / board column
# list_page_down = ["PageDown"]
# page_up = ["Ctrl-u"]                     # first card in the board column
# page_down = ["Ctrl-d"]                   # last card in the board column
# open_info = ["i"]
# toggle_pane = ["Tab"]                    # cycle the right pane: Preview / Info / Shell
# toggle_pane_reverse = ["Shift-Tab"]
# shrink_left_pane = ["<"]                 # move the list/pane divider left
# grow_left_pane = [">"]                   # move it right
# quit = ["q", "Ctrl-c"]
# set_session_base = ["B"]                 # palette-only by default; bind a key here
# toggle_keep_alive = ["K"]                # palette-only by default; bind a key here
# reset_session = ["Ctrl-r"]               # palette-only by default; bind a key here
# next_workspace = ["w"]                  # cycle workspaces (wraps)
# workspace_picker = ["W"]                 # switch / create a workspace
# previous_workspace = []                  # palette-only by default; bind a key here
# new_workspace = []                       # palette-only by default
# move_project_to_workspace = []           # palette-only by default

# Remote claude-commander servers. Each entry adds a server node to the
# session tree with that server's projects and sessions under it (full
# create/delete/review/attach parity over HTTP + WebSocket). Manage these
# from the palette ("Add remote server" / "Remove remote server", which
# includes a connection test) or by hand here — the file is hot-reloaded.
# These are a list-of-tables, so unlike the scalar options above they are
# NOT editable from the in-app settings modal; use the palette commands.
# The name "local" is reserved for the built-in local machine.
# [[remote_servers]]
# name = "buildbox"              # unique display name (the tree header)
# url = "http://buildbox:7878"   # base URL of claude-commander-server
# token = "..."                  # bearer token; omit only for servers
#                                # started with --allow-no-auth (loopback)

# Serving THIS machine's sessions over HTTP. The same table is read by the
# standalone `claude-commander-server` binary and by the TUI, which runs the
# server inside its own process when `auto_start` is on (or `--serve` is
# passed) and takes it down when it exits. Editable from the in-app settings
# modal: Settings > Server.
# [server]
# auto_start = true              # serve for as long as the TUI is open
# bind = "127.0.0.1"             # "0.0.0.0" to accept clients from the LAN
# port = 7878
# token = "..."                  # generated and written here on first serve
# cors_allowed_origins = []      # origins a browser may call /api from
```

### Serving this machine (`[server]`)

Wanting the server on the same machine as the TUI is the common case, and having to
remember to start it separately is friction — so `auto_start = true` brings it up
with the TUI. It runs *inside* the TUI process and shares its session manager, so
there is one set of background pollers, one writer to `state.json` and one
telemetry stream, and the listener goes away exactly when the TUI does. A `⇅ 7878`
chip in the status bar confirms it came up.

Everything under `[server]` is read once at startup, so changing it needs a
restart — the status bar says so after an edit. If the port is already taken (most
often because a standalone `claude-commander-server` is already running) the TUI
carries on without serving: the chip reads `⇅ server unavailable`, and the reason
appears once in the status bar and in the log.

The bind address defaults to loopback, so out of the box nothing off this machine
can reach it; set `bind = "0.0.0.0"` for a phone or another desktop on the LAN.
Authentication is never optional here: if `token` is unset, the first serve
generates one and writes it back to this file so a paired client keeps working
across restarts. Use the palette's **Copy server token** to hand it to a client —
the settings modal deliberately shows only whether a token is set, never its
value, and the copy reports the URL in the status bar rather than putting the
secret in your scrollback. (The standalone binary differs: it logs a one-time
token instead of persisting one, since a managed deployment's config file may be
read-only.)

A remote server's `token` is **operator-equivalent**: anyone holding it can create
sessions (which run arbitrary programs on that machine) and address projects by
server-side path. Treat it like an SSH key — don't commit it, don't share it, and
scope it to people you'd give a shell. On disk this file is protected only by its
`0600` permissions, so keep those intact.

Don't add a remote server that shares this machine's `state.json` (for example a
loopback `http://localhost:7878` server backed by the same data directory as your
local instance). Its sessions are already shown under the local machine, so they
would appear twice — once under the local root and once under the server header —
and rows under the server header could be misrouted to the local backend. A
loopback server is fine only when it runs against a **separate** data directory.
claude-commander logs a warning when a configured server's URL is a loopback
address as a reminder.

## Idle-session hibernation

Each live session holds a `claude` process open in tmux (~400MB RAM) whether or
not you're using it. With many sessions open, idle ones waste memory. When
`hibernate_enabled = true`, a background loop stops the tmux process of any
session that has been idle past `hibernate_idle_timeout_secs`, **keeping its
worktree, branch, and metadata** — only the process is stopped. The session
shows as `Stopped` and transparently resumes (with `--resume`, so the
conversation is preserved) the next time you attach.

A session counts as **idle** only when all of these hold:

- its agent is `Idle` — a `Working`, `WaitingForInput` (pending approval), or
  momentarily undetectable session is never hibernated;
- no tmux client is attached to it, and it wasn't attached within the last
  check interval or 30 seconds, whichever is longer (so recent attaches are
  protected slightly longer when the check interval is set below 30s);
- `keep_alive` is off for that session.

**Waking is automatic**: attaching (`Enter`) recreates the tmux session and
resumes the agent, even if `resume_session = false` globally — hibernation
always resumes, since that's what makes it non-destructive.

**Keeping a session alive**: run the **Toggle keep-alive** action from the
command palette on a session (or `claude-commander keep-alive <session>
[--on|--off]`) to exempt it from hibernation — useful for a long-running build,
a watched log, or anything you want to keep warm. A kept-alive session shows an
anchor (`⚓`) marker on the board. The action has no default hotkey; bind
one via `toggle_keep_alive` under `[keybindings]` (e.g. `toggle_keep_alive =
["K"]`) if you want a shortcut. The flag persists across restarts.

Enabling hibernation and changing `hibernate_check_interval_secs` are
restart-required (the loop is spawned once at launch); `hibernate_idle_timeout_secs`
is read live.

## Conversation mode (TTS)

Press **`Alt-c`** to open a full-screen **conversation overlay** — a chat with a dedicated
Claude session whose replies **stream in and are spoken aloud** through any **OpenAI-compatible**
TTS engine (it posts to `{base_url}/audio/speech`). Type a message, press Enter; the reply is
spoken sentence-by-sentence *as it's generated*. Closing the overlay (Esc / `Alt-c`) leaves the
session running, so the conversation continues where you left off.

This session is **separate from the commander** (`Shift-C`) and unrelated to it. It's driven over
Claude Code's stream-json protocol (`claude -p --input-format stream-json --output-format
stream-json --include-partial-messages`), which is the supported way to get clean token-level
text — so TTS can start within a sentence of Claude beginning to type, rather than waiting for the
whole reply. A new message interrupts in-flight speech. If the TTS server is unreachable, the chat
still works (text-only) and never blocks the UI.

`enabled` is the master switch for the whole feature and is **off by default** — set it (in
Settings ▸ Voice or config) before `Alt-c` will open the overlay. We develop against a
local [Kokoro](https://github.com/sizeak/kokoro-tts-rocm) container (default
`http://127.0.0.1:8002/v1`), but any OpenAI-compatible endpoint works.

```toml
[conversation]
enabled = true                          # speak replies via TTS (off = text-only chat)
command = "claude"                       # binary for the conversation session
base_url = "http://127.0.0.1:8002/v1"   # OpenAI-compatible TTS endpoint (include /v1)
model = "kokoro"                         # TTS model name (engines serving one model ignore it)
voice = "af_sky"                         # omit to use the server's default voice
response_format = "wav"                  # wav | mp3 | opus | flac (wav = lowest local latency)
speed = 1.0                              # 0.25–4.0
speak_scope = "prose_only"               # prose_only | verbatim
volume = 1.0                             # 0.0–2.0
```

`speak_scope` controls what's spoken (applied per sentence as it streams):

| Value | Behaviour |
|-------|-----------|
| `prose_only` (default) | Strip code blocks and markdown; speak the natural-language prose |
| `verbatim` | Speak the text unchanged |

> **Build note:** in-process playback (`rodio`) and microphone capture (`cpal`) use **PipeWire** as
> the default audio host on Linux (falling back to **ALSA** at runtime if PipeWire isn't running),
> so both backends are linked. They're gated behind the `audio` cargo feature, which is **on by
> default** — so building the TUI (`claude-commander`) from source needs the PipeWire and ALSA
> development libraries plus **clang/libclang** (cpal's PipeWire backend runs `bindgen`):
> `libpipewire-0.3-dev libasound2-dev libclang-dev` on Debian/Ubuntu, `pipewire alsa-lib clang` on
> Arch. The default Nix dev shell provides them automatically. The headless server and the Flutter
> client build with `audio` off (`default-features = false`) and never link either backend — remote
> clients do capture/playback on-device instead.

## Voice input (STT)

Press **`Alt-v`** to talk to the conversation agent. It's a **toggle**: the first press starts
recording the microphone, the next press stops it, transcribes the audio through any
**OpenAI-compatible** speech-to-text engine (it posts a WAV to `{base_url}/audio/transcriptions`),
and sends the resulting text to the conversation session — exactly as if you'd typed it. The reply
then streams back and is spoken aloud (if TTS is enabled). Voice input works **whether the overlay
is open or not**, mirroring spoken replies.

`stt.enabled` is a separate switch from `conversation.enabled` and is **off by default**. It is the
master switch for *both* uses of the microphone: `Alt-v` (this section, which does need conversation
mode, since that's where the transcript goes) and `Alt-t` ([dictation](#dictation-alt-t), which
types the transcript into the attached pane or the open dialog's text field, and needs no
conversation session at all). Microphone capture uses `cpal` (PipeWire/ALSA on Linux — see the
build note above). If no microphone is available or the STT server is unreachable, voice input
degrades gracefully (a status message) and never blocks the UI.

```toml
[stt]
enabled = true                          # master switch for Alt-v / Alt-t voice input (off = no mic)
base_url = "http://127.0.0.1:8000/v1"   # OpenAI-compatible transcription endpoint (include /v1)
model = "Systran/faster-whisper-base"    # transcription model name
language = "en"                          # ISO-639-1 hint; omit to auto-detect
# prompt = "Vitest, Kotlin, ..."         # optional decoding prompt (domain vocab / spelling hints)
# api_key = "..."                        # sent as a Bearer header; omit for local servers
# input_device = "alsa_input.pci-0000_c1_00.6.analog-stereo"  # device id; omit for the system default
pause_media = true                       # pause other players while recording, resume after the reply
dictation_submit = "never"               # never | agent | always — press Enter after a dictated transcript?
```

`input_device` picks which microphone to capture from — omit it (or leave it as **(default)**
in Settings ▸ Voice) to use the system default. Set it from the **Microphone** picker under
**Transcription** in the settings modal, which lists each device by a friendly name;
monitor/loopback sources (e.g. recording your speakers) are tagged **(loopback)**. The value stored is cpal's stable device *id*
(the PipeWire `node.name`, e.g. `alsa_input.pci-…`), not the friendly name — because a mic and its
speaker's loopback can share a name, so ids are what uniquely identify a device. If the configured
device isn't present when recording starts, capture falls back to the default (with a warning)
rather than failing. Selecting a microphone takes effect on the **next recording, live** — no
restart needed — whenever voice input is already running.

While you're recording (and until the assistant has finished its spoken reply), `pause_media`
pauses any other media players so they don't talk over the conversation, then resumes whatever was
playing once things go quiet. It's best-effort — `playerctl` on Linux, `osascript` (Spotify/Music)
on macOS — and a silent no-op when neither is available, so it never blocks or breaks voice input.
On by default; set to `false` to leave your media alone. Unlike the microphone, changing it takes
a restart once voice input is running — the settings row says "(restart to apply)" until then.

Audio is captured at the microphone's native rate, downmixed to mono, and encoded as 16-bit PCM
WAV; the server resamples as needed. Recording isn't chunked yet — the whole utterance is uploaded
when you stop — so very long dictations wait until the end to transcribe.

### Dictation (Alt-t)

`Alt-v` sends what you said to the *conversation agent*. **`Alt-t`** sends it to *where you're
typing*: it records the microphone, transcribes it through the same `[stt]` engine, and types the
result into the attached pane — an agent's prompt, a shell command line, local session or remote —
or, outside a pane, into the open dialog's text field (see below). It's the same toggle shape as
`Alt-v` (press to start, press to stop), and either key stops a recording the other started,
because there is only one microphone.

What is fixed **when recording starts** is the *destination kind*: a recording begun with `Alt-t`
is typed into a pane even if `Alt-v` is the key that stops it, and vice versa. *Which* pane is
decided when the text is typed, because it is delivered through the attach stream itself — the same
channel your keystrokes travel — rather than a server route. So it follows the client: if
`Ctrl-Space` moves you to another session mid-recording, the transcript lands in the pane you are
looking at when it arrives, and dictating into a remote session needs nothing installed on the
server.

While you dictate, the attached client's tmux status line shows **● Dictating… (Alt-t to type)**
and keeps showing it until you press a key (the `Alt-t` that stops recording counts); it is then
replaced by **● Transcribing…**, which in turn is replaced by a brief **✓ Typed** as the text
lands — or **✗ Nothing heard** / **✗ Transcription failed: …** if it doesn't. `Alt-v` gets the
same held **● Recording…** notice, retired by *✓ Sent to <assistant>*. For a remote session these
notices are best-effort: they target your local tmux by the session's name.

It also works **outside a pane**, into any open dialog's text field: a review comment being
written, the new-session name, the conversation overlay's input, the quick-switch and branch
search boxes, a path prompt. The text is inserted at the cursor exactly as a paste would be, and
nothing is ever submitted there — `dictation_submit` applies to panes only, because a dialog's
Enter commits it. The status bar shows the same **● Dictating…** → **● Transcribing…** → **✓ Typed**
progression; the full-screen views that cover it show it themselves — the review view in its
footer beside the comment's save/cancel, the conversation overlay in its input row and the rule
beneath it. A masked field (a server's bearer token) is excluded: a secret is not something to read
aloud to a transcription server.

Pressed with neither — the bare session list — there is nowhere to type, so it says *"Open a text
field or attach to a session to dictate"* and records nothing. It also refuses to start in the
in-session switcher, whose search box sits over a live pane. A transcript is placed wherever is on
screen *when it arrives*: stop a recording after the dialog has closed (or after you've detached)
and it is dropped with the same message rather than typed into whatever comes next. A pane wins
while one is attached — but a recording *started* in a dialog is never submitted, even if it ends
up typed into a pane you attached to while it was transcribing: `dictation_submit` only applies to
dictation you started in the pane.

#### What gets typed

The transcript is normalised to a single line before it is typed: every newline becomes a space.
A pane is a terminal, so a literal newline is not whitespace — it is Enter, and it would submit a
half-finished sentence in the middle of dictation. Transcription engines return trailing newlines
routinely, so this runs on every transcript. An empty result (silence) types nothing at all.

By default nothing is submitted — the text sits in the composer and you press Enter yourself, after
reading it. Transcription mishears, and one keystroke is a cheap price for never running a command
nobody said. `dictation_submit` under `[stt]` trades that review step for hands-free operation:

| Value | Behaviour |
|-------|-----------|
| `never` (default) | Type the text and stop. You press Enter |
| `agent` | Also press Enter on an **agent** pane, where a wrong submit costs a turn. A shell pane stays insert-only |
| `always` | Press Enter on **any** pane, shell included — so a misheard sentence is a command that runs. Choose it deliberately |

When it does submit, the Enter follows the text after whatever per-harness delay that agent needs
to read the typed text as its own keystrokes first (Codex needs one; the others don't). Change the
policy from **Settings ▸ Voice ▸ Transcription ▸ Dictation Submit** and it applies to the next
recording, live — no restart.

#### The keys it shadows

Unlike `Alt-v`, which is only intercepted on agent panes, `Alt-t` is intercepted on **shell panes
too** — dictating a command line is half the point. The cost is that the pane never sees the key:

- In a shell, it shadows readline's `transpose-words` (`Alt-t` swaps the two words around the
  cursor). If you use that, rebind dictation.
- In a Claude Code pane, it shadows Claude Code's own `Alt+T`, *toggle extended thinking*
  (its [keyboard-shortcuts table](https://code.claude.com/docs/en/interactive-mode.md#keyboard-shortcuts);
  those docs note the shortcut has no effect on Fable models).

Rebind it like any other action — it's `toggle_dictation`, in the **Review & AI** group of the
keybindings tab:

```toml
[keybindings]
toggle_dictation = ["Alt-x"]
```

> **Not yet:** the `listen-toggle` desktop hotkey below still drives `Alt-v`'s recording only —
> there is no `listen-toggle --dictate` for starting a dictation from outside the terminal. Nor is
> the submit policy settable per session; it's one config value for the whole app.

### Global voice hotkey

`Alt-v` only fires when the terminal window is focused — a terminal app can't read key events
otherwise. To toggle voice input from **anywhere on the desktop**, bind a desktop-level global
shortcut to the `listen-toggle` command:

```sh
claude-commander listen-toggle          # toggle (also: --start / --stop)
```

This connects to the running TUI over a per-user Unix socket
(`$XDG_RUNTIME_DIR/claude-commander.sock`, falling back to the OS temp dir on macOS) and toggles
recording exactly as `Alt-v` would — and it works even while you're attached to a session. The
socket server starts automatically when the TUI launches with `stt.enabled = true`; the command
prints `recording`/`stopped` and exits non-zero (with a message) if no TUI is running.

- **KDE Plasma / Linux:** System Settings ▸ Shortcuts ▸ **Add ▸ Command or Script**, set the command
  to `claude-commander listen-toggle`, and assign your key. Pick a combo **different from `Alt-v`** so
  the global grab doesn't shadow the in-app binding.
- **macOS:** bind the same command via skhd, Karabiner-Elements, Raycast, or Shortcuts.app.

> A command shortcut is used (rather than the in-process XDG `GlobalShortcuts` portal) because the
> portal can't assign a non-sandboxed terminal binary a stable app identity, so the compositor never
> persists a reliably-bindable shortcut for it. The command route has no such limitation.

Both this and in-app `Alt-v` feed the same recording state, so they stay consistent. Only one running
TUI instance owns the socket; a second instance logs and skips.

## Theme Presets

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
| `lcars` | Star Trek: TNG console palette — black canvas with amber, lilac, periwinkle and salmon accents on tan text. The peer of the mobile/desktop client's LCARS theme |

The four named presets (`monokai-dimmed`, `zedokai`, `rose-pine`, `lcars`) are
24-bit palettes and need a **truecolor** terminal; on a 16- or 256-color terminal
their colors will be approximated by the terminal itself. Use `basic` or `indexed`
there instead.

Two things are specific to `lcars`. Its status bar is solid amber with black text
rather than a dark band, and since `status_bar_bg` also drives the tmux status line
(`status-style`), that amber bar appears in attached sessions too — override
`status_bar_bg` / `status_bar_fg` if you would rather it stayed dark. Its Working
spinner is a solid amber instead of the cycling rainbow every other preset uses;
set `agent_working = "rainbow"` to get the rainbow back.

When `preset` is unset (or `"(auto)"`), the theme auto-detects your terminal's color capability.

### Status bar accent

The highlighted hotkey letter in `[n]ew session` and the board's top-bar title are
drawn **on** the status bar, so they get their own colour, `status_bar_accent`,
rather than the canvas-tuned `text_accent`:

```toml
[theme]
status_bar_accent = "#2e2e5c"
```

Every preset whose bar is dark sets this to the same value it already used, so the
appearance is unchanged. It exists because two presets could not: `lcars` has a
light amber bar where a lilac letter is barely legible, and `basic` drew a blue
letter on its own blue bar. Editable in-app from **Settings ▸ Theme ▸ Status Bar
Accent** (`,` key).

Individual color overrides (e.g. `border_focused = "#ff6600"`) still apply on top of the chosen preset.

In **Settings ▸ Theme** (`,` key) every colour row opens a swatch picker: the
current theme's colours in a grid (arrows or `h`/`j`/`k`/`l`, `Enter` to pick), plus
a hex row that takes a typed or pasted `#rrggbb` (`Tab` or `#` to reach it). The
first cell, **Inherit**, clears the row's own value so it falls back to the preset
again. A row with no value of its own shows the inherited one dim, marked
`(preset)`. The picker writes `#rrggbb`; a named (`"red"`) or indexed (`117`) value
still works when written in `config.toml` by hand.

### Light terminals

Every preset above is designed for a **dark** terminal background. On a light one, declare it:

```toml
[theme]
appearance = "light"   # "dark" (default) | "light"
```

This does not swap the palette — it changes the surface that *derived fills* are blended against.
The review diff view (`R`) builds its add/remove line bands by scaling the theme's diff colours
toward the background; scaling toward black on a light terminal produces a near-black band under
dark text. With `appearance = "light"` those bands blend toward white instead, so they read as a
pale green/red wash.

Nothing detects this for you — querying the terminal background (`OSC 11`) is deliberately out of
scope — so it is a claim you make about your own terminal. Leaving it unset keeps whatever the
preset declares, which is `dark` for all of them. Editable in-app from **Settings ▸ Theme ▸
Appearance** (`,` key); clear the field to fall back to the preset.

## Workspaces

A workspace is a **label on a project**. Every workspace shares one state file,
one server and one set of background loops; switching workspace only changes
which projects and sessions a frontend shows. A project with no label is in the
built-in **Main** workspace, which can be relabelled (`[main_workspace]`) but not
deleted. Workspace UI stays hidden until a second workspace exists.

- Definitions live in `config.toml` as `[[workspaces]]` (`name`); a project's workspace is stored with the project in
  `state.json`. Names are trimmed, at most 40 characters, contain no control
  characters, and may not be `last` or `main` (any case) — those are the
  non-name values of `startup_workspace`.
- **Renaming** a workspace rewrites every project tagged with it (and a
  `startup_workspace` pinned to it); **deleting** one moves its projects back to
  Main. Both go through the app (or the server's API) rather than a hand edit of
  `config.toml`, because a hand-edited rename would strand the projects under
  the old name. A project whose workspace has no definition still shows up —
  under a workspace of that name — and moving a project into an undefined
  workspace defines it.
- With remote servers, definitions **merge by name**: the local server's order
  (the TUI) or the first server's order (the Flutter app) comes first, and names
  only another server defines are appended. Main merges by being untagged, never
  by its label. Each server stores its own projects' labels and definitions;
  creating, renaming, deleting and reordering is sent to every connected server.
  Merging is exact, but a server refuses two names that differ only in case (or
  a name equal to its Main label), so if two servers ended up with "Work" and
  "work" each is sent the list with its own spelling kept and the other's
  dropped — the disagreement never blocks an edit. Rename one to reconcile.
- An older binary that rewrites `state.json` or `config.toml` drops the
  workspace fields it does not know (those projects fall back to Main). From
  this version on, unknown project fields in `state.json` are preserved.

In the TUI:

- `w` (`next_workspace`) cycles workspaces, wrapping; `W` (`workspace_picker`)
  opens a picker listing each workspace with its count of sessions waiting for
  input, where typing a name that doesn't exist creates it. **Previous
  workspace**, **New workspace…** and **Move project to workspace…** are
  palette-only until you bind them under `[keybindings]`.
- The active workspace scopes the list views, the board and its project
  sidebar, the Recent block and the status-bar counts. Switching lands on the
  first row and clears a board project filter. The palette and the in-session
  `Ctrl-Space` switcher search every workspace — the active one's sessions
  first, the rest tagged — and picking a session elsewhere switches there first.
- The status bar shows a chip naming the workspace and `Label ●N` hints
  for other workspaces with sessions waiting, each in its workspace's theme
  accent (see [Workspace themes](#workspace-themes)); the board header and an attached
  session's tmux status line carry the name. All of it is hidden while there is
  only one workspace.
- New projects, clones and directory scans go into the active workspace (in
  the Flutter app too — `POST /projects/scan` takes the same optional
  `workspace` as `POST /projects`).
- The active workspace is per client, remembered in `tui.json`
  (`last_workspace`) and applied at startup when `startup_workspace = "last"`.
- **Settings → Workspaces** edits everything above: `n` new, `r` rename
  (Main's label included), `d` delete (not Main), `J`/`K` reorder, `s` cycle
  the startup workspace, and `→`/`Enter` into a workspace's details, where
  `Enter` on **Theme** edits its theme and `m` moves a listed project. Every change is sent to each connected server at once; a server that
  refuses or can't be reached is named in a status message, and the rest
  still apply it. `startup_workspace` is saved to the local config only, and
  changing it leaves the local definitions as they are (a pinned workspace only
  another server defines is added locally, since the pin needs a definition).

From the CLI:

```sh
claude-commander list --workspace work        # only sessions in "Work"
claude-commander list --workspace main        # only Main (or use Main's label)
claude-commander new fix-login -d ~/src/app --workspace work
```

`list` adds a `[workspace]` column to each project line once there is more than
one workspace; `list --json` always carries a `workspace` field (`null` for
Main).
`new --workspace` requires `--path`: it applies only when that path registers a
new project — an existing project keeps its workspace — and an unknown name
creates the workspace.

### Workspace themes

Each workspace can have its own TUI theme in the local `config.toml`, under
`[workspace_themes."<name>"]`. The table takes the same keys as `[theme]`:

- no table for a workspace → it uses your usual `[theme]`;
- a table without `preset` → the usual theme, with the table's colours on top;
- a table with `preset` → that preset plus only the table's own colours (the
  usual `[theme]` overrides do not carry over).

The built-in Main workspace has no name of its own (its label can change), so
its theme is always keyed **`main`** — `[workspace_themes.main]`. No user
workspace can be called `main` (in any case), so the key cannot collide.

Switching workspace re-themes the whole TUI at once, and a config hot reload
rebuilds the active workspace's theme. The status-bar chip naming the active
workspace is drawn in its theme's `text_accent` (with black or white text,
whichever reads), and each `Label ●N` hint for another workspace in *that*
workspace's `text_accent`, as are the swatches in **Settings ▸ Workspaces**. While
there is only one workspace the usual `[theme]` is the one worn and edited, and a
leftover `[workspace_themes.main]` is ignored until a second workspace exists.

To edit one in the app, open **Settings ▸ Theme**. Once there are two or more
workspaces its first row, **Theme for**, picks what the tab edits: a workspace
(the active one by default) or **Usual theme (all workspaces)**, i.e. `[theme]`.
In a workspace's scope:

- **Reset to usual theme** removes the workspace's table;
- **Preset** is `(usual)` to layer over the usual theme, or a preset to start a
  new base from;
- every colour row opens the swatch picker, whose **Inherit** cell clears the
  workspace's own value. Inherited rows are shown dim, marked `(usual)` when
  they come from the usual theme and `(preset)` when the workspace has a preset
  of its own. A table left with nothing in it is removed.

**Settings ▸ Workspaces** shows each workspace's **Theme** as `usual` or
`customised`; `Enter` on it opens the Theme tab scoped to that workspace.

Themes are local to the machine running the TUI: they are not sent to servers,
a server's `GET /config` leaves them out, and they cannot be set through the
server's config API. Renaming a workspace in the app moves its table to the new
name (replacing any leftover table already under that name, which the renamed
workspace now owns) and deleting one removes it — including a workspace only a
remote server defines. A hand edit of a workspace's name in
`config.toml` does not, so rename through the app.

## Session List Sections

Sections are the **columns** of the [board](../README.md#board). Each configured section becomes one column, and a session's card lands in the first column whose predicate it matches.

Sections drive both the **board** columns and the section-grouped **list** views
(`v` cycles project list → Sections → Section Stacks → board). When `[[sections]]`
is empty, the board shows three baked-in default columns assigned automatically
from GitHub PR state — **In Progress** (the catch-all), **In Review** (open PR),
and **Merged** (merged PR) — and the list stays project-grouped. Declaring any
`[[sections]]` **replaces** the board defaults with your own columns and makes the
section list views meaningful; nothing is written back to `config.toml`.

An implicit **"In Progress"** section is always the first column / group and acts
as the catch-all — any session whose PR state doesn't match a later section's
predicate lands here. It also accounts for every project that hasn't placed a
session into a later column, so newly added projects stay visible.

The two section list views differ in how they treat stacks. The **Section
Stacks** layout (the default once sections are configured) keeps each
[PR stack](usage.md#pr-stacks) together as a unit under the section chosen by its
base (the stack root), with children nested under their base — so a draft session
stacked on top never drags the whole stack out of the base's section. The plain
**Sections** layout drops that nesting — ordering follows the section rules, so a
base and its child may land in different sections depending on their PR state. On
the board a stack is a run of contiguous cards that moves between columns as a
unit, its base and children always sharing one column. In every view the
underlying stack links are still tracked and the `t` hotkey still stacks new
sessions onto the top.

### Example

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

Each section is a column on the board and the leftmost column is the project
sidebar. Each session is its own card, titled with its number and name and
coloured by project; the interior line carries the status glyph and the
`[>_] [±] [i]` action buttons.

```
 Projects    │ In Progress (12)     │ Needs Review (1)     │ In Review (2)
 terraform 4 │ ╭ 1 session-a ─────╮ │ ╭ 5 fix-dns-spam ──╮ │ ╭ 6 new-metrics ───╮
 genio     3 │ │ ●      [>_][±][i]│ │ │ ●      [>_][±][i]│ │ │ ●      [>_][±][i]│
             │ ╰──────────────────╯ │ ╰──────────────────╯ │ ╰──────────────────╯
             │ ╭ 2 session-b ─────╮ │                      │ ╭ 7 add-ela… ──────╮
             │ │ ●      [>_][±][i]│ │                      │ │ ●      [>_][±][i]│
             │ ╰──────────────────╯ │                      │ ╰──────────────────╯
```

### Predicate fields

All fields are optional; a section matches when **every declared field** matches (AND). A section declared with no predicates is a **manual-only waypoint** — auto-matching never puts sessions there; you only move in via the palette.

| Field | Type | Notes |
|---|---|---|
| `pr_state` | `"open"` \| `"closed"` \| `"merged"` — scalar or array (any-of) | |
| `is_draft` | `bool` | |
| `has_pr` | `bool` | |
| `has_label` | string (literal) or array (any-of) | |
| `review_decision` | `"approved"` \| `"changes_requested"` \| `"review_required"` — scalar or array (any-of) | Mirrors GitHub's `reviewDecision` field |
| `has_reviewer` | `true` / `false`, a specific login, or an array of logins (any-of) | `true` excludes Copilot via case-insensitive `"copilot"` substring match; specific/array forms match literally |
| `max_sessions` | positive integer | Advisory WIP limit. Section header shows `count/limit`, warning-coloured at the limit and error-coloured over it. Never blocks creation. |

### Process order and forward-only

Config order is the pipeline. A session's section is re-evaluated on every PR refresh, but the scan **only considers sections at or after the session's current position** — auto never moves a session backwards. This keeps `"Needs Review"` sticky when a reviewer removes the label without leaving other signals; the session doesn't slide back to `"In Progress"`.

### Moving sessions manually

Select a session and press `m` (or open the palette with `Space`, or `Shift+Space` for commands-only, and run **Move session to section…**), then pick a target. An **Auto** entry clears an existing pin. The override is persisted to `state.json` and survives restarts; auto-moves are suppressed until the pin is released.

### Creating sessions inside a section

On the board, a session created with `n` lands in the column the cursor was in, not the "In Progress" catch-all. For a manual-only waypoint (no predicates) this sets the same pin as a manual move; for a predicate-bearing section it's a soft placement — the session starts there but still auto-advances through the pipeline as its PR progresses. Creating from "In Progress" keeps the default behaviour. The CLI's `claude-commander new --section` flag follows the same rules.

### WIP limits

Set `max_sessions = N` on any section to flag it when it accumulates too much work. The header renders `count/N`, switching to the warning colour when `count == N` and to the error colour when `count > N`. The catch-all "In Progress" section uses the top-level `in_progress_limit` instead:

```toml
in_progress_limit = 3

[[sections]]
name = "In Review"
pr_state = "open"
has_reviewer = true
max_sessions = 5
```

Limits are advisory — they never block session creation or section transitions. Sessions still flow through the pipeline as their PRs progress.

### Reordering, adding, or removing sections

These are edit-`config.toml`-and-restart actions — there's no hot reload. The cached `current_section` on each session is reconciled against the new config on next startup; if the referenced section no longer exists, the session falls back to `"In Progress"` and continues forward-only from there.

## Usage Telemetry

Claude Commander reports anonymous **feature-usage** telemetry so we can see which
features are used and retire the ones that aren't. It is **on by default** and
**opt-out**.

**What is sent:** the name of each feature you use (e.g. `review.open`,
`session.create`), a coarse environment fingerprint (OS, architecture, terminal
program, shell *name*, terminal colour mode), a non-sensitive config snapshot
(theme preset, whether custom sections are configured, which optional features
are enabled), the frontend name + version, the library version, and a random,
resettable install id.

**What is never sent:** typed text, prompts, Claude session content, comment
bodies, branch/session names, repository paths, command arguments, or arbitrary
environment variables. The event schema is a fixed set of typed fields — there
is no code path that forwards free-form text.

**To disable**, either set the config flag:

```toml
[telemetry]
enabled = false

# Self-hosters can point telemetry at their own OpenObserve instead:
# endpoint = "https://o2.example.com/api/<org>/<stream>/_json"
# token = "<base64 of \"email:token\">"   # HTTP Basic credential
```

…or export the standard [`DO_NOT_TRACK`](https://consoledonottrack.com/) variable
(any non-empty, non-`0` value), which disables it regardless of config:

```sh
export DO_NOT_TRACK=1
```

The ingest credential is committed in the source tree, so **all** builds —
including ones compiled from source — report by default. Distro packagers who
want telemetry compiled out entirely can build with an empty token:

```sh
CC_TELEMETRY_TOKEN="" cargo build --release
```

## Data Storage

Paths are platform-specific, determined by the `directories` crate:

| File | macOS | Linux |
|------|-------|-------|
| Config | `~/Library/Application Support/com.claude-commander.claude-commander/config.toml` | `~/.config/claude-commander/config.toml` |
| State | `~/Library/Application Support/com.claude-commander.claude-commander/state.json` | `~/.local/share/claude-commander/state.json` |
| Worktrees | `~/Library/Application Support/com.claude-commander.claude-commander/worktrees/` | `~/.local/share/claude-commander/worktrees/` |
