//! Input handling: keyboard events, modal keys, and command dispatch.

use super::*;

/// One screen's worth of scroll for the Help modal. Approximate — the
/// render pass clamps against the real content-area height each frame.
const HELP_PAGE: u16 = 10;

/// Outcome of interpreting a key inside the Help modal.
#[derive(Debug, PartialEq, Eq)]
enum HelpKey {
    ScrollBy(i16),
    Home,
    End,
    Close,
    Ignore,
}

/// Classify a key press within the Help modal. Kept as a free function
/// so it can be unit-tested without constructing an `App`.
///
/// Raw `KeyCode` matches take precedence over `kb.resolve` so modal-native
/// keys (arrows, Enter, Esc) are not shadowed by global bindings like
/// `NavigateUp`/`Submit`. `kb.resolve` fills in configured scroll bindings
/// — notably the default `Ctrl-u`/`Ctrl-d` for `PageUp`/`PageDown`.
fn classify_help_key(key: &crossterm::event::KeyEvent, kb: &crate::config::KeyBindings) -> HelpKey {
    use crossterm::event::KeyCode;

    match key.code {
        KeyCode::Up => return HelpKey::ScrollBy(-1),
        KeyCode::Down => return HelpKey::ScrollBy(1),
        KeyCode::PageUp => return HelpKey::ScrollBy(-(HELP_PAGE as i16)),
        KeyCode::PageDown => return HelpKey::ScrollBy(HELP_PAGE as i16),
        KeyCode::Home => return HelpKey::Home,
        KeyCode::End => return HelpKey::End,
        KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') | KeyCode::Char('?') => {
            return HelpKey::Close;
        }
        _ => {}
    }

    match kb.resolve(key) {
        Some(BindableAction::ScrollUp) => HelpKey::ScrollBy(-1),
        Some(BindableAction::ScrollDown) => HelpKey::ScrollBy(1),
        Some(BindableAction::PageUp) => HelpKey::ScrollBy(-(HELP_PAGE as i16)),
        Some(BindableAction::PageDown) => HelpKey::ScrollBy(HELP_PAGE as i16),
        _ => HelpKey::Ignore,
    }
}

impl App {
    pub(super) async fn handle_input(&mut self, input: InputEvent) {
        match input {
            InputEvent::Key(key) => {
                debug!(
                    "Key event: code={:?} modifiers={:?} kind={:?}",
                    key.code, key.modifiers, key.kind
                );

                // Suppress stray bytes from unrecognized escape sequences.
                // When crossterm can't parse a multi-byte sequence (e.g. from
                // modifier combos the terminal encodes as CSI), it emits each
                // byte as a separate key event ~8ms apart.  We suppress all
                // events for a short window after an unrecognized one.
                let now = Instant::now();
                if now < self.suppress_keys_until {
                    debug!("Suppressing key event (escape sequence cooldown)");
                    return;
                }

                // Check for modal-specific handling first
                if !matches!(self.ui_state.modal, Modal::None) {
                    self.handle_modal_key(key).await;
                    return;
                }

                // Check for configurable leader key (quick-switch).
                // Shift+<leader> opens directly in command-only mode
                // (VSCode-style command palette). We check the Shift-variant
                // first so it wins when the leader itself carries no Shift.
                let (leader_code, leader_mods) = self.config.parse_leader_key();
                if key.code == leader_code
                    && key.modifiers == (leader_mods | crossterm::event::KeyModifiers::SHIFT)
                    && !leader_mods.contains(crossterm::event::KeyModifiers::SHIFT)
                {
                    self.open_quick_switch_with_mode(PaletteMode::CommandOnly)
                        .await;
                    return;
                }
                if key.code == leader_code && key.modifiers == leader_mods {
                    self.open_quick_switch_with_mode(PaletteMode::Unified).await;
                    return;
                }

                // Number-jump: intercept digit keys to select by session number.
                if let crossterm::event::KeyCode::Char(c @ '0'..='9') = key.code
                    && key.modifiers.is_empty()
                {
                    let digit = c as u8 - b'0';
                    if let crate::tui::digit_accumulator::DigitResult::Jump(n) =
                        self.digit_accumulator.press(digit)
                    {
                        self.jump_to_session_number(n);
                    }
                    return;
                }

                // Convert to command and handle
                match UserCommand::from_key(key, &self.config.keybindings) {
                    Some(cmd) => self.handle_command(cmd).await,
                    None => {
                        // Unrecognized key event — likely the start of a
                        // broken escape sequence.  Suppress further events
                        // briefly so trailing bytes don't trigger commands.
                        self.suppress_keys_until = now + Duration::from_millis(50);
                    }
                }
            }
            InputEvent::Resize(_, _) => {
                // Terminal will re-render automatically
            }
            InputEvent::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollUp => {
                    self.scroll_pane_at(mouse.column, ScrollDirection::Up);
                }
                MouseEventKind::ScrollDown => {
                    self.scroll_pane_at(mouse.column, ScrollDirection::Down);
                }
                _ => {}
            },
            InputEvent::Paste(text) => {
                // Handle paste in modal input, ignore otherwise
                let clean = text.replace(['\n', '\r'], "");
                match &mut self.ui_state.modal {
                    Modal::Input { value, .. } => {
                        value.push_str(&clean);
                    }
                    Modal::PathInput {
                        value,
                        completer,
                        scroll,
                        ..
                    } => {
                        value.push_str(&clean);
                        completer.refilter(value);
                        *scroll = 0;
                    }
                    _ => {}
                }
            }
        }
    }

    /// Handle modal key input
    pub(super) async fn handle_modal_key(&mut self, key: crossterm::event::KeyEvent) {
        use crossterm::event::KeyCode;

        match &mut self.ui_state.modal {
            Modal::Input {
                value, on_submit, ..
            } => match key.code {
                KeyCode::Enter => {
                    let action = on_submit.clone();
                    let value = value.clone();
                    self.ui_state.modal = Modal::None;
                    self.handle_input_submit(action, value).await;
                }
                KeyCode::Esc => {
                    self.ui_state.modal = Modal::None;
                }
                KeyCode::Backspace => {
                    value.pop();
                }
                KeyCode::Char(c) => {
                    value.push(c);
                }
                _ => {}
            },

            Modal::PathInput {
                value,
                on_submit,
                completer,
                scroll,
                ..
            } => {
                use crate::config::keybindings::BindableAction;

                // Resolve configurable bindings first so arrow keys (and
                // their j/k/Ctrl-n/p aliases) navigate the completion list.
                match self.config.keybindings.resolve(&key) {
                    Some(BindableAction::NavigateUp) => {
                        completer.move_selection_up();
                        if let (_, Some(idx)) = completer.visible_completions() {
                            *scroll = super::actions::adjust_list_scroll(
                                idx,
                                *scroll,
                                super::actions::LIST_MAX_VISIBLE,
                            );
                        }
                    }
                    Some(BindableAction::NavigateDown) => {
                        completer.move_selection_down();
                        if let (_, Some(idx)) = completer.visible_completions() {
                            *scroll = super::actions::adjust_list_scroll(
                                idx,
                                *scroll,
                                super::actions::LIST_MAX_VISIBLE,
                            );
                        }
                    }
                    _ => match key.code {
                        KeyCode::Enter => {
                            // Prefer the highlighted completion over the
                            // typed value, so arrow-to-select-then-Enter
                            // works without first pressing Tab. Fall back
                            // to the typed value when the list is empty
                            // (e.g. the user typed a path that doesn't
                            // exist yet).
                            let action = on_submit.clone();
                            let submit_value = completer
                                .selected_completion()
                                .map(str::to_string)
                                .unwrap_or_else(|| value.clone());
                            self.ui_state.modal = Modal::None;
                            self.handle_input_submit(action, submit_value).await;
                        }
                        KeyCode::Esc => {
                            self.ui_state.modal = Modal::None;
                        }
                        KeyCode::Tab => {
                            // Tab extends the input to the longest common
                            // prefix. A single match completes fully + `/`
                            // and `refilter` below surfaces that dir's
                            // children so the user can keep drilling in.
                            *value = completer.complete(value);
                            completer.refilter(value);
                            *scroll = 0;
                        }
                        KeyCode::Backspace => {
                            value.pop();
                            completer.refilter(value);
                            *scroll = 0;
                        }
                        KeyCode::Char(c) => {
                            value.push(c);
                            completer.refilter(value);
                            *scroll = 0;
                        }
                        _ => {}
                    },
                }
            }

            Modal::Confirm { on_confirm, .. } => match key.code {
                KeyCode::Enter => {
                    let action = on_confirm.clone();
                    self.ui_state.modal = Modal::None;
                    self.handle_confirm(action).await;
                }
                KeyCode::Esc => {
                    self.ui_state.modal = Modal::None;
                }
                _ => {}
            },

            Modal::Loading { .. } => {
                // Non-interactive — swallow all keys while loading
            }

            Modal::Help { scroll } => match classify_help_key(&key, &self.config.keybindings) {
                HelpKey::ScrollBy(n) => {
                    *scroll = scroll.saturating_add_signed(n);
                }
                HelpKey::Home => {
                    *scroll = 0;
                }
                HelpKey::End => {
                    *scroll = u16::MAX;
                }
                HelpKey::Close => {
                    self.ui_state.modal = Modal::None;
                }
                HelpKey::Ignore => {}
            },

            Modal::Error { .. } => {
                // Any key closes the error modal.
                self.ui_state.modal = Modal::None;
            }

            Modal::Settings(_) => {
                // Extract the state to avoid borrow conflict with &mut self
                let state = match std::mem::replace(&mut self.ui_state.modal, Modal::None) {
                    Modal::Settings(s) => s,
                    _ => unreachable!(),
                };
                self.handle_settings_key(key, state).await;
            }

            Modal::QuickSwitch {
                query,
                matches,
                selected_idx,
                scroll,
                ..
            } => {
                use crate::config::keybindings::BindableAction;

                // Resolve configurable bindings first for navigation
                match self.config.keybindings.resolve(&key) {
                    Some(BindableAction::NavigateUp) => {
                        if !matches.is_empty() {
                            *selected_idx = if *selected_idx == 0 {
                                matches.len() - 1
                            } else {
                                *selected_idx - 1
                            };
                            *scroll = super::actions::adjust_list_scroll(
                                *selected_idx,
                                *scroll,
                                super::actions::LIST_MAX_VISIBLE,
                            );
                        }
                    }
                    Some(BindableAction::NavigateDown) => {
                        if !matches.is_empty() {
                            *selected_idx = (*selected_idx + 1) % matches.len();
                            *scroll = super::actions::adjust_list_scroll(
                                *selected_idx,
                                *scroll,
                                super::actions::LIST_MAX_VISIBLE,
                            );
                        }
                    }
                    _ => match key.code {
                        KeyCode::Esc => {
                            self.ui_state.modal = Modal::None;
                        }
                        KeyCode::Enter => {
                            // Clone the selected item so we can release the
                            // borrow on `matches` before we mutate `modal`
                            // and dispatch.
                            let selected = matches.get(*selected_idx).cloned();
                            match selected {
                                Some(QuickSwitchItem::Session(m)) => {
                                    let session_id = m.session_id;
                                    self.ui_state.modal = Modal::None;
                                    self.ui_state.selected_session_id = Some(session_id);
                                    if let Some(idx) =
                                        self.ui_state.list_items.iter().position(|item| {
                                            matches!(item, SessionListItem::Worktree { id, .. } if *id == session_id)
                                        })
                                    {
                                        self.ui_state.list_state.select(Some(idx));
                                    }
                                    self.update_selection();
                                    self.handle_select().await;
                                }
                                Some(QuickSwitchItem::Command(entry)) => {
                                    self.ui_state.modal = Modal::None;
                                    self.handle_command(entry.action.into()).await;
                                }
                                Some(QuickSwitchItem::SectionMove {
                                    session_id, target, ..
                                }) => {
                                    self.ui_state.modal = Modal::None;
                                    self.apply_section_move(session_id, target).await;
                                }
                                None => {}
                            }
                        }
                        KeyCode::Tab => {
                            // Tab autocompletes a session title into the
                            // query for further refinement. For command rows
                            // this is meaningless, so skip.
                            if let Some(QuickSwitchItem::Session(m)) =
                                matches.get(*selected_idx).cloned()
                            {
                                *query = m.title;
                                self.refilter_quick_switch();
                            }
                        }
                        KeyCode::Backspace => {
                            query.pop();
                            self.refilter_quick_switch();
                        }
                        KeyCode::Char(c) => {
                            query.push(c);
                            self.refilter_quick_switch();
                        }
                        _ => {}
                    },
                }
            }

            Modal::CheckoutBranch {
                query,
                all_branches: _,
                filtered,
                selected_idx,
                scroll,
                ..
            } => {
                use crate::config::keybindings::BindableAction;

                // Resolve configurable bindings first for navigation
                match self.config.keybindings.resolve(&key) {
                    Some(BindableAction::NavigateUp) => {
                        if !filtered.is_empty() {
                            *selected_idx = if *selected_idx == 0 {
                                filtered.len() - 1
                            } else {
                                *selected_idx - 1
                            };
                            // Ensure selection stays visible
                            if *selected_idx < *scroll {
                                *scroll = *selected_idx;
                            }
                        }
                    }
                    Some(BindableAction::NavigateDown) => {
                        if !filtered.is_empty() {
                            *selected_idx = (*selected_idx + 1) % filtered.len();
                            // Scroll forward when running off the bottom; a
                            // conservative window of 10 rows keeps the selection
                            // visible without knowing the exact pane height here.
                            let visible_rows: usize = 10;
                            if *selected_idx >= scroll.saturating_add(visible_rows) {
                                *scroll = selected_idx.saturating_sub(visible_rows - 1);
                            }
                            if *selected_idx < *scroll {
                                *scroll = *selected_idx;
                            }
                        }
                    }
                    _ => match key.code {
                        KeyCode::Esc => {
                            self.ui_state.modal = Modal::None;
                        }
                        KeyCode::Enter => {
                            // Decide which branch to check out:
                            //   - If filter produced matches, use the highlighted
                            //     one (even when the user has typed something).
                            //   - Otherwise fall back to the raw query text so a
                            //     pasted branch name still works.
                            let branch_label = if let Some(m) = filtered.get(*selected_idx) {
                                m.local_name.clone()
                            } else {
                                let trimmed = query.trim();
                                if trimmed.is_empty() {
                                    return;
                                }
                                // Strip a leading "origin/" so we always get a
                                // local branch name.
                                trimmed
                                    .strip_prefix("origin/")
                                    .unwrap_or(trimmed)
                                    .to_string()
                            };
                            let project_id = match &self.ui_state.modal {
                                Modal::CheckoutBranch { project_id, .. } => *project_id,
                                _ => return,
                            };
                            self.ui_state.modal = Modal::None;
                            self.start_checkout_session(project_id, branch_label).await;
                        }
                        KeyCode::Backspace => {
                            query.pop();
                            self.refilter_checkout_branches();
                        }
                        KeyCode::Char(c) => {
                            query.push(c);
                            self.refilter_checkout_branches();
                        }
                        _ => {}
                    },
                }
            }

            Modal::None => {}
        }
    }

    /// Handle a user command
    pub(super) async fn handle_command(&mut self, cmd: UserCommand) {
        match cmd {
            UserCommand::NavigateUp => {
                self.ui_state.list_state.previous();
            }
            UserCommand::NavigateDown => {
                self.ui_state.list_state.next();
            }
            UserCommand::Select => {
                self.handle_select().await;
            }
            UserCommand::SelectShell => {
                self.handle_select_shell().await;
            }
            UserCommand::NewSession => {
                self.handle_new_session();
            }
            UserCommand::NewStackedSession => {
                self.handle_new_stacked_session().await;
            }
            UserCommand::CascadeMergeMain => {
                self.handle_cascade_merge_main().await;
            }
            UserCommand::CascadeResume => {
                self.handle_cascade_resume().await;
            }
            UserCommand::CascadeAbandon => {
                self.handle_cascade_abandon().await;
            }
            UserCommand::PushStack => {
                self.handle_push_stack();
            }
            UserCommand::CheckoutBranch => {
                self.handle_checkout_branch().await;
            }
            UserCommand::NewProject => {
                self.open_path_input(
                    "Add Project".to_string(),
                    "Enter path to git repository:".to_string(),
                    InputAction::AddProject,
                );
            }
            UserCommand::ScanDirectory => {
                self.open_path_input(
                    "Scan Directory".to_string(),
                    "Enter directory to scan for git repos:".to_string(),
                    InputAction::ScanDirectory,
                );
            }
            UserCommand::DeleteSession => {
                self.handle_delete_session();
            }
            UserCommand::RenameSession => {
                self.handle_rename_session().await;
            }
            UserCommand::MoveToSection => {
                self.handle_move_to_section().await;
            }
            UserCommand::RestartSession => {
                self.handle_restart_session();
            }
            UserCommand::RemoveProject => {
                self.handle_remove_project();
            }
            UserCommand::OpenInEditor => {
                self.handle_open_in_editor().await;
            }
            UserCommand::OpenPullRequest => {
                self.handle_open_pull_request().await;
            }
            UserCommand::TogglePane => {
                let on_project = self.ui_state.selected_session_id.is_none()
                    && self.ui_state.selected_project_id.is_some();
                self.ui_state.right_pane_view = if on_project {
                    // Project: Shell → Info → Shell (no Preview)
                    match self.ui_state.right_pane_view {
                        RightPaneView::Shell => RightPaneView::Info,
                        _ => RightPaneView::Shell,
                    }
                } else {
                    // Session: Preview → Info → Shell → Preview
                    match self.ui_state.right_pane_view {
                        RightPaneView::Preview => RightPaneView::Info,
                        RightPaneView::Info => RightPaneView::Shell,
                        RightPaneView::Shell => RightPaneView::Preview,
                    }
                };
                self.ui_state.clear_right_pane = true;
                self.spawn_info_fetch();
            }
            UserCommand::TogglePaneReverse => {
                let on_project = self.ui_state.selected_session_id.is_none()
                    && self.ui_state.selected_project_id.is_some();
                self.ui_state.right_pane_view = if on_project {
                    // Project: Info → Shell → Info (no Preview)
                    match self.ui_state.right_pane_view {
                        RightPaneView::Info => RightPaneView::Shell,
                        _ => RightPaneView::Info,
                    }
                } else {
                    // Session: Shell → Info → Preview → Shell
                    match self.ui_state.right_pane_view {
                        RightPaneView::Preview => RightPaneView::Shell,
                        RightPaneView::Info => RightPaneView::Preview,
                        RightPaneView::Shell => RightPaneView::Info,
                    }
                };
                self.ui_state.clear_right_pane = true;
                self.spawn_info_fetch();
            }
            UserCommand::ShrinkLeftPane => {
                self.ui_state.left_pane_pct = self
                    .ui_state
                    .left_pane_pct
                    .saturating_sub(2)
                    .max(MIN_LEFT_PANE_PCT);
                self.save_left_pane_pct().await;
            }
            UserCommand::GrowLeftPane => {
                self.ui_state.left_pane_pct =
                    (self.ui_state.left_pane_pct + 2).min(MAX_LEFT_PANE_PCT);
                self.save_left_pane_pct().await;
            }
            UserCommand::ShowHelp => {
                self.ui_state.modal = Modal::Help { scroll: 0 };
            }
            UserCommand::ShowSettings => {
                let rows = self.build_settings_rows(SettingsTab::General);
                self.ui_state.modal = Modal::Settings(SettingsState {
                    tab: SettingsTab::General,
                    selected_row: 0,
                    editing: None,
                    rows,
                });
            }
            UserCommand::Quit => {
                self.ui_state.should_quit = true;
            }
            UserCommand::PageUp => self.active_pane_state().page_up(),
            UserCommand::PageDown => self.active_pane_state().page_down(),
            UserCommand::ScrollUp => self.active_pane_state().scroll_up(1),
            UserCommand::ScrollDown => self.active_pane_state().scroll_down(1),
            UserCommand::GenerateSummary => {
                // Context-specific: only works when Info pane is showing
                if self.ui_state.right_pane_view == RightPaneView::Info
                    && let Some(session_id) = self.ui_state.selected_session_id
                {
                    self.spawn_ai_summary_if_needed(session_id);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::KeyBindings;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    #[test]
    fn arrows_scroll_one_line() {
        let kb = KeyBindings::default();
        assert_eq!(
            classify_help_key(&key(KeyCode::Down), &kb),
            HelpKey::ScrollBy(1)
        );
        assert_eq!(
            classify_help_key(&key(KeyCode::Up), &kb),
            HelpKey::ScrollBy(-1)
        );
    }

    #[test]
    fn default_jk_bindings_scroll_one_line() {
        // Default KeyBindings bind j/k to NavigateDown/NavigateUp, not scroll,
        // so plain j/k should NOT produce ScrollBy here — they are ignored in
        // the Help modal. This pins the current default so a future remapping
        // doesn't silently change modal behavior.
        let kb = KeyBindings::default();
        assert_eq!(
            classify_help_key(&key(KeyCode::Char('j')), &kb),
            HelpKey::Ignore
        );
        assert_eq!(
            classify_help_key(&key(KeyCode::Char('k')), &kb),
            HelpKey::Ignore
        );
    }

    #[test]
    fn page_keys_scroll_by_page() {
        let kb = KeyBindings::default();
        let page = HELP_PAGE as i16;
        assert_eq!(
            classify_help_key(&key(KeyCode::PageDown), &kb),
            HelpKey::ScrollBy(page)
        );
        assert_eq!(
            classify_help_key(&key(KeyCode::PageUp), &kb),
            HelpKey::ScrollBy(-page)
        );
        // Default bindings: Ctrl-d / Ctrl-u for PageDown / PageUp.
        assert_eq!(
            classify_help_key(&ctrl(KeyCode::Char('d')), &kb),
            HelpKey::ScrollBy(page)
        );
        assert_eq!(
            classify_help_key(&ctrl(KeyCode::Char('u')), &kb),
            HelpKey::ScrollBy(-page)
        );
    }

    #[test]
    fn home_and_end_jump() {
        let kb = KeyBindings::default();
        assert_eq!(classify_help_key(&key(KeyCode::Home), &kb), HelpKey::Home);
        assert_eq!(classify_help_key(&key(KeyCode::End), &kb), HelpKey::End);
    }

    #[test]
    fn close_keys() {
        let kb = KeyBindings::default();
        for code in [
            KeyCode::Esc,
            KeyCode::Enter,
            KeyCode::Char('q'),
            KeyCode::Char('?'),
        ] {
            assert_eq!(
                classify_help_key(&key(code), &kb),
                HelpKey::Close,
                "{code:?}"
            );
        }
    }

    #[test]
    fn unrelated_key_is_ignored() {
        let kb = KeyBindings::default();
        assert_eq!(
            classify_help_key(&key(KeyCode::Char('x')), &kb),
            HelpKey::Ignore
        );
    }
}
