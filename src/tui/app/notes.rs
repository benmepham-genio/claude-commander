//! Notes tab inline editing: key handling and persistence.

use super::*;
use crossterm::event::{KeyCode, KeyEvent};

/// Compute the next right-pane view when `TogglePane` (forward cycle) fires.
///
/// On projects the cycle is `Shell ↔ Info` only — Preview and Notes have no
/// project content. On sessions the full cycle is
/// `Preview → Info → Shell → Notes → Preview`.
pub(super) fn next_pane(current: RightPaneView, on_project: bool) -> RightPaneView {
    if on_project {
        match current {
            RightPaneView::Shell => RightPaneView::Info,
            _ => RightPaneView::Shell,
        }
    } else {
        match current {
            RightPaneView::Preview => RightPaneView::Info,
            RightPaneView::Info => RightPaneView::Shell,
            RightPaneView::Shell => RightPaneView::Notes,
            RightPaneView::Notes => RightPaneView::Preview,
        }
    }
}

/// Compute the previous right-pane view when `TogglePaneReverse` fires.
pub(super) fn prev_pane(current: RightPaneView, on_project: bool) -> RightPaneView {
    if on_project {
        match current {
            RightPaneView::Info => RightPaneView::Shell,
            _ => RightPaneView::Info,
        }
    } else {
        match current {
            RightPaneView::Preview => RightPaneView::Notes,
            RightPaneView::Info => RightPaneView::Preview,
            RightPaneView::Shell => RightPaneView::Info,
            RightPaneView::Notes => RightPaneView::Shell,
        }
    }
}

impl App {
    /// Handle a key event while the Notes tab is active and focused.
    ///
    /// Returns `true` if the key was consumed (caller should not dispatch
    /// it further), `false` to let normal command dispatch run.
    pub(super) async fn handle_notes_key(&mut self, key: KeyEvent) -> bool {
        if self.ui_state.notes_editing {
            match key.code {
                KeyCode::Esc => {
                    self.commit_notes_edit().await;
                }
                KeyCode::Backspace => {
                    self.ui_state.notes_draft.pop();
                }
                KeyCode::Enter => {
                    self.ui_state.notes_draft.push('\n');
                }
                KeyCode::Tab => {
                    self.ui_state.notes_draft.push('\t');
                }
                KeyCode::Char(c) => {
                    self.ui_state.notes_draft.push(c);
                }
                _ => {}
            }
            return true;
        }

        // Not editing — entering edit mode requires a session selection.
        if !matches!(key.code, KeyCode::Char('i') | KeyCode::Char('e')) {
            return false;
        }
        let Some(session_id) = self.ui_state.selected_session_id else {
            return false;
        };

        let current = {
            let state = self.store.read().await;
            state
                .sessions
                .get(&session_id)
                .map(|s| s.notes.clone())
                .unwrap_or_default()
        };
        self.ui_state.notes_draft = current;
        self.ui_state.notes_editing = true;
        true
    }

    /// Commit the current `notes_draft` to the selected session's persisted
    /// `notes` field and exit edit mode. No-op when not editing.
    pub(super) async fn commit_notes_edit(&mut self) {
        if !self.ui_state.notes_editing {
            return;
        }
        self.ui_state.notes_editing = false;
        let Some(session_id) = self.ui_state.selected_session_id else {
            return;
        };
        let draft = std::mem::take(&mut self.ui_state.notes_draft);
        if let Err(e) = self
            .store
            .mutate(move |state| {
                if let Some(session) = state.get_session_mut(&session_id) {
                    session.notes = draft;
                }
            })
            .await
        {
            warn!("Failed to persist notes: {}", e);
        }
    }

    /// Convenience: commit notes if currently editing, otherwise no-op.
    /// Called from places that change context (tab switch, selection change).
    pub(super) async fn commit_notes_edit_if_active(&mut self) {
        if self.ui_state.notes_editing {
            self.commit_notes_edit().await;
        }
    }

    /// Sync variant of `commit_notes_edit_if_active` for callers that cannot
    /// await (e.g. sync `update_selection` path). Captures the draft + target
    /// session and spawns the persist call as fire-and-forget.
    pub(super) fn spawn_commit_notes_if_active(&mut self, target_session: SessionId) {
        if !self.ui_state.notes_editing {
            return;
        }
        self.ui_state.notes_editing = false;
        let draft = std::mem::take(&mut self.ui_state.notes_draft);
        let store = self.store.clone();
        tokio::spawn(async move {
            if let Err(e) = store
                .mutate(move |state| {
                    if let Some(session) = state.get_session_mut(&target_session) {
                        session.notes = draft;
                    }
                })
                .await
            {
                warn!("Failed to persist notes (spawn): {}", e);
            }
        });
    }
}
