# Usage

Deep usage guide for Claude Commander. For installation and the keyboard /
status-symbol reference, see the [README](../README.md). For configuration
options, see [Configuration](configuration.md).

## Interactive TUI (default)

```bash
claude-commander
```

## Commands

```bash
# List active sessions (add --all to include stopped, --json for machine-readable output)
claude-commander list

# Create a new session
claude-commander new "feature-auth" --path /path/to/repo

# Create a session with an initial prompt (starts working immediately)
claude-commander new "fix-auth" --initial-prompt "Fix the auth bypass in login.rs" --effort high

# Create a session in plan mode, forking from a specific branch
claude-commander new "feature-api" --base-branch develop --mode plan

# Create a session pinned to a specific model (Claude or Codex)
claude-commander new "feature-review" --model opus

# Create a session placed directly in a specific section (pinned for
# manual-only sections, soft placement for predicate-bearing ones — see
# "Creating sessions inside a section" in configuration.md)
claude-commander new "feature-ui" --section "Needs Review"

# Attach to a session
claude-commander attach feature-auth

# Create and attach to a session on a configured remote server (by name from
# [[remote_servers]]). Use --project to pick an existing server-side project by
# name (no need to know its path); use --path to seed a brand-new project (the
# path is resolved on the server, not this machine).
claude-commander new "feature-remote" --remote workstation --project genio
claude-commander new "new-repo" --remote workstation --path /repos/app
claude-commander attach feature-remote --remote workstation

# Dump recent terminal output from a session (default 100 lines, max 10000)
claude-commander log feature-auth --lines 200

# Inspect a session's review diff and mark files reviewed (see "Marking files
# reviewed from the CLI" below)
claude-commander review files feature-auth --unreviewed
claude-commander review diff feature-auth src/auth.rs
claude-commander review mark feature-auth src/auth.rs src/lib.rs

# Show configuration
claude-commander config

# Initialize config file
claude-commander config --init

# Use a custom config file
claude-commander --config /path/to/config.toml
```

## Views and the Board

The session list has four views, cycled with `v`: three **list** views (grouped by project, by section, or by section with PR stacks) and the full-screen kanban **board** (see [Views](../README.md#views)). The project list is the default; `v` rotates project → sections → stacks → board → project, skipping the section views when no `[[sections]]` are configured, and the chosen view is remembered across restarts.

The three list views pair the list with a **right-hand pane**, cycled with `Tab` (or `Shift-Tab` to go back) through three tabs: **Preview** (a live tail of the agent's pane), **Info** (the selected session's metadata, diffstat, PR details and stack chain — the same content the `i` modal shows, including `g` to generate an AI summary), and **Shell** (a live tail of its shell). `<` / `>` move the divider, and the width is remembered across restarts. A project row has no agent pane, so it cycles Shell ↔ Info and its Info tab describes the project — path, main branch, and any reason its background pull is blocked.

The pane is passive — keys always drive the list — so the two live captures render dimmed by default (`dim_unfocused_preview`); Info is left at full brightness, being static text rather than a tail. The mouse wheel over the pane scrolls its content rather than moving the selection. On a capture tab, scrolling away from the bottom stops the auto-follow and wheeling back to the bottom resumes it; Info stays anchored where you leave it.

The board is a full-screen takeover with no side pane, so `Tab` and `<`/`>` do nothing there — `i` is the only way to reach a session's Info there. In every view, `i` opens the Info modal, `Enter`/`s` a session's shell, and `r` its review diff.

The board is described below. Each column is a section, and every session is its own card bordered in its project's colour. The leftmost column is a project sidebar listing every project (sorted alphabetically) with its session count.

Within a column, cards are ordered by how likely each session is to need you: a **needs-you** band on top (waiting for input, a paused cascade, or unread output), an **active** band in the middle (working, idle, or a transient create/merge/push), and **stopped** sessions at the bottom. Within a band, newer sessions float above older ones, so recent work is easy to find. The banding is coarse on purpose — a session cycling between working and idle stays in the active band rather than jumping around. A PR stack stays contiguous and sorts by its most-attention-needing member. Empty columns are hidden by default (`hide_empty_sections` in [Configuration](configuration.md)); set it to false to always show every section column.

A card's border title is the session's number and title — the project name is never rendered on the card (project identity lives in the border colour and the sidebar legend). The card's single interior line carries the status glyph and a word describing it (`working…`, `waiting`, `idle`, …), any row markers (`*` pending comments, `⚓` keep-alive, `⇣ LFS`), the PR pill or, in `[brackets]`, the branch name, and — right aligned — three clickable action buttons: `[>_]` opens the session shell, `[±]` opens the review diff, and `[i]` opens the info panel. A single click on a button selects that card and fires the action; clicking elsewhere on a card selects it and double-clicking attaches.

**Selecting** a project in the sidebar (`Enter`, or double-click) **filters** the board to only that project's cards; the sidebar keeps listing every project, and the top bar names the active filter. Selecting the same project again — or **`Esc`** — clears the filter; selecting a different project refilters; moving into the columns keeps the active filter so you can browse and act on that project's cards. Merely moving the cursor over the sidebar does not filter. Jumping to a session through the quick-switch palette clears the filter when needed, so a jump to a session in another project always lands.

The branch name in `[brackets]` appears only when the branch differs from what the title would sanitize to. A session titled "Feature Auth" with branch `feature-auth` (or `prefix/feature-auth` when `branch_prefix` is set) shows no bracket; it reappears only when the branch carries new information, e.g. you renamed it to `feature-auth-v2` outside the app.

### PR Stacks

When a session's PR targets another session's branch (rather than `main`), the two form a stack. Each stack member is its own card, rendered contiguously in stack order: the base card first, with stacked children drawn as their own cards nested (indented) one level deeper beneath it in bottom-to-top stack order.

Press `t` on any session in a stack to create a new session on top of that stack — regardless of which member you have selected, the new branch is forked from the topmost session. When you launch Claude in the new session it is told to use `gh pr create --base <parent-branch>` so the PR targets the right place automatically.

Stacks are detected from the PR's `baseRefName` returned by the `gh` CLI, so they stay accurate across GitHub's auto-retargeting when a stack member is merged.

How a stack renders depends on the view (cycle with `v`). In the **project-grouped** and **Section Stacks** list views (the latter is the default when [Session List Sections](configuration.md#session-list-sections) is configured), children are nested under their base and the whole stack stays together under the section chosen by its base (the stack root), so a draft or early-stage session stacked on top never drags the whole stack out of the base's section. In the plain **Sections** list view, sessions are ordered by their section instead and stacked children render at the normal indent — the `t` hotkey and `stack_parent_session_id` still work, but a base and its child may land in different sections depending on their PR state. On the **board** a stack occupies exactly one column and moves between columns as a unit; the column is chosen by the stack's newest leaf (its section assignment, including any manual `m` move), so a base and its children never split across columns.

#### Fixing a stack: Set session base

When a stack is wired up wrongly — a session stacked on the wrong parent, or stacked when it should sit on `main` — **Set session base** from the command palette re-points it. Pick another session in the same project to stack onto, or the project's main branch to unstack entirely. Sessions stacked on *this* one come along automatically: they are stacked on its branch, which does not move.

The command updates the stack link and, when the session has an open PR, retargets that PR with `gh pr edit --base`. If the `gh` edit fails you are told so explicitly, because the base on GitHub is the source of truth — the next PR sync would otherwise quietly restore the old base.

**It does not rewrite git history.** The branch still contains its old base's commits, so until you rebase or merge onto the new base yourself, both the PR and the review diff will show them. Retargeting onto a *sibling* rather than an ancestor is the case to watch: the merge base falls back towards `main` and the diff grows to include the old parent's work as well as your own.

A retarget is refused when it would create a cycle (basing a session on one of its own descendants), when the session's PR is already merged or closed (PR status is polled with `--state all`, so a settled PR keeps re-asserting its own base and the change would be undone at the next sync), and while a cascade is paused for that project (resume or abandon it first — `Cascade resume` re-derives the stack from the current topology).

#### Cascade merge main through a stack

When `main` moves forward, use **Cascade merge main** from the command palette to propagate it up the stack in one step: the command merges `main` into the stack base, then the base into its child, and so on to the leaf. Running it from any session in a stack works — the cascade always starts from the base.

Before touching any worktree, the cascade fetches `origin`, verifies no live agent in the stack is `Working` or `WaitingForInput` (racing a `git merge` against Claude writing files is unrecoverable), and refuses if any worktree has uncommitted changes. Each session shows a `⟳` spinner while its step is running.

On the first conflict the cascade pauses: the affected session gets a persistent `⏸` glyph (survives a restart of the TUI), and the worktree is left in the usual `git merge` in-progress state so you can resolve it however you like — typically by attaching to the session and asking the live Claude. Once you've committed the resolved merge, **Cascade resume** from the palette picks up where it stopped and propagates the new commit on up the chain. **Cascade abandon** clears the pause without continuing, if you decide to back out.

#### Push stack

**Push stack** (palette) runs `git push -u origin <branch>` across every session in the stack, base first then each child up the chain — pushing the base before its children keeps GitHub's PR base refs consistent. Each session shows the spinner glyph while its own push is in flight.

Pre-flight is the same as cascade merge: no live agent may be `Working` or `WaitingForInput`, and worktrees must have no uncommitted changes. On the first `git push` failure (rejection, auth, non-fast-forward, etc.) the chain stops and the toast shows git's stderr — no "resume" command is needed since `git push` is idempotent, so fix the root cause and re-run **Push stack** to continue.

## AI Summary

The Info modal (`i`) can display an AI-generated summary of branch changes,
powered by the Claude CLI. Press `g` (configurable) while the Info modal is
open to generate a summary. Summaries are cached per-session — once generated, they
display instantly when you revisit the session. Press `g` again to regenerate
after making changes.

Requires the `claude` CLI to be installed and authenticated. If unavailable,
the summary section shows a placeholder instead.

| Setting | Default | Description |
|---------|---------|-------------|
| `ai_summary_enabled` | `true` | Set to `false` to disable AI summaries entirely |
| `ai_summary_model` | `claude-haiku-4-5-20251001` | Claude model used for summaries (Haiku recommended for cost efficiency) |

## Reviewing & commenting on changes

Press **`r`** on a selected session (or open the **command palette** with
`Space` and run **"Review diff & comment"**) to open a full-screen review view. It shows everything a PR
against the session's base branch would contain — committed, staged, unstaged,
and untracked changes — composed from `merge-base(base, HEAD)` through the
working tree.

While attached to a Claude session you can jump straight to its review diff
with **`Alt-r`**, and **`Alt-r`** again from inside the diff switches back to
the session — the same direct toggle the shell session has with `Ctrl-\`, without
detaching to the board. `Alt-r` is used rather than `Ctrl-r` so a
shell's `Ctrl-r` reverse-history-search is never shadowed; the toggle is wired
up for Claude sessions only.

### Marking files reviewed from the CLI

`claude-commander review` exposes the review view's per-file "reviewed" marks
(`m` in the view) so a script or an agent can triage a diff for you — e.g.
"mark every file whose only change is imports as reviewed":

```bash
claude-commander review files <session> [--unreviewed] [--json]  # files, +/- counts, reviewed state, hash
claude-commander review diff  <session> [FILE…] [--unreviewed]   # unified diff, exactly what the view shows; FILE may be PATH@HASH
claude-commander review mark   <session> FILE… | --all [--json]  # FILE may be PATH@HASH
claude-commander review unmark <session> FILE… | --all [--json]
```

Files are named by the path `review files` prints (the old path for a deleted
file). `mark`/`unmark` are idempotent and all-or-nothing: a path that is not in
the diff fails the command without marking anything. Giving a file as
`PATH@HASH`, with the hash from `review files --json`, also refuses the mark
if the file has changed since — so a script can't pin a mark to content it
never read while the session's agent is still editing. A refusal lists every
bad file at once, and `diff` accepts the same `PATH@HASH` form. As with `m`, a
mark is pinned to the file's current diff and clears itself when that file
changes again. An open TUI review view picks up marks changed this way within
about a second for a local session (about five for a remote one), collapsing
any directory they finish; the web and mobile review views show them on their
next refresh.
`files` and `diff` are read-only. The command works on local sessions; for a
session on a remote server, run it on that server.

### Pasting images into a remote session

Claude Code accepts a pasted image on `Ctrl-V` by reading the clipboard of the
machine it runs on. For a **local** session that's your machine, so it just
works and claude-commander forwards `Ctrl-V` untouched. For a session running on
a **remote** `claude-commander-server`, the agent is on the server and can't see
your clipboard — so while attached to a remote session, claude-commander
intercepts `Ctrl-V`, reads the image from *your local* clipboard, uploads it to
the server, saves it to a short-lived temp file there, and types that file's
path into the Claude prompt (the same plain-text path form Claude accepts). Add
your prompt text and submit as usual. If the clipboard holds no image (or the
build was compiled without the `clipboard` feature), `Ctrl-V` is forwarded
unchanged. Uploads are validated as real images and capped at 10 MiB; the server
prunes the temp files automatically. (A few terminals bind `Ctrl-V` to their own
paste and never deliver it to the app — and under an enhanced keyboard protocol
`Ctrl-V` may arrive as an escape sequence rather than the usual control byte; in
both cases it is forwarded rather than intercepted, matching local behaviour.)

The diff is rendered in a `lumen`/`hunk`-style colour scheme: dark green/red
line fills, a brighter highlight on the changed span within a line, and (on
true-color terminals) syntax highlighting of the code. It degrades to
foreground-only colouring on 256- and 16-colour terminals.

- **Navigate**: the changed files are shown as a collapsible **tree** (lazygit
  style — single-child directory chains are compressed). `Tab` switches focus
  between the tree and the diff body; in the tree, `↑↓`/`jk`/`Ctrl-n`/`Ctrl-p`
  move and `Enter` expands/collapses a directory; `[` / `]` jump between files;
  in the body, `↑↓`/`jk`/`Ctrl-n`/`Ctrl-p` move the cursor and the mouse wheel
  scrolls. Clicking a file in the tree selects it (clicking a directory
  expands/collapses it). `t` toggles
  between inline and side-by-side layouts. In the inline layout, lines longer
  than the pane soft-wrap onto continuation rows (GitHub-style) so nothing runs
  off-screen; side-by-side truncates to keep its columns aligned.
- **Expand context**: like GitHub, you can reveal more of the surrounding file
  around a hunk. Each collapsed gap (before, between, and after hunks) shows a
  clickable control — click `⌄`/`⌃` to reveal a chunk above/below, or `↕ all`
  to reveal the whole gap. From the keyboard, `{` expands context above the
  hunk under the cursor and `}` below it. Revealed lines are shown for context
  only — they aren't selectable or commentable. Expansion resets when the diff
  is refreshed.
- **Mark reviewed**: press `m` to toggle a ✓ reviewed mark on the current
  file. Marking dims the file in the tree and auto-advances to the next
  unreviewed file (wrapping); unmarking stays put. Marks persist per session
  across restarts and clear automatically when a file's diff changes or it
  leaves the diff (GitHub "Viewed" semantics), so a re-edited file demands a
  fresh look. A directory whose files are all reviewed folds away — when you
  mark its last file, when marks set elsewhere (the `review` CLI) arrive, when
  the view opens, and when a refresh finishes it. A finished directory you
  expand again stays open across refreshes. The tree title shows progress
  (`Files (3/7 reviewed)`).
- **Binary & image files**: image files (PNG, JPEG, GIF, WebP, BMP, …) render
  inline in the diff body using the terminal's graphics protocol (Kitty,
  iTerm2, or Sixel), falling back to Unicode half-blocks on terminals without
  one. For a modified image, `o` toggles between the **before** and **after**
  versions; added and deleted images show their only side. Image bytes are
  loaded lazily when the file scrolls into view. Other (non-image) binaries
  show a size/placeholder line instead of a textual diff.
- **Comment**: in the body, press `v` to start a line selection (arrows grow
  or shrink it; mouse drag also selects), then `Enter` to attach a comment.
  Right-clicking or double-clicking a line is a mouse shortcut for the same
  thing — it selects that line and opens the comment box directly.
  Comments are *staged* — they persist across restarts until applied, and
  show as `*` in the gutter and a coloured `*N` count on the file (and its
  parent directories) in the tree. A session with pending comments is also
  flagged with a `*` on the board. Each comment also renders as
  an inline box beneath its line; press `z` (or click the box) to fold it
  down to a single-line header or expand it again.
- **Apply**: press `a` to hand all staged comments to the session's agent.
  They're written to a markdown brief and the agent is prompted to address
  them — sent immediately when idle/working (it queues natively), held until a
  permission prompt clears, or deferred if the agent is stopped.
- **Refresh**: the diff is a snapshot taken when you opened the view, so the
  agent's edits (e.g. after applying comments) don't appear until it's
  re-composed. This happens automatically when the session's agent goes idle —
  i.e. finishes a turn — folding its changes in while keeping your place (same
  file, clamped cursor/scroll). Press `r` to re-compose on demand at any time;
  it reports "Review refreshed" or "Review already up to date".
- **Switch session**: press `Ctrl-Space` (or the leader key, `Space` by
  default) to open the session switcher over the diff — the same palette as
  everywhere else, filtered to sessions, since every command in it would have
  to close the review to run. Picking a session attaches to it, exactly as
  picking one from the board does; `Esc` closes the switcher and puts the diff
  back where you left it. The leader key opens the switcher only where the
  review has no meaning of its own for that key, so rebinding it to (say) `r`
  leaves `r` refreshing the diff. That is per-mode: a leader on a key the
  review binds in one place only — `v`, which selects in the diff body — still
  opens the switcher from the file list, where `v` does nothing. `Ctrl-Space`
  works anywhere in the view except the comment box, where every key is text.
- **Open in editor**: press `.` (the same open-in-editor binding as the session
  list) to open the session's worktree in your configured editor without leaving
  the review — a GUI editor launches alongside the TUI, a terminal editor takes
  over until you exit. Unavailable for remote sessions, whose worktrees live on
  the server.
- **Drift**: if the code under a comment changes before you apply, the view
  re-anchors it by its captured snippet. If it can't be located unambiguously
  the comment is marked `⚠` (drifted) and blocks apply until you review or
  delete it (`d` removes the comment under the cursor).
- **Reverted files**: if a file leaves the diff altogether — the change you
  commented on was reverted, so there is nothing left to review — its pending
  comments are dropped rather than left blocking apply from a file the view can
  no longer show you. The status bar reports what was dropped. Comments you have
  already applied are kept regardless.

Comments are stored per session under the data directory (alongside
`state.json`); the brief handed to the agent is written to a temp file outside
the worktree, so it's never committed.

By default each file's render caches (word-diff segments + syntax highlighting)
are built up front behind a brief loading spinner when you open the review, so
file switching is instant afterwards. Set `precompute_review_caches = false`
(see [Configuration](configuration.md)) for lazy behaviour instead — the view
opens instantly and each file's cache is built the first time you navigate to
it, at the cost of a brief jank on the first scroll through a large file.
