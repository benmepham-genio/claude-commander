//! Input handling: keyboard events, modal keys, and command dispatch.

use super::*;

/// One screen's worth of scroll for the Help modal. Approximate — the
/// render pass clamps against the real content-area height each frame.
const HELP_PAGE: u16 = 10;

/// A scroll/close action shared by the read-only scrolling modals (Help and
/// Info). Both classifiers map their keys onto this, and
/// [`apply_scroll_action`] applies it uniformly.
#[derive(Debug, PartialEq, Eq)]
enum ScrollAction {
    ScrollBy(i16),
    Home,
    End,
    Close,
}

/// Apply a [`ScrollAction`] to a modal's scroll offset. Returns `true` when the
/// action was `Close`, i.e. the caller should dismiss the modal. `End` uses
/// `u16::MAX` as a sentinel that the render pass clamps to the real content
/// height each frame.
fn apply_scroll_action(scroll: &mut u16, action: ScrollAction) -> bool {
    match action {
        ScrollAction::ScrollBy(n) => {
            *scroll = scroll.saturating_add_signed(n);
            false
        }
        ScrollAction::Home => {
            *scroll = 0;
            false
        }
        ScrollAction::End => {
            *scroll = u16::MAX;
            false
        }
        ScrollAction::Close => true,
    }
}

/// Classify a key press within the Help modal. Kept as a free function
/// so it can be unit-tested without constructing an `App`. Returns `None` when
/// the key is ignored.
///
/// Raw `KeyCode` matches take precedence over `kb.resolve` so modal-native
/// keys (arrows, Enter, Esc) are not shadowed by global bindings like
/// `NavigateUp`/`Submit`. `kb.resolve` fills in configured scroll bindings
/// — notably the default `Ctrl-u`/`Ctrl-d` for `PageUp`/`PageDown`.
fn classify_help_key(
    key: &crossterm::event::KeyEvent,
    kb: &claude_commander_core::config::KeyBindings,
) -> Option<ScrollAction> {
    use crossterm::event::KeyCode;

    match key.code {
        KeyCode::Up => return Some(ScrollAction::ScrollBy(-1)),
        KeyCode::Down => return Some(ScrollAction::ScrollBy(1)),
        KeyCode::PageUp => return Some(ScrollAction::ScrollBy(-(HELP_PAGE as i16))),
        KeyCode::PageDown => return Some(ScrollAction::ScrollBy(HELP_PAGE as i16)),
        KeyCode::Home => return Some(ScrollAction::Home),
        KeyCode::End => return Some(ScrollAction::End),
        KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') | KeyCode::Char('?') => {
            return Some(ScrollAction::Close);
        }
        _ => {}
    }

    match kb.resolve(key) {
        Some(BindableAction::ScrollUp) => Some(ScrollAction::ScrollBy(-1)),
        Some(BindableAction::ScrollDown) => Some(ScrollAction::ScrollBy(1)),
        Some(BindableAction::PageUp | BindableAction::ListPageUp) => {
            Some(ScrollAction::ScrollBy(-(HELP_PAGE as i16)))
        }
        Some(BindableAction::PageDown | BindableAction::ListPageDown) => {
            Some(ScrollAction::ScrollBy(HELP_PAGE as i16))
        }
        _ => None,
    }
}

/// Outcome of interpreting a key inside the Info modal: either a shared
/// scroll/close action, the Info-only summary trigger, or ignored.
#[derive(Debug, PartialEq, Eq)]
enum InfoKey {
    Scroll(ScrollAction),
    GenerateSummary,
    Ignore,
}

/// Classify a key press within the Info modal. Kept as a free function so it
/// can be unit-tested without constructing an `App`.
///
/// Unlike the Help modal, `j`/`k` scroll here (the board's navigation bindings
/// don't apply while the modal is open); `g` triggers AI-summary generation and
/// `Esc`/`q`/`i` all close.
fn classify_info_key(key: &crossterm::event::KeyEvent) -> InfoKey {
    use crossterm::event::KeyCode;

    match key.code {
        KeyCode::Up | KeyCode::Char('k') => InfoKey::Scroll(ScrollAction::ScrollBy(-1)),
        KeyCode::Down | KeyCode::Char('j') => InfoKey::Scroll(ScrollAction::ScrollBy(1)),
        KeyCode::PageUp => InfoKey::Scroll(ScrollAction::ScrollBy(-(HELP_PAGE as i16))),
        KeyCode::PageDown => InfoKey::Scroll(ScrollAction::ScrollBy(HELP_PAGE as i16)),
        KeyCode::Home => InfoKey::Scroll(ScrollAction::Home),
        KeyCode::End => InfoKey::Scroll(ScrollAction::End),
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('i') => {
            InfoKey::Scroll(ScrollAction::Close)
        }
        KeyCode::Char('g') => InfoKey::GenerateSummary,
        _ => InfoKey::Ignore,
    }
}

/// Map a mouse position to a session-list row index, given the rects recorded
/// on the last frame. Kept as a free function so the arithmetic can be
/// unit-tested without constructing an `App`.
///
/// A click in the pinned recents panel maps straight to its row (indices
/// `0..recents_len`), regardless of how far the main list is scrolled; a click
/// in the scrolling main list maps to
/// `recents_len + main_list_offset + visible_row`. Returns `None` when the click
/// is outside both areas, when no frame has recorded a list rect yet, or when
/// the position maps past the last item.
///
/// The two rects never overlap, so a hit inside the recents panel is resolved
/// there and never falls through to the main list.
fn list_row_at(
    col: u16,
    row: u16,
    recents_rect: Option<Rect>,
    list_rect: Option<Rect>,
    recents_len: usize,
    main_list_offset: usize,
    item_count: usize,
) -> Option<usize> {
    let hit =
        |rect: Rect| col >= rect.x && col < rect.right() && row >= rect.y && row < rect.bottom();

    // Pinned recents panel (rendered at offset 0).
    if let Some(rect) = recents_rect
        && hit(rect)
    {
        let idx = (row - rect.y) as usize;
        return (idx < recents_len && idx < item_count).then_some(idx);
    }

    // Scrolling main list, below the recents panel (when present).
    let rect = list_rect?;
    if !hit(rect) {
        return None;
    }
    let idx = recents_len
        .checked_add(main_list_offset)?
        .checked_add((row - rect.y) as usize)?;
    (idx < item_count).then_some(idx)
}

/// Which filterable modal needs its filter recomputed after text lands in it (a paste or a dictation).
/// Used to defer the `&mut self` refilter call until after the
/// `&mut self.ui_state.modal` borrow has been released.
#[derive(Debug, PartialEq, Eq)]
enum Refilter {
    CheckoutBranch,
    QuickSwitch,
}

/// The open modal's live text field: where a paste or a dictated transcript
/// lands. One match ([`text_field`]) decides which modals have one, so paste
/// and dictation cannot drift apart on it.
enum TextField<'a> {
    /// A single-line field with nothing to recompute afterwards.
    Line(&'a mut Input),
    /// A single-line query whose filtered list the caller recomputes.
    Query(&'a mut Input, Refilter),
    /// The path field, which owns its completer and refilters inline.
    Path {
        value: &'a mut Input,
        completer: &'a mut PathCompleter,
        scroll: &'a mut usize,
    },
    /// The review view's open comment draft.
    Draft(&'a mut DiffReviewState),
    /// Settings → Theme: a colour picker, whose text is a hex colour.
    Colour(&'a mut colour_picker::ColourPicker),
}

/// The open modal's text field, or `None` when it has none. The review view
/// only has one while a comment draft is open.
fn text_field(modal: &mut Modal) -> Option<TextField<'_>> {
    Some(match modal {
        Modal::EditSession(editor) if editor.focus == 0 => TextField::Line(&mut editor.name),
        Modal::Input { value, .. } => TextField::Line(value),
        Modal::Conversation { input, .. } => TextField::Line(input),
        Modal::PathInput {
            value,
            completer,
            scroll,
            ..
        } => TextField::Path {
            value,
            completer,
            scroll,
        },
        Modal::CheckoutBranch { query, .. } => TextField::Query(query, Refilter::CheckoutBranch),
        Modal::QuickSwitch { query, .. } => TextField::Query(query, Refilter::QuickSwitch),
        Modal::ReviewDiff(state) if state.comment.is_some() => TextField::Draft(state),
        // A paste over a colour picker is a hex colour, wherever the picker's
        // focus is.
        Modal::Settings(SettingsState {
            editing: Some(super::SettingsEditing::Colour { picker }),
            ..
        }) => TextField::Colour(picker),
        _ => return None,
    })
}

/// The open modal's text field if dictation may type into it. Narrower than
/// [`text_field`] in two places: a masked field holds a secret, which is not
/// something to read aloud to a transcription server (nor could the user see
/// what was heard), and a colour picker takes hex, which nobody speaks.
fn dictation_field(modal: &mut Modal) -> Option<TextField<'_>> {
    if matches!(modal, Modal::Input { mask: true, .. }) {
        return None;
    }
    text_field(modal).filter(|f| !matches!(f, TextField::Colour(_)))
}

impl TextField<'_> {
    /// Insert `text` at the cursor. Newlines are stripped for every
    /// single-line field so a multi-line paste doesn't accidentally submit;
    /// the comment draft is multi-line capable and gets the raw text (newline
    /// handling lives in `paste_into_draft`). Returns the refilter the caller
    /// still owes, if any.
    fn insert(self, text: &str) -> Option<Refilter> {
        let clean = || text.replace(['\n', '\r'], "");
        match self {
            TextField::Line(value) => super::insert_into_input(value, &clean()),
            TextField::Query(query, refilter) => {
                super::insert_into_input(query, &clean());
                return Some(refilter);
            }
            TextField::Path {
                value,
                completer,
                scroll,
            } => {
                super::insert_into_input(value, &clean());
                completer.refilter(value.value());
                *scroll = 0;
            }
            TextField::Draft(state) => {
                state.paste_into_draft(text);
            }
            TextField::Colour(picker) => picker.paste(&clean()),
        }
        None
    }
}

/// Append clipboard text to the open modal's text field (see [`TextField`]).
/// Returns `Some(Refilter::…)` when the caller still needs to recompute a
/// filtered list via an `&mut self` helper; `None` when handling is complete
/// (or the modal has no text field).
fn apply_paste_to_modal(modal: &mut Modal, text: &str) -> Option<Refilter> {
    text_field(modal)?.insert(text)
}

/// Whether `key` should open the quick-switch palette: the configured leader
/// key, or `Ctrl+Space` — which works everywhere the palette can be reached
/// (the tree, an attached pane, the review view) whatever the leader is bound
/// to. Shared by the main dispatch and the review view so the two can't drift.
///
/// The review view calls this from the fallthrough of its own key match — so
/// only for keys it has no meaning for *in the current focus and sub-mode*, and
/// a leader rebound onto one of its letters keeps the review's meaning there.
/// That is per-mode, not absolute: `v` is bound in the diff body only, so a
/// leader of `v` opens the switcher from the file list, where `v` does nothing.
pub(super) fn opens_palette(
    key: &crossterm::event::KeyEvent,
    leader_code: crossterm::event::KeyCode,
    leader_mods: crossterm::event::KeyModifiers,
) -> bool {
    use crossterm::event::{KeyCode, KeyModifiers};
    (key.code == leader_code && key.modifiers == leader_mods)
        || (key.code == KeyCode::Char(' ') && key.modifiers == KeyModifiers::CONTROL)
}

/// Plain printable characters (no modifier, or Shift only) belong in a
/// fuzzy-search query box and must not be intercepted by global j/k →
/// NavigateUp/Down bindings. Other key combos (Ctrl/Alt, arrows, Tab, …)
/// return `None` and fall through to the configurable resolver.
fn palette_text_char(key: &crossterm::event::KeyEvent) -> Option<char> {
    use crossterm::event::{KeyCode, KeyModifiers};
    if let KeyCode::Char(c) = key.code
        && (key.modifiers - KeyModifiers::SHIFT).is_empty()
    {
        return Some(c);
    }
    None
}

/// Re-list the highlighted project's branches so the New Session dialog's
/// existing-branch collision hint tracks the project the user is now targeting.
/// Results are memoized per repo path in the picker, so listing runs at most
/// once per project rather than on every navigation keystroke.
fn refresh_branch_hint(
    existing_branches: &mut Option<Vec<String>>,
    picker: &mut super::ProjectPicker,
) {
    // A remote backend's picker disables the local gix hint (its repo path is
    // server-side); don't scan and don't fabricate a misleading hint.
    if !picker.branch_hint_enabled {
        *existing_branches = None;
        return;
    }
    let Some(path) = picker.selected_repo_path() else {
        *existing_branches = None;
        return;
    };
    *existing_branches = picker
        .branch_cache
        .entry(path.clone())
        .or_insert_with(|| super::actions::existing_branch_names(&path))
        .clone();
}

/// Result of routing a key through the New Session (`Modal::Input`) dialog.
#[derive(Debug, PartialEq, Eq)]
enum InputKeyOutcome {
    /// The key mutated in-modal state (focus, expansion, filter, name text) and
    /// the dialog stays open.
    Handled,
    /// Submit the dialog (create the session / apply the input action).
    Submit,
    /// Close the dialog without submitting.
    Cancel,
    /// The server picker confirmed a *different* backend; the caller must rebuild
    /// the project/program/section pickers for it (an async, `App`-level step).
    ServerChanged,
}

/// Pure key routing for the New Session dialog, mirroring `apply_paste_to_modal`
/// so it can be unit-tested without an `App`. Owns focus movement, dropdown
/// expand/collapse, project-filter editing, and name-field editing. Returns
/// `Submit`/`Cancel` for the two outcomes the caller must action (they need
/// `App` to run); everything else is `Handled`.
///
/// Interaction (collapsed): ↑/↓ and Tab/Shift+Tab move focus between the present
/// rows; Enter submits; Esc cancels; on a picker row Space/→ opens the dropdown,
/// and typing on the Project row opens it and starts filtering. Expanded: ↑/↓
/// navigate the picker, Enter/Space/→ confirm-and-collapse, Esc collapses, and
/// (Project only) characters filter.
fn handle_input_modal_key(modal: &mut Modal, key: crossterm::event::KeyEvent) -> InputKeyOutcome {
    use super::InputFocus;
    use crossterm::event::KeyCode;
    let Modal::Input {
        value,
        existing_branches,
        project_picker,
        program_picker,
        server_picker,
        section_picker,
        focus,
        expanded,
        ..
    } = modal
    else {
        return InputKeyOutcome::Handled;
    };

    // --- Expanded: keys drive the open dropdown. ---
    if *expanded {
        match focus {
            InputFocus::Project => {
                let Some(picker) = project_picker.as_mut() else {
                    *expanded = false;
                    return InputKeyOutcome::Handled;
                };
                match key.code {
                    KeyCode::Up => {
                        picker.select_up();
                        refresh_branch_hint(existing_branches, picker);
                    }
                    KeyCode::Down => {
                        picker.select_down();
                        refresh_branch_hint(existing_branches, picker);
                    }
                    KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Right | KeyCode::Esc => {
                        *expanded = false;
                    }
                    KeyCode::Backspace => {
                        if picker.filter.pop().is_some() {
                            picker.apply_filter();
                            refresh_branch_hint(existing_branches, picker);
                        }
                    }
                    _ => {
                        if let Some(c) = palette_text_char(&key) {
                            picker.filter.push(c);
                            picker.apply_filter();
                            refresh_branch_hint(existing_branches, picker);
                        }
                    }
                }
            }
            InputFocus::Program => {
                let Some(picker) = program_picker.as_mut() else {
                    *expanded = false;
                    return InputKeyOutcome::Handled;
                };
                match key.code {
                    KeyCode::Up => picker.select_up(),
                    KeyCode::Down => picker.select_down(),
                    KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Right | KeyCode::Esc => {
                        *expanded = false;
                    }
                    _ => {}
                }
            }
            InputFocus::Section => {
                let Some(picker) = section_picker.as_mut() else {
                    *expanded = false;
                    return InputKeyOutcome::Handled;
                };
                match key.code {
                    KeyCode::Up => picker.select_up(),
                    KeyCode::Down => picker.select_down(),
                    KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Right | KeyCode::Esc => {
                        *expanded = false;
                    }
                    _ => {}
                }
            }
            InputFocus::Server => {
                let Some(picker) = server_picker.as_mut() else {
                    *expanded = false;
                    return InputKeyOutcome::Handled;
                };
                match key.code {
                    KeyCode::Up => picker.select_up(),
                    KeyCode::Down => picker.select_down(),
                    KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Right => {
                        *expanded = false;
                        // A confirm that moved off the applied selection needs the
                        // caller to rebuild the dependent pickers for the new
                        // backend; re-confirming the same server is a no-op.
                        if picker.selected != picker.committed {
                            picker.committed = picker.selected;
                            return InputKeyOutcome::ServerChanged;
                        }
                    }
                    // Esc abandons the in-progress highlight, snapping back to the
                    // applied server so no rebuild is triggered.
                    KeyCode::Esc => {
                        picker.selected = picker.committed;
                        *expanded = false;
                    }
                    _ => {}
                }
            }
            // The name field never expands; treat as a stray state and reset.
            InputFocus::Name => *expanded = false,
        }
        return InputKeyOutcome::Handled;
    }

    // --- Collapsed: Enter/Esc act on the whole dialog; movement + activation. ---
    let has_project = project_picker.is_some();
    let has_program = program_picker.is_some();
    // A section picker holding only the catch-all (no configured sections, or a
    // remote whose options haven't loaded) is hidden and unfocusable.
    let has_section = section_picker.as_ref().is_some_and(|p| p.choices.len() > 1);
    let fields = super::FieldsPresent {
        server: server_picker.is_some(),
        project: has_project,
        program: has_program,
        section: has_section,
    };
    match key.code {
        KeyCode::Enter => {
            // A project picker with no current match has nothing to create
            // under. Rather than silently ignore Enter, reopen the Project
            // dropdown so the `(no matching projects)` row is visible and the
            // user can see why — and keep the gate here (pure + testable)
            // rather than in the `App` caller.
            if project_picker
                .as_ref()
                .is_some_and(|p| p.selected_id().is_none())
            {
                *focus = InputFocus::Project;
                *expanded = true;
                return InputKeyOutcome::Handled;
            }
            return InputKeyOutcome::Submit;
        }
        KeyCode::Esc => return InputKeyOutcome::Cancel,
        KeyCode::Tab | KeyCode::Down => *focus = focus.next(fields),
        KeyCode::BackTab | KeyCode::Up => *focus = focus.prev(fields),
        _ => match focus {
            InputFocus::Name => {
                super::edit_text_input(value, key);
            }
            InputFocus::Server => {
                if matches!(key.code, KeyCode::Char(' ') | KeyCode::Right) && fields.server {
                    *expanded = true;
                }
            }
            InputFocus::Project => match key.code {
                // Space / → open the dropdown.
                KeyCode::Char(' ') | KeyCode::Right if has_project => *expanded = true,
                // Typing a filter char opens the dropdown and starts filtering.
                _ => {
                    if let (Some(picker), Some(c)) =
                        (project_picker.as_mut(), palette_text_char(&key))
                    {
                        *expanded = true;
                        picker.filter.push(c);
                        picker.apply_filter();
                        refresh_branch_hint(existing_branches, picker);
                    }
                }
            },
            InputFocus::Program => {
                if matches!(key.code, KeyCode::Char(' ') | KeyCode::Right) && has_program {
                    *expanded = true;
                }
            }
            InputFocus::Section => {
                if matches!(key.code, KeyCode::Char(' ') | KeyCode::Right) && has_section {
                    *expanded = true;
                }
            }
        },
    }
    InputKeyOutcome::Handled
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

                // The two voice toggles (Alt-V, Alt-T) are intercepted before
                // modal routing so they work whether the conversation overlay
                // (or any modal) is open or not — mirroring how spoken replies
                // play regardless of UI state. Their Alt modifier means they
                // never shadow text entry, including a modal's input field.
                match self.config.keybindings.resolve(&key) {
                    Some(BindableAction::ToggleVoiceInput) => {
                        self.toggle_voice_input().await;
                        return;
                    }
                    Some(BindableAction::ToggleDictation) => {
                        self.toggle_dictation().await;
                        return;
                    }
                    _ => {}
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
                // The leader itself, or Ctrl+Space — which always opens the
                // palette, mirroring the in-session switcher (see
                // `tmux/attach.rs`) so the same physical shortcut works whether
                // attached, in the tree, or in the review view.
                if opens_palette(&key, leader_code, leader_mods) {
                    self.open_quick_switch_with_mode(PaletteMode::Unified).await;
                    return;
                }

                // Esc clears an active project filter (set by selecting a
                // project in the board sidebar). Board-only — the filter has no
                // effect or indicator in the list views.
                if key.code == crossterm::event::KeyCode::Esc
                    && key.modifiers.is_empty()
                    && self.ui_state.view_mode.is_board()
                    && self.ui_state.board_filter.is_some()
                {
                    self.ui_state.board_filter = None;
                    self.refresh_list_items().await;
                    return;
                }

                // Number-jump: intercept digit keys to select by session number.
                if let crossterm::event::KeyCode::Char(c @ '0'..='9') = key.code
                    && key.modifiers.is_empty()
                {
                    let digit = c as u8 - b'0';
                    if let crate::digit_accumulator::DigitResult::Jump(n) =
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
            InputEvent::Resize(width, height) => {
                // The terminal re-renders on its own, but an open colour
                // picker's row width must follow now: a key queued behind the
                // resize would otherwise step by the old one.
                let area = Rect::new(0, 0, width, height);
                if let Modal::Settings(SettingsState {
                    editing: Some(super::SettingsEditing::Colour { picker }),
                    ..
                }) = &mut self.ui_state.modal
                {
                    picker.fit_to_width(super::settings::colour_picker_width(area));
                }
            }
            InputEvent::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollUp => {
                    if matches!(self.ui_state.modal, Modal::ReviewDiff(_)) {
                        let over_files = self.review_wheel_over_file_list(&mouse);
                        if let Modal::ReviewDiff(state) = &mut self.ui_state.modal {
                            if over_files {
                                state.wheel_tree(false);
                            } else {
                                state.wheel(false);
                            }
                        }
                    } else if !matches!(self.ui_state.modal, Modal::None) {
                        self.modal_wheel(false);
                    } else {
                        self.scroll_pane_at(mouse.column, ScrollDirection::Up);
                    }
                }
                MouseEventKind::ScrollDown => {
                    if matches!(self.ui_state.modal, Modal::ReviewDiff(_)) {
                        let over_files = self.review_wheel_over_file_list(&mouse);
                        if let Modal::ReviewDiff(state) = &mut self.ui_state.modal {
                            if over_files {
                                state.wheel_tree(true);
                            } else {
                                state.wheel(true);
                            }
                        }
                    } else if !matches!(self.ui_state.modal, Modal::None) {
                        self.modal_wheel(true);
                    } else {
                        self.scroll_pane_at(mouse.column, ScrollDirection::Down);
                    }
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    // In the review view, a click selects a file in the tree or
                    // positions the diff cursor, depending on which pane it hits.
                    let body = self.ui_state.review_body_rect;
                    let files = self.ui_state.review_file_list_rect;
                    let file_pane = self.ui_state.review_file_pane_rect;
                    if matches!(self.ui_state.modal, Modal::ReviewDiff(_)) {
                        let (col, row) = (mouse.column, mouse.row);
                        // A footer button replays the key it labels through the
                        // normal review key path (which expects the modal to be
                        // extracted first, exactly like the keyboard dispatch).
                        if let Some(key) =
                            super::review::review_button_at(&self.ui_state.review_buttons, col, row)
                        {
                            self.ui_state.review_last_click = None;
                            if let Modal::ReviewDiff(state) =
                                std::mem::replace(&mut self.ui_state.modal, Modal::None)
                            {
                                self.handle_review_key(key, state).await;
                            }
                            return;
                        }
                        let mut selected_file = false;
                        if let Modal::ReviewDiff(state) = &mut self.ui_state.modal {
                            if let Some(rect) = files.filter(|r| r.contains((col, row).into())) {
                                self.ui_state.review_last_click = None;
                                state.click_file_list_at(col, row, rect);
                                selected_file = true;
                            } else if file_pane.is_some_and(|r| r.contains((col, row).into())) {
                                // The pane's heading bar: focus the list, select
                                // nothing.
                                self.ui_state.review_last_click = None;
                                state.focus_file_list();
                            } else if let Some(rect) = body {
                                // A double-click on the same body row selects that
                                // line and opens its comment box (like right-click);
                                // a single click just positions the cursor (or
                                // reveals context when it lands on an expand control).
                                use crate::list_nav::DOUBLE_CLICK_WINDOW;
                                let now = Instant::now();
                                let is_double = matches!(
                                    self.ui_state.review_last_click,
                                    Some((prev_row, prev_at))
                                        if prev_row == row
                                            && now.duration_since(prev_at) <= DOUBLE_CLICK_WINDOW
                                );
                                if is_double {
                                    self.ui_state.review_last_click = None;
                                    if !state.double_click_comment(col, row, rect) {
                                        state.click_at(col, row, rect);
                                    }
                                } else {
                                    if state.click_at(col, row, rect) {
                                        self.record_feature("review.expand_context");
                                    }
                                    self.ui_state.review_last_click = Some((row, now));
                                }
                            }
                        }
                        // A file-list click may have changed the selected file, or
                        // a body click may have expanded context; either way kick
                        // off the lazy image + working-tree-content fetches for the
                        // shown file. The keyboard nav path does this in
                        // `handle_review_key`, but mouse selection bypasses it —
                        // without this, an image file stays on "Loading image…" and
                        // an expand control has no lines to reveal.
                        if let Modal::ReviewDiff(state) = &self.ui_state.modal {
                            if selected_file {
                                self.ensure_review_image(state).await;
                            }
                            self.ensure_review_file_lines(state).await;
                        }
                        return;
                    }
                    // List modals: a click highlights the row under the
                    // cursor, a double-click activates it (same convention
                    // as the session tree).
                    if matches!(
                        self.ui_state.modal,
                        Modal::QuickSwitch { .. }
                            | Modal::CheckoutBranch { .. }
                            | Modal::PathInput { .. }
                    ) {
                        self.handle_modal_list_click(mouse.column, mouse.row).await;
                        return;
                    }
                    // Remaining modals are keyboard-only; an underlying row
                    // select would be confusing.
                    if !matches!(self.ui_state.modal, Modal::None) {
                        return;
                    }
                    self.handle_left_click(mouse.column, mouse.row).await;
                }
                MouseEventKind::Drag(MouseButton::Left) => {
                    // Drag-select a line range in the review view.
                    let body = self.ui_state.review_body_rect;
                    if let Modal::ReviewDiff(state) = &mut self.ui_state.modal
                        && let Some(rect) = body
                    {
                        state.drag_at(mouse.column, mouse.row, rect);
                    }
                }
                MouseEventKind::Down(MouseButton::Right) => {
                    // Right-click comments in the review view: with no active
                    // selection it first selects the clicked line, otherwise it
                    // comments on the current selection (mouse equivalent of v+Enter).
                    let body = self.ui_state.review_body_rect;
                    if let Modal::ReviewDiff(state) = &mut self.ui_state.modal
                        && let Some(rect) = body
                    {
                        state.right_click_comment(mouse.column, mouse.row, rect);
                    }
                }
                _ => {}
            },
            InputEvent::Paste(text) => {
                // A paste refilters the list, so drop any pending first-click.
                self.ui_state.modal_list_last_click = None;
                // Handle paste in modal input, ignore otherwise
                let refilter = apply_paste_to_modal(&mut self.ui_state.modal, &text);
                self.finish_text_insert(refilter);
            }
        }
    }

    /// Run the refilter a [`TextField::insert`] left to the caller.
    fn finish_text_insert(&mut self, refilter: Option<Refilter>) {
        match refilter {
            Some(Refilter::CheckoutBranch) => self.refilter_checkout_branches(),
            Some(Refilter::QuickSwitch) => self.refilter_quick_switch(),
            None => {}
        }
    }

    /// Whether the open modal has a text field dictation may type into.
    pub(super) fn modal_accepts_dictation(&mut self) -> bool {
        dictation_field(&mut self.ui_state.modal).is_some()
    }

    /// Type a dictated transcript into the open modal's text field, exactly as
    /// a paste would land. Returns `false` (and changes nothing) when there is
    /// no such field — the modal closed, or changed, while the user spoke.
    pub(super) fn dictate_into_modal(&mut self, text: &str) -> bool {
        let Some(field) = dictation_field(&mut self.ui_state.modal) else {
            return false;
        };
        let refilter = field.insert(text);
        // The insert can refilter a list, so a pending first-click no longer
        // points at a meaningful row — the same reset a paste does.
        self.ui_state.modal_list_last_click = None;
        self.finish_text_insert(refilter);
        true
    }

    /// Handle modal key input
    pub(super) async fn handle_modal_key(&mut self, key: crossterm::event::KeyEvent) {
        use crossterm::event::KeyCode;

        // Any keystroke can refilter the list or swap the modal, so a
        // pending first-click no longer points at a meaningful row.
        self.ui_state.modal_list_last_click = None;

        // The conversation overlay owns all keys while open (typing, send,
        // scroll, close) — dispatch before the shared modal match to avoid a
        // double mutable borrow of `self`.
        if matches!(self.ui_state.modal, Modal::Conversation { .. }) {
            self.handle_conversation_key(key).await;
            return;
        }

        if matches!(
            self.ui_state.modal,
            Modal::EditSession(_) | Modal::EditSessionRestart(_)
        ) {
            self.handle_edit_session_key(key);
            return;
        }
        match &mut self.ui_state.modal {
            Modal::Input { .. } => {
                // All in-modal routing (focus, dropdown expand/collapse, filter
                // editing) lives in the pure `handle_input_modal_key` helper so
                // it is unit-testable without an `App`.
                match handle_input_modal_key(&mut self.ui_state.modal, key) {
                    InputKeyOutcome::Handled => {}
                    InputKeyOutcome::Cancel => self.ui_state.modal = Modal::None,
                    InputKeyOutcome::ServerChanged => {
                        self.on_new_session_server_changed().await;
                    }
                    InputKeyOutcome::Submit => {
                        // `handle_input_modal_key` only returns `Submit` once a
                        // project (if any) is selectable, so no re-gating here.
                        let Modal::Input {
                            value,
                            on_submit,
                            project_picker,
                            program_picker,
                            server_picker,
                            section_picker,
                            ..
                        } = &self.ui_state.modal
                        else {
                            return;
                        };
                        let mut action = on_submit.clone();
                        // A chosen project and section override the ones baked in
                        // at open time.
                        if let InputAction::CreateSession {
                            project_id,
                            section,
                        } = &mut action
                        {
                            if let Some(chosen) =
                                project_picker.as_ref().and_then(|p| p.selected_id())
                            {
                                *project_id = chosen;
                            }
                            if let Some(picker) = section_picker.as_ref() {
                                *section = picker.selected_section();
                            }
                        }
                        // The Server field, when shown, is authoritative for which
                        // backend the session is created on — don't re-derive it
                        // from the project (two backend handles can expose the same
                        // project id).
                        let backend = server_picker.as_ref().and_then(|p| p.selected_backend());
                        let value = value.value().to_string();
                        let program = program_picker.as_ref().and_then(|p| p.selected_command());
                        self.ui_state.modal = Modal::None;
                        self.handle_input_submit(action, value, program, backend)
                            .await;
                    }
                }
            }

            Modal::PathInput {
                value,
                completer,
                scroll,
                ..
            } => {
                use claude_commander_core::config::keybindings::BindableAction;

                // Plain printable chars are text input — keep them out of
                // the j/k navigation bindings.
                if palette_text_char(&key).is_some() {
                    if super::edit_text_input(value, key) {
                        completer.refilter(value.value());
                        *scroll = 0;
                    }
                    return;
                }

                // Arrow keys (and Ctrl-n/p aliases) navigate the completion
                // list via the configurable resolver.
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
                            self.submit_path_input().await;
                        }
                        KeyCode::Esc => {
                            self.ui_state.modal = Modal::None;
                        }
                        KeyCode::Tab => {
                            // Tab extends the input to the longest common
                            // prefix. A single match completes fully + `/`
                            // and `refilter` below surfaces that dir's
                            // children so the user can keep drilling in.
                            let completed = completer.complete(value.value());
                            *value = completed.into();
                            completer.refilter(value.value());
                            *scroll = 0;
                        }
                        // Backspace/Delete/cursor moves and word/line edits.
                        _ => {
                            if super::edit_text_input(value, key) {
                                completer.refilter(value.value());
                                *scroll = 0;
                            }
                        }
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

            Modal::EditSession(_) | Modal::EditSessionRestart(_) => {}
            Modal::Loading { .. } => {
                // Non-interactive — swallow all keys while loading
            }

            Modal::Help { scroll } => {
                if let Some(action) = classify_help_key(&key, &self.config.keybindings)
                    && apply_scroll_action(scroll, action)
                {
                    self.ui_state.modal = Modal::None;
                }
            }

            Modal::Info { scroll } => match classify_info_key(&key) {
                InfoKey::Scroll(action) => {
                    if apply_scroll_action(scroll, action) {
                        self.ui_state.modal = Modal::None;
                    }
                }
                InfoKey::GenerateSummary => {
                    if let Some(session_id) = self.ui_state.selected_session_id.map(|r| r.id) {
                        self.spawn_ai_summary_if_needed(session_id);
                    }
                }
                InfoKey::Ignore => {}
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
                mode,
                query,
                matches,
                selected_idx,
                scroll,
                review,
            } => {
                use claude_commander_core::config::keybindings::BindableAction;

                // Plain printable chars are text input — keep them out of
                // the j/k navigation bindings.
                if palette_text_char(&key).is_some() {
                    if super::edit_text_input(query, key) {
                        self.refilter_quick_switch();
                    }
                    return;
                }

                // Ctrl-R re-lists the repo picker. It has to be a *modified* key:
                // the picker is fuzzy-filterable, so a plain `r` is query text
                // (see the branch above) and could never mean "refresh".
                if *mode == PaletteMode::RepositoryPicker
                    && key.code == KeyCode::Char('r')
                    && key
                        .modifiers
                        .contains(crossterm::event::KeyModifiers::CONTROL)
                {
                    self.refetch_repositories();
                    return;
                }

                // Arrow keys (and Ctrl-n/p aliases) navigate the match list.
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
                            // Put back the review view this palette was opened
                            // over, if any: the switcher is a transient overlay
                            // on it, not a replacement (see `Modal::QuickSwitch`).
                            self.ui_state.modal = match review.take() {
                                Some(state) => Modal::ReviewDiff(state),
                                None => Modal::None,
                            };
                        }
                        KeyCode::Enter => {
                            self.activate_quick_switch_selection().await;
                        }
                        KeyCode::Tab => {
                            // Tab autocompletes a session title into the
                            // query for further refinement. For command rows
                            // this is meaningless, so skip.
                            if let Some(QuickSwitchItem::Session(m)) =
                                matches.get(*selected_idx).cloned()
                            {
                                *query = m.title.into();
                                self.refilter_quick_switch();
                            }
                        }
                        // Backspace/Delete/cursor moves and word/line edits.
                        _ => {
                            if super::edit_text_input(query, key) {
                                self.refilter_quick_switch();
                            }
                        }
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
                use claude_commander_core::config::keybindings::BindableAction;

                // Plain printable chars are text input — keep them out of
                // the j/k navigation bindings.
                if palette_text_char(&key).is_some() {
                    if super::edit_text_input(query, key) {
                        self.refilter_checkout_branches();
                    }
                    return;
                }

                // Arrow keys (and Ctrl-n/p aliases) navigate the branch list.
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
                            self.activate_checkout_selection().await;
                        }
                        // Backspace/Delete/cursor moves and word/line edits.
                        _ => {
                            if super::edit_text_input(query, key) {
                                self.refilter_checkout_branches();
                            }
                        }
                    },
                }
            }

            Modal::ReviewDiff(_) => {
                // Extract the state to avoid a borrow conflict with &mut self.
                let state = match std::mem::replace(&mut self.ui_state.modal, Modal::None) {
                    Modal::ReviewDiff(s) => s,
                    _ => unreachable!(),
                };
                self.handle_review_key(key, state).await;
            }

            // Handled by the early dispatch above.
            Modal::Conversation { .. } => {}

            Modal::None => {}
        }
    }

    /// Handle a left-mouse click at the given absolute terminal position.
    ///
    /// Clicks outside the board are ignored. A click on a card row or sidebar
    /// row moves the cursor there; two clicks on the same row within
    /// [`DOUBLE_CLICK_WINDOW`] act as `UserCommand::Select` (attach for
    /// sessions, project shell/new-session for sidebar rows).
    pub(super) async fn handle_left_click(&mut self, col: u16, row: u16) {
        use crate::list_nav::DOUBLE_CLICK_WINDOW;

        // Status-bar action buttons sit outside the board. A hit dispatches the
        // bound command — behaving exactly like the keypress — and consumes the
        // click.
        if let Some(action) = crate::hotkey::button_at(&self.ui_state.action_buttons, col, row) {
            self.ui_state.last_left_click = None;
            self.handle_command(UserCommand::from(action)).await;
            return;
        }

        // List views have no board regions/buttons — route to the list handler.
        if !self.ui_state.view_mode.is_board() {
            self.handle_list_left_click(col, row).await;
            return;
        }

        // Card action buttons win over the row region they sit within: select
        // the card's session, then dispatch the button's command through the
        // normal handle_command path, exactly as the equivalent keypress would.
        // Note handle_command does NOT consult is_command_available (only the
        // palette and status bar do) — like keypresses, this relies on each
        // handler self-gating (SelectShell/OpenReviewDiff/OpenInfo all no-op
        // safely without a valid selection).
        // A click on a sidebar server heading opens that server's Programs
        // settings (its ⚙ affordance advertises this; the whole row is the
        // target). Headings aren't selectable rows, so pass the backend
        // straight through rather than routing via the selection.
        if let Some(backend) = self.board_heading_at(col, row) {
            self.ui_state.last_left_click = None;
            self.open_settings_on_programs(backend);
            return;
        }
        if let Some((pos, button)) = self.board_button_at(col, row) {
            use crate::widgets::board::CardButton;
            self.ui_state.last_left_click = None;
            if self.ui_state.board_state.selected() != Some(pos) {
                self.ui_state.board_state.select(Some(pos));
                self.update_selection();
                self.ui_state.preview_update_spawned_at = None;
                self.spawn_preview_update();
            }
            let cmd = match button {
                CardButton::Shell => UserCommand::SelectShell,
                CardButton::Review => UserCommand::OpenReviewDiff,
                CardButton::Info => UserCommand::OpenInfo,
            };
            self.handle_command(cmd).await;
            return;
        }

        let Some(pos) = self.board_pos_at(col, row) else {
            self.ui_state.last_left_click = None;
            return;
        };

        let now = Instant::now();
        let is_double_click = matches!(
            self.ui_state.last_left_click,
            Some((prev_pos, prev_at))
                if prev_pos == pos && now.duration_since(prev_at) <= DOUBLE_CLICK_WINDOW
        );

        if self.ui_state.board_state.selected() != Some(pos) {
            self.ui_state.board_state.select(Some(pos));
            self.update_selection();
            self.ui_state.preview_update_spawned_at = None;
            self.spawn_preview_update();
        }

        if is_double_click {
            // Consume the click pair so a third click within the window
            // doesn't fire again.
            self.ui_state.last_left_click = None;
            self.handle_command(UserCommand::Select).await;
        } else {
            self.ui_state.last_left_click = Some((pos, now));
        }
    }

    /// Map a mouse `(col, row)` in absolute terminal coordinates to a session-
    /// list row index, using the rects recorded on the last frame. See
    /// [`list_row_at`] for the mapping itself.
    fn list_index_at(&self, col: u16, row: u16) -> Option<usize> {
        list_row_at(
            col,
            row,
            self.ui_state.recents_rect,
            self.ui_state.list_rect,
            self.ui_state.recents_len,
            self.ui_state.main_list_offset,
            self.ui_state.list_items.len(),
        )
    }

    /// Left-click handling for the session-list views (project / sections /
    /// stacks): select the clicked row, open a server header's Programs pane,
    /// and treat a double-click on the same row as `Select` (like Enter).
    async fn handle_list_left_click(&mut self, col: u16, row: u16) {
        use crate::list_nav::DOUBLE_CLICK_WINDOW;

        let Some(idx) = self.list_index_at(col, row) else {
            self.ui_state.last_list_click = None;
            return;
        };

        // A click on a server header opens that server's Programs settings.
        // Route through the command so it selects the header first and records
        // the UI-feature telemetry.
        if matches!(
            self.ui_state.list_items.get(idx),
            Some(SessionListItem::ServerHeader { .. })
        ) {
            self.ui_state.last_list_click = None;
            self.ui_state.list_state.select(Some(idx));
            self.update_selection();
            self.handle_command(UserCommand::EditServerPrograms).await;
            return;
        }
        // Ignore non-selectable rows (spacers, and — for a single click —
        // section/project headers still select so their children are reachable).
        match self.ui_state.list_items.get(idx) {
            Some(item) if !item.is_selectable() => {
                self.ui_state.last_list_click = None;
                return;
            }
            None => {
                self.ui_state.last_list_click = None;
                return;
            }
            _ => {}
        }

        let now = Instant::now();
        let is_double_click = matches!(
            self.ui_state.last_list_click,
            Some((prev_idx, prev_at))
                if prev_idx == idx && now.duration_since(prev_at) <= DOUBLE_CLICK_WINDOW
        );

        if self.ui_state.list_state.selected() != Some(idx) {
            self.ui_state.list_state.select(Some(idx));
            self.update_selection();
            self.ui_state.preview_update_spawned_at = None;
            self.spawn_preview_update();
        }

        if is_double_click {
            self.ui_state.last_list_click = None;
            self.handle_command(UserCommand::Select).await;
        } else {
            self.ui_state.last_list_click = Some((idx, now));
        }
    }

    /// Handle a left click while a list modal (quick-switch, checkout-branch,
    /// path-input) is open. A click on a row moves the highlight there; a
    /// second click on the same row within [`DOUBLE_CLICK_WINDOW`] activates
    /// it, exactly as Enter would. Clicks anywhere else are ignored.
    pub(super) async fn handle_modal_list_click(&mut self, col: u16, row: u16) {
        use crate::list_nav::{DOUBLE_CLICK_WINDOW, list_index_at};

        let Some(rows) = self.ui_state.modal_list_rect else {
            return;
        };
        let clicked = match &mut self.ui_state.modal {
            Modal::QuickSwitch {
                matches,
                selected_idx,
                scroll,
                ..
            } => list_index_at(col, row, rows, *scroll, matches.len())
                .inspect(|&idx| *selected_idx = idx),
            Modal::CheckoutBranch {
                filtered,
                selected_idx,
                scroll,
                ..
            } => list_index_at(col, row, rows, *scroll, filtered.len())
                .inspect(|&idx| *selected_idx = idx),
            Modal::PathInput {
                completer, scroll, ..
            } => {
                let len = completer.visible_completions().0.len();
                list_index_at(col, row, rows, *scroll, len).inspect(|&idx| completer.select(idx))
            }
            _ => return,
        };

        let Some(idx) = clicked else {
            // Border, input line, or an empty row: not a row, so any
            // pending first-click is stale.
            self.ui_state.modal_list_last_click = None;
            return;
        };
        let now = Instant::now();
        let is_double_click = matches!(
            self.ui_state.modal_list_last_click,
            Some((prev_idx, prev_at))
                if prev_idx == idx && now.duration_since(prev_at) <= DOUBLE_CLICK_WINDOW
        );
        if is_double_click {
            // Consume the click pair so a third click doesn't re-fire.
            self.ui_state.modal_list_last_click = None;
            self.activate_modal_list_selection().await;
        } else {
            self.ui_state.modal_list_last_click = Some((idx, now));
        }
    }

    /// Activate the highlighted row of the open list modal — the shared
    /// endpoint for Enter and double-click.
    async fn activate_modal_list_selection(&mut self) {
        match &self.ui_state.modal {
            Modal::QuickSwitch { .. } => self.activate_quick_switch_selection().await,
            Modal::CheckoutBranch { .. } => self.activate_checkout_selection().await,
            Modal::PathInput { .. } => self.submit_path_input().await,
            _ => {}
        }
    }

    /// Activate the highlighted quick-switch row: jump to the session, run
    /// the command, or apply the section move.
    pub(super) async fn activate_quick_switch_selection(&mut self) {
        // Clone the selected item so the borrow on `matches` is released
        // before we mutate `modal` and dispatch. `unmatched` carries the typed
        // query for the one mode that can act without a highlighted row.
        let (mode, selected, unmatched) = match &self.ui_state.modal {
            Modal::QuickSwitch {
                mode,
                query,
                matches,
                selected_idx,
                ..
            } => (
                *mode,
                matches.get(*selected_idx).cloned(),
                // The repo picker's "clone something not in the list" path: with
                // no row matching, the query itself is the clone source. Mirrors
                // the checkout modal, where an unmatched query is used as-is.
                // The workspace pickers use it the same way, as a new name.
                matches!(
                    mode,
                    PaletteMode::RepositoryPicker
                        | PaletteMode::WorkspacePicker
                        | PaletteMode::MoveProjectPicker { .. }
                )
                .then(|| query.value().trim().to_string())
                .filter(|q| !q.is_empty()),
            ),
            _ => return,
        };
        match selected {
            Some(QuickSwitchItem::Session(m)) => {
                let session_id = m.session_id;
                self.ui_state.modal = Modal::None;
                // The palette spans every workspace; a pick from another one
                // switches there first so the jump lands on a visible row.
                if m.other_workspace.is_some() {
                    self.switch_workspace(m.workspace.clone(), false).await;
                }
                // The target may be hidden by an active project filter (the
                // palette lists every session regardless of the filter). Clear
                // it and rebuild so the jump always lands — quick-switch is the
                // primary jump path and must never silently no-op.
                if self.ui_state.board_filter.is_some()
                    && self.ui_state.board.position_of(session_id).is_none()
                {
                    self.ui_state.board_filter = None;
                    self.refresh_list_items().await;
                }
                // `reveal_…`, not `select_…`: a collapsed section hides its
                // sessions' rows entirely, and a failed select would leave the
                // attach pointed at whatever was selected before.
                if !self.reveal_session_in_tree(session_id).await {
                    // No row anywhere. Point the selection at the pick directly
                    // — the same move the review view's re-attach toggle makes
                    // (`review.rs`) — so what gets attached is decided by the
                    // pick rather than by whatever the list happens to show.
                    let backend = self.backend_of_session(session_id);
                    self.ui_state.selected_session_id = Some(SessionRef::new(backend, session_id));
                }
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
                self.apply_section_move(session_id, target);
            }
            Some(QuickSwitchItem::RemoteServerRemove { name, .. }) => {
                self.ui_state.modal = Modal::Confirm {
                    title: "Remove Remote Server".to_string(),
                    message: format!(
                        "Remove remote server \"{name}\"?\n\nSessions keep running on the server; this only removes it from this TUI's config."
                    ),
                    on_confirm: ConfirmAction::RemoveRemoteServer { name },
                };
            }
            Some(QuickSwitchItem::HostedRepository {
                full_name,
                dir_name,
                ..
            }) => {
                let backend = self.ui_state.repo_picker.backend;
                self.open_clone_dest_prompt(
                    backend,
                    match self.ui_state.repo_picker.host.provider {
                        claude_commander_protocol::hosting::CodeHostProvider::Github => {
                            claude_commander_protocol::hosting::CloneSource::Github {
                                full_name: full_name.clone(),
                            }
                        }
                        claude_commander_protocol::hosting::CodeHostProvider::Gitlab => {
                            claude_commander_protocol::hosting::CloneSource::Gitlab {
                                full_name: full_name.clone(),
                                hostname: self.ui_state.repo_picker.host.hostname.clone(),
                            }
                        }
                    },
                    &full_name,
                    &dir_name,
                );
            }
            Some(QuickSwitchItem::BaseChange {
                session_id,
                target,
                base_branch,
                ..
            }) => {
                let backend = self.backend_of_session(session_id);
                let title = self
                    .session(SessionRef {
                        backend,
                        id: session_id,
                    })
                    .map(|s| s.title.clone())
                    .unwrap_or_else(|| "this session".to_string());
                self.ui_state.modal = Modal::Confirm {
                    title: "Set Session Base".to_string(),
                    message: super::actions::set_session_base_confirm_message(
                        self.view_for(self.backend_of_session(session_id))
                            .snapshot
                            .server
                            .effective_code_host()
                            .provider,
                        &title,
                        &base_branch,
                    ),
                    on_confirm: ConfirmAction::SetSessionBase { session_id, target },
                };
            }
            Some(QuickSwitchItem::Workspace { name, .. }) => {
                self.ui_state.modal = Modal::None;
                self.switch_workspace(name, true).await;
            }
            Some(QuickSwitchItem::ProjectWorkspace {
                project_id, target, ..
            }) => {
                self.ui_state.modal = Modal::None;
                self.move_project_to_workspace(project_id, target).await;
            }
            Some(QuickSwitchItem::ProgramChange {
                session_id,
                program,
                ..
            }) => {
                self.ui_state.modal = Modal::Confirm {
                    title: "Change Program".to_string(),
                    message: format!(
                        "Change program to `{program}` and restart this session?\n\nThe current agent conversation will be terminated."
                    ),
                    on_confirm: ConfirmAction::ChangeProgram {
                        session_id,
                        program,
                    },
                };
            }
            // No row is highlighted. Only the repo picker can still act: the
            // typed text is a clone URL (validated in `open_clone_url_prompt`,
            // which leaves the picker open and says why if it's refused).
            None => match (mode, unmatched) {
                (PaletteMode::RepositoryPicker, Some(typed)) => {
                    self.open_clone_url_prompt(&typed);
                }
                // A name no workspace matches: create it, then act on it.
                (PaletteMode::WorkspacePicker, Some(typed)) => {
                    self.ui_state.modal = Modal::None;
                    if let Some(name) = self.create_workspace(&typed).await {
                        self.switch_workspace(Some(name), true).await;
                    }
                }
                (PaletteMode::MoveProjectPicker { project_id }, Some(typed)) => {
                    self.ui_state.modal = Modal::None;
                    if let Some(name) = self.create_workspace(&typed).await {
                        self.move_project_to_workspace(project_id, Some(name)).await;
                    }
                }
                _ => {}
            },
        }
    }

    /// Check out the branch the checkout modal currently points at: the
    /// highlighted match when the filter produced any, otherwise the raw
    /// query text (so a pasted branch name still works — a leading
    /// "origin/" is stripped to always get a local branch name).
    async fn activate_checkout_selection(&mut self) {
        let (project_id, branch_label) = match &self.ui_state.modal {
            Modal::CheckoutBranch {
                project_id,
                query,
                filtered,
                selected_idx,
                ..
            } => {
                let label = if let Some(m) = filtered.get(*selected_idx) {
                    m.local_name.clone()
                } else {
                    let trimmed = query.value().trim();
                    if trimmed.is_empty() {
                        return;
                    }
                    trimmed
                        .strip_prefix("origin/")
                        .unwrap_or(trimmed)
                        .to_string()
                };
                (*project_id, label)
            }
            _ => return,
        };
        self.ui_state.modal = Modal::None;
        self.start_checkout_session(project_id, branch_label).await;
    }

    /// Submit the path-input modal, preferring the highlighted completion
    /// over the typed value (so arrow-to-select-then-Enter works without
    /// first pressing Tab) and falling back to the typed value when the
    /// list is empty (e.g. a path that doesn't exist yet).
    async fn submit_path_input(&mut self) {
        let (action, submit_value) = match &self.ui_state.modal {
            Modal::PathInput {
                value,
                on_submit,
                completer,
                ..
            } => (
                on_submit.clone(),
                completer
                    .selected_completion()
                    .map(str::to_string)
                    .unwrap_or_else(|| value.value().to_string()),
            ),
            _ => return,
        };
        self.ui_state.modal = Modal::None;
        // Path-input flows (AddProject…) never create a session, so no backend
        // override applies.
        self.handle_input_submit(action, submit_value, None, None)
            .await;
    }

    /// Whether a review-view wheel event landed over the file-list pane,
    /// heading bar included (so it should scroll the file list rather than the
    /// diff body).
    fn review_wheel_over_file_list(&self, mouse: &crossterm::event::MouseEvent) -> bool {
        self.ui_state
            .review_file_pane_rect
            .is_some_and(|r| r.contains((mouse.column, mouse.row).into()))
    }

    /// Mouse wheel while a (non-review) modal is open. List modals move the
    /// highlighted row, clamping at the ends; the Help modal scrolls its
    /// content. Other modals swallow the event so the panes underneath
    /// don't scroll while covered.
    fn modal_wheel(&mut self, down: bool) {
        use super::actions::{LIST_MAX_VISIBLE, adjust_list_scroll};
        use crate::list_nav::wheel_step;
        match &mut self.ui_state.modal {
            Modal::QuickSwitch {
                matches,
                selected_idx,
                scroll,
                ..
            } if !matches.is_empty() => {
                *selected_idx = wheel_step(*selected_idx, down, matches.len());
                *scroll = adjust_list_scroll(*selected_idx, *scroll, LIST_MAX_VISIBLE);
            }
            Modal::CheckoutBranch {
                filtered,
                selected_idx,
                scroll,
                ..
            } if !filtered.is_empty() => {
                *selected_idx = wheel_step(*selected_idx, down, filtered.len());
                *scroll = adjust_list_scroll(*selected_idx, *scroll, LIST_MAX_VISIBLE);
            }
            Modal::PathInput {
                completer, scroll, ..
            } => {
                let (list, highlighted) = completer.visible_completions();
                if let Some(idx) = highlighted {
                    let new_idx = wheel_step(idx, down, list.len());
                    completer.select(new_idx);
                    *scroll = adjust_list_scroll(new_idx, *scroll, LIST_MAX_VISIBLE);
                }
            }
            Modal::Help { scroll } | Modal::Info { scroll } => {
                *scroll = scroll.saturating_add_signed(if down { 1 } else { -1 });
            }
            _ => {}
        }
    }

    /// Handle a user command
    pub(super) async fn handle_command(&mut self, cmd: UserCommand) {
        // Single dispatch chokepoint: record UI-level feature usage here.
        // Commands handled by an instrumented service method (and pure
        // navigation noise) map to `None` — see `UserCommand::telemetry_feature`.
        if let Some(feature) = cmd.telemetry_feature() {
            self.record_feature(feature);
        }
        match cmd {
            UserCommand::NavigateUp => self.nav_up(),
            UserCommand::NavigateDown => self.nav_down(),
            UserCommand::NavigateRight => self.nav_right(),
            UserCommand::NavigateLeft => self.nav_left(),
            // On the board these switch column; in a list they jump between
            // project/section group headers.
            UserCommand::NextGroup => self.nav_next_group(),
            UserCommand::PreviousGroup => self.nav_previous_group(),
            UserCommand::NavigateFirst => self.nav_first(),
            UserCommand::NavigateLast => self.nav_last(),
            UserCommand::ListPageUp | UserCommand::ListPageDown => {
                self.nav_page(matches!(cmd, UserCommand::ListPageDown));
            }
            UserCommand::Select => {
                // On a card row, attach. On the board sidebar's project row (a
                // project is selected but no session), Select toggles the board
                // filter for that project — select to filter, select again to
                // unfilter. The filter is board-only, so in the list views a
                // project row falls through to the normal select handler.
                if self.ui_state.view_mode.is_board()
                    && self.ui_state.selected_session_id.is_none()
                    && let Some((_, pid)) = self.ui_state.selected_project_id
                {
                    self.ui_state.board_filter = if self.ui_state.board_filter == Some(pid) {
                        None
                    } else {
                        Some(pid)
                    };
                    self.refresh_list_items().await;
                } else {
                    self.handle_select().await;
                }
            }
            UserCommand::SelectShell => {
                self.handle_select_shell().await;
            }
            UserCommand::NewSession => {
                self.handle_new_session().await;
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
                self.handle_cascade_abandon();
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
            UserCommand::CloneRepository => {
                self.handle_clone_repository();
            }
            UserCommand::ScanDirectory => {
                self.open_path_input(
                    "Scan Directory".to_string(),
                    "Enter directory to scan for git repos:".to_string(),
                    InputAction::ScanDirectory,
                );
            }
            UserCommand::DeleteSession => {
                self.handle_delete_session().await;
            }
            UserCommand::DeleteMergedPrSessions => {
                self.handle_delete_merged_pr_sessions().await;
            }
            UserCommand::EditSession => self.handle_edit_session(),
            UserCommand::RenameSession => {
                self.handle_rename_session().await;
            }
            UserCommand::MoveToSection => {
                self.handle_move_to_section().await;
            }
            UserCommand::SetSessionBase => {
                self.handle_set_session_base().await;
            }
            UserCommand::ToggleViewMode => {
                self.handle_toggle_view_mode().await;
            }
            UserCommand::ToggleSection => {
                self.handle_toggle_section().await;
            }
            UserCommand::NextWorkspace => {
                self.handle_cycle_workspace(true).await;
            }
            UserCommand::PreviousWorkspace => {
                self.handle_cycle_workspace(false).await;
            }
            UserCommand::WorkspacePicker => {
                self.open_workspace_picker().await;
            }
            UserCommand::NewWorkspace => {
                self.handle_new_workspace();
            }
            UserCommand::MoveProjectToWorkspace => {
                self.handle_move_project_to_workspace().await;
            }
            UserCommand::RestartSession => {
                self.handle_restart_session();
            }
            UserCommand::ResetSession => {
                self.handle_reset_session();
            }
            UserCommand::ChangeProgram => {
                self.handle_change_program();
            }
            UserCommand::ToggleKeepAlive => {
                self.handle_toggle_keep_alive().await;
            }
            UserCommand::RemoveProject => {
                self.handle_remove_project();
            }
            UserCommand::OpenInEditor => {
                self.handle_open_in_editor().await;
            }
            // Only meaningful with a session selected (matches OpenInfo's
            // availability gating); no-op on a sidebar/empty selection.
            UserCommand::OpenInfo if self.ui_state.selected_session_id.is_some() => {
                self.ui_state.modal = Modal::Info { scroll: 0 };
                // Opening Info is the discoverable retry for an enriched-PR
                // fetch that came back empty (before the marker existed, every
                // reopen retried). Dropping it here keeps the anti-spam property
                // — the marker still suppresses the per-tick refetch loop while
                // a surface stays open — without making PR-refresh the only way.
                self.ui_state.enriched_pr_unavailable = None;
                // Kick off the enriched-PR + working-tree diff fetches that
                // populate the modal; clear the in-flight guard so the diff
                // fetch runs even if one fired recently.
                self.ui_state.preview_update_spawned_at = None;
                self.spawn_info_fetch();
                self.spawn_preview_update();
            }
            UserCommand::OpenPullRequest => {
                self.handle_open_pull_request().await;
            }
            UserCommand::RefreshPrStatus => {
                // Wake every connected backend's PR-status loop; refreshed
                // results arrive via each backend's change feed.
                self.refresh_pr_status_all();
                // This is the retry path for an enriched-PR fetch that came back
                // empty, so drop the marker suppressing it and re-request.
                self.ui_state.enriched_pr_unavailable = None;
                self.spawn_info_fetch();
            }
            UserCommand::AddRemoteServer => {
                self.handle_add_remote_server();
            }
            UserCommand::RemoveRemoteServer => {
                self.handle_remove_remote_server();
            }
            UserCommand::OpenCommander => {
                self.handle_open_commander().await;
            }
            UserCommand::ToggleConversationOverlay => {
                self.toggle_conversation_overlay().await;
            }
            UserCommand::ToggleVoiceInput => {
                self.toggle_voice_input().await;
            }
            UserCommand::ToggleDictation => {
                self.toggle_dictation().await;
            }
            UserCommand::OpenReviewDiff => {
                self.handle_open_review().await;
            }
            UserCommand::ShowHelp => {
                self.ui_state.modal = Modal::Help { scroll: 0 };
            }
            UserCommand::ShowSettings => {
                // Populate the mic-device cache so the STT Microphone row shows a
                // friendly label (not the raw id) as soon as the Conversation tab
                // is opened, without enumerating on the render path.
                self.refresh_input_devices();
                let rows = self.build_settings_rows(SettingsTab::General);
                let selected_row = super::settings::first_selectable_from(&rows, 0);
                self.ui_state.modal = Modal::Settings(SettingsState {
                    tab: SettingsTab::General,
                    selected_row,
                    editing: None,
                    rows,
                    sections_state: SectionsState::default(),
                    workspaces_state: WorkspacesState::default(),
                    programs_state: ProgramsState::default(),
                    theme_scope: self.default_theme_scope(),
                    search: None,
                });
            }
            UserCommand::EditServerPrograms => {
                self.open_settings_on_programs(self.selected_backend_id());
            }
            UserCommand::CopyServerToken => {
                self.copy_server_token().await;
            }
            UserCommand::Quit => {
                self.ui_state.should_quit = true;
            }
            // Column paging within the board: to the ends of the selected
            // column, and single-row steps for the (unbound) scroll actions.
            UserCommand::PageUp => self.nav_first(),
            UserCommand::PageDown => self.nav_last(),
            UserCommand::ScrollUp => self.nav_up(),
            UserCommand::ScrollDown => self.nav_down(),
            UserCommand::GenerateSummary => {
                // The summary surfaces in the Info modal, but generation is safe
                // to trigger for the selected session regardless.
                if let Some(session_id) = self.ui_state.selected_session_id.map(|r| r.id) {
                    self.spawn_ai_summary_if_needed(session_id);
                }
            }
            // Right-pane commands are list-view only — the board is full-screen
            // and draws no right pane, so they would have nothing to act on.
            UserCommand::TogglePane | UserCommand::TogglePaneReverse
                if !self.ui_state.view_mode.is_board() =>
            {
                let forward = matches!(cmd, UserCommand::TogglePane);
                self.ui_state.right_pane_view = self
                    .ui_state
                    .right_pane_view
                    .cycled(self.is_project_selected(), forward);
                // No explicit clear here: `render_right_pane` resets the pane
                // whenever the effective view changes, which covers this and the
                // selection-driven swaps a key handler can't see.
                // Landing on Info needs the enriched-PR / summary fetches that
                // only run while an Info surface is showing.
                self.spawn_info_fetch();
                self.spawn_preview_update();
            }
            UserCommand::ShrinkLeftPane => self.resize_left_pane(-2).await,
            UserCommand::GrowLeftPane => self.resize_left_pane(2).await,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use claude_commander_core::config::KeyBindings;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    // ── list_row_at: mouse → session-list row ──────────────────────────
    //
    // The list views record their rects at render time; these cover the
    // arithmetic that turns a click inside them into a `list_items` index.

    /// A 3-row pinned recents panel at the top of a 30-wide list column, with
    /// the scrolling main list directly below it.
    fn recents() -> Rect {
        Rect::new(2, 5, 30, 3)
    }

    /// The scrolling main list below [`recents`].
    fn main_list() -> Rect {
        Rect::new(2, 8, 30, 10)
    }

    /// `list_row_at` with no recents panel — the whole column is one list.
    fn row_at_plain(col: u16, row: u16, offset: usize, item_count: usize) -> Option<usize> {
        list_row_at(
            col,
            row,
            None,
            Some(Rect::new(2, 5, 30, 10)),
            0,
            offset,
            item_count,
        )
    }

    /// `list_row_at` with the [`recents`] + [`main_list`] pair.
    fn row_at_panelled(col: u16, row: u16, offset: usize, item_count: usize) -> Option<usize> {
        list_row_at(
            col,
            row,
            Some(recents()),
            Some(main_list()),
            3,
            offset,
            item_count,
        )
    }

    #[test]
    fn list_row_at_maps_top_row_to_the_first_item() {
        assert_eq!(row_at_plain(2, 5, 0, 10), Some(0));
        assert_eq!(row_at_plain(2, 6, 0, 10), Some(1));
        // Anywhere across the row's width maps to the same item.
        assert_eq!(row_at_plain(31, 5, 0, 10), Some(0));
    }

    #[test]
    fn list_row_at_adds_the_scroll_offset() {
        // Second visible row of a list scrolled by 5 → item 6.
        assert_eq!(row_at_plain(2, 6, 5, 20), Some(6));
    }

    #[test]
    fn list_row_at_maps_pinned_recents_rows_directly() {
        // The recents panel renders at offset 0, so its rows are indices 0..3
        // no matter how far the main list beneath it has scrolled.
        assert_eq!(row_at_panelled(2, 5, 7, 20), Some(0));
        assert_eq!(row_at_panelled(2, 7, 7, 20), Some(2));
    }

    #[test]
    fn list_row_at_maps_the_first_main_list_row_below_the_recents_panel() {
        // The boundary case: the row immediately under a 3-row recents panel is
        // the main list's scrolled top — recents_len(3) + offset(5) = 8. An
        // off-by-one here would silently select the wrong session on click.
        assert_eq!(row_at_panelled(2, 8, 5, 20), Some(8));
        assert_eq!(row_at_panelled(2, 9, 5, 20), Some(9));
        // With the list unscrolled the rows run straight on from the panel.
        assert_eq!(row_at_panelled(2, 8, 0, 20), Some(3));
    }

    #[test]
    fn list_row_at_rejects_positions_outside_the_list_column() {
        // Left of, right of, above and below the rects.
        assert_eq!(row_at_plain(1, 5, 0, 10), None);
        assert_eq!(row_at_plain(32, 5, 0, 10), None);
        assert_eq!(row_at_plain(2, 4, 0, 10), None);
        assert_eq!(row_at_plain(2, 15, 0, 10), None);
    }

    #[test]
    fn list_row_at_rejects_rows_past_the_last_item() {
        // Inside the rect, but the list is shorter than the viewport.
        assert_eq!(row_at_plain(2, 8, 0, 3), None);
        assert_eq!(row_at_plain(2, 5, 0, 0), None);
        // Scrolled such that the visible row is past the end.
        assert_eq!(row_at_plain(2, 9, 18, 20), None);
    }

    #[test]
    fn list_row_at_rejects_recents_rows_past_the_panel_contents() {
        // A 3-row rect holding only 2 recents: the third row is padding, and
        // the panel never falls through to the main list beneath it.
        assert_eq!(
            list_row_at(2, 7, Some(recents()), Some(main_list()), 2, 5, 20),
            None
        );
        // Nor past the end of a list shorter than the panel claims.
        assert_eq!(
            list_row_at(2, 6, Some(recents()), Some(main_list()), 3, 0, 1),
            None
        );
    }

    #[test]
    fn list_row_at_resolves_an_overlapping_recents_hit_in_the_panel() {
        // The renderer stacks the panel above the list, so these rects never
        // actually overlap — this pins the documented precedence anyway, so a
        // future layout change can't silently start reading a panel row as a
        // main-list row (which would apply the scroll offset to it).
        let overlapping = Rect::new(2, 5, 30, 10);
        assert_eq!(
            list_row_at(2, 6, Some(recents()), Some(overlapping), 3, 5, 20),
            Some(1),
            "a hit inside the panel maps to its own row, not recents_len + offset"
        );
        // A padding row inside the panel resolves to nothing rather than
        // falling through to the list rect underneath it.
        assert_eq!(
            list_row_at(2, 7, Some(recents()), Some(overlapping), 2, 5, 20),
            None
        );
    }

    #[test]
    fn list_row_at_without_a_recorded_frame_maps_nothing() {
        // Before the first render there is no list rect to hit-test against.
        assert_eq!(list_row_at(2, 5, None, None, 0, 0, 10), None);
        // A recents rect alone still resolves its own rows.
        assert_eq!(list_row_at(2, 5, Some(recents()), None, 3, 0, 10), Some(0));
        assert_eq!(list_row_at(2, 9, Some(recents()), None, 3, 0, 10), None);
    }

    #[test]
    fn arrows_scroll_one_line() {
        let kb = KeyBindings::default();
        assert_eq!(
            classify_help_key(&key(KeyCode::Down), &kb),
            Some(ScrollAction::ScrollBy(1))
        );
        assert_eq!(
            classify_help_key(&key(KeyCode::Up), &kb),
            Some(ScrollAction::ScrollBy(-1))
        );
    }

    #[test]
    fn default_jk_bindings_scroll_one_line() {
        // Default KeyBindings bind j/k to NavigateDown/NavigateUp, not scroll,
        // so plain j/k should NOT produce ScrollBy here — they are ignored in
        // the Help modal. This pins the current default so a future remapping
        // doesn't silently change modal behavior.
        let kb = KeyBindings::default();
        assert_eq!(classify_help_key(&key(KeyCode::Char('j')), &kb), None);
        assert_eq!(classify_help_key(&key(KeyCode::Char('k')), &kb), None);
    }

    #[test]
    fn page_keys_scroll_by_page() {
        let kb = KeyBindings::default();
        let page = HELP_PAGE as i16;
        assert_eq!(
            classify_help_key(&key(KeyCode::PageDown), &kb),
            Some(ScrollAction::ScrollBy(page))
        );
        assert_eq!(
            classify_help_key(&key(KeyCode::PageUp), &kb),
            Some(ScrollAction::ScrollBy(-page))
        );
        // Default bindings: Ctrl-d / Ctrl-u for PageDown / PageUp.
        assert_eq!(
            classify_help_key(&ctrl(KeyCode::Char('d')), &kb),
            Some(ScrollAction::ScrollBy(page))
        );
        assert_eq!(
            classify_help_key(&ctrl(KeyCode::Char('u')), &kb),
            Some(ScrollAction::ScrollBy(-page))
        );
    }

    #[test]
    fn home_and_end_jump() {
        let kb = KeyBindings::default();
        assert_eq!(
            classify_help_key(&key(KeyCode::Home), &kb),
            Some(ScrollAction::Home)
        );
        assert_eq!(
            classify_help_key(&key(KeyCode::End), &kb),
            Some(ScrollAction::End)
        );
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
                Some(ScrollAction::Close),
                "{code:?}"
            );
        }
    }

    #[test]
    fn unrelated_key_is_ignored() {
        let kb = KeyBindings::default();
        assert_eq!(classify_help_key(&key(KeyCode::Char('x')), &kb), None);
    }

    // -- Info modal key classification --

    #[test]
    fn info_arrows_and_jk_scroll_one_line() {
        // Unlike the Help modal, j/k scroll the Info modal (the board's
        // navigation bindings don't apply while it's open).
        for code in [KeyCode::Up, KeyCode::Char('k')] {
            assert_eq!(
                classify_info_key(&key(code)),
                InfoKey::Scroll(ScrollAction::ScrollBy(-1)),
                "{code:?}"
            );
        }
        for code in [KeyCode::Down, KeyCode::Char('j')] {
            assert_eq!(
                classify_info_key(&key(code)),
                InfoKey::Scroll(ScrollAction::ScrollBy(1)),
                "{code:?}"
            );
        }
    }

    #[test]
    fn info_page_and_home_end_keys() {
        let page = HELP_PAGE as i16;
        assert_eq!(
            classify_info_key(&key(KeyCode::PageDown)),
            InfoKey::Scroll(ScrollAction::ScrollBy(page))
        );
        assert_eq!(
            classify_info_key(&key(KeyCode::PageUp)),
            InfoKey::Scroll(ScrollAction::ScrollBy(-page))
        );
        assert_eq!(
            classify_info_key(&key(KeyCode::Home)),
            InfoKey::Scroll(ScrollAction::Home)
        );
        assert_eq!(
            classify_info_key(&key(KeyCode::End)),
            InfoKey::Scroll(ScrollAction::End)
        );
    }

    #[test]
    fn info_close_keys() {
        for code in [KeyCode::Esc, KeyCode::Char('q'), KeyCode::Char('i')] {
            assert_eq!(
                classify_info_key(&key(code)),
                InfoKey::Scroll(ScrollAction::Close),
                "{code:?}"
            );
        }
    }

    #[test]
    fn info_g_generates_summary_and_others_ignored() {
        assert_eq!(
            classify_info_key(&key(KeyCode::Char('g'))),
            InfoKey::GenerateSummary
        );
        assert_eq!(classify_info_key(&key(KeyCode::Char('x'))), InfoKey::Ignore);
        // Enter does not close the Info modal (distinct from Help).
        assert_eq!(classify_info_key(&key(KeyCode::Enter)), InfoKey::Ignore);
    }

    #[test]
    fn palette_text_char_accepts_plain_letters_including_jk() {
        for c in ['j', 'k', 'a', 'z', ' ', '1', '?'] {
            assert_eq!(
                palette_text_char(&key(KeyCode::Char(c))),
                Some(c),
                "plain {c:?}"
            );
        }
    }

    #[test]
    fn palette_text_char_accepts_shifted_letters() {
        // Kitty-style: Char('K') with SHIFT.
        assert_eq!(
            palette_text_char(&KeyEvent::new(KeyCode::Char('K'), KeyModifiers::SHIFT)),
            Some('K')
        );
        // Non-kitty: Char('J') with no modifier.
        assert_eq!(palette_text_char(&key(KeyCode::Char('J'))), Some('J'));
    }

    #[test]
    fn palette_text_char_rejects_modifier_combos() {
        assert_eq!(palette_text_char(&ctrl(KeyCode::Char('p'))), None);
        assert_eq!(palette_text_char(&ctrl(KeyCode::Char('n'))), None);
        assert_eq!(
            palette_text_char(&KeyEvent::new(KeyCode::Char('j'), KeyModifiers::ALT)),
            None
        );
    }

    #[test]
    fn palette_text_char_rejects_non_char_keys() {
        for code in [
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::Tab,
            KeyCode::Enter,
            KeyCode::Esc,
            KeyCode::Backspace,
        ] {
            assert_eq!(palette_text_char(&key(code)), None, "{code:?}");
        }
    }

    // -----------------------------------------------------------------------
    // apply_paste_to_modal: routes clipboard text into the open modal's
    // text field and reports whether the caller still needs to recompute
    // a filtered list.
    // -----------------------------------------------------------------------

    use crate::app::{InputAction, Modal, PaletteMode};
    use crate::path_completer::PathCompleter;
    use claude_commander_core::session::ProjectId;

    fn checkout_modal(query: &str) -> Modal {
        Modal::CheckoutBranch {
            project_id: ProjectId::new(),
            query: query.into(),
            all_branches: Vec::new(),
            filtered: Vec::new(),
            selected_idx: 0,
            scroll: 0,
            fetching: false,
        }
    }

    fn quick_switch_modal(query: &str) -> Modal {
        Modal::QuickSwitch {
            mode: PaletteMode::Unified,
            query: query.into(),
            matches: Vec::new(),
            selected_idx: 0,
            scroll: 0,
            review: None,
        }
    }

    fn input_modal(value: &str) -> Modal {
        Modal::Input {
            title: String::new(),
            prompt: String::new(),
            value: value.into(),
            on_submit: InputAction::AddProject,
            existing_branches: None,
            project_picker: None,
            program_picker: None,
            server_picker: None,
            section_picker: None,
            focus: crate::app::InputFocus::Name,
            expanded: false,
            mask: false,
        }
    }

    #[test]
    fn paste_into_checkout_branch_appends_and_requests_refilter() {
        // Regression: paste was silently dropped because the CheckoutBranch
        // arm was missing from the InputEvent::Paste match.
        let mut modal = checkout_modal("");
        let refilter = apply_paste_to_modal(&mut modal, "feature-foo");
        assert_eq!(refilter, Some(Refilter::CheckoutBranch));
        match modal {
            Modal::CheckoutBranch { query, .. } => assert_eq!(query.value(), "feature-foo"),
            _ => panic!("modal variant changed"),
        }
    }

    #[test]
    fn paste_into_checkout_branch_appends_to_existing_query() {
        let mut modal = checkout_modal("feat-");
        apply_paste_to_modal(&mut modal, "bar");
        match modal {
            Modal::CheckoutBranch { query, .. } => assert_eq!(query.value(), "feat-bar"),
            _ => panic!("modal variant changed"),
        }
    }

    #[test]
    fn paste_into_checkout_branch_strips_newlines() {
        // A multi-line paste must not contain \n / \r — Enter handling would
        // otherwise submit prematurely if the input handler ever forwarded
        // newlines as KeyCode::Enter.
        let mut modal = checkout_modal("");
        apply_paste_to_modal(&mut modal, "feature-foo\nfeature-bar\r\n");
        match modal {
            Modal::CheckoutBranch { query, .. } => {
                assert_eq!(query.value(), "feature-foofeature-bar");
            }
            _ => panic!("modal variant changed"),
        }
    }

    #[test]
    fn paste_into_quick_switch_appends_and_requests_refilter() {
        let mut modal = quick_switch_modal("");
        let refilter = apply_paste_to_modal(&mut modal, "hello");
        assert_eq!(refilter, Some(Refilter::QuickSwitch));
        match modal {
            Modal::QuickSwitch { query, .. } => assert_eq!(query.value(), "hello"),
            _ => panic!("modal variant changed"),
        }
    }

    fn review_modal_with_open_draft() -> Modal {
        use crate::app::DiffReviewState;
        use claude_commander_core::git::parse_unified_diff;
        use claude_commander_core::session::SessionId;
        let diff = parse_unified_diff(
            "\
diff --git a/a.rs b/a.rs
--- a/a.rs
+++ b/a.rs
@@ -1,2 +1,3 @@
 fn main() {
+    let y = 3;
 }
",
        );
        let mut state = DiffReviewState::new(
            SessionId::new(),
            "test".to_string(),
            "main".to_string(),
            diff,
            Vec::new(),
        );
        state.begin_comment();
        Modal::ReviewDiff(Box::new(state))
    }

    #[test]
    fn paste_into_review_comment_draft_appends_without_refilter() {
        // Regression: paste in the review view fell into the `_ => None`
        // arm and was dropped even with the comment box open.
        let mut modal = review_modal_with_open_draft();
        let refilter = apply_paste_to_modal(&mut modal, "use a helper");
        assert_eq!(refilter, None);
        match modal {
            Modal::ReviewDiff(state) => {
                assert_eq!(
                    state.comment.as_ref().unwrap().input.value(),
                    "use a helper"
                );
            }
            _ => panic!("modal variant changed"),
        }
    }

    #[test]
    fn paste_into_the_conversation_input_appends() {
        // Regression: the overlay's input had no paste arm, so a paste there
        // was silently dropped. It shares `text_field` with dictation now.
        let mut modal = Modal::Conversation {
            input: "hi ".into(),
            scroll: 0,
        };
        assert_eq!(apply_paste_to_modal(&mut modal, "there\n"), None);
        match modal {
            Modal::Conversation { input, .. } => assert_eq!(input.value(), "hi there"),
            _ => panic!("modal variant changed"),
        }
    }

    #[test]
    fn review_without_an_open_comment_has_no_text_field() {
        let mut modal = review_modal_with_open_draft();
        if let Modal::ReviewDiff(state) = &mut modal {
            state.comment = None;
        }
        assert!(text_field(&mut modal).is_none());
        assert!(dictation_field(&mut modal).is_none());
    }

    #[test]
    fn dictation_takes_every_text_field_but_secrets_and_colours() {
        // Dictation and paste share one list of text fields; dictation narrows
        // it by the masked (secret) field and the colour picker (covered in
        // `app::tests`, which can build a Settings modal).
        assert!(dictation_field(&mut input_modal("")).is_some());
        assert!(dictation_field(&mut quick_switch_modal("")).is_some());
        assert!(dictation_field(&mut checkout_modal("")).is_some());
        assert!(dictation_field(&mut review_modal_with_open_draft()).is_some());
        let mut masked = input_modal("");
        if let Modal::Input { mask, .. } = &mut masked {
            *mask = true;
        }
        assert!(
            text_field(&mut masked).is_some(),
            "a secret still takes a paste"
        );
        assert!(dictation_field(&mut masked).is_none());
        assert!(dictation_field(&mut Modal::None).is_none());
    }

    #[test]
    fn paste_into_input_appends_without_refilter() {
        // Modal::Input has no filtered list — no refilter is requested.
        let mut modal = input_modal("foo");
        let refilter = apply_paste_to_modal(&mut modal, "bar");
        assert_eq!(refilter, None);
        match modal {
            Modal::Input { value, .. } => assert_eq!(value.value(), "foobar"),
            _ => panic!("modal variant changed"),
        }
    }

    #[test]
    fn paste_into_path_input_appends_and_refilters_inline() {
        // PathInput owns its completer, so it can refilter inline without
        // the caller's help — `None` is returned.
        let mut modal = Modal::PathInput {
            title: String::new(),
            prompt: String::new(),
            value: "/tm".into(),
            on_submit: InputAction::AddProject,
            completer: PathCompleter::new(),
            scroll: 7,
        };
        let refilter = apply_paste_to_modal(&mut modal, "p");
        assert_eq!(refilter, None);
        match modal {
            Modal::PathInput { value, scroll, .. } => {
                assert_eq!(value.value(), "/tmp");
                assert_eq!(scroll, 0, "scroll resets on input change");
            }
            _ => panic!("modal variant changed"),
        }
    }

    #[test]
    fn paste_into_no_modal_is_noop() {
        let mut modal = Modal::None;
        assert_eq!(apply_paste_to_modal(&mut modal, "hello"), None);
        assert!(matches!(modal, Modal::None));
    }

    #[test]
    fn paste_into_unhandled_modal_is_noop() {
        // Help/Error/Confirm/Loading/Settings have no text input — paste is
        // intentionally ignored. Spot-check one to pin the behavior.
        let mut modal = Modal::Help { scroll: 0 };
        assert_eq!(apply_paste_to_modal(&mut modal, "hello"), None);
        assert!(matches!(modal, Modal::Help { scroll: 0 }));
    }

    // -----------------------------------------------------------------------
    // handle_input_modal_key: pure routing for the New Session dialog —
    // collapsed field focus, dropdown expand/collapse, filtering, submit.
    // -----------------------------------------------------------------------

    use crate::app::{
        InputFocus, ProgramPicker, ProjectChoice, ProjectPicker, SectionPicker, ServerPicker,
    };
    use claude_commander_core::backend::{BackendId, LOCAL_BACKEND_ID};
    use claude_commander_core::config::ProgramEntry;

    fn project_fixture(names: &[&str], selected: usize) -> ProjectPicker {
        let choices: Vec<ProjectChoice> = names
            .iter()
            .map(|n| ProjectChoice {
                id: ProjectId::new(),
                name: n.to_string(),
                repo_path: std::path::PathBuf::from(format!("/repos/{n}")),
            })
            .collect();
        let id = choices[selected].id;
        ProjectPicker::new(choices, id)
    }

    fn program_fixture(cmds: &[&str], selected: usize) -> ProgramPicker {
        ProgramPicker {
            choices: cmds
                .iter()
                .map(|c| ProgramEntry {
                    label: c.to_string(),
                    command: c.to_string(),
                })
                .collect(),
            selected,
        }
    }

    fn session_modal(project: Option<ProjectPicker>, program: Option<ProgramPicker>) -> Modal {
        Modal::Input {
            title: String::new(),
            prompt: String::new(),
            value: "".into(),
            on_submit: InputAction::AddProject,
            existing_branches: None,
            project_picker: project,
            program_picker: program,
            server_picker: None,
            section_picker: None,
            focus: InputFocus::Name,
            expanded: false,
            mask: false,
        }
    }

    /// A new-session modal carrying a server picker (local + one remote) and a
    /// section picker (catch-all + configured sections), for the server/section
    /// key-handling tests.
    fn session_modal_with_server_and_section() -> Modal {
        Modal::Input {
            title: String::new(),
            prompt: String::new(),
            value: "".into(),
            on_submit: InputAction::CreateSession {
                project_id: ProjectId::new(),
                section: None,
            },
            existing_branches: None,
            project_picker: Some(project_fixture(&["a"], 0)),
            program_picker: Some(program_fixture(&["claude"], 0)),
            server_picker: Some(ServerPicker::new(
                vec![
                    (LOCAL_BACKEND_ID, "local".to_string()),
                    (BackendId(1), "buildbox".to_string()),
                ],
                LOCAL_BACKEND_ID,
            )),
            section_picker: Some(SectionPicker::new(
                vec!["Open PRs".to_string(), "Merged".to_string()],
                None,
            )),
            focus: InputFocus::Name,
            expanded: false,
            mask: false,
        }
    }

    fn focus_of(m: &Modal) -> InputFocus {
        match m {
            Modal::Input { focus, .. } => *focus,
            _ => panic!("not an Input modal"),
        }
    }

    fn expanded_of(m: &Modal) -> bool {
        match m {
            Modal::Input { expanded, .. } => *expanded,
            _ => panic!("not an Input modal"),
        }
    }

    #[test]
    fn collapsed_enter_submits_and_esc_cancels() {
        let mut m = session_modal(
            Some(project_fixture(&["a", "b"], 0)),
            Some(program_fixture(&["claude"], 0)),
        );
        assert_eq!(
            handle_input_modal_key(&mut m, key(KeyCode::Enter)),
            InputKeyOutcome::Submit
        );
        assert_eq!(
            handle_input_modal_key(&mut m, key(KeyCode::Esc)),
            InputKeyOutcome::Cancel
        );
    }

    #[test]
    fn arrows_move_focus_between_present_rows() {
        let mut m = session_modal(
            Some(project_fixture(&["a"], 0)),
            Some(program_fixture(&["claude"], 0)),
        );
        handle_input_modal_key(&mut m, key(KeyCode::Down));
        assert_eq!(focus_of(&m), InputFocus::Project);
        handle_input_modal_key(&mut m, key(KeyCode::Down));
        assert_eq!(focus_of(&m), InputFocus::Program);
    }

    #[test]
    fn name_row_edits_text_and_stays_collapsed() {
        let mut m = session_modal(Some(project_fixture(&["a"], 0)), None);
        assert_eq!(
            handle_input_modal_key(&mut m, key(KeyCode::Char('x'))),
            InputKeyOutcome::Handled
        );
        match &m {
            Modal::Input {
                value,
                focus,
                expanded,
                ..
            } => {
                assert_eq!(value.value(), "x");
                assert_eq!(*focus, InputFocus::Name);
                assert!(!expanded);
            }
            _ => panic!("not an Input modal"),
        }
    }

    #[test]
    fn space_opens_project_dropdown() {
        let mut m = session_modal(Some(project_fixture(&["a", "b"], 0)), None);
        handle_input_modal_key(&mut m, key(KeyCode::Down)); // focus Project
        assert_eq!(focus_of(&m), InputFocus::Project);
        assert_eq!(
            handle_input_modal_key(&mut m, key(KeyCode::Char(' '))),
            InputKeyOutcome::Handled
        );
        assert!(expanded_of(&m));
    }

    #[test]
    fn typing_on_project_row_opens_and_filters() {
        let mut m = session_modal(Some(project_fixture(&["alpha", "beta"], 0)), None);
        handle_input_modal_key(&mut m, key(KeyCode::Down)); // focus Project
        handle_input_modal_key(&mut m, key(KeyCode::Char('b')));
        match &m {
            Modal::Input {
                expanded,
                project_picker: Some(p),
                ..
            } => {
                assert!(expanded);
                assert_eq!(p.filter, "b");
                assert_eq!(p.filtered.len(), 1); // only "beta" matches
            }
            _ => panic!("not an Input modal with project picker"),
        }
    }

    #[test]
    fn dropdown_navigation_then_enter_confirms_and_collapses() {
        let mut m = session_modal(Some(project_fixture(&["alpha", "beta", "gamma"], 0)), None);
        handle_input_modal_key(&mut m, key(KeyCode::Down)); // focus Project
        handle_input_modal_key(&mut m, key(KeyCode::Char(' '))); // open
        handle_input_modal_key(&mut m, key(KeyCode::Down)); // move to index 1
        assert_eq!(
            handle_input_modal_key(&mut m, key(KeyCode::Enter)),
            InputKeyOutcome::Handled
        );
        assert!(!expanded_of(&m));
        match &m {
            Modal::Input {
                project_picker: Some(p),
                ..
            } => assert_eq!(p.selected, 1),
            _ => panic!("not an Input modal with project picker"),
        }
    }

    #[test]
    fn dropdown_esc_collapses_without_cancelling() {
        let mut m = session_modal(Some(project_fixture(&["a", "b"], 0)), None);
        handle_input_modal_key(&mut m, key(KeyCode::Down));
        handle_input_modal_key(&mut m, key(KeyCode::Char(' ')));
        assert!(expanded_of(&m));
        assert_eq!(
            handle_input_modal_key(&mut m, key(KeyCode::Esc)),
            InputKeyOutcome::Handled
        );
        assert!(!expanded_of(&m));
    }

    #[test]
    fn enter_with_no_matching_project_reopens_dropdown_instead_of_submitting() {
        let mut m = session_modal(Some(project_fixture(&["alpha", "beta"], 0)), None);
        // Filter to nothing so the picker has no selectable project.
        match &mut m {
            Modal::Input {
                project_picker: Some(p),
                ..
            } => {
                p.filter = "zzz".to_string();
                p.apply_filter();
                assert!(p.selected_id().is_none());
            }
            _ => panic!("not an Input modal with project picker"),
        }
        // Enter must not submit; it reopens the Project dropdown so the empty
        // result is visible.
        assert_eq!(
            handle_input_modal_key(&mut m, key(KeyCode::Enter)),
            InputKeyOutcome::Handled
        );
        assert_eq!(focus_of(&m), InputFocus::Project);
        assert!(expanded_of(&m));
    }

    #[test]
    fn enter_submits_from_a_collapsed_picker_row_with_a_valid_selection() {
        let mut m = session_modal(
            Some(project_fixture(&["alpha"], 0)),
            Some(program_fixture(&["claude"], 0)),
        );
        // Name → Project → Program, all collapsed, valid selections.
        handle_input_modal_key(&mut m, key(KeyCode::Down));
        handle_input_modal_key(&mut m, key(KeyCode::Down));
        assert_eq!(focus_of(&m), InputFocus::Program);
        assert!(!expanded_of(&m));
        assert_eq!(
            handle_input_modal_key(&mut m, key(KeyCode::Enter)),
            InputKeyOutcome::Submit
        );
    }

    #[test]
    fn program_only_modal_skips_project_and_selects() {
        let mut m = session_modal(None, Some(program_fixture(&["claude", "codex"], 0)));
        handle_input_modal_key(&mut m, key(KeyCode::Down)); // Name → Program (no project)
        assert_eq!(focus_of(&m), InputFocus::Program);
        handle_input_modal_key(&mut m, key(KeyCode::Char(' '))); // open
        assert!(expanded_of(&m));
        handle_input_modal_key(&mut m, key(KeyCode::Down)); // select index 1
        handle_input_modal_key(&mut m, key(KeyCode::Enter)); // confirm
        assert!(!expanded_of(&m));
        match &m {
            Modal::Input {
                program_picker: Some(p),
                ..
            } => assert_eq!(p.selected, 1),
            _ => panic!("not an Input modal with program picker"),
        }
    }

    #[test]
    fn tab_visits_server_and_section_rows_when_present() {
        let mut m = session_modal_with_server_and_section();
        // Name → Server → Project → Program → Section → Name.
        handle_input_modal_key(&mut m, key(KeyCode::Tab));
        assert_eq!(focus_of(&m), InputFocus::Server);
        handle_input_modal_key(&mut m, key(KeyCode::Tab));
        assert_eq!(focus_of(&m), InputFocus::Project);
        handle_input_modal_key(&mut m, key(KeyCode::Tab));
        assert_eq!(focus_of(&m), InputFocus::Program);
        handle_input_modal_key(&mut m, key(KeyCode::Tab));
        assert_eq!(focus_of(&m), InputFocus::Section);
        handle_input_modal_key(&mut m, key(KeyCode::Tab));
        assert_eq!(focus_of(&m), InputFocus::Name);
    }

    #[test]
    fn section_dropdown_selects_and_stays_local() {
        let mut m = session_modal_with_server_and_section();
        // Focus Section (last in the ring), open it, move down, confirm.
        handle_input_modal_key(&mut m, key(KeyCode::BackTab)); // Name → Section
        assert_eq!(focus_of(&m), InputFocus::Section);
        handle_input_modal_key(&mut m, key(KeyCode::Char(' '))); // open
        assert!(expanded_of(&m));
        handle_input_modal_key(&mut m, key(KeyCode::Down)); // catch-all → "Open PRs"
        assert_eq!(
            handle_input_modal_key(&mut m, key(KeyCode::Enter)),
            InputKeyOutcome::Handled
        );
        assert!(!expanded_of(&m));
        match &m {
            Modal::Input {
                section_picker: Some(p),
                ..
            } => {
                assert_eq!(p.selected, 1);
                assert_eq!(p.selected_section().as_deref(), Some("Open PRs"));
            }
            _ => panic!("not an Input modal with a section picker"),
        }
    }

    #[test]
    fn server_confirm_after_change_signals_server_changed() {
        let mut m = session_modal_with_server_and_section();
        handle_input_modal_key(&mut m, key(KeyCode::Tab)); // Name → Server
        handle_input_modal_key(&mut m, key(KeyCode::Char(' '))); // open
        handle_input_modal_key(&mut m, key(KeyCode::Down)); // local → buildbox
        // Confirming a *changed* selection bubbles up for the async rebuild…
        assert_eq!(
            handle_input_modal_key(&mut m, key(KeyCode::Enter)),
            InputKeyOutcome::ServerChanged
        );
        match &m {
            Modal::Input {
                server_picker: Some(p),
                ..
            } => {
                assert_eq!(p.selected_backend(), Some(BackendId(1)));
                assert_eq!(p.committed, p.selected, "the change was committed");
            }
            _ => panic!("not an Input modal with a server picker"),
        }
        // …and re-confirming the same server is a no-op.
        handle_input_modal_key(&mut m, key(KeyCode::Char(' '))); // reopen
        assert_eq!(
            handle_input_modal_key(&mut m, key(KeyCode::Enter)),
            InputKeyOutcome::Handled
        );
    }

    #[test]
    fn server_dropdown_esc_snaps_back_without_signalling() {
        let mut m = session_modal_with_server_and_section();
        handle_input_modal_key(&mut m, key(KeyCode::Tab)); // Name → Server
        handle_input_modal_key(&mut m, key(KeyCode::Char(' '))); // open
        handle_input_modal_key(&mut m, key(KeyCode::Down)); // highlight buildbox
        // Esc abandons the highlight → back to the applied server, no signal.
        assert_eq!(
            handle_input_modal_key(&mut m, key(KeyCode::Esc)),
            InputKeyOutcome::Handled
        );
        assert!(!expanded_of(&m));
        match &m {
            Modal::Input {
                server_picker: Some(p),
                ..
            } => assert_eq!(p.selected_backend(), Some(LOCAL_BACKEND_ID)),
            _ => panic!("not an Input modal with a server picker"),
        }
    }

    #[test]
    fn opens_palette_accepts_the_leader_and_ctrl_space() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let leader = (KeyCode::Char(' '), KeyModifiers::NONE);
        assert!(opens_palette(&key(KeyCode::Char(' ')), leader.0, leader.1));
        assert!(opens_palette(
            &KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL),
            leader.0,
            leader.1
        ));
        // A different key, and the leader with an extra modifier (Shift+leader
        // is the command palette, handled before this), are not it.
        assert!(!opens_palette(&key(KeyCode::Char('j')), leader.0, leader.1));
        assert!(!opens_palette(
            &KeyEvent::new(KeyCode::Char(' '), KeyModifiers::SHIFT),
            leader.0,
            leader.1
        ));
    }

    #[test]
    fn ctrl_space_opens_the_palette_whatever_the_leader_is() {
        // Ctrl+Space is the one binding every surface honours, so it must not
        // depend on how `leader_key` is configured.
        use crossterm::event::{KeyCode, KeyModifiers};
        assert!(opens_palette(
            &KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL),
            KeyCode::F(1),
            KeyModifiers::NONE
        ));
    }
}
