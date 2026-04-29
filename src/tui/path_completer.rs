//! Live-filtered directory completer backing the `Modal::PathInput` list.
//!
//! Behaviour is modelled on fish-style live completion rather than
//! shell-style Tab cycling:
//!
//! - The input handler calls [`PathCompleter::refilter`] on every keystroke
//!   so the visible list tracks what the user has typed.
//! - Arrow keys move the highlighted row via [`PathCompleter::move_selection_up`]
//!   / [`move_selection_down`].
//! - [`PathCompleter::complete`] (Tab) only extends the input to the longest
//!   common prefix — cycling through matches is now the arrow keys' job.
//! - Tilde (`~`) is expanded for disk reads and collapsed back on return.

use std::path::{Path, PathBuf};

/// Live-filtered directory completer backing the `Modal::PathInput` list.
///
/// Invariant: `selected_idx` is `Some(i)` exactly when `completions` is
/// non-empty, and `i < completions.len()`.
#[derive(Debug, Clone)]
pub struct PathCompleter {
    /// Directory paths matching the current input.
    completions: Vec<String>,
    /// Highlighted row within `completions`. See invariant above.
    selected_idx: Option<usize>,
    /// The expanded input `completions` were computed from — lets [`complete`]
    /// reuse a recent refilter without re-reading the directory.
    completed_from: String,
}

impl Default for PathCompleter {
    fn default() -> Self {
        Self::new()
    }
}

impl PathCompleter {
    pub fn new() -> Self {
        Self {
            completions: Vec::new(),
            selected_idx: None,
            completed_from: String::new(),
        }
    }

    /// Recompute the completion list from `value`.
    ///
    /// Called on every keystroke in the modal so the list stays live. Resets
    /// the highlighted row to the first entry (or clears it if the list is
    /// now empty), which matches the palette's "fresh filter, fresh
    /// selection" behaviour.
    pub fn refilter(&mut self, value: &str) {
        let expanded = expand_tilde(value);
        self.completions = list_matching_dirs(&expanded);
        self.selected_idx = if self.completions.is_empty() {
            None
        } else {
            Some(0)
        };
        self.completed_from = expanded;
    }

    /// Handle a Tab press: extend the input to the longest common prefix.
    ///
    /// Returns the new value (possibly unchanged). A single-match list
    /// completes to `<name>/` so the next [`refilter`] lists the children.
    /// Unlike the old implementation, repeated Tab never cycles — arrow keys
    /// handle that instead.
    pub fn complete(&mut self, value: &str) -> String {
        let expanded = expand_tilde(value);

        // Cheap guard: if the caller hasn't refiltered since the last edit,
        // do it now so we're completing against a current view of disk.
        if self.completed_from != expanded {
            self.completions = list_matching_dirs(&expanded);
            self.selected_idx = if self.completions.is_empty() {
                None
            } else {
                Some(0)
            };
            self.completed_from = expanded.clone();
        }

        if self.completions.is_empty() {
            return value.to_string();
        }

        if self.completions.len() == 1 {
            let full = format!("{}/", &self.completions[0]);
            self.completed_from = full.clone();
            return maybe_unexpand_tilde(value, &full);
        }

        // Multiple matches: only extend to the common prefix. Once the user
        // has typed (or Tab-completed) to the common prefix, further Tabs are
        // no-ops — the arrow keys do the cycling work.
        let lcp = longest_common_prefix(&self.completions);
        if lcp.len() > expanded.len() {
            self.completed_from = lcp.clone();
            maybe_unexpand_tilde(value, &lcp)
        } else {
            value.to_string()
        }
    }

    /// Move the highlighted row up, wrapping to the last entry from row 0.
    pub fn move_selection_up(&mut self) {
        if let Some(idx) = self.selected_idx
            && !self.completions.is_empty()
        {
            self.selected_idx = Some(if idx == 0 {
                self.completions.len() - 1
            } else {
                idx - 1
            });
        }
    }

    /// Move the highlighted row down, wrapping back to row 0 from the end.
    pub fn move_selection_down(&mut self) {
        if let Some(idx) = self.selected_idx
            && !self.completions.is_empty()
        {
            self.selected_idx = Some((idx + 1) % self.completions.len());
        }
    }

    /// The currently highlighted completion, or `None` when the list is empty.
    pub fn selected_completion(&self) -> Option<&str> {
        self.selected_idx
            .and_then(|i| self.completions.get(i))
            .map(String::as_str)
    }

    /// The full completion list and the currently highlighted row.
    pub fn visible_completions(&self) -> (&[String], Option<usize>) {
        (&self.completions, self.selected_idx)
    }
}

/// Expand a leading `~` or `~/` to the user's home directory.
pub(super) fn expand_tilde(path: &str) -> String {
    if (path == "~" || path.starts_with("~/"))
        && let Some(home) = home_dir()
    {
        let rest = &path[1..]; // "" or "/..."
        return format!("{}{}", home.display(), rest);
    }
    path.to_string()
}

/// If the original input used `~`, re-collapse the home prefix.
fn maybe_unexpand_tilde(original: &str, expanded: &str) -> String {
    if original.starts_with('~')
        && let Some(home) = home_dir()
    {
        let home_str = home.display().to_string();
        if let Some(rest) = expanded.strip_prefix(&home_str) {
            return format!("~{}", rest);
        }
    }
    expanded.to_string()
}

/// List directories inside `parent` whose names start with `partial`.
///
/// `value` is the full expanded path typed so far. We split it into
/// `(parent_dir, partial_name)` at the last `/`.
fn list_matching_dirs(value: &str) -> Vec<String> {
    let (parent, partial) = split_path(value);

    let parent_path = if parent.is_empty() {
        Path::new(".")
    } else {
        Path::new(&parent)
    };

    let Ok(entries) = std::fs::read_dir(parent_path) else {
        return Vec::new();
    };

    let mut matches: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_type()
                .map(|ft| ft.is_dir() || ft.is_symlink())
                .unwrap_or(false)
        })
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with(partial) {
                // For symlinks, verify the target is a directory
                if e.file_type().map(|ft| ft.is_symlink()).unwrap_or(false) && !e.path().is_dir() {
                    return None;
                }
                let full = if parent.is_empty() {
                    name
                } else if parent.ends_with('/') {
                    format!("{}{}", parent, name)
                } else {
                    format!("{}/{}", parent, name)
                };
                Some(full)
            } else {
                None
            }
        })
        .collect();

    matches.sort();
    matches
}

/// Split a path into (parent_dir, partial_name) at the last `/`.
///
/// Examples:
/// - `/home/user/pro` → (`/home/user`, `pro`)
/// - `/home/user/` → (`/home/user/`, ``)
/// - `pro` → (``, `pro`)
fn split_path(value: &str) -> (&str, &str) {
    match value.rfind('/') {
        Some(pos) => (&value[..=pos], &value[pos + 1..]),
        None => ("", value),
    }
}

/// Longest common prefix of a set of strings.
fn longest_common_prefix(strings: &[String]) -> String {
    if strings.is_empty() {
        return String::new();
    }

    let first = &strings[0];
    let mut len = first.len();

    for s in &strings[1..] {
        len = len.min(s.len());
        for (i, (a, b)) in first.bytes().zip(s.bytes()).enumerate() {
            if a != b {
                len = len.min(i);
                break;
            }
        }
    }

    first[..len].to_string()
}

/// Get the user's home directory.
fn home_dir() -> Option<PathBuf> {
    // Use directories crate (already a dependency)
    directories::BaseDirs::new().map(|bd| bd.home_dir().to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Helper: create a temp dir with the given subdirectories.
    fn setup_dirs(names: &[&str]) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        for name in names {
            fs::create_dir_all(tmp.path().join(name)).unwrap();
        }
        tmp
    }

    #[test]
    fn single_match_completes_with_trailing_slash() {
        let tmp = setup_dirs(&["projects"]);
        let mut c = PathCompleter::new();

        let input = format!("{}/pro", tmp.path().display());
        let result = c.complete(&input);
        assert_eq!(result, format!("{}/projects/", tmp.path().display()));
    }

    #[test]
    fn no_match_returns_unchanged() {
        let tmp = setup_dirs(&["alpha"]);
        let mut c = PathCompleter::new();

        let input = format!("{}/zzz", tmp.path().display());
        let result = c.complete(&input);
        assert_eq!(result, input);
    }

    #[test]
    fn multiple_matches_completes_to_common_prefix() {
        let tmp = setup_dirs(&["project-a", "project-b"]);
        let mut c = PathCompleter::new();

        let input = format!("{}/pro", tmp.path().display());
        let result = c.complete(&input);
        assert_eq!(result, format!("{}/project-", tmp.path().display()));
    }

    #[test]
    fn repeated_tab_at_common_prefix_is_noop() {
        // Regression guard: the old impl cycled through matches on repeat
        // Tab. The new one delegates cycling to arrow keys, so Tab at the
        // common prefix must leave the value alone.
        let tmp = setup_dirs(&["project-a", "project-b"]);
        let mut c = PathCompleter::new();

        let input = format!("{}/project-", tmp.path().display());
        let r1 = c.complete(&input);
        assert_eq!(r1, input, "already at LCP — Tab should not extend");

        let r2 = c.complete(&r1);
        assert_eq!(r2, r1, "second Tab should not cycle");
    }

    #[test]
    fn trailing_slash_keeps_value_on_tab_but_populates_list() {
        // With `<tmp>/` as input, LCP of the matches ("aaa", "bbb") is
        // `<tmp>/` — already the input — so Tab is a no-op. The list still
        // contains the children, so arrow-nav + Enter can pick one.
        let tmp = setup_dirs(&["aaa", "bbb"]);
        let mut c = PathCompleter::new();

        let input = format!("{}/", tmp.path().display());
        let result = c.complete(&input);
        assert_eq!(result, input);
        let (completions, _) = c.visible_completions();
        assert_eq!(completions.len(), 2);
    }

    #[test]
    fn nonexistent_parent_returns_unchanged() {
        let mut c = PathCompleter::new();
        let input = "/nonexistent_surely_xyz_123/foo";
        let result = c.complete(input);
        assert_eq!(result, input);
    }

    #[test]
    fn hidden_dirs_are_included() {
        let tmp = setup_dirs(&[".hidden", "visible"]);
        let mut c = PathCompleter::new();

        let input = format!("{}/.h", tmp.path().display());
        let result = c.complete(&input);
        assert_eq!(result, format!("{}/.hidden/", tmp.path().display()));
    }

    #[test]
    fn tilde_expansion() {
        // We can only test the expand/unexpand helpers since home dir varies
        let home = home_dir().unwrap();

        assert_eq!(expand_tilde("~"), home.display().to_string());
        assert_eq!(expand_tilde("~/foo"), format!("{}/foo", home.display()));
        assert_eq!(expand_tilde("/absolute"), "/absolute");
        assert_eq!(expand_tilde("relative"), "relative");
    }

    #[test]
    fn tilde_is_preserved_in_output() {
        let home = home_dir().unwrap();
        let expanded = format!("{}/Documents", home.display());
        let result = maybe_unexpand_tilde("~/Doc", &expanded);
        assert_eq!(result, "~/Documents");
    }

    #[test]
    fn longest_common_prefix_works() {
        assert_eq!(
            longest_common_prefix(&["abc".into(), "abd".into(), "abx".into()]),
            "ab"
        );
        assert_eq!(longest_common_prefix(&["hello".into()]), "hello");
        assert_eq!(longest_common_prefix(&["a".into(), "b".into()]), "");
    }

    #[test]
    fn files_are_excluded() {
        let tmp = setup_dirs(&["dir_a"]);
        // Create a regular file
        fs::write(tmp.path().join("file_a"), "content").unwrap();

        let mut c = PathCompleter::new();
        let input = format!("{}/", tmp.path().display());
        let result = c.complete(&input);
        // Only dir_a should match, not file_a
        assert_eq!(result, format!("{}/dir_a/", tmp.path().display()));
    }

    // ------------------------------------------------------------------------
    // Live-filter behaviour (new — drives the PathInput modal's visible list)
    // ------------------------------------------------------------------------

    #[test]
    fn refilter_populates_list_and_selects_first() {
        let tmp = setup_dirs(&["aaa", "bbb", "ccc"]);
        let mut c = PathCompleter::new();

        let input = format!("{}/", tmp.path().display());
        c.refilter(&input);

        let (completions, selected) = c.visible_completions();
        assert_eq!(completions.len(), 3);
        assert_eq!(selected, Some(0), "fresh list should highlight row 0");
        assert_eq!(
            c.selected_completion().map(str::to_string),
            Some(format!("{}/aaa", tmp.path().display())),
        );
    }

    #[test]
    fn refilter_clears_selection_when_no_matches() {
        let tmp = setup_dirs(&["alpha"]);
        let mut c = PathCompleter::new();

        c.refilter(&format!("{}/zzz", tmp.path().display()));

        let (completions, selected) = c.visible_completions();
        assert!(completions.is_empty());
        assert_eq!(selected, None);
        assert!(c.selected_completion().is_none());
    }

    #[test]
    fn move_selection_wraps_both_directions() {
        let tmp = setup_dirs(&["aaa", "bbb", "ccc"]);
        let mut c = PathCompleter::new();
        c.refilter(&format!("{}/", tmp.path().display()));

        // Start at 0. Up wraps to last.
        c.move_selection_up();
        assert_eq!(c.visible_completions().1, Some(2));

        // Down from last wraps to 0.
        c.move_selection_down();
        assert_eq!(c.visible_completions().1, Some(0));

        // Down again → 1.
        c.move_selection_down();
        assert_eq!(c.visible_completions().1, Some(1));
    }

    #[test]
    fn move_selection_on_empty_list_is_noop() {
        let mut c = PathCompleter::new();
        c.move_selection_up();
        c.move_selection_down();
        assert_eq!(c.visible_completions().1, None);
    }

    #[test]
    fn refilter_after_typing_reselects_row_zero() {
        // Guards the "Char → refilter → scroll = 0" contract the modal
        // relies on: every keystroke yields a fresh list and puts the
        // selection at the top, regardless of where it was before.
        let tmp = setup_dirs(&["aaa", "bbb"]);
        let mut c = PathCompleter::new();
        c.refilter(&format!("{}/", tmp.path().display()));
        c.move_selection_down();
        assert_eq!(c.visible_completions().1, Some(1));

        // User types a character that narrows the list
        c.refilter(&format!("{}/b", tmp.path().display()));
        assert_eq!(c.visible_completions().1, Some(0));
        assert_eq!(
            c.selected_completion().map(str::to_string),
            Some(format!("{}/bbb", tmp.path().display()))
        );
    }
}
