//! Agent state detection via tmux pane inspection
//!
//! Detects whether a Claude Code session is Working, Idle, or WaitingForInput
//! by inspecting the tmux pane title and visible pane content.

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use regex::Regex;
use tracing::debug;

/// Pre-compiled regex for stripping ANSI escape sequences.
static ANSI_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\x1B\[[0-9;]*[a-zA-Z]|\x1B\][^\x07]*\x07|\x1B\][^\x1B]*\x1B\\")
        .expect("valid regex")
});

use super::TmuxExecutor;
use crate::error::Result;
use crate::session::{AgentState, SessionId};

/// Detect agent state from the tmux pane title.
///
/// Claude Code sets the pane title to a braille spinner character (U+2800..U+28FF)
/// while working and `✳` (U+2733) when idle/waiting. Returns `Some(Working)` if a
/// braille spinner is found, `None` if we need to inspect content.
pub fn parse_pane_title(title: &str) -> Option<AgentState> {
    if title.contains(|c: char| ('\u{2800}'..='\u{28FF}').contains(&c)) {
        Some(AgentState::Working)
    } else {
        // Either contains ✳ (idle/waiting) or no known indicator — either way,
        // fall through to secondary content check.
        None
    }
}

/// Detect agent state from the visible pane content.
///
/// Checks the last non-empty lines for known Claude Code prompt patterns
/// that indicate the agent is waiting for user input.
pub fn parse_pane_content(content: &str) -> AgentState {
    let content = strip_ansi(content);

    // Check last 10 non-empty lines for prompt patterns
    let lines: Vec<&str> = content
        .lines()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .take(10)
        .collect();

    for line in &lines {
        // Permission prompt: "Esc to cancel"
        if line.contains("Esc to cancel") {
            return AgentState::WaitingForInput;
        }

        // Rejection menu option
        if line.contains("No, and tell Claude what to do differently") {
            return AgentState::WaitingForInput;
        }

        // Selection menu: ❯ followed by a digit
        if let Some(pos) = line.find('\u{276F}') {
            let after = line[pos + '\u{276F}'.len_utf8()..].trim_start();
            if after.starts_with(|c: char| c.is_ascii_digit()) {
                return AgentState::WaitingForInput;
            }
        }
    }

    AgentState::Idle
}

/// Strip ANSI escape sequences from a string.
pub fn strip_ansi(s: &str) -> String {
    ANSI_RE.replace_all(s, "").into_owned()
}

/// Whether a program string launches Claude Code.
///
/// Handles path prefixes and trailing arguments, e.g. both `claude`,
/// `claude --dangerously-skip-permissions`, and `/usr/local/bin/claude -c`
/// are recognised.
pub fn is_claude_program(program: &str) -> bool {
    program
        .split_whitespace()
        .next()
        .and_then(|tok| tok.rsplit('/').next())
        .is_some_and(|name| name.eq_ignore_ascii_case("claude"))
}

/// Agent state detector that polls tmux sessions and caches results.
pub struct AgentStateDetector {
    executor: TmuxExecutor,
    cache: HashMap<String, (AgentState, Instant)>,
    cache_ttl: Duration,
}

impl AgentStateDetector {
    /// Create a new detector with the given cache TTL.
    pub fn new(executor: TmuxExecutor, cache_ttl: Duration) -> Self {
        Self {
            executor,
            cache: HashMap::new(),
            cache_ttl,
        }
    }

    /// Detect agent state for a single tmux session, using cache if fresh.
    pub async fn detect(&mut self, tmux_session_name: &str) -> AgentState {
        // Check cache
        if let Some((state, cached_at)) = self.cache.get(tmux_session_name)
            && cached_at.elapsed() < self.cache_ttl
        {
            return *state;
        }

        let state = self.detect_fresh(tmux_session_name).await;

        self.cache
            .insert(tmux_session_name.to_string(), (state, Instant::now()));

        state
    }

    /// Perform fresh detection bypassing cache.
    async fn detect_fresh(&self, tmux_session_name: &str) -> AgentState {
        // Primary: check pane title
        match self.get_pane_title(tmux_session_name).await {
            Ok(title) => {
                if let Some(state) = parse_pane_title(&title) {
                    debug!(
                        "Pane title detection for {}: {:?}",
                        tmux_session_name, state
                    );
                    return state;
                }
                // Title says "not working", check content
            }
            Err(e) => {
                debug!("Failed to get pane title for {}: {}", tmux_session_name, e);
                return AgentState::Unknown;
            }
        }

        // Secondary: parse visible pane content
        match self.capture_visible_pane(tmux_session_name).await {
            Ok(content) => {
                let state = parse_pane_content(&content);
                debug!(
                    "Pane content detection for {}: {:?}",
                    tmux_session_name, state
                );
                state
            }
            Err(e) => {
                debug!("Failed to capture pane for {}: {}", tmux_session_name, e);
                AgentState::Unknown
            }
        }
    }

    /// Get the tmux pane title.
    async fn get_pane_title(&self, tmux_session_name: &str) -> Result<String> {
        self.executor
            .execute(&[
                "display-message",
                "-t",
                tmux_session_name,
                "-p",
                "#{pane_title}",
            ])
            .await
    }

    /// Capture visible pane content (no scrollback, no ANSI escapes).
    async fn capture_visible_pane(&self, tmux_session_name: &str) -> Result<String> {
        self.executor
            .execute(&["capture-pane", "-t", tmux_session_name, "-p"])
            .await
    }

    /// Detect agent states for a batch of sessions.
    ///
    /// Filters to Claude programs only; non-Claude sessions get `Unknown`.
    pub async fn detect_all(
        &mut self,
        sessions: &[(SessionId, String, String)],
    ) -> HashMap<SessionId, AgentState> {
        let mut results = HashMap::new();

        for (session_id, tmux_name, program) in sessions {
            let state = if is_claude_program(program) {
                self.detect(tmux_name).await
            } else {
                AgentState::Unknown
            };
            results.insert(*session_id, state);
        }

        results
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- parse_pane_title tests --

    #[test]
    fn test_pane_title_working_braille_spinner() {
        // Braille dot pattern U+2802 indicates Claude is working
        assert_eq!(
            parse_pane_title("⠂ feature-branch"),
            Some(AgentState::Working)
        );
    }

    #[test]
    fn test_pane_title_working_braille_spinner_alt() {
        // Braille dot pattern U+2810 — another spinner frame
        assert_eq!(
            parse_pane_title("⠐ feature-branch"),
            Some(AgentState::Working)
        );
    }

    #[test]
    fn test_pane_title_not_working() {
        // Eight-spoked asterisk indicates idle/waiting — needs content check
        assert_eq!(parse_pane_title("✳ Claude Code"), None);
    }

    #[test]
    fn test_pane_title_empty() {
        assert_eq!(parse_pane_title(""), None);
    }

    #[test]
    fn test_pane_title_no_indicator() {
        assert_eq!(parse_pane_title("some random title"), None);
    }

    // -- parse_pane_content tests --

    #[test]
    fn test_content_esc_to_cancel() {
        let content = "Some output\n  Allow tool? Esc to cancel\n";
        assert_eq!(parse_pane_content(content), AgentState::WaitingForInput);
    }

    #[test]
    fn test_content_rejection_menu() {
        let content = "Result\nNo, and tell Claude what to do differently\n";
        assert_eq!(parse_pane_content(content), AgentState::WaitingForInput);
    }

    #[test]
    fn test_content_selection_menu() {
        let content = "Choose:\n❯ 1. Allow once\n  2. Allow always\n";
        assert_eq!(parse_pane_content(content), AgentState::WaitingForInput);
    }

    #[test]
    fn test_content_selection_menu_with_space() {
        let content = "Choose:\n❯  3. Deny\n";
        assert_eq!(parse_pane_content(content), AgentState::WaitingForInput);
    }

    #[test]
    fn test_content_idle() {
        let content = "Done editing files.\n\n❯ \n";
        // ❯ not followed by a digit = idle prompt, not selection menu
        assert_eq!(parse_pane_content(content), AgentState::Idle);
    }

    #[test]
    fn test_content_empty() {
        assert_eq!(parse_pane_content(""), AgentState::Idle);
    }

    #[test]
    fn test_content_no_patterns() {
        let content = "Building project...\nCompiling main.rs\nFinished in 2.3s\n";
        assert_eq!(parse_pane_content(content), AgentState::Idle);
    }

    // -- strip_ansi tests --

    #[test]
    fn test_strip_ansi_clean_string() {
        assert_eq!(strip_ansi("hello world"), "hello world");
    }

    #[test]
    fn test_strip_ansi_csi_sequences() {
        assert_eq!(strip_ansi("\x1B[31mred\x1B[0m text"), "red text");
    }

    #[test]
    fn test_strip_ansi_preserves_content() {
        let input = "\x1B[1;32m● working\x1B[0m Esc to cancel";
        let stripped = strip_ansi(input);
        assert!(stripped.contains("Esc to cancel"));
        assert!(!stripped.contains("\x1B"));
    }

    #[test]
    fn test_content_with_ansi_escapes() {
        let content = "\x1B[1mAllow?\x1B[0m \x1B[33mEsc to cancel\x1B[0m\n";
        assert_eq!(parse_pane_content(content), AgentState::WaitingForInput);
    }

    // -- cache tests --

    #[test]
    fn test_cache_ttl_fresh() {
        let entry = (AgentState::Working, Instant::now());
        assert!(entry.1.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn test_cache_ttl_stale() {
        let entry = (
            AgentState::Working,
            Instant::now() - Duration::from_secs(10),
        );
        assert!(entry.1.elapsed() > Duration::from_secs(5));
    }

    // -- detect() TTL boundary tests --
    //
    // These tests target the cache freshness check at the top of `detect()`:
    //   if let Some((state, cached_at)) = self.cache.get(tmux_session_name)
    //       && cached_at.elapsed() < self.cache_ttl
    //   { return *state; }
    //
    // They distinguish `<` from `==`, `<=`, and `>` by pre-populating the
    // private `cache` field directly (same-module privacy) so we can place
    // entries at precise points relative to the TTL window without relying on
    // wall-clock sleeps. `Instant::elapsed()` saturates to `Duration::ZERO`
    // for instants in the future, which gives us an exact boundary handle.
    //
    // We deliberately don't call `detect()` through the cache-miss branch
    // in the first test (which would invoke the real tmux executor against
    // a non-existent session); instead the second test inspects the cache
    // entry post-call to observe whether a miss occurred.
    //
    // Truth table for `elapsed=0, ttl=0` (the future-instant + ZERO TTL case):
    //   `<`  : 0 < 0  → false → miss   (correct)
    //   `<=` : 0 <= 0 → true  → hit
    //   `==` : 0 == 0 → true  → hit
    //   `>`  : 0 > 0  → false → miss
    //
    // Truth table for `elapsed≈0, ttl=1h` (fresh entry, large TTL):
    //   `<`  : tiny < hour → true  → hit (correct)
    //   `<=` : tiny <= hour → true → hit
    //   `==` : tiny == hour → false → miss
    //   `>`  : tiny > hour  → false → miss

    #[tokio::test]
    async fn test_detect_cache_hit_with_fresh_entry() {
        // Large TTL + just-now cached_at => elapsed is tiny relative to TTL,
        // so the freshness check is firmly inside the cache window. Returns
        // the cached state without invoking the executor.
        //
        // Kills `replace < with ==` (miss: tiny != hour → Unknown via miss)
        // Kills `replace < with >`  (miss: tiny not > hour → Unknown via miss)
        // The `<=` mutant still says hit here — covered by the next test.
        let executor = TmuxExecutor::new();
        let mut detector = AgentStateDetector::new(executor, Duration::from_secs(3600));
        detector.cache.insert(
            "tts-fresh".to_string(),
            (AgentState::Working, Instant::now()),
        );

        let result = detector.detect("tts-fresh").await;
        assert_eq!(
            result,
            AgentState::Working,
            "fresh cache entry within TTL must hit and return the cached state"
        );
    }

    #[tokio::test]
    async fn test_detect_cache_miss_at_exact_boundary() {
        // `Instant::elapsed()` saturates to zero for instants in the future,
        // so a future cached_at + a TTL of zero yields `elapsed == ttl == 0`
        // deterministically. Correct `<` says miss; `<=` and `==` say hit.
        //
        // We observe hit-vs-miss by inspecting the cache entry's instant
        // after the call: on hit the entry is unchanged (still in the future,
        // so `elapsed()` is zero); on miss `detect_fresh` runs and the entry
        // is re-inserted with `Instant::now()`, whose `elapsed()` becomes
        // positive almost immediately. This sidesteps the executor's return
        // value (which would otherwise depend on the tmux environment).
        //
        // Kills `replace < with <=` (would hit, leave future instant intact)
        // Kills `replace < with ==` (same — would hit at exact equality)
        let executor = TmuxExecutor::new();
        let mut detector = AgentStateDetector::new(executor, Duration::ZERO);
        let future = Instant::now() + Duration::from_secs(3600);
        detector
            .cache
            .insert("tts-boundary".to_string(), (AgentState::Working, future));

        let _ = detector.detect("tts-boundary").await;

        let (_, after) = detector
            .cache
            .get("tts-boundary")
            .expect("cache entry must still exist after detect");
        assert!(
            after.elapsed() > Duration::ZERO,
            "with ttl=0 and elapsed=0, correct `<` must miss and re-insert \
             a now-stamped entry; a `<=` or `==` mutant would have hit and \
             left the original future instant in place (elapsed == 0)"
        );
    }

    // -- is_claude_program tests --

    #[test]
    fn test_is_claude_program_bare() {
        assert!(is_claude_program("claude"));
    }

    #[test]
    fn test_is_claude_program_case_insensitive() {
        assert!(is_claude_program("Claude"));
        assert!(is_claude_program("CLAUDE"));
    }

    #[test]
    fn test_is_claude_program_with_args() {
        assert!(is_claude_program(
            "claude --allow-dangerously-skip-permissions"
        ));
        assert!(is_claude_program("claude -c"));
    }

    #[test]
    fn test_is_claude_program_with_absolute_path() {
        assert!(is_claude_program("/usr/local/bin/claude"));
        assert!(is_claude_program("/usr/local/bin/claude --debug"));
    }

    #[test]
    fn test_is_claude_program_rejects_others() {
        assert!(!is_claude_program("aider"));
        assert!(!is_claude_program("bash"));
        assert!(!is_claude_program(""));
        assert!(!is_claude_program("claude-code")); // different binary name
    }

    // -- detect_all filtering --

    #[tokio::test]
    async fn test_detect_all_filters_non_claude() {
        // We can't make real tmux calls in tests, but we can test that
        // non-Claude programs get Unknown without attempting detection.
        // This test verifies the filtering logic by checking that aider
        // sessions produce Unknown.
        let executor = TmuxExecutor::new();
        let mut detector = AgentStateDetector::new(executor, Duration::from_secs(10));

        let sessions = vec![(
            SessionId::new(),
            "non-existent-session".to_string(),
            "aider".to_string(),
        )];

        let results = detector.detect_all(&sessions).await;
        assert_eq!(results.len(), 1);
        for state in results.values() {
            assert_eq!(*state, AgentState::Unknown);
        }
    }
}
