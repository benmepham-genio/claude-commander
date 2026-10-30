//! Agent harness abstraction.
//!
//! Claude Commander launches different agent CLIs (Claude Code, OpenAI Codex,
//! OpenCode, Oh My Pi, Muse Code) inside tmux sessions. Each harness differs in how it is
//! resumed, whether it accepts a positional prompt, and what it renders in the
//! tmux pane while working or waiting for the user.
//!
//! [`AgentKind`] is *derived* from the persisted `program` command string (never
//! stored separately) and owns this per-harness behaviour, so the divergences
//! live in one place. Adding a new harness is a new enum variant plus filling in
//! its methods — the compiler then flags every behaviour left unimplemented.

use std::ffi::OsStr;
use std::path::Path;
use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;

use crate::session::AgentState;

/// Pre-compiled regex for stripping ANSI escape sequences (CSI sequences and
/// OSC strings terminated by BEL or ST).
static ANSI_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\x1B\[[0-9;]*[a-zA-Z]|\x1B\][^\x07]*\x07|\x1B\][^\x1B]*\x1B\\")
        .expect("valid regex")
});

/// Claude Code's working status line can carry an elapsed-time and token-count
/// parenthetical after its ellipsis — `✽ Boondoggling… (5m 35s · ↓ 18.6k tokens ·
/// still thinking with high effort)`, `· Clauding… (4s · ↓ 4 tokens)`. Only a
/// turn that is actually in flight renders a *live* counter, so this is a safe
/// `Working` signal for the content fallback when the title check comes up empty
/// (a user can disable the terminal title, and Claude has changed its spinner
/// glyphs before now — see [`has_circle_spinner`]).
///
/// This fallback is deliberately **partial**, and the title check is the primary
/// signal. Two things constrain it:
///
/// - The parenthetical is not always present. On a long-running turn it is:
///   sampled 200× at 50ms against a live working pane, all 200 samples carried
///   it. On a short turn the line can be a bare `✽ Bunning…` with no
///   parenthetical at all (observed across 40 snapshots of one ~16s turn), which
///   this regex does not match. That is the accepted gap — see
///   `claude_content_bare_gerund_line_is_not_working`.
/// - Matching the bare `<glyph> <Gerund>…` line instead would be unsafe. Claude
///   renders tool lines in the same shape (`● Listing 1 directory… (ctrl+o to
///   expand)`) and they *persist in the transcript after completing*, so an idle
///   pane would read `Working` forever. Requiring a live duration *and* token
///   count is what rules those out; a finished turn's line reads
///   `✻ Cogitated for 15s`, with neither.
///
/// Receipt: every example line above captured verbatim from live panes
/// (claude-code 2.1.228). This version renders no `esc to interrupt` hint, so
/// unlike Codex there is no static string to match on instead.
static CLAUDE_WORKING_STATUS_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"…\s*\(\d+(?:\.\d+)?(?:ms|s|m|h)\b[^)\n]*\btokens\b").expect("valid regex")
});

/// OpenCode renders completed assistant turns as an agent/model/duration line,
/// e.g. `▣ Build · GPT-5.5 · 8.5s`. Active turns have the same agent/model
/// prefix but no duration; the active signal is the separate `esc interrupt`
/// footer below.
static OPENCODE_COMPLETED_TURN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?m)^\s*▣\s+.+\s·\s+\d+(?:\.\d+)?(?:ms|s|m|h)(?:\s+\d+(?:\.\d+)?(?:ms|s|m|h))*\s*$",
    )
    .expect("valid regex")
});

/// Oh My Pi renders one status row while a turn is in flight —
/// `⠴ Working… ⟦esc⟧`, or the running tool's progress message in place of
/// `Working…` (`⠇ Read existing glTF exporter patterns ⟨esc⟩`). Neither the
/// spinner nor the message is matchable (the message is arbitrary tool text),
/// but the trailing interrupt hint is: it is rendered only on that row, and the
/// row is dropped the moment the turn ends.
///
/// The bracket pair is glyph-mode dependent, so all three of omp's modes are
/// accepted — Unicode `⟦⟧` (U+27E6/U+27E7), Nerd Font `⟨⟩` (U+27E8/U+27E9) and
/// ASCII `[]`. The hint always ends its row, which is what keeps the ASCII form
/// from matching `[esc]` occurring mid-line in transcript prose or tool output.
///
/// Receipts (omp 17.2.15): the Unicode form captured from a local pane driven to
/// a live turn against a stalling mock endpoint; the Nerd Font form from the
/// user's working session on another host. The bracket set is omp's own
/// `format.bracketLeft`/`format.bracketRight` theme keys, whose three values are
/// exactly those pairs.
static OMP_WORKING_HINT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)(?:\u{27E6}esc\u{27E7}|\u{27E8}esc\u{27E9}|\[esc\])\s*$").expect("valid regex")
});

/// Pane-content substrings Oh My Pi renders in an overlay that is blocking on a
/// user decision. The tool-approval options cover a session whose title state
/// indicator has been turned off; the plan-review options cover the case the
/// title cannot report at all, because the turn ends *underneath* an open plan
/// review and drives the title to `idle` — see [`omp_title_state`] for the
/// sequencing and its receipt.
///
/// Receipt (omp 17.2.15): both option sets are verbatim from omp's own bundle —
/// the approval options are its `allow_once`/`allow_always` option table, and
/// the plan-review options are the literal argument list passed to
/// `showPlanReview`. Neither survives the overlay being dismissed.
const OMP_ATTENTION_MARKERS: [&str; 4] = [
    // Tool-approval overlay options.
    "Allow once",
    "Always allow",
    // Plan-review overlay options.
    "Approve and execute",
    "Refine plan",
];

/// Strip ANSI escape sequences from a string.
pub fn strip_ansi(s: &str) -> String {
    ANSI_RE.replace_all(s, "").into_owned()
}

/// The last `n` non-empty lines of `content`, bottom-first.
///
/// Both content detectors anchor their status-line matches to the bottom of the
/// pane, because everything above it is transcript — text the model and its
/// tools wrote, which can contain anything a status line contains.
fn trailing_nonempty_lines(content: &str, n: usize) -> Vec<&str> {
    content
        .lines()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .take(n)
        .collect()
}

/// Whether `title` contains a braille spinner glyph (U+2800..U+28FF). The Codex
/// TUI animates a braille spinner in the terminal title while the model is
/// working. Older Claude Code builds did too, so the Claude arm of
/// [`AgentKind::title_state`] still accepts it alongside its current spinner.
fn has_braille_spinner(title: &str) -> bool {
    title.contains(|c: char| ('\u{2800}'..='\u{28FF}').contains(&c))
}

/// Whether `title` contains a quadrant-circle spinner glyph (U+25D0..U+25D3).
/// Current Claude Code animates `◐`/`◑` in the terminal title while a turn is in
/// flight and shows a static `✳` (U+2733) when idle, so the spinner is a clean
/// `Working` signal.
///
/// Receipt: claude-code 2.1.228 on Linux, sampled with `tmux display-message -p
/// '#{pane_title}'` 200× at 50ms against a long-running working session and 100×
/// at 200ms across a fresh session's first turn. Only U+25D0 and U+25D1 appeared
/// while working, and only U+2733 while idle. U+25D2/U+25D3 complete the same
/// four-frame set and are accepted for that reason, but were never observed.
///
/// This is why Claude sessions read `Idle` while working: the detector looked
/// only for [`has_braille_spinner`], which Claude Code no longer renders.
fn has_circle_spinner(title: &str) -> bool {
    title.contains(|c: char| ('\u{25D0}'..='\u{25D3}').contains(&c))
}

/// Pane-content substrings Codex renders when it is blocked waiting for the user
/// to approve a command, edit, or network access. These are part of the
/// always-rendered approval overlay (independent of the user-configurable
/// terminal-title items), so they are the durable signal for `WaitingForInput`.
const CODEX_APPROVAL_MARKERS: [&str; 5] = [
    "Would you like to run the following command?",
    "Do you want to approve network access to",
    "Would you like to grant these permissions?",
    "Would you like to make the following edits?",
    "needs your approval.",
];

/// Pane-content substring Codex renders in its status line while a task is
/// actively running — the interrupt hint in `• Working (12s • esc to interrupt)`.
/// A durable `Working` signal that survives a user customising the terminal
/// title (e.g. dropping the spinner via `/title`), so working sessions aren't
/// mislabelled `Idle` when the title check can't see a spinner. Distinct from
/// the approval footer, which reads "esc to cancel".
const CODEX_WORKING_MARKER: &str = "esc to interrupt";

/// Pane-content substring OpenCode renders in both full and mini TUI footers
/// while a task is actively running. Completed turns keep the prompt footer but
/// drop this interrupt hint, so it is a durable active-task signal.
const OPENCODE_WORKING_MARKER: &str = "esc interrupt";

/// How many trailing non-empty lines of the pane count as OpenCode's *footer*
/// for [`OPENCODE_WORKING_MARKER`] purposes.
///
/// The marker must be matched against the footer only, never the whole pane: the
/// transcript above it is model- and tool-authored text, and a session that
/// merely *renders* the words `esc interrupt` — reading this file, quoting
/// OpenCode's own keybindings, echoing a grep hit — pinned itself to `Working`
/// forever, because `Working` is checked before the completed-turn line that sat
/// right below it. Re-attaching and scrolling pushed the text out of the visible
/// pane, which is exactly the reported "shows working until I re-enter it".
///
/// Receipt: the hint is the *last* non-empty line in 14/14 working captures
/// (OpenCode 1.17.15) — full TUI at 200x50 with the sidebar, mini TUI at 80x24
/// and 55x13, and a pane resized by an attach/detach cycle. The window is 3
/// rather than 1 purely as slack for a future build adding a line beneath it.
const OPENCODE_FOOTER_LINES: usize = 3;

/// Pane-content substring OpenCode renders in its permission prompt overlay
/// when the agent is blocked on a user approval decision.
const OPENCODE_PERMISSION_MARKER: &str = "Permission required";

/// Muse Code's status line while a turn is in flight: a phase glyph, the phase
/// (`Thinking`, `Working`, `Calling tools`, `Double checking`, …), and a live
/// duration plus interrupt hint — `◈ Working (13m 52s · esc to interrupt)`. The
/// line is dropped the moment the turn ends, leaving `◆ Worked for 2m 04s`.
///
/// Matching the shape anywhere in the pane is not enough: a diff line
/// (`  1 +◆ Thinking (5s · …)`) or a markdown bullet quoting it fits any
/// single-glyph pattern. So [`muse_content_state`] reads only the status line's
/// *slot* — see [`muse_status_slot`] — and this regex then checks that line
/// whole. The hour and millisecond units were not observed and are accepted on
/// the shape of the others.
///
/// Receipts (Muse Code 1.4.14): `◈ Working (13m 52s · …)` from the user's
/// working session; `◆ Thinking (5s · …)` from a local `--provider echo
/// --echo-delay-ms 30000` turn; `◇ Calling tools (39s · …)` and `◇ Double
/// checking (1m 27s · …)` from a throwaway session driven to a shell-approval
/// prompt.
static MUSE_WORKING_STATUS_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[◆◇◈]\s+[^(\n]+\((?:\d+(?:ms|[hms])\s+)*\d+(?:ms|[hms]) · esc to interrupt\)\s*$")
        .expect("valid regex")
});

/// Footer lines Muse Code renders under an overlay that is blocking on the
/// user: the shell-command approval card, the `request_user_input` question,
/// and the workspace-trust prompt shown at launch in an untrusted directory —
/// which a fresh commander worktree usually is.
///
/// Receipts (Muse Code 1.4.14), each captured live: the approval card from a
/// throwaway session run with `--approval-mode untrusted --approval-judge off`
/// and asked to `ls -la`; the question from the same session asked to call
/// `request_user_input`; the trust prompt from a fresh local launch. File
/// writes were not gated in that mode, so no edit-approval card was seen; it is
/// assumed, not verified, to share the approval card's footer.
const MUSE_ATTENTION_FOOTERS: [&str; 3] = [
    "Change approval defaults in /permissions",
    "Enter to select · ↑/↓ to move",
    "Use Up/Down or 1/2, then Enter.",
];

/// How many trailing non-empty lines of the pane may hold a
/// [`MUSE_ATTENTION_FOOTERS`] marker. Each footer sits at the bottom of the
/// screen — the approval card's is 3rd from the last line and replaces the
/// composer, the question's 5th (above the composer's two rules, prompt row and
/// statusline), the trust prompt's last — so the window keeps transcript text
/// quoting one out, with slack for a multi-line draft in the composer.
const MUSE_FOOTER_LINES: usize = 8;

/// The agent CLI harness backing a session, derived from its `program` command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentKind {
    /// Anthropic Claude Code (`claude`).
    Claude,
    /// OpenAI Codex CLI (`codex`).
    Codex,
    /// OpenCode TUI (`opencode`).
    OpenCode,
    /// Oh My Pi (`omp`).
    Omp,
    /// Muse Code (`muse-code`). Its real binary is `muse`, which `muse-code`
    /// symlinks to, but that name is too generic to claim: an existing entry
    /// running some other `muse` would be relaunched as `muse resume --last`.
    Muse,
    /// Any other program (a bare shell, an unrecognised agent, …). We launch it
    /// but make no assumptions about its flags or TUI output.
    Unknown,
}

impl AgentKind {
    /// Every recognised harness — all variants but [`Self::Unknown`] — in the
    /// order program discovery appends them to the picker.
    pub const KNOWN: [Self; 5] = [
        Self::Claude,
        Self::Codex,
        Self::OpenCode,
        Self::Omp,
        Self::Muse,
    ];

    /// The executable name this harness answers to: what
    /// [`Self::from_program`] matches, what [`Self::installed_on`] looks for,
    /// and the command a discovered program entry launches. `None` for
    /// [`Self::Unknown`].
    pub fn binary_name(self) -> Option<&'static str> {
        match self {
            Self::Claude => Some("claude"),
            Self::Codex => Some("codex"),
            Self::OpenCode => Some("opencode"),
            Self::Omp => Some("omp"),
            Self::Muse => Some("muse-code"),
            Self::Unknown => None,
        }
    }

    /// Human-readable name, used as the label of a discovered program entry.
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Codex => "Codex",
            Self::OpenCode => "OpenCode",
            Self::Omp => "Oh My Pi",
            Self::Muse => "Muse Code",
            Self::Unknown => "Unknown",
        }
    }

    /// Derive the harness from a program command string by its first token,
    /// tolerating path prefixes and trailing arguments — e.g. `claude`,
    /// `Claude --resume`, and `/usr/local/bin/codex -m gpt-5` all resolve.
    pub fn from_program(program: &str) -> Self {
        let name = program
            .split_whitespace()
            .next()
            .and_then(|tok| tok.rsplit('/').next())
            .unwrap_or("");
        Self::KNOWN
            .into_iter()
            .find(|kind| {
                kind.binary_name()
                    .is_some_and(|b| name.eq_ignore_ascii_case(b))
            })
            .unwrap_or(Self::Unknown)
    }

    /// The recognised harnesses whose binary is an executable file
    /// in one of `path`'s directories (a `PATH`-style list), in [`Self::KNOWN`]
    /// order.
    pub fn installed_on(path: &OsStr) -> Vec<Self> {
        let dirs: Vec<_> = std::env::split_paths(path).collect();
        Self::KNOWN
            .into_iter()
            .filter(|kind| {
                kind.binary_name()
                    .is_some_and(|binary| dirs.iter().any(|dir| is_executable(&dir.join(binary))))
            })
            .collect()
    }

    /// Whether this harness is Claude Code. Claude-only launch flags
    /// (`--permission-mode`, `--effort`, `-n <name>`) gate on this.
    pub fn is_claude(self) -> bool {
        self == Self::Claude
    }

    /// Whether the harness accepts a single positional prompt argument at
    /// launch. Claude, Codex, Oh My Pi and Muse do (`claude '<prompt>'`,
    /// `codex '<prompt>'`, `omp '<prompt>'`, `muse-code '<prompt>'`); an unknown
    /// program (e.g. a bare shell) does not, so we must not append a prompt it
    /// would mis-parse.
    ///
    /// Receipt for omp (17.2.15): its `--help` documents a positional
    /// `MESSAGES` argument and the example `omp "List all .ts files in src/"`.
    /// Receipt for Muse (1.4.14): its `--help` usage line is `muse [OPTIONS]
    /// [PROMPT]`, "pass a prompt to start a session".
    pub fn accepts_positional_prompt(self) -> bool {
        matches!(self, Self::Claude | Self::Codex | Self::Omp | Self::Muse)
    }

    /// Whether this harness accepts a `--model <name>` launch flag. Claude,
    /// Codex, OpenCode, Oh My Pi and Muse support it (Muse 1.4.14's `--help`:
    /// `--model <MODEL>`); an unknown program's flags are unconstrained.
    pub fn supports_model_flag(self) -> bool {
        match self {
            Self::Claude | Self::Codex | Self::OpenCode | Self::Omp | Self::Muse => true,
            Self::Unknown => false,
        }
    }

    /// Delay to wait between injecting prompt *text* into the pane and sending
    /// the submit `Enter`, or `None` to send the two back-to-back.
    ///
    /// Codex folds a carriage-return that arrives in the same terminal read as
    /// the preceding text into the pasted text (as a literal newline) rather
    /// than treating it as a submit keystroke, so a back-to-back text+Enter
    /// leaves the prompt sitting unsent in the composer until a *separate* Enter
    /// arrives. Spacing the Enter out lets Codex drain the text first, so the
    /// Enter lands as its own read and submits. (Verified against codex-cli
    /// 0.144.3: a coalesced text+Enter write never submitted across 5/5 trials;
    /// a ~200ms gap submitted 15/15.)
    ///
    /// Claude Code does the same once the burst is long enough to look like a
    /// paste: the Enter becomes a newline in the composer. Short text hides it,
    /// which is why dictation auto-submit only *sometimes* failed. Verified
    /// against Claude Code 2.1.284 by writing text+`\r` as one `write` to an
    /// attached tmux client's PTY — the path the attach pump takes: 40 chars
    /// submitted 3/3; 70, 300 and 1200 chars submitted 0/3 each; with a 100ms
    /// or 250ms gap before the `\r`, 300 and 1200 chars submitted 3/3 each.
    ///
    /// Oh My Pi needs no delay, verified rather than assumed: a single
    /// `send-keys '<text>' Enter` — one write carrying both — submitted 3/3
    /// against omp 17.2.15, each time leaving an empty composer and dispatching
    /// the turn; and a 300-char text+`\r` single PTY write submitted against
    /// omp 18.1.17.
    ///
    /// Muse Code folds the Enter at *every* length, verified against 1.4.14
    /// with `--provider echo`: a single `send-keys '<text>' Enter` submitted
    /// 0/15 times across 40, 300 and 1200 chars — each Enter became a newline,
    /// and the prompts piled up in the composer — while a 250ms gap before the
    /// Enter submitted 9/9.
    pub fn submit_key_delay(self) -> Option<Duration> {
        match self {
            Self::Codex | Self::Claude | Self::Muse => Some(Duration::from_millis(250)),
            _ => None,
        }
    }

    /// Build the command that resumes this harness's previous session,
    /// preserving any flags on the base command. Returns `None` when the harness
    /// has no resume mechanism we can drive (so the caller launches fresh).
    ///
    /// Claude appends a `--resume` flag; Codex and Muse use a `resume --last`
    /// subcommand that must follow the binary, before its other flags; OpenCode
    /// and Oh My Pi append `--continue`.
    ///
    /// Receipt for Muse (1.4.14): `muse resume --help` documents `--last` as
    /// "Resume the most recent session in this workspace" and that root options
    /// "may appear on either side of `resume`", so the base command's flags can
    /// follow it. A bare `resume` opens a picker instead.
    ///
    /// Receipt for omp (17.2.15): `--help` documents `-c, --continue  Continue
    /// previous session` and the example `omp --continue "What did we
    /// discuss?"`. Its `-r, --resume` takes an optional session id and opens a
    /// picker when given none, so it is not the flag to drive unattended.
    pub fn resume_command(self, program: &str) -> Option<String> {
        let mut parts = program.splitn(2, char::is_whitespace);
        let binary = parts.next().unwrap_or("");
        let rest = parts.next();
        match self {
            Self::Claude => Some(match rest {
                Some(r) => format!("{binary} {r} --resume"),
                None => format!("{binary} --resume"),
            }),
            Self::Codex | Self::Muse => Some(match rest {
                Some(r) => format!("{binary} resume --last {r}"),
                None => format!("{binary} resume --last"),
            }),
            Self::OpenCode | Self::Omp => Some(match rest {
                Some(r) => format!("{binary} {r} --continue"),
                None => format!("{binary} --continue"),
            }),
            Self::Unknown => None,
        }
    }

    /// Detect agent state from the tmux pane *title*. Returns `Some` when the
    /// title alone is conclusive (so the caller can skip capturing pane
    /// content), `None` when content must be inspected.
    ///
    /// Returning `Some(Working)` on a spinner is only sound because a spinner
    /// title never coexists with a pending prompt: `AgentStateDetector::
    /// detect_fresh` takes a `Some` as final and never captures content, so a
    /// harness that kept spinning through an approval prompt would have its
    /// `WaitingForInput` masked as `Working` — a worse failure than the `Idle`
    /// bug this detector was fixed for.
    ///
    /// Receipt (claude-code 2.1.228): the title reverts from the `◐`/`◑` spinner
    /// to the idle `✳ <topic>` the moment a prompt appears. Measured two ways —
    /// 14/14 samples of a session driven to a mid-turn Bash approval prompt with
    /// `Esc to cancel` visible, and 5/5 live sessions found sitting at a prompt,
    /// none of which carried a spinner glyph.
    pub fn title_state(self, title: &str) -> Option<AgentState> {
        match self {
            // Codex prefixes the title with "Action Required" (no spinner) while
            // blocked on approval — check it before the shared spinner since the
            // two are mutually exclusive in Codex's title.
            Self::Codex if title.contains("Action Required") => Some(AgentState::WaitingForInput),
            // Claude animates a quadrant-circle spinner, Codex a braille one.
            // Both arms accept either glyph set: the two never collide with an
            // idle title (Claude's is `✳ <name>`, Codex's a bare project name),
            // so a harness changing its frames again degrades to the content
            // fallback rather than silently reporting Idle.
            Self::Claude | Self::Codex
                if has_circle_spinner(title) || has_braille_spinner(title) =>
            {
                Some(AgentState::Working)
            }
            Self::Omp => omp_title_state(title),
            // Muse animates a braille spinner before its label while a turn is
            // in flight (`⠙ ws`) and shows the bare label when idle — but the
            // spinner keeps going while an overlay waits on the user, so it
            // can't be taken as `Working` (see the soundness note above), and
            // the bare label can't vouch for `Idle` either. Content decides.
            //
            // Receipt (Muse Code 1.4.14): 40/40 title samples at 50ms carried a
            // spinner frame — all ten cycling — with the shell-approval card on
            // screen, and 20/20 with the `request_user_input` question up.
            // tmux flattens the glyph to `_` on a non-UTF-8 client, as it does
            // omp's: the user's working session read `_ build-3794954b`.
            Self::Muse => None,
            // OpenCode's title reports which *view* it is showing, never the
            // agent state: `OC | <session title>` on a session that has been
            // titled (truncated to 40 chars), `OC | <plugin id>` on a plugin
            // view, and a bare `OpenCode` on the home view or before the
            // session has a title. It can also be neither: setting
            // `OPENCODE_DISABLE_TERMINAL_TITLE` returns from the effect before
            // any title is set, leaving whatever the pane already had, and the
            // in-app toggle clears it to the empty string. None of those
            // distinguish Working from Idle, so fall through to content.
            //
            // Receipt (opencode 1.17.15): those branches are the entire body of
            // the one reactive effect that composes a title, and it reads only
            // the current route and the session's own title — every other
            // `setTerminalTitle` in the bundle is opentui plumbing, the
            // destroy-time clear, or that toggle. Live confirmation: a fresh
            // untitled session reports `OpenCode`, while a titled session on
            // 1.18.11 reports `OC | PLAN.md review`.
            _ => None,
        }
    }

    /// Detect agent state from the visible pane *content* (the fallback when the
    /// title is inconclusive). Recognised harnesses with no durable idle signal
    /// should return `Unknown` rather than guessing. The `Unknown` harness arm is
    /// retained as a benign fallback for direct unit tests; `AgentStateDetector`
    /// short-circuits unknown programs before pane inspection.
    pub fn content_state(self, content: &str) -> AgentState {
        let content = strip_ansi(content);
        match self {
            Self::Claude => claude_content_state(&content),
            Self::Codex => codex_content_state(&content),
            Self::OpenCode => opencode_content_state(&content),
            Self::Omp => omp_content_state(&content),
            Self::Muse => muse_content_state(&content),
            Self::Unknown => AgentState::Idle,
        }
    }
}

/// Claude content patterns: the last visible lines carry permission/selection
/// prompts when Claude is waiting for the user, and an in-flight turn's status
/// line when it is working. Waiting takes precedence over working, as it does
/// for Codex — a permission prompt can be up while the status line is still
/// rendered, and needs-attention is the more urgent read.
fn claude_content_state(content: &str) -> AgentState {
    let lines = trailing_nonempty_lines(content, 10);

    for line in &lines {
        // Permission prompt footer.
        if line.contains("Esc to cancel") {
            return AgentState::WaitingForInput;
        }
        // Rejection menu option.
        if line.contains("No, and tell Claude what to do differently") {
            return AgentState::WaitingForInput;
        }
        // Selection menu: ❯ followed by a digit.
        if let Some(pos) = line.find('\u{276F}') {
            let after = line[pos + '\u{276F}'.len_utf8()..].trim_start();
            if after.starts_with(|c: char| c.is_ascii_digit()) {
                return AgentState::WaitingForInput;
            }
        }
    }

    // Working: the in-flight turn's status line, which sits directly above the
    // composer and so falls inside the same last-10-lines window.
    if lines
        .iter()
        .any(|line| CLAUDE_WORKING_STATUS_RE.is_match(line))
    {
        return AgentState::Working;
    }

    AgentState::Idle
}

/// Codex content patterns. The approval overlay's question text is rendered in
/// the visible pane whenever Codex is blocked on the user; the interrupt hint is
/// rendered while a task runs. Scanning the whole visible pane (which
/// `capture-pane -p` already bounds to the current screen) is robust to the
/// overlay's variable height. Approval takes precedence over working.
fn codex_content_state(content: &str) -> AgentState {
    if CODEX_APPROVAL_MARKERS
        .iter()
        .any(|marker| content.contains(marker))
    {
        return AgentState::WaitingForInput;
    }
    if content.contains(CODEX_WORKING_MARKER) {
        return AgentState::Working;
    }
    AgentState::Idle
}

/// OpenCode content patterns. The interrupt hint is rendered in the footer while
/// a task runs — matched against the footer alone, see [`OPENCODE_FOOTER_LINES`].
/// Completed turns render an agent/model/duration line, which is the durable idle
/// signal. Brand-new sessions still return `Unknown`: "Ask anything" only
/// appears before the first turn and is not a general idle marker.
///
/// The permission overlay *is* matched against the whole pane: it is a centred
/// modal box whose distance from the bottom varies with its own height.
fn opencode_content_state(content: &str) -> AgentState {
    if content.contains(OPENCODE_PERMISSION_MARKER) {
        return AgentState::WaitingForInput;
    }
    if trailing_nonempty_lines(content, OPENCODE_FOOTER_LINES)
        .iter()
        .any(|line| line.contains(OPENCODE_WORKING_MARKER))
    {
        return AgentState::Working;
    }
    if OPENCODE_COMPLETED_TURN_RE.is_match(content) {
        return AgentState::Idle;
    }
    AgentState::Unknown
}

/// Oh My Pi's title is `π <state glyph> <label>`, where the label is the
/// auto-generated session title (falling back to the working directory's
/// basename) and the state glyph is one of exactly four values: a braille
/// spinner frame while a turn is in flight, `!` while blocked on the user, `>`
/// when idle, or `:` in place of the spinner on Windows. With the state
/// indicator turned off the title is `π: <label>` instead, carrying no state.
///
/// Receipt (omp 17.2.15): the glyph set and the `π <glyph> <label>` assembly are
/// from omp's own title builder, whose state is driven by three transitions —
/// `working` when a turn starts, `attention` when a tool call needs approval or
/// the `ask` tool runs, `idle` when a terminal turn ends. Live confirmations:
/// `π ⠼ omp-support-5e59cfe0` cycling the documented ten frames against a
/// stalling mock endpoint, and `π > omp-support-5e59cfe0` at an idle composer.
///
/// The title is *not* subject to omp's glyph-mode setting, unlike its pane
/// content: driven to ASCII glyph mode — where the same pane's status row
/// degrades to `| Working… [esc]` and its statusline brand to a literal `pi` —
/// the title still read `π > …` idle and `π ⠼ …` working.
///
/// The idle `>` is deliberately *not* treated as conclusive: it returns `None`
/// rather than `Some(Idle)`, because omp ends the turn *before* raising its
/// full-screen plan review — so an idle title can sit above an overlay that is
/// waiting on the user. Falling through lets [`omp_content_state`] see the
/// overlay. Receipt (omp 17.2.15): `handlePlanApproval` awaits an internal
/// `session.abort()` — bracketed by `markPlanInternalAbortPending` /
/// `clearPlanInternalAbortPending`, and called out in omp's own changelog as
/// the "internal approval abort" — which forcibly ends the turn, and only then
/// calls `showPlanReview`. The idle title follows from the terminal turn-end
/// event. The cost of falling through is one extra `capture-pane` per fresh
/// detection, the same price OpenCode already pays; note the hibernation loop
/// and several `CommanderService` paths construct their detector with a
/// `Duration::ZERO` cache deliberately, so they pay it on every tick.
///
/// The flattened `_` in the glyph position is read as the spinner. tmux replaces
/// every non-ASCII character of a pane title with `_` when its server's locale
/// is not UTF-8, so a working omp session reports `_ _ <label>` — brand and
/// spinner both flattened — which is what the author's own sessions look like.
/// The inference is exact rather than a guess about tmux: the glyph position
/// holds one of exactly four values and three of them (`!`, `>`, `:`) are ASCII
/// that passes through untouched, so a replaced glyph can only have been a
/// spinner frame. It cannot mask a pending prompt either, since `attention`
/// renders the ASCII `!` and plan review leaves the ASCII `>`. Should tmux ever
/// substitute something else, this arm simply stops matching and the content
/// fallback covers it.
///
/// Receipts: flattening reproduced on tmux 3.6a — an identical `printf` of
/// `π ⠦ Probe` round-tripped byte-for-byte under `LANG=en_GB.UTF-8` but came
/// back `_ _ Probe` under `LANG=C` and under an unset locale. Only the *title*
/// is affected; pane cells keep their UTF-8 either way, confirmed on a
/// POSIX-locale server whose `capture-pane` returned `⠇`/`⟨esc⟩` intact while
/// that same pane's title read `_ _ <label>`.
fn omp_title_state(title: &str) -> Option<AgentState> {
    let mut tokens = title.split_whitespace();
    // The brand prefix is a single character (`π`, or `_` once tmux has
    // flattened it). `π:` — the state-indicator-off form — is two, and so
    // declines here, as does any title an extension has overridden wholesale.
    let brand = tokens.next()?;
    if brand.chars().count() != 1 {
        return None;
    }
    match tokens.next()? {
        "!" => Some(AgentState::WaitingForInput),
        ":" => Some(AgentState::Working),
        glyph if has_braille_spinner(glyph) => Some(AgentState::Working),
        // The spinner is the glyph position's only non-ASCII inhabitant, so a
        // glyph tmux has flattened to `_` was one.
        "_" => Some(AgentState::Working),
        _ => None,
    }
}

/// Oh My Pi content patterns. A blocking overlay's options are rendered in the
/// visible pane, and the in-flight turn's status row carries the interrupt hint;
/// waiting takes precedence over working, as it does for the other harnesses.
///
/// Unlike OpenCode this falls back to `Idle` rather than `Unknown`, because omp
/// *does* have a durable idle signal: the status row exists only while a turn is
/// in flight and is dropped when it ends (verified — an idle pane carries no
/// interrupt hint at all). So "no working hint and no overlay" is a positive
/// idle reading rather than an absence of information.
///
/// The attention markers are ordinary English phrases, so a session whose pane
/// happens to *display* one — an agent reading this very file, say — reads
/// `WaitingForInput` while it is merely working. That is the safe direction (a
/// spurious needs-attention flag holds hibernation off; it can never kill a live
/// session), and Codex's `"needs your approval."` shares the weakness. Where the
/// state indicator is on, a real approval also flips the title to `!` and this
/// fallback is never reached for it — but with the indicator off (`π: <label>`)
/// these markers are the *only* approval signal, which is what they are here
/// for, so they cannot be tightened away.
fn omp_content_state(content: &str) -> AgentState {
    if OMP_ATTENTION_MARKERS
        .iter()
        .any(|marker| content.contains(marker))
    {
        return AgentState::WaitingForInput;
    }
    if OMP_WORKING_HINT_RE.is_match(content) {
        return AgentState::Working;
    }
    AgentState::Idle
}

/// Muse Code content patterns. An overlay waiting on the user is recognised by
/// its footer at the bottom of the pane ([`MUSE_ATTENTION_FOOTERS`]); a turn in
/// flight by its status line ([`MUSE_WORKING_STATUS_RE`]). Waiting takes
/// precedence, and here it must: the approval card is drawn *beneath* a working
/// status line that stays on screen until it is answered.
///
/// Falls back to `Idle` rather than `Unknown`, as omp's does, because Muse has
/// a durable idle signal: the status line exists only while a turn is in
/// flight (verified — a finished turn leaves `◆ Worked for 2m 04s` and no
/// interrupt hint), so its absence is a positive reading.
fn muse_content_state(content: &str) -> AgentState {
    if trailing_nonempty_lines(content, MUSE_FOOTER_LINES)
        .iter()
        .any(|line| MUSE_ATTENTION_FOOTERS.iter().any(|m| line.contains(m)))
    {
        return AgentState::WaitingForInput;
    }
    if muse_status_slot(content).is_some_and(|line| MUSE_WORKING_STATUS_RE.is_match(line)) {
        return AgentState::Working;
    }
    AgentState::Idle
}

/// The line in the slot Muse Code draws its working status line in, if any:
/// the last column-0 line above the composer's top rule.
///
/// The composer is framed by two full-width rules (`─`), so its top rule is the
/// second-to-last rule on the pane. Between it and the status line Muse draws
/// only indented rows — the task list (`  ✓ …`, `  ◆ …`) — and `└ last event …`
/// sub-lines, which are skipped. Everything else at column 0 is transcript, and
/// a reply always starts there with its own `◆`, so the slot holds the status
/// line only while the status line is the newest thing on screen: a quoted copy
/// further up is behind a later reply, and one inside a diff or a list is
/// indented or carries a different first glyph.
///
/// Receipt (Muse Code 1.4.14): the working, approval, question and idle panes
/// captured for the fixtures in this file's tests all have this layout.
fn muse_status_slot(content: &str) -> Option<&str> {
    let lines: Vec<&str> = content.lines().collect();
    let is_rule = |line: &&str| {
        let t = line.trim();
        !t.is_empty() && t.chars().all(|c| c == '─')
    };
    let top_rule = lines
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, l)| is_rule(l))
        .nth(1)?
        .0;
    lines[..top_rule]
        .iter()
        .rev()
        .find(|l| {
            !l.trim().is_empty() && !l.starts_with(char::is_whitespace) && !l.starts_with('└')
        })
        .copied()
}

/// Whether `path` is a file someone may execute. Off Unix, being a file is
/// taken as enough.
fn is_executable(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.is_file() && meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        meta.is_file()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- from_program ---

    #[test]
    fn from_program_detects_claude() {
        assert_eq!(AgentKind::from_program("claude"), AgentKind::Claude);
        assert_eq!(
            AgentKind::from_program("claude --resume"),
            AgentKind::Claude
        );
        assert_eq!(AgentKind::from_program("Claude"), AgentKind::Claude);
        assert_eq!(AgentKind::from_program("CLAUDE -c"), AgentKind::Claude);
        assert_eq!(
            AgentKind::from_program("/usr/local/bin/claude --debug"),
            AgentKind::Claude
        );
    }

    #[test]
    fn from_program_detects_codex() {
        assert_eq!(AgentKind::from_program("codex"), AgentKind::Codex);
        assert_eq!(AgentKind::from_program("codex -m gpt-5"), AgentKind::Codex);
        assert_eq!(AgentKind::from_program("Codex"), AgentKind::Codex);
        assert_eq!(
            AgentKind::from_program("/opt/homebrew/bin/codex --full-auto"),
            AgentKind::Codex
        );
    }

    #[test]
    fn from_program_detects_opencode() {
        assert_eq!(AgentKind::from_program("opencode"), AgentKind::OpenCode);
        assert_eq!(
            AgentKind::from_program("opencode --auto"),
            AgentKind::OpenCode
        );
        assert_eq!(AgentKind::from_program("OpenCode"), AgentKind::OpenCode);
        assert_eq!(
            AgentKind::from_program("/usr/local/bin/opencode"),
            AgentKind::OpenCode
        );
    }

    #[test]
    fn from_program_detects_omp() {
        assert_eq!(AgentKind::from_program("omp"), AgentKind::Omp);
        assert_eq!(AgentKind::from_program("omp --model opus"), AgentKind::Omp);
        assert_eq!(AgentKind::from_program("OMP"), AgentKind::Omp);
        assert_eq!(AgentKind::from_program("/usr/bin/omp"), AgentKind::Omp);
    }

    #[test]
    fn from_program_detects_muse() {
        assert_eq!(AgentKind::from_program("muse-code"), AgentKind::Muse);
        assert_eq!(
            AgentKind::from_program("muse-code --model muse-spark-1.3"),
            AgentKind::Muse
        );
        assert_eq!(AgentKind::from_program("Muse-Code"), AgentKind::Muse);
        assert_eq!(
            AgentKind::from_program("/usr/bin/muse-code -w"),
            AgentKind::Muse
        );
    }

    #[test]
    fn from_program_unknown_for_others() {
        assert_eq!(AgentKind::from_program("bash"), AgentKind::Unknown);
        assert_eq!(AgentKind::from_program(""), AgentKind::Unknown);
        // Different binary names that merely contain the substring must not match.
        assert_eq!(AgentKind::from_program("claude-code"), AgentKind::Unknown);
        assert_eq!(AgentKind::from_program("codex-cli"), AgentKind::Unknown);
        assert_eq!(AgentKind::from_program("opencode-ai"), AgentKind::Unknown);
        assert_eq!(AgentKind::from_program("ompx"), AgentKind::Unknown);
        assert_eq!(AgentKind::from_program("omp-wrapper"), AgentKind::Unknown);
        assert_eq!(AgentKind::from_program("muse-codex"), AgentKind::Unknown);
        assert_eq!(AgentKind::from_program("muse-import"), AgentKind::Unknown);
        // Muse Code's real binary is `muse`, but so might be any other tool's;
        // an existing entry running one must stay a plain program rather than
        // be relaunched as `muse resume --last`.
        assert_eq!(AgentKind::from_program("muse"), AgentKind::Unknown);
        assert_eq!(
            AgentKind::from_program("/usr/bin/muse -w"),
            AgentKind::Unknown
        );
    }

    #[test]
    fn every_known_harness_round_trips_through_its_binary_name() {
        // Discovery adds an entry whose command is the binary name, so that
        // name must resolve back to the same harness — or a discovered entry
        // would launch with no launch/resume/state handling at all.
        for kind in AgentKind::KNOWN {
            let name = kind.binary_name().expect("known harness has a binary");
            assert_eq!(AgentKind::from_program(name), kind, "{name}");
        }
        assert_eq!(AgentKind::Unknown.binary_name(), None);
        assert!(!AgentKind::KNOWN.contains(&AgentKind::Unknown));
    }

    // --- installed_on ---

    #[cfg(unix)]
    fn touch(dir: &std::path::Path, name: &str, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, "").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn installed_on_finds_executables_across_path_entries() {
        let a = tempfile::TempDir::new().unwrap();
        let b = tempfile::TempDir::new().unwrap();
        touch(a.path(), "codex", 0o755);
        touch(b.path(), "muse-code", 0o755);
        // Present but not runnable, and a directory wearing the name: neither
        // is an install.
        touch(b.path(), "omp", 0o644);
        std::fs::create_dir(a.path().join("opencode")).unwrap();
        let path = std::env::join_paths([a.path(), b.path()]).unwrap();
        // KNOWN order, not PATH order.
        assert_eq!(
            AgentKind::installed_on(&path),
            vec![AgentKind::Codex, AgentKind::Muse]
        );
    }

    #[cfg(unix)]
    #[test]
    fn installed_on_ignores_a_bare_muse() {
        // `muse` is a generic name; only `muse-code` is taken as evidence that
        // Muse Code is installed.
        let dir = tempfile::TempDir::new().unwrap();
        touch(dir.path(), "muse", 0o755);
        assert_eq!(AgentKind::installed_on(dir.path().as_os_str()), vec![]);
    }

    #[test]
    fn installed_on_empty_path_finds_nothing() {
        assert_eq!(AgentKind::installed_on(std::ffi::OsStr::new("")), vec![]);
    }

    // --- capability flags ---

    #[test]
    fn accepts_positional_prompt_for_agents_only() {
        assert!(AgentKind::Claude.accepts_positional_prompt());
        assert!(AgentKind::Codex.accepts_positional_prompt());
        // OpenCode's plain `opencode` does not accept a positional prompt;
        // prompts are passed via `opencode run [message..]` instead.
        assert!(!AgentKind::OpenCode.accepts_positional_prompt());
        // omp takes a positional `MESSAGES` argument, like Claude and Codex.
        assert!(AgentKind::Omp.accepts_positional_prompt());
        // So does Muse (`muse [OPTIONS] [PROMPT]`).
        assert!(AgentKind::Muse.accepts_positional_prompt());
        assert!(!AgentKind::Unknown.accepts_positional_prompt());
    }

    #[test]
    fn is_claude_only_for_claude() {
        assert!(AgentKind::Claude.is_claude());
        assert!(!AgentKind::Codex.is_claude());
        assert!(!AgentKind::OpenCode.is_claude());
        assert!(!AgentKind::Omp.is_claude());
        assert!(!AgentKind::Muse.is_claude());
        assert!(!AgentKind::Unknown.is_claude());
    }

    #[test]
    fn supports_model_flag_for_agents_only() {
        assert!(AgentKind::Claude.supports_model_flag());
        assert!(AgentKind::Codex.supports_model_flag());
        assert!(AgentKind::OpenCode.supports_model_flag());
        assert!(AgentKind::Omp.supports_model_flag());
        assert!(AgentKind::Muse.supports_model_flag());
        assert!(!AgentKind::Unknown.supports_model_flag());
    }

    #[test]
    fn submit_key_delay_for_codex_and_claude() {
        // Codex and Claude Code both need the submit Enter spaced out from the
        // injected prompt text, or they read the whole burst as a paste and the
        // Enter lands as a newline in the composer instead of a submit (Claude
        // only once the text is past ~60 chars, which is why short prompts hid
        // it). Muse folds it at every length. OpenCode and omp submit on the
        // carriage-return regardless. Removing any of these delays reintroduces
        // "the prompt sits unsent in the composer" — for dictation auto-submit
        // and comment apply alike.
        for kind in [AgentKind::Codex, AgentKind::Claude, AgentKind::Muse] {
            assert_eq!(
                kind.submit_key_delay(),
                Some(Duration::from_millis(250)),
                "{kind:?}"
            );
        }
        assert_eq!(AgentKind::OpenCode.submit_key_delay(), None);
        assert_eq!(AgentKind::Omp.submit_key_delay(), None);
        assert_eq!(AgentKind::Unknown.submit_key_delay(), None);
    }

    // --- resume_command ---

    #[test]
    fn resume_command_claude_appends_flag() {
        assert_eq!(
            AgentKind::Claude.resume_command("claude"),
            Some("claude --resume".to_string())
        );
        assert_eq!(
            AgentKind::Claude.resume_command("claude -c"),
            Some("claude -c --resume".to_string())
        );
    }

    #[test]
    fn resume_command_codex_uses_subcommand_after_binary() {
        assert_eq!(
            AgentKind::Codex.resume_command("codex"),
            Some("codex resume --last".to_string())
        );
        // Flags on the base command survive, and the subcommand lands right
        // after the binary (not at the end).
        assert_eq!(
            AgentKind::Codex.resume_command("codex -m gpt-5"),
            Some("codex resume --last -m gpt-5".to_string())
        );
    }

    #[test]
    fn resume_command_muse_uses_subcommand_after_binary() {
        assert_eq!(
            AgentKind::Muse.resume_command("muse-code"),
            Some("muse-code resume --last".to_string())
        );
        assert_eq!(
            AgentKind::Muse.resume_command("muse-code --model muse-spark-1.3"),
            Some("muse-code resume --last --model muse-spark-1.3".to_string())
        );
    }

    #[test]
    fn resume_command_none_for_unknown() {
        assert_eq!(AgentKind::Unknown.resume_command("bash"), None);
    }

    #[test]
    fn resume_command_opencode_appends_continue() {
        assert_eq!(
            AgentKind::OpenCode.resume_command("opencode"),
            Some("opencode --continue".to_string())
        );
        assert_eq!(
            AgentKind::OpenCode.resume_command("opencode --auto"),
            Some("opencode --auto --continue".to_string())
        );
    }

    #[test]
    fn resume_command_omp_appends_continue() {
        assert_eq!(
            AgentKind::Omp.resume_command("omp"),
            Some("omp --continue".to_string())
        );
        assert_eq!(
            AgentKind::Omp.resume_command("omp --model opus"),
            Some("omp --model opus --continue".to_string())
        );
    }

    // --- title_state ---

    #[test]
    fn title_state_working_braille_both_harnesses() {
        // Braille spinner frame U+280B → Working for both.
        assert_eq!(
            AgentKind::Claude.title_state("⠋ feature-branch"),
            Some(AgentState::Working)
        );
        assert_eq!(
            AgentKind::Codex.title_state("⠹ my-project"),
            Some(AgentState::Working)
        );
    }

    #[test]
    fn title_state_working_claude_circle_spinner() {
        // Claude Code animates a half-filled-circle spinner in the pane title
        // while a turn is in flight, not a braille one. Captured verbatim from
        // live panes (claude-code 2.1.228); without these frames every Claude
        // session's title reads inconclusive and the session shows as Idle.
        assert_eq!(
            AgentKind::Claude.title_state("◐ audio-cleanup-video"),
            Some(AgentState::Working)
        );
        assert_eq!(
            AgentKind::Claude.title_state("◑ audio-cleanup-video"),
            Some(AgentState::Working)
        );
    }

    #[test]
    fn title_state_accepts_either_spinner_set_for_both_harnesses() {
        // Deliberate: each harness accepts the other's frames too, so a harness
        // changing its spinner again degrades to the content fallback instead of
        // silently reporting Idle (the failure this detector was fixed for).
        // Neither glyph set can collide with an idle title — Claude's is
        // `✳ <name>`, Codex's a bare project name — so the widening is free.
        // Codex has never been observed rendering a circle frame; this pins the
        // behaviour as intended rather than incidental.
        assert_eq!(
            AgentKind::Codex.title_state("◐ my-project"),
            Some(AgentState::Working)
        );
        assert_eq!(
            AgentKind::Claude.title_state("⠹ feature-branch"),
            Some(AgentState::Working)
        );
        // Endpoints of the accepted quadrant-circle run. `◒`/`◓` complete the
        // four-frame set but were never observed in 2.1.228 — pinned so the
        // range boundary can't silently narrow.
        assert_eq!(
            AgentKind::Claude.title_state("◒ x"),
            Some(AgentState::Working)
        );
        assert_eq!(
            AgentKind::Claude.title_state("◓ x"),
            Some(AgentState::Working)
        );
        // Just outside the range on both sides.
        assert_eq!(AgentKind::Claude.title_state("● x"), None);
        assert_eq!(AgentKind::Claude.title_state("◔ x"), None);
    }

    #[test]
    fn title_state_claude_idle_asterisk_is_not_working() {
        // The static `✳` is what an idle Claude title carries — it must stay
        // inconclusive so the content fallback gets a say, and must never be
        // mistaken for the spinner.
        assert_eq!(AgentKind::Claude.title_state("✳ extract-tui-crate"), None);
    }

    #[test]
    fn title_state_codex_action_required_is_waiting() {
        assert_eq!(
            AgentKind::Codex.title_state("[ ! ] Action Required | my-project"),
            Some(AgentState::WaitingForInput)
        );
        // Blink phase variant.
        assert_eq!(
            AgentKind::Codex.title_state("[ . ] Action Required"),
            Some(AgentState::WaitingForInput)
        );
    }

    // --- title_state: omp ---

    #[test]
    fn title_state_omp_working_across_every_spinner_frame() {
        // omp's ten braille frames, verbatim from its title builder and
        // observed cycling in a live pane. Every one must read Working, or the
        // state flickers between Working and Idle at 80ms.
        for frame in [
            '\u{280B}', '\u{2819}', '\u{2839}', '\u{2838}', '\u{283C}', '\u{2834}', '\u{2826}',
            '\u{2827}', '\u{2807}', '\u{280F}',
        ] {
            let title = format!("\u{3C0} {frame} omp-support-5e59cfe0");
            assert_eq!(
                AgentKind::Omp.title_state(&title),
                Some(AgentState::Working),
                "frame {frame:?} should signal Working"
            );
        }
    }

    #[test]
    fn title_state_omp_attention_is_waiting() {
        // `!` replaces the spinner while omp is blocked on a tool approval or
        // the `ask` tool. Captured shape: `π ! <session title>`.
        assert_eq!(
            AgentKind::Omp.title_state("\u{3C0} ! Promote model loaders to api"),
            Some(AgentState::WaitingForInput)
        );
    }

    #[test]
    fn title_state_omp_windows_colon_glyph_is_working() {
        // On Windows omp substitutes a static `:` for the animated spinner.
        assert_eq!(
            AgentKind::Omp.title_state("\u{3C0} : omp-support"),
            Some(AgentState::Working)
        );
    }

    #[test]
    fn title_state_omp_idle_glyph_falls_through_to_content() {
        // `>` is omp's idle glyph, but it is deliberately inconclusive: omp ends
        // the turn before raising its full-screen plan review, so an idle title
        // can sit above an overlay that is waiting on the user. Returning
        // `Some(Idle)` here would skip the content capture that spots it.
        assert_eq!(
            AgentKind::Omp.title_state("\u{3C0} > omp-support-5e59cfe0"),
            None
        );
    }

    #[test]
    fn title_state_omp_ignores_punctuation_in_the_label() {
        // Only the glyph *position* may be read. A session title ending in `!`
        // under the idle glyph must not be mistaken for the attention glyph —
        // matching `!` anywhere in the title would report WaitingForInput for an
        // idle session forever.
        assert_eq!(
            AgentKind::Omp.title_state("\u{3C0} > Fix the parser!"),
            None
        );
        // And a label that merely starts with `!` sits in the label position, so
        // it cannot reach the glyph match either.
        assert_eq!(
            AgentKind::Omp.title_state("\u{3C0} > !important cleanup"),
            None
        );
    }

    #[test]
    fn title_state_omp_state_indicator_disabled_is_inconclusive() {
        // With the state indicator off omp renders `π: <label>` — the brand and
        // a colon as one token, carrying no state. It must not be read as the
        // Windows `:` working glyph.
        assert_eq!(
            AgentKind::Omp.title_state("\u{3C0}: omp-support-5e59cfe0"),
            None
        );
    }

    #[test]
    fn title_state_omp_survives_tmux_flattening_the_brand() {
        // tmux replaces non-ASCII in a pane title with `_` when its server's
        // locale is not UTF-8 (reproduced on tmux 3.6a under `LANG=C`), so the
        // `π` brand can arrive as `_`. The ASCII glyphs still land, and this is
        // the shape the user's own working session reports.
        assert_eq!(
            AgentKind::Omp.title_state("_ ! Promote model loaders to api"),
            Some(AgentState::WaitingForInput)
        );
        // The spinner flattens to `_` too, and is still read as Working: the
        // glyph position holds one of exactly four values, and the other three
        // are ASCII that tmux passes through, so a replaced glyph can only have
        // been a spinner frame. Without this, every session on a non-UTF-8 tmux
        // server — which is what the author's own are — loses the title signal
        // for Working entirely and rests on the content fallback alone.
        assert_eq!(
            AgentKind::Omp.title_state("_ _ Promote model loaders to api"),
            Some(AgentState::Working)
        );
        // The flattened brand alone is not enough: an idle or attention glyph
        // still wins, so the inference cannot mask a pending prompt.
        assert_eq!(
            AgentKind::Omp.title_state("_ > Promote model loaders"),
            None
        );
    }

    #[test]
    fn title_state_omp_needs_both_a_brand_and_a_glyph() {
        assert_eq!(AgentKind::Omp.title_state(""), None);
        assert_eq!(AgentKind::Omp.title_state("\u{3C0}"), None);
        // A shell prompt left behind after omp exits must not parse as a state.
        assert_eq!(AgentKind::Omp.title_state("si@togusa:~/Projects"), None);
        // Another harness's title must not either.
        assert_eq!(
            AgentKind::Omp.title_state("\u{2733} extract-tui-crate"),
            None
        );
    }

    #[test]
    fn title_state_omp_glyphs_are_inert_for_other_harnesses() {
        // The omp parser is reached only via the Omp arm — `!` and `>` carry no
        // meaning in Claude's or Codex's titles.
        assert_eq!(AgentKind::Claude.title_state("\u{3C0} ! working"), None);
        assert_eq!(AgentKind::Codex.title_state("\u{3C0} > my-project"), None);
    }

    #[test]
    fn title_state_inconclusive_returns_none() {
        assert_eq!(AgentKind::Claude.title_state("✳ Claude Code"), None);
        assert_eq!(AgentKind::Codex.title_state("my-project"), None);
        assert_eq!(AgentKind::Claude.title_state(""), None);
        // Claude has no "Action Required" concept — the literal alone must not
        // trip its detector via the title path.
        assert_eq!(AgentKind::Claude.title_state("Action Required"), None);
        assert_eq!(AgentKind::Unknown.title_state("⠋ working"), None);
        // OpenCode's title names the view it is showing, not the agent state,
        // so none of its forms is conclusive. The bare `OpenCode` is only what
        // the home view and a not-yet-titled session render — a session that
        // has been titled reports `OC | <session title>`, which is what a real
        // pane is showing almost all of the time. Asserting the bare form alone
        // is what let this arm be documented as "always `OpenCode`".
        assert_eq!(AgentKind::OpenCode.title_state("OpenCode"), None);
        assert_eq!(AgentKind::OpenCode.title_state("OC | PLAN.md review"), None);
        assert_eq!(AgentKind::OpenCode.title_state("OC | my-plugin"), None);
        // Switched off (`OPENCODE_DISABLE_TERMINAL_TITLE` or the in-app
        // toggle), the title is cleared to the empty string.
        assert_eq!(AgentKind::OpenCode.title_state(""), None);
    }

    #[test]
    fn title_state_muse_spinner_is_inconclusive() {
        // Muse keeps spinning its title while an approval or question overlay
        // waits on the user, so a spinner must NOT short-circuit to Working —
        // that would mask every WaitingForInput (see `muse_content_state`).
        for frame in ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'] {
            assert_eq!(AgentKind::Muse.title_state(&format!("{frame} ws")), None);
        }
        // tmux's non-UTF-8 flattening of the same title.
        assert_eq!(AgentKind::Muse.title_state("_ build-3794954b"), None);
        // Idle: the bare label.
        assert_eq!(AgentKind::Muse.title_state("dev-94767866"), None);
    }

    // --- content_state: Claude ---

    #[test]
    fn claude_content_waiting_patterns() {
        assert_eq!(
            AgentKind::Claude.content_state("Some output\n  Allow tool? Esc to cancel\n"),
            AgentState::WaitingForInput
        );
        assert_eq!(
            AgentKind::Claude.content_state("Result\nNo, and tell Claude what to do differently\n"),
            AgentState::WaitingForInput
        );
        assert_eq!(
            AgentKind::Claude.content_state("Choose:\n❯ 1. Allow once\n  2. Allow always\n"),
            AgentState::WaitingForInput
        );
    }

    #[test]
    fn claude_content_idle() {
        // ❯ not followed by a digit = idle prompt, not a selection menu.
        assert_eq!(
            AgentKind::Claude.content_state("Done editing files.\n\n❯ \n"),
            AgentState::Idle
        );
        assert_eq!(AgentKind::Claude.content_state(""), AgentState::Idle);
    }

    #[test]
    fn claude_content_working_from_status_line() {
        // Both lines captured verbatim from live working panes (claude-code
        // 2.1.228): a long-running turn and a fresh session's first turn. The
        // elapsed/token parenthetical is the durable Working signal for the
        // content fallback — this version renders no `esc to interrupt` hint.
        let long_run = "  ⎿  Read src/main.rs\n\n✽ Boondoggling… (5m 35s · ↓ 18.6k tokens · still thinking with high effort)\n\n─── audio-cleanup-video ──\n❯ \n";
        let first_turn = "· Clauding… (4s · ↓ 4 tokens)\n\n❯ \n";
        assert_eq!(
            AgentKind::Claude.content_state(long_run),
            AgentState::Working
        );
        assert_eq!(
            AgentKind::Claude.content_state(first_turn),
            AgentState::Working
        );
    }

    #[test]
    fn claude_content_waiting_takes_precedence_over_working() {
        // A permission prompt can be up while the status line is still
        // rendered; needs-attention must win, as it does for Codex.
        let content = "✽ Boondoggling… (12s · ↓ 900 tokens)\nAllow tool? Esc to cancel\n";
        assert_eq!(
            AgentKind::Claude.content_state(content),
            AgentState::WaitingForInput
        );
    }

    #[test]
    fn claude_content_bare_gerund_line_is_not_working() {
        // Known, accepted gap in the content fallback: a short turn can render
        // the status line with no elapsed/token parenthetical. Matching the bare
        // `<glyph> <Gerund>…` shape instead would be unsafe, because completed
        // tool lines keep that shape in the transcript forever — so this reads
        // Idle and the title check is what covers the case. Captured verbatim
        // from a live pane mid-turn (claude-code 2.1.228).
        assert_eq!(
            AgentKind::Claude.content_state("✽ Bunning…\n\n❯ \n"),
            AgentState::Idle
        );
    }

    #[test]
    fn claude_content_finished_turn_and_tool_lines_are_not_working() {
        // The lines that persist in the transcript after a turn ends must never
        // read Working — this is what the duration+tokens requirement buys.
        // Both captured verbatim from live panes.
        assert_eq!(
            AgentKind::Claude.content_state("✻ Cogitated for 15s\n\n❯ \n"),
            AgentState::Idle
        );
        assert_eq!(
            AgentKind::Claude.content_state("● Listing 1 directory… (ctrl+o to expand)\n\n❯ \n"),
            AgentState::Idle
        );
    }

    #[test]
    fn claude_content_prose_ellipsis_is_not_working() {
        // Requiring both an elapsed duration and the token count keeps ordinary
        // transcript prose from being read as an in-flight turn.
        assert_eq!(
            AgentKind::Claude.content_state("Compiling… (2s)\nDone.\n❯ \n"),
            AgentState::Idle
        );
    }

    #[test]
    fn claude_content_strips_ansi_before_matching() {
        assert_eq!(
            AgentKind::Claude.content_state("\x1B[1mAllow?\x1B[0m \x1B[33mEsc to cancel\x1B[0m\n"),
            AgentState::WaitingForInput
        );
        // The Working path needs the same guarantee: Claude colours the status
        // line, so the regex would never match un-stripped input.
        assert_eq!(
            AgentKind::Claude
                .content_state("\x1B[36m✽ Boondoggling…\x1B[0m \x1B[2m(4s · ↓ 4 tokens)\x1B[0m\n"),
            AgentState::Working
        );
    }

    // --- content_state: Codex ---

    #[test]
    fn codex_content_approval_markers_are_waiting() {
        for marker in CODEX_APPROVAL_MARKERS {
            let content = format!("codex output\n\n{marker}\n\n  Yes   No\n");
            assert_eq!(
                AgentKind::Codex.content_state(&content),
                AgentState::WaitingForInput,
                "marker {marker:?} should signal WaitingForInput"
            );
        }
    }

    #[test]
    fn codex_content_idle_when_no_marker() {
        assert_eq!(
            AgentKind::Codex.content_state("Edited src/main.rs\nDone.\n› \n"),
            AgentState::Idle
        );
        assert_eq!(AgentKind::Codex.content_state(""), AgentState::Idle);
    }

    #[test]
    fn codex_content_working_from_interrupt_hint() {
        // Real status-line shape captured from a live Codex session. The
        // interrupt hint is a durable Working signal independent of the
        // (user-configurable) terminal-title spinner.
        let content = "› Create a file…\n• Working (13s • esc to interrupt)\n";
        assert_eq!(AgentKind::Codex.content_state(content), AgentState::Working);
    }

    #[test]
    fn codex_content_approval_takes_precedence_over_working() {
        // If both a working hint and an approval question are visible, the
        // pending approval (needs-attention) must win.
        let content =
            "• Working (2s • esc to interrupt)\nWould you like to run the following command?\n";
        assert_eq!(
            AgentKind::Codex.content_state(content),
            AgentState::WaitingForInput
        );
    }

    #[test]
    fn codex_content_approval_footer_is_not_working() {
        // The approval footer reads "esc to cancel", not "esc to interrupt",
        // so it must not be mistaken for the working hint.
        let content = "Press enter to confirm or esc to cancel\n";
        assert_eq!(AgentKind::Codex.content_state(content), AgentState::Idle);
    }

    #[test]
    fn codex_content_strips_ansi_before_matching() {
        assert_eq!(
            AgentKind::Codex
                .content_state("\x1B[1mWould you like to run the following command?\x1B[0m\n"),
            AgentState::WaitingForInput
        );
    }

    #[test]
    fn unknown_content_is_idle() {
        assert_eq!(
            AgentKind::Unknown.content_state("Esc to cancel"),
            AgentState::Idle
        );
    }

    // --- content_state: OpenCode ---

    #[test]
    fn opencode_content_new_session_prompt_is_unknown() {
        // The brand-new-session prompt is not a general idle marker: after a
        // completed turn OpenCode leaves a blank prompt instead. Avoid using it
        // as an idle signal for hibernation.
        let content = "Ask anything... \"Fix broken tests\"\n  Build · GPT-5.5\n";
        assert_eq!(
            AgentKind::OpenCode.content_state(content),
            AgentState::Unknown
        );
    }

    #[test]
    fn opencode_content_working_from_interrupt_hint() {
        // Real active-task footer captured from OpenCode full and mini TUIs.
        let full = "⬝⬝⬝⬝⬝⬝⬝⬝  esc interrupt                         tab agents  ctrl+p commands\n";
        let mini =
            "BUILD  ⬝■■■■■■⬝ esc interrupt                                       ctrl+p cmd\n";
        assert_eq!(AgentKind::OpenCode.content_state(full), AgentState::Working);
        assert_eq!(AgentKind::OpenCode.content_state(mini), AgentState::Working);
    }

    #[test]
    fn opencode_content_permission_prompt_is_waiting() {
        // Real approval overlay captured from OpenCode when accessing `~/` from
        // a session sandboxed in another directory.
        let content = "⚠ Permission required\n← Access external directory ~\n\nPatterns\n\n- /home/si/*\n\nAllow once   Allow always   Reject\n";
        assert_eq!(
            AgentKind::OpenCode.content_state(content),
            AgentState::WaitingForInput
        );
    }

    #[test]
    fn opencode_content_permission_takes_precedence() {
        let content = "Permission required\nesc interrupt\n▣ Build · GPT-5.5 · 2.7s\n";
        assert_eq!(
            AgentKind::OpenCode.content_state(content),
            AgentState::WaitingForInput
        );
    }

    #[test]
    fn opencode_content_completed_turn_is_idle() {
        let content = "▣  Build · GPT-5.5 · 2.7s\n7.6K (1%) · $0.04  ctrl+p commands\n";
        assert_eq!(AgentKind::OpenCode.content_state(content), AgentState::Idle);
    }

    #[test]
    fn opencode_content_completed_turn_accepts_longer_duration() {
        let content = "▣  Build · GPT-5.5 · 1m 8.5s\n";
        assert_eq!(AgentKind::OpenCode.content_state(content), AgentState::Idle);
    }

    #[test]
    fn opencode_content_active_agent_line_without_duration_is_unknown() {
        // Active turns show the agent/model line before the footer appears, but
        // no duration. Do not call it idle unless the completed-turn duration is
        // present.
        let content = "▣  Build · GPT-5.5\n";
        assert_eq!(
            AgentKind::OpenCode.content_state(content),
            AgentState::Unknown
        );
    }

    #[test]
    fn opencode_content_unknown_when_ambiguous() {
        // No working or idle marker visible — return Unknown, not Idle, to
        // avoid false auto-hibernation.
        assert_eq!(
            AgentKind::OpenCode.content_state("Some intermediate TUI state\n"),
            AgentState::Unknown
        );
        assert_eq!(AgentKind::OpenCode.content_state(""), AgentState::Unknown);
    }

    #[test]
    fn opencode_content_tip_is_not_working() {
        // This text appears as a rotating idle tip, not as the active-task
        // footer's "esc interrupt" status signal.
        let content = "● Tip Press escape to stop the AI mid-response\n";
        assert_eq!(
            AgentKind::OpenCode.content_state(content),
            AgentState::Unknown
        );
    }

    #[test]
    fn opencode_content_interrupt_hint_in_transcript_is_not_working() {
        // Regression: an *idle* session whose visible transcript happens to
        // contain the footer's wording stayed `Working` for as long as the text
        // stayed on screen, because the marker was matched against the whole
        // pane and `Working` is checked before the completed-turn line sitting
        // right below it. Captured verbatim from OpenCode 1.17.15 after asking
        // it to echo the phrase; the session was at its prompt throughout.
        let content = concat!(
            "  ┃  running\n",
            "  ┃\n",
            "\n",
            "     the footer said esc interrupt while running\n",
            "\n",
            "     ▣  Build · GPT-5.5 · 1.9s\n",
            "\n",
            "  ┃\n",
            "  ┃  Build · GPT-5.5 GitHub Copilot\n",
            "  ╹▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀\n",
            "                8.4K (1%) · $0.08  ctrl+p commands\n",
        );
        assert_eq!(AgentKind::OpenCode.content_state(content), AgentState::Idle);
    }

    #[test]
    fn opencode_content_working_footer_is_last_line() {
        // The live hint is the last non-empty line, so footer-anchoring keeps
        // reading it as `Working` — with the same transcript contamination
        // above that the previous test pins as harmless.
        let content = concat!(
            "     the footer said esc interrupt while running\n",
            "\n",
            "     ▣  Build · GPT-5.5 · 1.9s\n",
            "\n",
            "  ⬝⬝⬝⬝⬝⬝⬝⬝  esc interrupt          tab agents  ctrl+p commands\n",
        );
        assert_eq!(
            AgentKind::OpenCode.content_state(content),
            AgentState::Working
        );
    }

    #[test]
    fn opencode_content_permission_overlay_matches_above_the_footer() {
        // The permission modal is centred, not docked to the bottom, so it must
        // keep matching against the whole pane rather than the footer window.
        let content = concat!(
            "⚠ Permission required\n",
            "← Access external directory ~\n",
            "\n",
            "Allow once   Allow always   Reject\n",
            "\n",
            "  ┃\n",
            "  ┃  Build · GPT-5.5 GitHub Copilot\n",
            "  ╹▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀\n",
            "                8.4K (1%) · $0.08  ctrl+p commands\n",
        );
        assert_eq!(
            AgentKind::OpenCode.content_state(content),
            AgentState::WaitingForInput
        );
    }

    #[test]
    fn opencode_content_strips_ansi_before_matching() {
        assert_eq!(
            AgentKind::OpenCode.content_state("\x1B[1mesc interrupt\x1B[0m  ctrl+p commands\n"),
            AgentState::Working
        );
    }

    // --- content_state: omp ---

    #[test]
    fn omp_content_working_from_interrupt_hint_in_every_glyph_mode() {
        // All three captured verbatim from live working panes (omp 17.2.15).
        // Unicode is the default glyph mode; ASCII was reached by driving the
        // setup wizard; the Nerd Font row is from the user's own session, and
        // shows the status row carrying a running tool's progress message in
        // place of `Working…`.
        let unicode = " \u{2834} Working\u{2026} \u{27E6}esc\u{27E7}\n";
        let ascii = " | Working\u{2026} [esc]\n";
        let nerd = " \u{2826} Read existing glTF exporter patterns \u{27E8}esc\u{27E9}\n";
        for content in [unicode, ascii, nerd] {
            assert_eq!(
                AgentKind::Omp.content_state(content),
                AgentState::Working,
                "content {content:?} should signal Working"
            );
        }
    }

    #[test]
    fn omp_content_working_hint_is_found_above_the_statusline() {
        // The status row is not the last row of the pane — omp's statusline is
        // drawn below it. This is the real capture, and it is what the
        // end-of-*line* anchor buys over an end-of-input one.
        let content = concat!(
            " count to ten\n",
            " \u{2834} Working\u{2026} \u{27E6}esc\u{27E7}\n",
            "\u{256D}\u{2500}\u{2500} \u{3C0}  > \u{2B22} Sonnet 5 \u{B7} \u{25D2} high > ",
            "\u{1F333} claude-commander/omp-support \u{2500}\u{2500}\u{256E}\n",
            "\u{2570}\u{2500} \u{2500}\u{256F}\n",
        );
        assert_eq!(AgentKind::Omp.content_state(content), AgentState::Working);
    }

    #[test]
    fn omp_content_idle_when_the_status_row_is_gone() {
        // omp drops the status row the moment the turn ends, so an idle pane
        // carries no interrupt hint. Verified against a live idle composer: zero
        // occurrences of the hint anywhere in the visible pane.
        let content = concat!(
            " Tip: Ctrl+D can be used to exit, but with your draft saved!\n",
            " \u{2718} 401 {\"type\":\"error\"}\n",
            " Dismissed when you send your next message.\n",
            "\u{256D}\u{2500}\u{2500} \u{3C0}  > \u{2B22} Sonnet 5 \u{B7} \u{25D2} high ",
            "\u{2500}\u{2500}\u{256E}\n",
        );
        assert_eq!(AgentKind::Omp.content_state(content), AgentState::Idle);
        assert_eq!(AgentKind::Omp.content_state(""), AgentState::Idle);
    }

    #[test]
    fn omp_content_mid_line_ascii_hint_is_not_working() {
        // In ASCII glyph mode the hint is a bare `[esc]`, which can plausibly
        // occur in transcript prose or tool output. Requiring it to end its row
        // is what keeps that from reading as an in-flight turn.
        assert_eq!(
            AgentKind::Omp.content_state("Press [esc] twice to clear the composer.\n"),
            AgentState::Idle
        );
    }

    #[test]
    fn omp_content_overlay_options_are_waiting() {
        for marker in OMP_ATTENTION_MARKERS {
            let content = format!("omp output\n\n  \u{276F} {marker}\n");
            assert_eq!(
                AgentKind::Omp.content_state(&content),
                AgentState::WaitingForInput,
                "marker {marker:?} should signal WaitingForInput"
            );
        }
    }

    #[test]
    fn each_omp_attention_marker_is_pinned_by_an_independent_literal() {
        // The loop above builds its input *from* the marker constant, so it
        // cannot catch a typo in one. These are the same option rows spelt out
        // independently — verbatim from omp 17.2.15, where the approval options
        // come from its `allow_once`/`allow_always` table and the plan-review
        // options from the literal list passed to `showPlanReview`
        // (`["Approve and execute", "Approve and compact context", <keep>,
        // "Refine plan"]`, under the heading `Plan mode - next step`).
        //
        // Each input carries exactly ONE option, which is the point: with two in
        // the same string, either one matching would mask a typo in the other.
        for content in [
            "   \u{276F} Allow once\n",
            "     Always allow\n",
            "   \u{276F} Approve and execute\n",
            "     Refine plan\n",
        ] {
            assert_eq!(
                AgentKind::Omp.content_state(content),
                AgentState::WaitingForInput,
                "option row {content:?} should signal WaitingForInput"
            );
        }
    }

    #[test]
    fn omp_content_plan_review_overlay_is_waiting() {
        // The whole overlay as a session actually shows it, so the realistic
        // shape is pinned alongside the isolated options above.
        let content = concat!(
            " Plan mode - next step\n",
            " \u{276F} Approve and execute\n",
            "   Approve and compact context\n",
            "   Approve and keep context\n",
            "   Refine plan\n",
            " esc cancel\n",
        );
        assert_eq!(
            AgentKind::Omp.content_state(content),
            AgentState::WaitingForInput
        );
    }

    #[test]
    fn omp_content_waiting_takes_precedence_over_working() {
        // The approval overlay is drawn over a pane whose status row is still
        // up; needs-attention must win, as it does for the other harnesses.
        let content =
            " \u{2834} Working\u{2026} \u{27E6}esc\u{27E7}\n  Allow once   Always allow   Reject\n";
        assert_eq!(
            AgentKind::Omp.content_state(content),
            AgentState::WaitingForInput
        );
    }

    #[test]
    fn omp_content_strips_ansi_before_matching() {
        // omp colours both the status row and the overlay, so the regex would
        // never see a clean end-of-line without the strip.
        assert_eq!(
            AgentKind::Omp.content_state(
                "\x1B[36m \u{2834} Working\u{2026}\x1B[0m \x1B[2m\u{27E6}esc\u{27E7}\x1B[0m\n"
            ),
            AgentState::Working
        );
    }

    #[test]
    fn omp_content_patterns_are_inert_for_other_harnesses() {
        // omp's hint must not leak into the other harnesses' detectors.
        assert_eq!(
            AgentKind::Codex.content_state(" \u{2834} Working\u{2026} \u{27E6}esc\u{27E7}\n"),
            AgentState::Idle
        );
        assert_eq!(
            AgentKind::OpenCode.content_state(" \u{2834} Working\u{2026} \u{27E6}esc\u{27E7}\n"),
            AgentState::Unknown
        );
    }

    // --- content_state: Muse ---
    //
    // Every fixture below is a verbatim `capture-pane -p` of Muse Code 1.4.14
    // (blank lines dropped, long rules shortened). See `muse_content_state` for
    // where each was captured.

    const MUSE_RULE: &str = "────────────────────────────────────────";

    fn muse_pane(body: &str) -> String {
        format!(
            "  Muse Code 1.4.14\n{body}\n{MUSE_RULE}\n❯\n{MUSE_RULE}\n  muse-spark-1.3 · xhigh · /tmp/ws · Launch overrides\n"
        )
    }

    #[test]
    fn muse_content_working_from_status_line() {
        // The status line names the phase and carries a live duration; it is
        // separated from the composer by an arbitrarily long task list.
        let pane = muse_pane(
            "◈ Working (13m 52s · esc to interrupt)\n\
             \n\
             \x20 ✓ Finish T-25.4 Mind draw review (field 8→9 heads)\n\
             \x20 ◆ Implement T-25.5 Tile draw (LABEL_WIDTH 10→11, colon, dim labels)\n\
             \x20 ◇ T-25.6 golden re-bless review (19 colon goldens)\n\
             \x20 ◇ T-25.7 milestone check and merge",
        );
        assert_eq!(AgentKind::Muse.content_state(&pane), AgentState::Working);
        for line in [
            "◆ Thinking (5s · esc to interrupt)",
            "◇ Calling tools (39s · esc to interrupt)\n└ last event 33s ago",
            "◇ Double checking (1m 27s · esc to interrupt)",
            "◈ Working (2h 3m 4s · esc to interrupt)",
        ] {
            assert_eq!(
                AgentKind::Muse.content_state(&muse_pane(line)),
                AgentState::Working,
                "{line}"
            );
        }
    }

    #[test]
    fn muse_content_idle_when_the_status_line_is_gone() {
        let pane = muse_pane(
            "❯ Now create a new file b.txt containing the word hello.\n\
             ◆ Creating that file for you.\n\
             ◆ Wrote b.txt (+1)\n\
             \x20 1 +hello\n\
             ◆ Created b.txt.\n\
             ◆ Worked for 2m 04s · 10:58 AM · 30/Oct",
        );
        assert_eq!(AgentKind::Muse.content_state(&pane), AgentState::Idle);
        // A fresh session, before any turn.
        assert_eq!(
            AgentKind::Muse.content_state(&muse_pane("")),
            AgentState::Idle
        );
    }

    #[test]
    fn muse_content_interrupt_hint_in_transcript_prose_is_not_working() {
        // The status line is matched as a whole line, so prose that merely
        // mentions the hint — or quotes the line inside other text — is inert.
        let pane = muse_pane(
            "◆ The footer reads (5s · esc to interrupt) while a turn runs.\n\
             \x20 20 +    \"◆ Thinking (5s · esc to interrupt)\",",
        );
        assert_eq!(AgentKind::Muse.content_state(&pane), AgentState::Idle);
        // A one-digit diff line number and a markdown bullet both put a single
        // character and a space before the quoted status line.
        for line in [
            "◆ Wrote x.rs (+1)\n\x20 1 +◆ Thinking (5s · esc to interrupt)",
            "◆ Notes:\n- The status line reads (5s · esc to interrupt)",
            "◆ Thinking (5s · esc to interrupt)\n◆ That is what the status line shows.",
        ] {
            assert_eq!(
                AgentKind::Muse.content_state(&muse_pane(line)),
                AgentState::Idle,
                "{line}"
            );
        }
    }

    #[test]
    fn muse_content_status_line_must_sit_above_the_composer() {
        // Without the composer's rules there is no status-line slot to read.
        assert_eq!(
            AgentKind::Muse.content_state("◆ Thinking (5s · esc to interrupt)\n"),
            AgentState::Idle
        );
        // Sub-second durations are accepted on the shape of the other units.
        assert_eq!(
            AgentKind::Muse.content_state(&muse_pane("◆ Thinking (500ms · esc to interrupt)")),
            AgentState::Working
        );
    }

    #[test]
    fn muse_content_command_approval_is_waiting() {
        // The approval card replaces the composer, under a still-rendered
        // working status line — waiting must win.
        let pane = "\
  Muse Code 1.4.14
❯ Run the shell command: ls -la
◆ I'll run that directory listing for you.
◇ Calling tools (39s · esc to interrupt)
└ last event 33s ago
────────────────────────────────────────
Would you like to run the following command?
  $ ls -la
  Stage 1/1
  Current argv: [\"ls\",\"-la\"]
⟩ 1. Allow this stage once (y)
  2. Always allow in this workspace: ls ... (p)
  3. Abort the entire command (esc)
Change approval defaults in /permissions
────────────────────────────────────────
  muse-spark-1.3 · xhigh · /tmp/ws · Launch overrides
";
        assert_eq!(
            AgentKind::Muse.content_state(pane),
            AgentState::WaitingForInput
        );
    }

    #[test]
    fn muse_content_question_overlay_is_waiting() {
        let pane = muse_pane(
            "❯ Ask me whether I prefer red or blue.\n\
             ◈ Request user input Color — running (33s)\n\
             \x20 Do you prefer red or blue?\n\
             \x20 ⟩ 1. Red                Warm, bold choice.\n\
             \x20   2. Blue               Cool, calm choice.\n\
             \x20   3. None of the above  Optionally, add details in notes (tab).\n\
             \x20 Enter to select · ↑/↓ to move · Tab for an optional note · Esc to interrupt",
        );
        assert_eq!(
            AgentKind::Muse.content_state(&pane),
            AgentState::WaitingForInput
        );
    }

    #[test]
    fn muse_content_workspace_trust_prompt_is_waiting() {
        // Shown at launch in a workspace Muse has not been told to trust —
        // which a fresh commander worktree usually is.
        let pane = "\
Do you trust this workspace?
Workspace:
/tmp/ws
Trusting allows project-local skills, rules, hooks, and plugin config to load before the model runs.
Only trust this workspace when you trust its contents.
⟩ 1  Trust and continue
  2  Quit
Use Up/Down or 1/2, then Enter. Esc quits.
";
        assert_eq!(
            AgentKind::Muse.content_state(pane),
            AgentState::WaitingForInput
        );
    }

    #[test]
    fn muse_content_overlay_footers_in_transcript_are_not_waiting() {
        // The overlay markers are matched against the bottom of the pane only:
        // a transcript quoting one, far above the composer, is not a prompt.
        let mut body = String::from(
            "◆ The card ends with Change approval defaults in /permissions\n\
             \x20 Enter to select · ↑/↓ to move · Tab for an optional note\n\
             \x20 Use Up/Down or 1/2, then Enter. Esc quits.\n",
        );
        for i in 0..8 {
            body.push_str(&format!("◆ Line {i} of an ordinary reply.\n"));
        }
        assert_eq!(
            AgentKind::Muse.content_state(&muse_pane(body.trim_end())),
            AgentState::Idle
        );
    }

    #[test]
    fn muse_content_strips_ansi_before_matching() {
        assert_eq!(
            AgentKind::Muse.content_state(&muse_pane(
                "\x1B[35m◆\x1B[0m Thinking \x1B[2m(5s · esc to interrupt)\x1B[0m"
            )),
            AgentState::Working
        );
    }

    #[test]
    fn muse_content_patterns_are_inert_for_other_harnesses() {
        // OpenCode's interrupt hint is "esc interrupt", and its footer window
        // must not be satisfied by Muse's differently-worded status line.
        assert_eq!(
            AgentKind::OpenCode.content_state(&muse_pane("◆ Thinking (5s · esc to interrupt)")),
            AgentState::Unknown
        );
        assert_eq!(
            AgentKind::Omp.content_state(&muse_pane("Change approval defaults in /permissions")),
            AgentState::Idle
        );
    }

    // --- strip_ansi ---

    #[test]
    fn strip_ansi_removes_csi_sequences() {
        assert_eq!(strip_ansi("\x1B[31mred\x1B[0m text"), "red text");
    }

    #[test]
    fn strip_ansi_leaves_clean_text() {
        assert_eq!(strip_ansi("hello world"), "hello world");
    }
}
