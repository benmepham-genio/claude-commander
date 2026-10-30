//! Settings modal: row building, rendering, editing, and key handling.

use super::colour_picker::PickerOutcome;
use super::*;
use claude_commander_core::config::theme::ThemeOverrides;

/// Shown in the Server tab's Bearer Token row in place of the token itself, which
/// is operator-equivalent and must not be on screen during a screen-share. Also
/// what `apply_settings_edit` recognises as "submitted unchanged", so the two
/// cannot drift apart.
const TOKEN_SET_PLACEHOLDER: &str = "(set)";
const TOKEN_UNSET_PLACEHOLDER: &str = "(not set \u{2014} generated on first serve)";

impl App {
    /// Refresh the cached input-device list (id + friendly label) backing the
    /// STT Microphone setting. Called when the settings modal or the mic picker
    /// opens, so the row and picker show current devices without enumerating the
    /// audio host on the render path.
    pub(super) fn refresh_input_devices(&mut self) {
        self.stt_input_devices = claude_commander_core::conversation::recorder::input_devices();
    }

    /// Friendly display label for the currently-selected STT microphone: the
    /// cached device's label, `(default)` when unset, or the raw id tagged
    /// `(not connected)` when the configured device isn't currently present.
    fn input_device_label(&self) -> String {
        match self.config.stt.input_device.as_deref() {
            None => "(default)".to_string(),
            Some(id) => self
                .stt_input_devices
                .iter()
                .find(|d| d.id == id)
                .map(|d| d.label.clone())
                .unwrap_or_else(|| format!("{id} (not connected)")),
        }
    }

    pub(super) fn build_settings_rows(&self, tab: SettingsTab) -> Vec<SettingsRow> {
        match tab {
            SettingsTab::General => {
                // Grouped into logical sections, each preceded by a
                // non-selectable header row; `with_section_spacers` inserts a
                // blank line between groups so the long list is easy to scan
                // (mirrors the Keybindings tab).
                let c = &self.config;
                with_section_spacers(vec![
                    SettingsRow::header("Sessions & Worktrees"),
                    SettingsRow::text(
                        "Branch Prefix",
                        if c.branch_prefix.is_empty() {
                            "(none)".to_string()
                        } else {
                            c.branch_prefix.clone()
                        },
                        "branch_prefix",
                    ),
                    SettingsRow::text("Shell Program", c.shell_program.clone(), "shell_program"),
                    SettingsRow::text(
                        "Worktrees Directory",
                        c.worktrees_dir
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "(default)".into()),
                        "worktrees_dir",
                    ),
                    SettingsRow::text(
                        "Projects Directory",
                        c.projects_dir
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "(default)".into()),
                        "projects_dir",
                    ),
                    SettingsRow::text(
                        "Clone Timeout (s)",
                        c.clone_timeout_secs.to_string(),
                        "clone_timeout_secs",
                    ),
                    SettingsRow::text(
                        "Repo List Timeout (s)",
                        c.repo_list_timeout_secs.to_string(),
                        "repo_list_timeout_secs",
                    ),
                    SettingsRow::toggle(
                        "Per-Repo Worktree Dirs",
                        c.per_repo_worktree_dirs,
                        "per_repo_worktree_dirs",
                    ),
                    SettingsRow::toggle(
                        "Fetch Before Create",
                        c.fetch_before_create,
                        "fetch_before_create",
                    ),
                    SettingsRow::toggle("Skip LFS Smudge", c.skip_lfs_smudge, "skip_lfs_smudge"),
                    SettingsRow::toggle("Resume Session", c.resume_session, "resume_session"),
                    SettingsRow::toggle("Nix Develop", c.nix_develop, "nix_develop"),
                    SettingsRow::toggle(
                        "Hibernate Idle Sessions",
                        c.hibernate_enabled,
                        "hibernate_enabled",
                    ),
                    SettingsRow::text(
                        "Hibernate Idle Timeout (s)",
                        c.hibernate_idle_timeout_secs.to_string(),
                        "hibernate_idle_timeout_secs",
                    ),
                    SettingsRow::text(
                        "Hibernate Check Interval (s)",
                        c.hibernate_check_interval_secs.to_string(),
                        "hibernate_check_interval_secs",
                    ),
                    SettingsRow::text(
                        "In Progress WIP Limit",
                        c.in_progress_limit
                            .map(|n| n.to_string())
                            .unwrap_or_else(|| "(unlimited)".into()),
                        "in_progress_limit",
                    ),
                    SettingsRow::header("Editor"),
                    SettingsRow::text(
                        "Editor",
                        c.editor.clone().unwrap_or_else(|| "(auto)".into()),
                        "editor",
                    ),
                    SettingsRow::text(
                        "Editor is GUI",
                        match c.editor_gui {
                            Some(true) => "true",
                            Some(false) => "false",
                            None => "(auto)",
                        },
                        "editor_gui",
                    ),
                    SettingsRow::header("Pull Requests & Sync"),
                    SettingsRow::text(
                        "PR Check Interval (s)",
                        c.pr_check_interval_secs.to_string(),
                        "pr_check_interval_secs",
                    ),
                    SettingsRow::toggle(
                        "Project Pull Enabled",
                        c.project_pull_enabled,
                        "project_pull_enabled",
                    ),
                    SettingsRow::text(
                        "Project Pull Interval (s)",
                        c.project_pull_interval_secs.to_string(),
                        "project_pull_interval_secs",
                    ),
                    SettingsRow::header("Appearance"),
                    SettingsRow::toggle(
                        "Invert PR Label Color",
                        c.invert_pr_label_color,
                        "invert_pr_label_color",
                    ),
                    SettingsRow::toggle(
                        "Show Session Program",
                        c.show_session_program,
                        "show_session_program",
                    ),
                    SettingsRow::toggle(
                        "Hide Empty Sections",
                        c.hide_empty_sections,
                        "hide_empty_sections",
                    ),
                    SettingsRow::text(
                        "Recent Sessions Limit",
                        c.recent_sessions_limit.to_string(),
                        "recent_sessions_limit",
                    ),
                    SettingsRow::toggle("Rounded Borders", c.rounded_borders, "rounded_borders"),
                    SettingsRow::toggle(
                        "Dim Right Pane",
                        c.dim_unfocused_preview,
                        "dim_unfocused_preview",
                    ),
                    SettingsRow::text(
                        "Dim Opacity",
                        c.dim_unfocused_opacity.to_string(),
                        "dim_unfocused_opacity",
                    ),
                    SettingsRow::header("Performance"),
                    SettingsRow::text(
                        "UI Refresh FPS",
                        c.ui_refresh_fps.to_string(),
                        "ui_refresh_fps",
                    ),
                    SettingsRow::text(
                        "Max Concurrent Tmux",
                        c.max_concurrent_tmux.to_string(),
                        "max_concurrent_tmux",
                    ),
                    SettingsRow::toggle(
                        "Precompute Review Caches",
                        c.precompute_review_caches,
                        "precompute_review_caches",
                    ),
                    SettingsRow::text(
                        "Number Debounce (ms)",
                        c.session_number_debounce_ms.to_string(),
                        "session_number_debounce_ms",
                    ),
                    SettingsRow::header("AI Summaries"),
                    SettingsRow::toggle(
                        "AI Summary Enabled",
                        c.ai_summary_enabled,
                        "ai_summary_enabled",
                    ),
                    SettingsRow::text(
                        "AI Summary Model",
                        c.ai_summary_model.clone(),
                        "ai_summary_model",
                    ),
                    SettingsRow::header("Commander"),
                    SettingsRow::toggle(
                        "Commander Enabled",
                        c.commander_enabled,
                        "commander_enabled",
                    ),
                    SettingsRow::text(
                        "Commander Program",
                        c.commander_program
                            .clone()
                            .unwrap_or_else(|| "(default)".into()),
                        "commander_program",
                    ),
                    SettingsRow::text(
                        "Commander Directory",
                        c.commander_dir
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "(default)".into()),
                        "commander_dir",
                    ),
                    SettingsRow::header("Privacy"),
                    SettingsRow::toggle(
                        "Usage Telemetry",
                        c.telemetry.enabled,
                        "telemetry_enabled",
                    ),
                ])
            }
            SettingsTab::Voice => {
                // Three headed groups, in the order a reader goes looking:
                // speech coming in, speech going out, and the conversation agent
                // that joins the two. The grouping is PRESENTATION ONLY — the
                // `[stt]` and `[conversation]` TOML tables and every `field_key`
                // below are persisted shapes and stay exactly where they are, so
                // a config file written before this split still loads and a
                // settings edit still lands in the same table. What changed is
                // which heading a row is drawn under, nothing more. (Note the
                // split crosses the tables: "Text-to-Speech" and "Conversation
                // Mode" are both `[conversation]` fields, because "is this about
                // the voice or about the agent?" is the question a reader has,
                // and "which table is it in?" is not.)
                //
                // The STT labels drop their "STT " prefix: the group heading now
                // carries that, and "Base URL" under Transcription cannot be
                // confused with "Base URL" under Text-to-Speech.
                let c = &self.config.conversation;
                let s = &self.config.stt;
                with_section_spacers(vec![
                    SettingsRow::header("Transcription"),
                    SettingsRow::toggle("Enable Voice Input (STT)", s.enabled, "stt_enabled"),
                    SettingsRow::text("Base URL", s.base_url.clone(), "stt_base_url"),
                    SettingsRow::text("Model", s.model.clone(), "stt_model"),
                    SettingsRow::text(
                        "Language",
                        s.language.clone().unwrap_or_else(|| "(auto)".into()),
                        "stt_language",
                    ),
                    SettingsRow::text(
                        "Prompt",
                        s.prompt.clone().unwrap_or_else(|| "(none)".into()),
                        "stt_prompt",
                    ),
                    SettingsRow::text("Microphone", self.input_device_label(), "stt_input_device"),
                    SettingsRow::toggle(
                        if self.pause_media_needs_restart() {
                            "Pause Media While Recording (restart to apply)"
                        } else {
                            "Pause Media While Recording"
                        },
                        s.pause_media,
                        "stt_pause_media",
                    ),
                    SettingsRow::text(
                        "Dictation Submit",
                        s.dictation_submit.label().to_string(),
                        "stt_dictation_submit",
                    ),
                    SettingsRow::header("Text-to-Speech"),
                    SettingsRow::text("Base URL", c.base_url.clone(), "conversation_base_url"),
                    SettingsRow::text("Model", c.model.clone(), "conversation_model"),
                    SettingsRow::text(
                        "Voice",
                        c.voice.clone().unwrap_or_else(|| "(default)".into()),
                        "conversation_voice",
                    ),
                    SettingsRow::text(
                        "Response Format",
                        c.response_format.clone(),
                        "conversation_format",
                    ),
                    SettingsRow::text("Speed", format!("{:.2}", c.speed), "conversation_speed"),
                    SettingsRow::text("Volume", format!("{:.2}", c.volume), "conversation_volume"),
                    SettingsRow::text(
                        "Speak Scope",
                        c.speak_scope.label().to_string(),
                        "conversation_speak_scope",
                    ),
                    SettingsRow::header("Conversation Mode"),
                    SettingsRow::toggle(
                        "Enable Conversation Mode",
                        c.enabled,
                        "conversation_enabled",
                    ),
                    SettingsRow::text("Assistant Name", c.name.clone(), "conversation_name"),
                ])
            }
            SettingsTab::Server => {
                let s = &self.config.server;
                let token_display = match &s.token {
                    Some(_) => TOKEN_SET_PLACEHOLDER,
                    None => TOKEN_UNSET_PLACEHOLDER,
                };
                with_section_spacers(vec![
                    SettingsRow::header("Embedded Server"),
                    SettingsRow::toggle("Auto Start With TUI", s.auto_start, "server_auto_start"),
                    SettingsRow::text("Bind Address", s.bind.to_string(), "server_bind"),
                    SettingsRow::text("Port", s.port.to_string(), "server_port"),
                    // The token is deliberately not shown. It is operator-
                    // equivalent (see docs/configuration.md), the settings modal
                    // is on screen while screen-sharing, and the STT API key sets
                    // the same precedent by being absent from this UI entirely.
                    // Use the palette's "Copy server token" to hand it to a
                    // client; typing over this row replaces it.
                    SettingsRow::text("Bearer Token", token_display.to_string(), "server_token"),
                    SettingsRow::header("Browser Access"),
                    SettingsRow::text(
                        "CORS Allowed Origins",
                        if s.cors_allowed_origins.is_empty() {
                            "(none)".to_string()
                        } else {
                            s.cors_allowed_origins.join(", ")
                        },
                        "server_cors_allowed_origins",
                    ),
                ])
            }
            SettingsTab::Sections | SettingsTab::Workspaces => {
                vec![]
            }
            SettingsTab::Programs => {
                vec![]
            }
            SettingsTab::Keybindings => {
                // Grouped into logical sections (see `BindableAction::section`),
                // each preceded by a non-selectable header row and a blank
                // spacer so the long list is easy to scan.
                let kb = &self.config.keybindings;
                let mut rows = Vec::new();
                for (section, actions) in kb.sections() {
                    rows.push(SettingsRow::header(section));
                    for (action, keys) in actions {
                        rows.push(SettingsRow::text(
                            action.description(),
                            keys,
                            action.config_name(),
                        ));
                    }
                }
                with_section_spacers(rows)
            }
            SettingsTab::Theme => self.theme_rows(&self.default_theme_scope()),
        }
    }

    /// The rows for `state`'s tab — the Theme tab's for its scope.
    pub(super) fn settings_rows(&self, state: &SettingsState) -> Vec<SettingsRow> {
        match state.tab {
            SettingsTab::Theme => self.theme_rows(&state.theme_scope),
            tab => self.build_settings_rows(tab),
        }
    }

    /// The Theme tab for `scope`: a scope row once there are workspaces, a
    /// reset row for a workspace, then the preset, appearance and every colour
    /// row. A row with no value of its own in the scope shows the value it
    /// inherits, dim, with where it comes from — `(usual)` for a workspace
    /// layered over the usual theme, `(preset)` otherwise.
    pub(super) fn theme_rows(&self, scope: &ThemeScope) -> Vec<SettingsRow> {
        let scope = self.effective_theme_scope(scope);
        let merged = self.merged_workspaces();
        let none = ThemeOverrides::default();
        let (o, t) = match &scope {
            ThemeScope::Usual => (
                &self.config.theme,
                crate::theme::Theme::from_overrides(&self.config.theme),
            ),
            ThemeScope::Workspace(ws) => (
                self.config
                    .workspace_themes
                    .get(crate::theme::workspace_theme_key(ws.as_deref()))
                    .unwrap_or(&none),
                self.workspace_theme(ws.as_deref()),
            ),
        };
        let from = inherited_from(&scope, o);

        let mut rows = Vec::new();
        if claude_commander_viewmodel::workspace::workspaces_visible(&merged) {
            rows.push(SettingsRow::text(
                "Theme for",
                theme_scope_label(&scope, &merged),
                "theme_scope",
            ));
        }
        if let ThemeScope::Workspace(ws) = &scope {
            rows.push(SettingsRow::text(
                "Reset to usual theme",
                if self.workspace_theme_customised(ws.as_deref()) {
                    "customised"
                } else {
                    "(already usual)"
                },
                "theme_reset",
            ));
        }
        rows.push(SettingsRow::text(
            "Preset",
            o.preset.clone().unwrap_or_else(|| match scope {
                ThemeScope::Usual => "(auto)".into(),
                ThemeScope::Workspace(_) => "(usual)".into(),
            }),
            "preset",
        ));
        rows.push(SettingsRow::text(
            "Appearance",
            o.appearance
                .map(|a| a.as_str().to_string())
                .unwrap_or_else(|| format!("({from})")),
            "appearance",
        ));

        macro_rules! theme_row {
            ($label:expr, $field:ident) => {{
                let mut row = SettingsRow::swatch(
                    $label,
                    // The config spelling (`format_color` writes the same
                    // forms `ColorValue` serializes to), whether set here or
                    // inherited.
                    format_color(o.$field.map_or(t.$field, |cv| cv.0)),
                    stringify!($field),
                    t.$field,
                );
                if o.$field.is_none() {
                    row.inherited_from = Some(from);
                }
                row
            }};
        }

        rows.extend([
            theme_row!("Border Focused", border_focused),
            theme_row!("Border Unfocused", border_unfocused),
            theme_row!("Selection BG", selection_bg),
            theme_row!("Status Running", status_running),
            theme_row!("Status Stopped", status_stopped),
            theme_row!("Status PR", status_pr),
            theme_row!("Status PR Merged", status_pr_merged),
            theme_row!("PR Open", pr_open),
            theme_row!("PR Draft", pr_draft),
            theme_row!("PR Closed", pr_closed),
            theme_row!("Text Primary", text_primary),
            theme_row!("Text Secondary", text_secondary),
            theme_row!("Text Accent", text_accent),
            theme_row!("Diff Added", diff_added),
            theme_row!("Diff Removed", diff_removed),
            theme_row!("Diff Hunk Header", diff_hunk_header),
            theme_row!("Diff File Header", diff_file_header),
            theme_row!("Modal Info", modal_info),
            theme_row!("Modal Warning", modal_warning),
            theme_row!("Modal Error", modal_error),
            theme_row!("Status Bar BG", status_bar_bg),
            theme_row!("Status Bar FG", status_bar_fg),
            theme_row!("Status Bar Accent", status_bar_accent),
        ]);
        rows
    }

    /// The theme `scope` edits, resolved — what its rows show and its colour
    /// picker offers.
    fn scope_theme(&self, scope: &ThemeScope) -> crate::theme::Theme {
        match self.effective_theme_scope(scope) {
            ThemeScope::Usual => crate::theme::Theme::from_overrides(&self.config.theme),
            ThemeScope::Workspace(ws) => self.workspace_theme(ws.as_deref()),
        }
    }

    /// The overrides `scope` edits, as they stand (an empty table for a
    /// workspace without an entry).
    fn scope_overrides(&self, scope: &ThemeScope) -> ThemeOverrides {
        match self.effective_theme_scope(scope) {
            ThemeScope::Usual => self.config.theme.clone(),
            ThemeScope::Workspace(ws) => self
                .config
                .workspace_themes
                .get(crate::theme::workspace_theme_key(ws.as_deref()))
                .cloned()
                .unwrap_or_default(),
        }
    }

    /// Edit the overrides `scope` stands for, then rebuild the worn theme and
    /// persist. A workspace entry edited back to empty is removed, so the
    /// workspace reads as using the usual theme again.
    fn edit_theme_overrides(&mut self, scope: &ThemeScope, edit: impl FnOnce(&mut ThemeOverrides)) {
        match self.effective_theme_scope(scope) {
            ThemeScope::Usual => edit(&mut self.config.theme),
            ThemeScope::Workspace(ws) => {
                let key = crate::theme::workspace_theme_key(ws.as_deref()).to_string();
                let entry = self.config.workspace_themes.entry(key.clone()).or_default();
                edit(entry);
                if *entry == ThemeOverrides::default() {
                    self.config.workspace_themes.remove(&key);
                }
            }
        }
        // Also refreshes the project-colour cache so card borders repaint.
        self.reload_theme();
        self.persist_config();
    }

    /// A Preset or Appearance pick in `scope`. The inherit spellings — empty,
    /// `(auto)`, `(usual)`, `(preset)` — clear the scope's own value.
    pub(super) fn apply_theme_edit(&mut self, scope: &ThemeScope, field_key: &str, value: &str) {
        use claude_commander_core::config::theme::AppearanceValue;
        let inherit = value.is_empty() || matches!(value, "(auto)" | "(usual)" | "(preset)");
        self.edit_theme_overrides(scope, |o| match field_key {
            "preset" => o.preset = (!inherit).then(|| value.to_string()),
            // Anything unparseable is ignored rather than silently flipping
            // the surface.
            "appearance" => {
                o.appearance = if inherit {
                    None
                } else if let Some(a) = AppearanceValue::parse(value) {
                    Some(a)
                } else {
                    warn!("Unknown theme appearance: {value:?} (expected dark or light)");
                    o.appearance
                };
            }
            _ => {}
        });
    }

    /// A colour picker's pick on colour row `field_key` in `scope`: `Some`
    /// sets the scope's own value, `None` clears it so the row inherits.
    pub(super) fn apply_theme_colour(
        &mut self,
        scope: &ThemeScope,
        field_key: &str,
        hex: Option<&str>,
    ) {
        use claude_commander_core::config::theme::ColorValue;
        let value = hex
            .and_then(crate::widgets::parse_hex_color)
            .map(ColorValue);
        if hex.is_some() && value.is_none() {
            warn!("Ignoring an unparseable theme colour {hex:?}");
            return;
        }
        self.edit_theme_overrides(scope, |o| {
            if let Some(slot) = theme_colour_slot(o, field_key) {
                *slot = value;
            }
        });
    }

    /// The scope a Theme-tab editor opened on (pinned by `theme_row_enter`),
    /// if a pick can still land there. A workspace that went while the editor
    /// was open — a hot reload, a remote snapshot dropping it — discards the
    /// pick with a toast rather than falling back to the usual theme, which the
    /// editor's header never named.
    fn theme_pick_scope(&mut self, scope: &ThemeScope) -> Option<ThemeScope> {
        if self.effective_theme_scope(scope) == *scope {
            return Some(scope.clone());
        }
        self.toast(match scope {
            ThemeScope::Workspace(Some(name)) => {
                format!("{name} is no longer a workspace; nothing was changed")
            }
            _ => "Main's own theme is unused with one workspace; nothing was changed".to_string(),
        });
        None
    }

    /// "Reset to usual theme": drop `scope`'s workspace entry.
    fn reset_workspace_theme(&mut self, scope: &ThemeScope) {
        let ThemeScope::Workspace(ws) = self.effective_theme_scope(scope) else {
            return;
        };
        let label = self
            .merged_workspaces()
            .into_iter()
            .find(|w| w.name == ws)
            .map_or_else(|| "Main".to_string(), |w| w.label);
        let key = crate::theme::workspace_theme_key(ws.as_deref());
        if self.config.workspace_themes.remove(key).is_none() {
            self.toast(format!("{label} already uses the usual theme"));
            return;
        }
        self.reload_theme();
        self.persist_config();
        self.toast(format!("{label} now uses the usual theme"));
    }

    /// Open the colour picker on colour row `field_key` in `scope`, fitted to
    /// the terminal's current size so a key in the same burst as the Enter
    /// already steps by the drawn row width. `None` if the row is not a colour.
    fn open_theme_colour_picker(
        &self,
        scope: &ThemeScope,
        field_key: &str,
    ) -> Option<super::colour_picker::ColourPicker> {
        let mut overrides = self.scope_overrides(scope);
        let current = (*theme_colour_slot(&mut overrides, field_key)?)
            .and_then(|cv| crate::theme::color_to_hex(cv.0));
        let from = inherited_from(&self.effective_theme_scope(scope), &overrides);
        let mut picker = super::colour_picker::ColourPicker::open(
            &self.scope_theme(scope),
            current.as_deref(),
            format!("Inherit ({from})"),
        );
        self.fit_colour_picker(&mut picker, self.ui_state.terminal_size);
        Some(picker)
    }

    /// Fit `picker` to the pane it is drawn in within a frame `area`. Before
    /// the first frame the size is unknown, so the picker keeps its default.
    pub(super) fn fit_colour_picker(
        &self,
        picker: &mut super::colour_picker::ColourPicker,
        area: Rect,
    ) {
        if area.width > 0 && area.height > 0 {
            picker.fit_to_width(colour_picker_width(area));
        }
    }

    /// Enter on a Theme-tab row outside editing.
    fn theme_row_enter(&mut self, state: &mut SettingsState) {
        let Some(row) = state.rows.get(state.selected_row) else {
            return;
        };
        let field_key = row.field_key.clone();
        let current_value = row.text_value().to_string();
        // Pinned: the editor opened here names this scope, so its pick applies
        // to it — or nowhere, if it has gone by then ([`Self::theme_pick_scope`]).
        let scope = self.effective_theme_scope(&state.theme_scope);
        state.theme_scope = scope.clone();
        let picker_of = |options: Vec<PickerOption>, current: &str| {
            let selected = options.iter().position(|o| o.value == current).unwrap_or(0);
            SettingsEditing::OptionPicker { options, selected }
        };
        match field_key.as_str() {
            "theme_scope" => {
                let merged = self.merged_workspaces();
                let mut options: Vec<PickerOption> = merged
                    .iter()
                    .map(|w| PickerOption {
                        label: w.label.clone(),
                        value: theme_scope_value(&ThemeScope::Workspace(w.name.clone())),
                    })
                    .collect();
                options.push(PickerOption {
                    label: theme_scope_label(&ThemeScope::Usual, &merged),
                    value: theme_scope_value(&ThemeScope::Usual),
                });
                state.editing = Some(picker_of(options, &theme_scope_value(&scope)));
            }
            "theme_reset" => {
                self.reset_workspace_theme(&scope);
                state.rows = self.settings_rows(state);
            }
            "preset" => {
                // A workspace either inherits the usual theme or picks a base
                // of its own; "(auto)" is the usual theme's inherit spelling.
                use crate::theme::PRESET_NAMES;
                let options: Vec<PickerOption> = match scope {
                    ThemeScope::Usual => PRESET_NAMES
                        .iter()
                        .map(|s| PickerOption::plain(*s))
                        .collect(),
                    ThemeScope::Workspace(_) => std::iter::once("(usual)")
                        .chain(PRESET_NAMES.iter().copied().filter(|p| *p != "(auto)"))
                        .map(PickerOption::plain)
                        .collect(),
                };
                state.editing = Some(picker_of(options, &current_value));
            }
            "appearance" => {
                // Two named values plus "inherit" — a picker rather than free
                // text, so there is nothing to mistype.
                let inherit = format!(
                    "({})",
                    inherited_from(&scope, &self.scope_overrides(&scope))
                );
                let options: Vec<PickerOption> = [inherit.as_str(), "dark", "light"]
                    .iter()
                    .map(|s| PickerOption::plain(*s))
                    .collect();
                state.editing = Some(picker_of(options, &current_value));
            }
            key => {
                if let Some(picker) = self.open_theme_colour_picker(&scope, key) {
                    state.editing = Some(SettingsEditing::Colour {
                        picker: Box::new(picker),
                    });
                }
            }
        }
    }

    /// Render the settings modal.
    pub(super) fn render_settings_modal(
        &self,
        frame: &mut Frame,
        area: Rect,
        state: &SettingsState,
    ) {
        let SettingsAreas {
            modal: modal_area,
            content: content_area,
            body: body_area,
            footer: footer_area,
        } = settings_areas(area);
        frame.render_widget(Clear, modal_area);

        let block = Block::default()
            .title(" Settings ")
            .borders(Borders::ALL)
            .border_type(self.border_type())
            .border_style(Style::default().fg(self.theme.modal_info));
        frame.render_widget(block, modal_area);

        if content_area.height < 4 {
            return;
        }

        // --- Tab bar (row 0) ---
        let tab_area = Rect {
            height: 1,
            ..content_area
        };
        let mut tab_spans: Vec<Span> = Vec::new();
        for (i, tab) in SettingsTab::ALL.iter().enumerate() {
            if i > 0 {
                tab_spans.push(Span::raw("  "));
            }
            let style = if *tab == state.tab {
                Style::default()
                    .fg(self.theme.text_primary)
                    .add_modifier(Modifier::BOLD)
                    .add_modifier(Modifier::UNDERLINED)
            } else {
                Style::default().fg(self.theme.text_secondary)
            };
            tab_spans.push(Span::styled(tab.label(), style));
        }
        frame.render_widget(Paragraph::new(Line::from(tab_spans)), tab_area);

        // --- Separator ---
        let sep_area = Rect {
            y: content_area.y + 1,
            height: 1,
            ..content_area
        };
        let separator = "─".repeat(content_area.width as usize);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                separator,
                Style::default().fg(self.theme.border_unfocused),
            ))),
            sep_area,
        );

        if state.tab == SettingsTab::Sections {
            self.render_sections_tab(frame, body_area, footer_area, &state.sections_state);
        } else if state.tab == SettingsTab::Workspaces {
            self.render_workspaces_tab(frame, body_area, footer_area, &state.workspaces_state);
        } else if state.tab == SettingsTab::Programs {
            self.render_programs_tab(frame, body_area, footer_area, &state.programs_state);
        } else {
            self.render_settings_rows(frame, body_area, footer_area, state);
        }
    }

    fn render_settings_rows(
        &self,
        frame: &mut Frame,
        rows_area: Rect,
        footer_area: Rect,
        state: &SettingsState,
    ) {
        // When the Keybindings search box is focused, reserve the top line for
        // the filter input and shrink the list below it.
        let rows_area = if let Some(query) = &state.search {
            let search_area = Rect {
                height: 1,
                ..rows_area
            };
            let prompt = format!("/{}", super::input_with_caret(query));
            frame.render_widget(
                Paragraph::new(Span::styled(
                    prompt,
                    Style::default()
                        .fg(self.theme.text_accent)
                        .add_modifier(Modifier::BOLD),
                )),
                search_area,
            );
            Rect {
                y: rows_area.y + 1,
                height: rows_area.height.saturating_sub(1),
                ..rows_area
            }
        } else {
            rows_area
        };

        // An open colour picker takes the rows' place: the row it edits (and
        // for which theme), then the grid and the hex row.
        if let Some(SettingsEditing::Colour { picker }) = &state.editing {
            let label = state
                .rows
                .get(state.selected_row)
                .map_or_else(String::new, |r| r.label.clone());
            let scope = theme_scope_label(
                &self.effective_theme_scope(&state.theme_scope),
                &self.merged_workspaces(),
            );
            let mut lines = vec![
                Line::from(vec![
                    Span::styled(
                        label,
                        Style::default()
                            .fg(self.theme.text_primary)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("  {scope}"),
                        Style::default().fg(self.theme.text_secondary),
                    ),
                ]),
                Line::from(""),
            ];
            lines.extend(picker.lines(&self.theme));
            frame.render_widget(Paragraph::new(lines), rows_area);
            self.render_settings_footer(frame, footer_area, state);
            return;
        }

        if state.rows.is_empty() {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    "  (no matching shortcuts)",
                    Style::default().fg(self.theme.text_secondary),
                )),
                rows_area,
            );
            self.render_settings_footer(frame, footer_area, state);
            return;
        }

        let label_width = settings_label_width(&state.rows, rows_area.width);
        let value_width = rows_area.width.saturating_sub(label_width + 3);

        let visible_rows = rows_area.height as usize;
        let scroll_offset = list_scroll_offset(state.selected_row, visible_rows);

        // Check if the OptionPicker is active and how many rows it occupies
        let picker_info: Option<(usize, &[PickerOption], usize)> =
            if let Some(SettingsEditing::OptionPicker { options, selected }) = &state.editing {
                // screen_row is the row index (within visible area) where the picker starts
                let screen_row = state.selected_row.saturating_sub(scroll_offset);
                Some((screen_row, options.as_slice(), *selected))
            } else {
                None
            };

        // How many rows the picker will overlay (starting from the selected row)
        let picker_row_count = picker_info
            .map(|(screen_row, opts, _)| {
                let rows_below = visible_rows.saturating_sub(screen_row);
                opts.len().min(rows_below)
            })
            .unwrap_or(0);

        for (i, row) in state
            .rows
            .iter()
            .enumerate()
            .skip(scroll_offset)
            .take(visible_rows)
        {
            let screen_idx = i - scroll_offset;
            let y = rows_area.y + screen_idx as u16;
            let is_selected = i == state.selected_row;

            // If the OptionPicker is open, skip rendering normal rows that are
            // overlaid by picker options (except the first picker row itself,
            // which replaces the selected row).
            if let Some((picker_screen_row, _, _)) = picker_info
                && screen_idx > picker_screen_row
                && screen_idx < picker_screen_row + picker_row_count
            {
                continue;
            }

            // Section headers span the full width and are never selectable.
            if matches!(row.kind, SettingsRowKind::Header) {
                let header_area = Rect {
                    x: rows_area.x,
                    y,
                    width: rows_area.width,
                    height: 1,
                };
                frame.render_widget(
                    Paragraph::new(Span::styled(
                        row.label.clone(),
                        Style::default()
                            .fg(self.theme.text_accent)
                            .add_modifier(Modifier::BOLD),
                    )),
                    header_area,
                );
                continue;
            }

            let row_style = if is_selected {
                self.theme.selection()
            } else {
                Style::default()
            };

            // Label
            let label_area = Rect {
                x: rows_area.x,
                y,
                width: label_width.min(rows_area.width),
                height: 1,
            };
            let label = format!("{:<width$}", row.label, width = label_width as usize);
            frame.render_widget(Paragraph::new(Span::styled(label, row_style)), label_area);

            // Color swatch + Value
            if rows_area.width > label_width + 2 {
                let swatch_width: u16 = if row.color_swatch.is_some() { 3 } else { 0 };
                let val_x = rows_area.x + label_width + 2;

                // Render color swatch if present
                if let Some(swatch_color) = row.color_swatch {
                    let swatch_area = Rect {
                        x: val_x,
                        y,
                        width: swatch_width.min(value_width),
                        height: 1,
                    };
                    let swatch_style = if is_selected {
                        Style::default()
                            .fg(swatch_color)
                            .bg(self.theme.selection_bg)
                    } else {
                        Style::default().fg(swatch_color)
                    };
                    frame.render_widget(
                        Paragraph::new(Span::styled("██ ", swatch_style)),
                        swatch_area,
                    );
                }

                let val_area = Rect {
                    x: val_x + swatch_width,
                    y,
                    width: value_width.saturating_sub(swatch_width),
                    height: 1,
                };

                let display_val = match &row.kind {
                    // Toggles never enter an editing state; render as a checkbox.
                    SettingsRowKind::Toggle(on) => {
                        if *on {
                            "[x]".to_string()
                        } else {
                            "[ ]".to_string()
                        }
                    }
                    SettingsRowKind::Text(text) if is_selected => {
                        if let Some(SettingsEditing::TextInput { value }) = &state.editing {
                            super::input_with_caret(value)
                        } else if let Some(SettingsEditing::OptionPicker { options, selected }) =
                            &state.editing
                        {
                            // Show the currently highlighted option on the selected row
                            format!("▸ {}", options[*selected].label)
                        } else {
                            text.clone()
                        }
                    }
                    SettingsRowKind::Text(text) => match row.inherited_from {
                        Some(from) => format!("{text}  ({from})"),
                        None => text.clone(),
                    },
                    // Headers `continue` above and never reach the value column.
                    SettingsRowKind::Header => String::new(),
                };

                let val_style = if !matches!(row.kind, SettingsRowKind::Toggle(_))
                    && is_selected
                    && state.editing.is_some()
                {
                    row_style.add_modifier(Modifier::UNDERLINED)
                } else if row.inherited_from.is_some() {
                    // Inherited, not set here: dim, so the scope's own values
                    // stand out.
                    row_style.fg(self.theme.text_secondary)
                } else {
                    row_style.fg(self.theme.text_accent)
                };

                frame.render_widget(
                    Paragraph::new(Span::styled(display_val, val_style)),
                    val_area,
                );
            }
        }

        // Render the OptionPicker dropdown rows below the selected row
        if let Some((picker_screen_row, options, selected_opt)) = picker_info {
            let val_x = rows_area.x + label_width + 2;
            let val_w = value_width;

            for (opt_idx, option) in options.iter().enumerate().take(picker_row_count) {
                let row_y = rows_area.y + (picker_screen_row + opt_idx) as u16;
                let is_highlighted = opt_idx == selected_opt;

                // Clear the label area for overlay rows beyond the first
                if opt_idx > 0 {
                    let clear_area = Rect {
                        x: rows_area.x,
                        y: row_y,
                        width: label_width.min(rows_area.width),
                        height: 1,
                    };
                    frame.render_widget(Clear, clear_area);
                    frame.render_widget(Paragraph::new(Span::raw("")), clear_area);
                }

                let opt_area = Rect {
                    x: val_x,
                    y: row_y,
                    width: val_w,
                    height: 1,
                };

                // Clear before rendering
                frame.render_widget(Clear, opt_area);

                let prefix = if is_highlighted { "▸ " } else { "  " };
                let opt_style = if is_highlighted {
                    self.theme.selection()
                } else {
                    Style::default().fg(self.theme.text_accent)
                };

                frame.render_widget(
                    Paragraph::new(Span::styled(format!("{prefix}{}", option.label), opt_style)),
                    opt_area,
                );
            }
        }

        self.render_settings_footer(frame, footer_area, state);
    }

    /// Footer hint line for the settings rows view.
    fn render_settings_footer(&self, frame: &mut Frame, footer_area: Rect, state: &SettingsState) {
        let selected_is_toggle = state
            .rows
            .get(state.selected_row)
            .is_some_and(|r| matches!(r.kind, SettingsRowKind::Toggle(_)));
        let footer_text = if state.search.is_some() {
            "Type to filter  ↑/↓: navigate  Enter: keep  Esc: clear"
        } else if state.editing.is_some() {
            match &state.editing {
                Some(SettingsEditing::OptionPicker { .. }) => {
                    "j/k: navigate  Enter: select  Esc: cancel"
                }
                Some(SettingsEditing::Colour { picker }) => picker.footer_hint(),
                _ => "Enter: save  Esc: cancel",
            }
        } else if state.tab == SettingsTab::Keybindings {
            "Tab: switch tab  j/k: navigate  /: search  Esc: close"
        } else if selected_is_toggle {
            "Tab: switch tab  j/k: navigate  Space/Enter: toggle  Esc: close"
        } else {
            "Tab: switch tab  j/k: navigate  Enter: edit  Esc: close"
        };
        frame.render_widget(
            Paragraph::new(Span::styled(
                footer_text,
                Style::default().fg(self.theme.text_secondary),
            )),
            footer_area,
        );
    }

    /// Draw the full-height `│` divider between the list and detail panes of a
    /// two-pane settings tab (Sections / Programs).
    pub(super) fn render_settings_divider(&self, frame: &mut Frame, divider_area: Rect) {
        for row in 0..divider_area.height {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    "│",
                    Style::default().fg(self.theme.border_unfocused),
                )),
                Rect {
                    x: divider_area.x,
                    y: divider_area.y + row,
                    width: 1,
                    height: 1,
                },
            );
        }
    }

    fn render_sections_tab(
        &self,
        frame: &mut Frame,
        body_area: Rect,
        footer_area: Rect,
        sec: &SectionsState,
    ) {
        let sections = &self.config.sections;
        let list_width = body_area.width.clamp(16, 28);
        let divider_width = 1_u16;
        let pred_width = body_area
            .width
            .saturating_sub(list_width + divider_width + 1);

        let list_area = Rect {
            width: list_width,
            ..body_area
        };
        let divider_area = Rect {
            x: body_area.x + list_width,
            width: divider_width,
            ..body_area
        };
        let pred_area = Rect {
            x: body_area.x + list_width + divider_width + 1,
            width: pred_width,
            ..body_area
        };

        // --- Divider ---
        self.render_settings_divider(frame, divider_area);

        // --- Section list ---
        if let Some(SectionsEditing::CreatingSection { value }) = &sec.editing {
            let visible = list_area.height as usize;
            let name_rows = sections.len().min(visible.saturating_sub(1));
            for (i, section) in sections.iter().enumerate().take(name_rows) {
                let y = list_area.y + i as u16;
                let style = Style::default().fg(self.theme.text_secondary);
                let name = truncate_str(&section.name, list_width as usize - 2);
                frame.render_widget(
                    Paragraph::new(Span::styled(format!("  {name}"), style)),
                    Rect {
                        y,
                        height: 1,
                        ..list_area
                    },
                );
            }
            let input_y = list_area.y + name_rows as u16;
            let input_style = self.theme.selection().add_modifier(Modifier::UNDERLINED);
            let display = format!("  {}", super::input_with_caret(value));
            frame.render_widget(
                Paragraph::new(Span::styled(display, input_style)),
                Rect {
                    y: input_y,
                    height: 1,
                    ..list_area
                },
            );
        } else {
            let visible = list_area.height as usize;
            let scroll = list_scroll_offset(sec.selected_section, visible);
            for (i, section) in sections.iter().enumerate().skip(scroll).take(visible) {
                let y = list_area.y + (i - scroll) as u16;
                let is_selected = i == sec.selected_section;
                let is_focused = sec.focus == SectionsFocus::List;

                let style = if is_selected && is_focused {
                    self.theme.selection()
                } else if is_selected {
                    Style::default()
                        .fg(self.theme.text_primary)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(self.theme.text_secondary)
                };

                let prefix = if is_selected { "▸ " } else { "  " };
                let name = if is_selected {
                    if let Some(SectionsEditing::RenamingSection { value }) = &sec.editing {
                        format!("{prefix}{}", super::input_with_caret(value))
                    } else {
                        let n = truncate_str(&section.name, list_width as usize - 2);
                        format!("{prefix}{n}")
                    }
                } else {
                    let n = truncate_str(&section.name, list_width as usize - 2);
                    format!("{prefix}{n}")
                };

                let row_style = if is_selected
                    && matches!(sec.editing, Some(SectionsEditing::RenamingSection { .. }))
                {
                    style.add_modifier(Modifier::UNDERLINED)
                } else {
                    style
                };

                frame.render_widget(
                    Paragraph::new(Span::styled(name, row_style)),
                    Rect {
                        y,
                        height: 1,
                        ..list_area
                    },
                );
            }

            if sections.is_empty() {
                frame.render_widget(
                    Paragraph::new(Span::styled(
                        "  (no sections)",
                        Style::default().fg(self.theme.text_secondary),
                    )),
                    Rect {
                        height: 1,
                        ..list_area
                    },
                );
            }
        }

        // --- Predicate editor (right side) ---
        if !sections.is_empty() && sec.selected_section < sections.len() {
            let section = &sections[sec.selected_section];
            let pred_rows = predicate_rows(section);
            let is_pred_focused = sec.focus == SectionsFocus::Predicates;

            for (i, (label, value)) in pred_rows.iter().enumerate() {
                if i as u16 >= pred_area.height {
                    break;
                }
                let y = pred_area.y + i as u16;
                let is_selected = is_pred_focused && i == sec.pred_selected;

                let style = if is_selected {
                    self.theme.selection()
                } else {
                    Style::default()
                };

                let label_w = 18_u16.min(pred_area.width);
                frame.render_widget(
                    Paragraph::new(Span::styled(
                        format!("{:<w$}", label, w = label_w as usize),
                        style,
                    )),
                    Rect {
                        x: pred_area.x,
                        y,
                        width: label_w,
                        height: 1,
                    },
                );

                if pred_area.width > label_w + 1 {
                    let val_x = pred_area.x + label_w + 1;
                    let val_w = pred_area.width.saturating_sub(label_w + 1);

                    let display_val = if is_selected {
                        if let Some(SectionsEditing::EditingPredicate { value: v }) = &sec.editing {
                            super::input_with_caret(v)
                        } else {
                            value.clone()
                        }
                    } else {
                        value.clone()
                    };

                    let val_style = if is_selected && sec.editing.is_some() {
                        style.add_modifier(Modifier::UNDERLINED)
                    } else {
                        style.fg(self.theme.text_accent)
                    };

                    frame.render_widget(
                        Paragraph::new(Span::styled(display_val, val_style)),
                        Rect {
                            x: val_x,
                            y,
                            width: val_w,
                            height: 1,
                        },
                    );
                }
            }
        }

        // --- Footer ---
        let footer_text = match &sec.editing {
            Some(
                SectionsEditing::RenamingSection { .. } | SectionsEditing::EditingPredicate { .. },
            ) => "Enter: save  Esc: cancel",
            Some(SectionsEditing::CreatingSection { .. }) => "Enter: create  Esc: cancel",
            None if sec.focus == SectionsFocus::List => {
                "n: new  r: rename  d: delete  J/K: reorder  →/Enter: predicates  Tab: switch tab"
            }
            None => "Enter: edit  ←: back to list  Tab: switch tab",
        };
        frame.render_widget(
            Paragraph::new(Span::styled(
                footer_text,
                Style::default().fg(self.theme.text_secondary),
            )),
            footer_area,
        );
    }

    /// Apply an edited value from the settings modal to the config.
    pub(super) fn apply_settings_edit(&mut self, tab: SettingsTab, field_key: &str, value: &str) {
        match tab {
            SettingsTab::General => match field_key {
                "branch_prefix" => self.config.branch_prefix = value.to_string(),
                "shell_program" => self.config.shell_program = value.to_string(),
                "worktrees_dir" => {
                    self.config.worktrees_dir = if value.is_empty() || value == "(default)" {
                        None
                    } else {
                        Some(PathBuf::from(value))
                    };
                }
                "projects_dir" => {
                    self.config.projects_dir = if value.is_empty() || value == "(default)" {
                        None
                    } else {
                        Some(PathBuf::from(value))
                    };
                }
                "clone_timeout_secs" => match value.parse::<u64>() {
                    Ok(v) if v >= 30 => {
                        self.config.clone_timeout_secs = v;
                    }
                    Ok(_) => {
                        self.ui_state.status_message = Some((
                            "Clone Timeout must be at least 30 seconds".into(),
                            std::time::Instant::now() + std::time::Duration::from_secs(4),
                        ));
                    }
                    Err(_) => {}
                },
                // Same 30s floor as the clone timeout, and for the same reason: a
                // sub-30s budget cannot list a large account, and there is no page
                // cap to fall back on, so it would produce an empty picker rather
                // than a short one. No zero-disables case — the whole point of the
                // bound is that an abandoned `gh api --paginate` gets killed.
                "repo_list_timeout_secs" => match value.parse::<u64>() {
                    Ok(v) if v >= 30 => {
                        self.config.repo_list_timeout_secs = v;
                    }
                    Ok(_) => {
                        self.ui_state.status_message = Some((
                            "Repo List Timeout must be at least 30 seconds".into(),
                            std::time::Instant::now() + std::time::Duration::from_secs(4),
                        ));
                    }
                    Err(_) => {}
                },
                "editor" => {
                    self.config.editor = if value.is_empty() || value == "(auto)" {
                        None
                    } else {
                        Some(value.to_string())
                    };
                }
                "editor_gui" => {
                    self.config.editor_gui = match value {
                        "true" => Some(true),
                        "false" => Some(false),
                        _ => None,
                    };
                }
                "ui_refresh_fps" => match value.parse::<u32>() {
                    Ok(v) if v >= 1 => {
                        self.config.ui_refresh_fps = v;
                    }
                    Ok(_) => {
                        self.ui_state.status_message = Some((
                            "UI Refresh FPS must be at least 1".into(),
                            std::time::Instant::now() + std::time::Duration::from_secs(4),
                        ));
                    }
                    Err(_) => {}
                },
                "pr_check_interval_secs" => {
                    if let Ok(v) = value.parse::<u64>() {
                        self.config.pr_check_interval_secs = v;
                    }
                }
                "hibernate_idle_timeout_secs" => match value.parse::<u64>() {
                    Ok(v) if v >= 60 => {
                        self.config.hibernate_idle_timeout_secs = v;
                    }
                    Ok(_) => {
                        self.ui_state.status_message = Some((
                            "Hibernate Idle Timeout must be at least 60 seconds".into(),
                            std::time::Instant::now() + std::time::Duration::from_secs(4),
                        ));
                    }
                    Err(_) => {}
                },
                "hibernate_check_interval_secs" => match value.parse::<u64>() {
                    Ok(v) if v >= 10 => {
                        self.config.hibernate_check_interval_secs = v;
                    }
                    Ok(_) => {
                        self.ui_state.status_message = Some((
                            "Hibernate Check Interval must be at least 10 seconds".into(),
                            std::time::Instant::now() + std::time::Duration::from_secs(4),
                        ));
                    }
                    Err(_) => {}
                },
                "project_pull_interval_secs" => match value.parse::<u64>() {
                    Ok(v) if v >= 60 => {
                        self.config.project_pull_interval_secs = v;
                    }
                    Ok(_) => {
                        self.ui_state.status_message = Some((
                            "Project Pull Interval must be at least 60 seconds".into(),
                            std::time::Instant::now() + std::time::Duration::from_secs(4),
                        ));
                    }
                    Err(_) => {}
                },
                "max_concurrent_tmux" => match value.parse::<usize>() {
                    Ok(v) if v >= 1 => {
                        self.config.max_concurrent_tmux = v;
                    }
                    Ok(_) => {
                        self.ui_state.status_message = Some((
                            "Max Concurrent Tmux must be at least 1".into(),
                            std::time::Instant::now() + std::time::Duration::from_secs(4),
                        ));
                    }
                    Err(_) => {}
                },
                "session_number_debounce_ms" => {
                    if let Ok(v) = value.parse::<u64>() {
                        self.config.session_number_debounce_ms = v;
                    }
                }
                "recent_sessions_limit" => {
                    if let Ok(v) = value.parse::<u32>() {
                        self.config.recent_sessions_limit = v;
                    }
                }
                // An opacity outside 0.0..=1.0 would render the pane black or
                // brighter than the source, so clamp rather than reject.
                "dim_unfocused_opacity" => {
                    if let Ok(v) = value.parse::<f32>() {
                        self.config.dim_unfocused_opacity = v.clamp(0.0, 1.0);
                    }
                }
                "ai_summary_model" => {
                    self.config.ai_summary_model = value.to_string();
                }
                "commander_enabled" => {
                    if let Ok(b) = value.parse::<bool>() {
                        self.config.commander_enabled = b;
                    }
                }
                "commander_program" => {
                    self.config.commander_program = if value.is_empty() || value == "(default)" {
                        None
                    } else {
                        Some(value.to_string())
                    };
                }
                "commander_dir" => {
                    self.config.commander_dir = if value.is_empty() || value == "(default)" {
                        None
                    } else {
                        Some(PathBuf::from(value))
                    };
                }
                "in_progress_limit" => {
                    self.config.in_progress_limit = if value.is_empty() || value == "(unlimited)" {
                        None
                    } else {
                        value.parse::<u32>().ok().filter(|&n| n > 0)
                    };
                }
                _ => {}
            },
            SettingsTab::Voice => match field_key {
                "conversation_name" => {
                    let v = value.trim();
                    self.config.conversation.name = if v.is_empty() {
                        "Claudette".to_string()
                    } else {
                        v.to_string()
                    };
                }
                "conversation_base_url" => self.config.conversation.base_url = value.to_string(),
                "conversation_model" => self.config.conversation.model = value.to_string(),
                "conversation_voice" => {
                    self.config.conversation.voice = if value.is_empty() || value == "(default)" {
                        None
                    } else {
                        Some(value.to_string())
                    };
                }
                "conversation_format" => {
                    self.config.conversation.response_format = value.to_string();
                }
                "conversation_speed" => {
                    if let Ok(v) = value.parse::<f32>() {
                        self.config.conversation.speed = v.clamp(0.25, 4.0);
                    }
                }
                "conversation_volume" => {
                    if let Ok(v) = value.parse::<f32>() {
                        self.config.conversation.volume = v.clamp(0.0, 2.0);
                    }
                }
                "conversation_speak_scope" => {
                    // The picker passes the human label; config/tests use tokens.
                    if let Some(scope) =
                        claude_commander_core::conversation::SpeakScope::from_token(value).or_else(
                            || claude_commander_core::conversation::SpeakScope::from_label(value),
                        )
                    {
                        self.config.conversation.speak_scope = scope;
                    }
                }
                "stt_base_url" => self.config.stt.base_url = value.to_string(),
                "stt_model" => self.config.stt.model = value.to_string(),
                "stt_language" => {
                    self.config.stt.language = if value.is_empty() || value == "(auto)" {
                        None
                    } else {
                        Some(value.to_string())
                    };
                }
                "stt_prompt" => {
                    self.config.stt.prompt = if value.is_empty() || value == "(none)" {
                        None
                    } else {
                        Some(value.to_string())
                    };
                }
                "stt_input_device" => {
                    let v = value.trim();
                    self.config.stt.input_device =
                        if v.is_empty() || v == "(default)" || v == "(auto)" {
                            None
                        } else {
                            Some(v.to_string())
                        };
                }
                "stt_dictation_submit" => {
                    // The picker passes the human label; config/tests use tokens.
                    // An unrecognised value is ignored rather than reset to the
                    // default, so a typo can't quietly disable a submit policy.
                    if let Some(policy) =
                        claude_commander_core::conversation::DictationSubmit::from_token(value)
                            .or_else(|| {
                                claude_commander_core::conversation::DictationSubmit::from_label(
                                    value,
                                )
                            })
                    {
                        self.config.stt.dictation_submit = policy;
                        // The dictation consumer captures the policy when the
                        // listener is built, so a running listener would keep
                        // applying the old one until the next restart. Rebuild
                        // it now — the same mechanism the microphone picker
                        // uses, and a no-op when no listener is running.
                        self.respawn_listener();
                    }
                }
                _ => {}
            },
            SettingsTab::Theme => {
                // The key handler edits the tab's own scope through
                // `apply_theme_edit`; an unscoped edit is the usual theme's.
                self.apply_theme_edit(&ThemeScope::Usual, field_key, value);
                return;
            }
            SettingsTab::Keybindings => {
                use claude_commander_core::config::keybindings::{BindableAction, KeyBinding};
                use std::str::FromStr;

                let Ok(action) = BindableAction::from_str(field_key) else {
                    warn!("Unknown keybinding action: {}", field_key);
                    return;
                };

                // The row value is rendered as a comma-separated list
                // (e.g. `"k, Up, Ctrl-p"`). Parse each entry back into a
                // `KeyBinding`, ignoring empty tokens. If every token fails
                // to parse we leave the binding alone rather than silently
                // clear it.
                let mut parsed: Vec<KeyBinding> = Vec::new();
                let mut had_token = false;
                let mut any_err = false;
                for token in value.split(',') {
                    let t = token.trim();
                    if t.is_empty() {
                        continue;
                    }
                    had_token = true;
                    match KeyBinding::from_str(t) {
                        Ok(kb) => parsed.push(kb),
                        Err(e) => {
                            warn!("Invalid keybinding '{}': {}", t, e);
                            any_err = true;
                        }
                    }
                }

                if had_token && parsed.is_empty() && any_err {
                    // User tried to edit but every token was malformed —
                    // show the error but don't wipe their existing binding.
                    self.ui_state.modal = Modal::Error {
                        message: format!(
                            "Could not parse any key bindings from '{}'. \
                             Use e.g. 'k', 'Ctrl-p', 'Shift-N', 'Enter'.",
                            value
                        ),
                    };
                    return;
                }

                self.config.keybindings.set_keys_for(action, parsed);
            }
            SettingsTab::Server => match field_key {
                "server_bind" => match value.trim().parse::<std::net::IpAddr>() {
                    Ok(ip) => self.config.server.bind = ip,
                    Err(_) => {
                        self.ui_state.status_message = Some((
                            format!("Not an IP address: {value} (try 127.0.0.1 or 0.0.0.0)"),
                            std::time::Instant::now() + std::time::Duration::from_secs(4),
                        ));
                        return;
                    }
                },
                // Port 0 is a legal port that binds an ephemeral one the
                // operator cannot predict — useless for a client that has to be
                // pointed at it — so it is refused here.
                "server_port" => match value.trim().parse::<u16>() {
                    Ok(p) if p > 0 => self.config.server.port = p,
                    _ => {
                        self.ui_state.status_message = Some((
                            format!("Not a port: {value} (1-65535)"),
                            std::time::Instant::now() + std::time::Duration::from_secs(4),
                        ));
                        return;
                    }
                },
                "server_token" => {
                    let trimmed = value.trim();
                    // The row renders a placeholder instead of the secret, so
                    // submitting it unchanged has to mean "leave it alone" rather
                    // than "set the token to the literal text `(set)`". Matched
                    // exactly, not by a leading `(`, so a token that happens to
                    // start with one is still settable.
                    if trimmed == TOKEN_SET_PLACEHOLDER || trimmed == TOKEN_UNSET_PLACEHOLDER {
                        return;
                    }
                    self.config.server.token = if trimmed.is_empty() {
                        None
                    } else {
                        Some(trimmed.to_string())
                    };
                }
                "server_cors_allowed_origins" => {
                    let trimmed = value.trim();
                    self.config.server.cors_allowed_origins =
                        if trimmed.is_empty() || trimmed == "(none)" {
                            Vec::new()
                        } else {
                            trimmed
                                .split(',')
                                .map(|o| o.trim().to_string())
                                .filter(|o| !o.is_empty())
                                .collect()
                        };
                }
                _ => {
                    warn!("Unknown server setting: {}", field_key);
                    return;
                }
            },
            SettingsTab::Sections => {
                // Sections tab handles its own persistence via save_sections_config
                return;
            }
            SettingsTab::Workspaces => {
                // Workspaces tab propagates its own edits (handle_workspaces_key)
                return;
            }
            SettingsTab::Programs => {
                // Programs tab handles its own persistence in handle_programs_key
                return;
            }
        }

        self.persist_config();
    }

    /// Set a boolean General-tab setting to a typed value and persist.
    ///
    /// Booleans are stored as real `bool`s on [`Config`]; this avoids the
    /// stringify/parse round-trip used by the text-input edit path.
    pub(super) fn apply_bool_setting(&mut self, field_key: &str, value: bool) {
        match field_key {
            "per_repo_worktree_dirs" => self.config.per_repo_worktree_dirs = value,
            "fetch_before_create" => self.config.fetch_before_create = value,
            "skip_lfs_smudge" => self.config.skip_lfs_smudge = value,
            "resume_session" => self.config.resume_session = value,
            "hibernate_enabled" => self.config.hibernate_enabled = value,
            "nix_develop" => self.config.nix_develop = value,
            "project_pull_enabled" => self.config.project_pull_enabled = value,
            "invert_pr_label_color" => self.config.invert_pr_label_color = value,
            "show_session_program" => self.config.show_session_program = value,
            "hide_empty_sections" => self.config.hide_empty_sections = value,
            "rounded_borders" => self.config.rounded_borders = value,
            "dim_unfocused_preview" => self.config.dim_unfocused_preview = value,
            "precompute_review_caches" => self.config.precompute_review_caches = value,
            "ai_summary_enabled" => self.config.ai_summary_enabled = value,
            "commander_enabled" => self.config.commander_enabled = value,
            "conversation_enabled" => self.config.conversation.enabled = value,
            "stt_enabled" => self.config.stt.enabled = value,
            "stt_pause_media" => self.config.stt.pause_media = value,
            "telemetry_enabled" => self.config.telemetry.enabled = value,
            "server_auto_start" => self.config.server.auto_start = value,
            _ => {
                warn!("Unknown boolean setting: {}", field_key);
                return;
            }
        }
        self.persist_config();
    }

    /// Persist the current config via the store (updates mtime so hot-reload
    /// won't re-read our own write).
    pub(super) fn persist_config(&mut self) {
        let updated = self.config.clone();
        if let Err(e) = self.service.update_config(updated) {
            warn!("Failed to save config: {}", e);
        }
    }

    /// Switch the settings modal to the next (`forward`) or previous tab,
    /// rebuilding its rows and resetting the selection to the first selectable
    /// row. The caller restores the modal afterwards.
    pub(super) fn switch_settings_tab(&self, state: &mut SettingsState, forward: bool) {
        state.tab = if forward {
            state.tab.next()
        } else {
            state.tab.prev()
        };
        state.rows = self.settings_rows(state);
        state.selected_row = first_selectable_from(&state.rows, 0);
        // Landing on the Programs tab loads its target's list (local from config,
        // a remote server asynchronously).
        if state.tab == SettingsTab::Programs {
            self.load_programs_target(&mut state.programs_state);
        }
    }

    /// Handle a keypress in the settings modal.
    pub(super) async fn handle_settings_key(
        &mut self,
        key: crossterm::event::KeyEvent,
        mut state: SettingsState,
    ) {
        use crossterm::event::KeyCode;

        if state.tab == SettingsTab::Sections {
            self.handle_sections_key(key, state).await;
            return;
        }

        if state.tab == SettingsTab::Workspaces {
            self.handle_workspaces_key(key, state).await;
            return;
        }

        if state.tab == SettingsTab::Programs {
            self.handle_programs_key(key, state);
            return;
        }

        // Keybindings-tab search: while the filter box is focused, typing
        // narrows the list live and the arrows navigate the matches.
        if state.search.is_some() {
            self.handle_keybinding_search_key(key, state);
            return;
        }

        // `/` opens the search box on the Keybindings tab.
        if state.tab == SettingsTab::Keybindings
            && state.editing.is_none()
            && key.code == KeyCode::Char('/')
        {
            state.search = Some(Input::default());
            state.selected_row = first_selectable_from(&state.rows, 0);
            self.ui_state.modal = Modal::Settings(state);
            return;
        }

        if let Some(ref mut editing) = state.editing {
            // Currently editing a field
            match editing {
                SettingsEditing::TextInput { value } => match key.code {
                    KeyCode::Enter => {
                        let val = value.value().to_string();
                        let field_key = state.rows[state.selected_row].field_key.clone();
                        state.editing = None;
                        self.apply_settings_edit(state.tab, &field_key, &val);
                        // Refresh rows after applying
                        state.rows = self.settings_rows(&state);
                        self.ui_state.modal = Modal::Settings(state);
                    }
                    KeyCode::Esc => {
                        state.editing = None;
                        self.ui_state.modal = Modal::Settings(state);
                    }
                    _ => {
                        super::edit_text_input(value, key);
                        self.ui_state.modal = Modal::Settings(state);
                    }
                },
                SettingsEditing::KeyCapture { .. } => {
                    // For key capture, any keypress except Esc is captured as the new binding
                    match key.code {
                        KeyCode::Esc => {
                            state.editing = None;
                            self.ui_state.modal = Modal::Settings(state);
                        }
                        _ => {
                            // Key capture is a simplified version — store the key display
                            // Full keybinding editing would require more complex UX
                            state.editing = None;
                            self.ui_state.modal = Modal::Settings(state);
                        }
                    }
                }
                SettingsEditing::Colour { picker } => {
                    match picker.handle_key(key) {
                        PickerOutcome::Open => {}
                        PickerOutcome::Cancel => state.editing = None,
                        PickerOutcome::Pick(hex) => {
                            state.editing = None;
                            let field_key = state.rows[state.selected_row].field_key.clone();
                            if let Some(scope) = self.theme_pick_scope(&state.theme_scope) {
                                self.apply_theme_colour(&scope, &field_key, hex.as_deref());
                            }
                            state.rows = self.settings_rows(&state);
                        }
                    }
                    self.ui_state.modal = Modal::Settings(state);
                }
                SettingsEditing::OptionPicker { options, selected } => match key.code {
                    KeyCode::Enter => {
                        // Commit the entry's value (the display label and stored
                        // value differ only for the mic picker's device ids).
                        let chosen = options[*selected].value.clone();
                        let field_key = state.rows[state.selected_row].field_key.clone();
                        // Treat "(auto)" as empty string for apply_settings_edit
                        let val = if chosen == "(auto)" {
                            String::new()
                        } else {
                            chosen
                        };
                        state.editing = None;
                        if state.tab == SettingsTab::Theme {
                            if field_key == "theme_scope" {
                                if let Some(scope) = parse_theme_scope(&val) {
                                    state.theme_scope = scope;
                                }
                            } else if let Some(scope) = self.theme_pick_scope(&state.theme_scope) {
                                self.apply_theme_edit(&scope, &field_key, &val);
                            }
                            state.rows = self.settings_rows(&state);
                            state.selected_row =
                                state.selected_row.min(state.rows.len().saturating_sub(1));
                            self.ui_state.modal = Modal::Settings(state);
                            return;
                        }
                        let prev_input_device = self.config.stt.input_device.clone();
                        self.apply_settings_edit(state.tab, &field_key, &val);
                        // Selecting a *different* microphone rebuilds the running
                        // listener so the new device is used on the next recording,
                        // live. Skip the respawn when the id is unchanged (e.g.
                        // re-picking the current device) to avoid a needless
                        // teardown / re-open of the same device.
                        if field_key == "stt_input_device"
                            && self.config.stt.input_device != prev_input_device
                        {
                            self.respawn_listener();
                        }
                        state.rows = self.settings_rows(&state);
                        self.ui_state.modal = Modal::Settings(state);
                    }
                    KeyCode::Esc => {
                        state.editing = None;
                        self.ui_state.modal = Modal::Settings(state);
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        *selected = (*selected + 1) % options.len();
                        self.ui_state.modal = Modal::Settings(state);
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        *selected = if *selected == 0 {
                            options.len() - 1
                        } else {
                            *selected - 1
                        };
                        self.ui_state.modal = Modal::Settings(state);
                    }
                    _ => {
                        self.ui_state.modal = Modal::Settings(state);
                    }
                },
            }
        } else {
            // Not editing — navigation mode: resolve via configurable keybindings
            use claude_commander_core::config::keybindings::BindableAction;

            // Boolean rows flip in place on Enter/Space/Left/Right without
            // opening an editor.
            let toggle_key = matches!(
                key.code,
                KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Left | KeyCode::Right
            );
            let new_val = toggle_key
                .then(|| {
                    state
                        .rows
                        .get(state.selected_row)
                        .and_then(SettingsRow::toggled)
                })
                .flatten();
            if let Some(new_val) = new_val {
                let field_key = state.rows[state.selected_row].field_key.clone();
                self.apply_bool_setting(&field_key, new_val);
                state.rows = self.settings_rows(&state);
                self.ui_state.modal = Modal::Settings(state);
                return;
            }

            match self.config.keybindings.resolve(&key) {
                Some(BindableAction::NavigateDown) => {
                    state.selected_row = step_selectable(&state.rows, state.selected_row, true);
                    self.ui_state.modal = Modal::Settings(state);
                }
                Some(BindableAction::NavigateUp) => {
                    state.selected_row = step_selectable(&state.rows, state.selected_row, false);
                    self.ui_state.modal = Modal::Settings(state);
                }
                Some(BindableAction::Quit) => {
                    self.ui_state.modal = Modal::None;
                }
                _ => match key.code {
                    KeyCode::Esc => {
                        self.ui_state.modal = Modal::None;
                    }
                    KeyCode::Tab => {
                        self.switch_settings_tab(&mut state, true);
                        self.ui_state.modal = Modal::Settings(state);
                    }
                    KeyCode::BackTab => {
                        self.switch_settings_tab(&mut state, false);
                        self.ui_state.modal = Modal::Settings(state);
                    }
                    KeyCode::Enter if state.tab == SettingsTab::Theme => {
                        self.theme_row_enter(&mut state);
                        self.ui_state.modal = Modal::Settings(state);
                    }
                    KeyCode::Enter => {
                        if !state.rows.is_empty() {
                            let field_key = &state.rows[state.selected_row].field_key;
                            if field_key == "conversation_speak_scope" {
                                // Inline option picker for the speak-scope enum.
                                use claude_commander_core::conversation::SpeakScope;
                                let options: Vec<PickerOption> = SpeakScope::ALL
                                    .iter()
                                    .map(|s| PickerOption::plain(s.label()))
                                    .collect();
                                let current_value = state.rows[state.selected_row].text_value();
                                let selected = options
                                    .iter()
                                    .position(|o| o.value == current_value)
                                    .unwrap_or(0);
                                state.editing =
                                    Some(SettingsEditing::OptionPicker { options, selected });
                            } else if field_key == "stt_dictation_submit" {
                                // Inline option picker for the dictation submit
                                // policy — three named values, so a picker
                                // rather than free text (same shape as the
                                // speak-scope row above).
                                use claude_commander_core::conversation::DictationSubmit;
                                let options: Vec<PickerOption> = DictationSubmit::ALL
                                    .iter()
                                    .map(|s| PickerOption::plain(s.label()))
                                    .collect();
                                let current_value = state.rows[state.selected_row].text_value();
                                let selected = options
                                    .iter()
                                    .position(|o| o.value == current_value)
                                    .unwrap_or(0);
                                state.editing =
                                    Some(SettingsEditing::OptionPicker { options, selected });
                            } else if field_key == "stt_input_device" {
                                // Inline option picker for the microphone: each entry
                                // shows a friendly label but commits the stable device
                                // id. "(default)" comes first for the system default.
                                // Refresh the device cache so the list is current.
                                self.refresh_input_devices();
                                let mut options = vec![PickerOption::plain("(default)")];
                                options.extend(self.stt_input_devices.iter().map(|d| {
                                    PickerOption {
                                        label: d.label.clone(),
                                        value: d.id.clone(),
                                    }
                                }));
                                // Preselect by the stored device id (the row shows
                                // the friendly label, so read config directly).
                                let current_id = self
                                    .config
                                    .stt
                                    .input_device
                                    .clone()
                                    .unwrap_or_else(|| "(default)".into());
                                // Keep a configured-but-unplugged device selectable so
                                // opening the picker doesn't silently reset it.
                                if current_id != "(default)"
                                    && !options.iter().any(|o| o.value == current_id)
                                {
                                    options.push(PickerOption {
                                        label: format!("{current_id} (not connected)"),
                                        value: current_id.clone(),
                                    });
                                }
                                let selected = options
                                    .iter()
                                    .position(|o| o.value == current_id)
                                    .unwrap_or(0);
                                state.editing =
                                    Some(SettingsEditing::OptionPicker { options, selected });
                            } else {
                                let current_value = state.rows[state.selected_row].text_value();
                                let initial =
                                    if current_value == "(auto)" || current_value == "(none)" {
                                        String::new()
                                    } else {
                                        current_value.to_string()
                                    };
                                state.editing = Some(SettingsEditing::TextInput {
                                    value: initial.into(),
                                });
                            }
                        }
                        self.ui_state.modal = Modal::Settings(state);
                    }
                    _ => {
                        self.ui_state.modal = Modal::Settings(state);
                    }
                },
            }
        }
    }

    /// Handle a keypress while the Keybindings search box is focused. Typing
    /// filters the shortcut list live; arrows navigate the matches; `Enter`
    /// keeps the filter and returns to browsing; `Esc` clears it.
    fn handle_keybinding_search_key(
        &mut self,
        key: crossterm::event::KeyEvent,
        mut state: SettingsState,
    ) {
        use crossterm::event::KeyCode;

        match key.code {
            KeyCode::Esc => {
                // Clear the filter and restore the full grouped list.
                state.search = None;
                state.rows = self.settings_rows(&state);
                state.selected_row = first_selectable_from(&state.rows, 0);
            }
            KeyCode::Enter => {
                // Keep the filtered view; drop back to list navigation.
                state.search = None;
                state.selected_row = first_selectable_from(&state.rows, state.selected_row);
            }
            KeyCode::Down => {
                state.selected_row = step_selectable(&state.rows, state.selected_row, true);
            }
            KeyCode::Up => {
                state.selected_row = step_selectable(&state.rows, state.selected_row, false);
            }
            _ => {
                if let Some(input) = state.search.as_mut()
                    && super::edit_text_input(input, key)
                {
                    // Query changed: re-filter from the full grouped list.
                    let query = input.value().to_string();
                    let full = self.build_settings_rows(state.tab);
                    state.rows = filter_keybinding_rows(full, &query);
                    state.selected_row = first_selectable_from(&state.rows, 0);
                }
            }
        }
        self.ui_state.modal = Modal::Settings(state);
    }

    /// Handle a keypress while the Sections tab is active.
    async fn handle_sections_key(
        &mut self,
        key: crossterm::event::KeyEvent,
        mut state: SettingsState,
    ) {
        use crossterm::event::KeyCode;

        let sec = &mut state.sections_state;

        // --- Editing mode ---
        if let Some(ref mut editing) = sec.editing {
            match editing {
                SectionsEditing::RenamingSection { value } => match key.code {
                    KeyCode::Enter => {
                        let new_name = value.value().trim().to_string();
                        let old_name = self
                            .config
                            .sections
                            .get(sec.selected_section)
                            .map(|s| s.name.clone());
                        sec.editing = None;
                        self.ui_state.modal = Modal::Settings(state);
                        if let Some(old_name) = old_name {
                            self.rename_section(&old_name, &new_name).await;
                        }
                    }
                    KeyCode::Esc => {
                        sec.editing = None;
                        self.ui_state.modal = Modal::Settings(state);
                    }
                    _ => {
                        super::edit_text_input(value, key);
                        self.ui_state.modal = Modal::Settings(state);
                    }
                },
                SectionsEditing::EditingPredicate { value } => match key.code {
                    KeyCode::Enter => {
                        let val = value.value().to_string();
                        let pred_idx = sec.pred_selected;
                        if sec.selected_section < self.config.sections.len() {
                            apply_predicate_edit(
                                &mut self.config.sections[sec.selected_section],
                                pred_idx,
                                &val,
                            );
                            self.save_sections_config().await;
                        }
                        sec.editing = None;
                        self.ui_state.modal = Modal::Settings(state);
                    }
                    KeyCode::Esc => {
                        sec.editing = None;
                        self.ui_state.modal = Modal::Settings(state);
                    }
                    _ => {
                        super::edit_text_input(value, key);
                        self.ui_state.modal = Modal::Settings(state);
                    }
                },
                SectionsEditing::CreatingSection { value } => match key.code {
                    KeyCode::Enter => {
                        let new_name = value.value().trim().to_string();
                        if claude_commander_core::session::section_name_available(
                            &new_name,
                            &self.config.sections,
                        ) {
                            self.config.sections.push(
                                claude_commander_core::session::SectionConfig {
                                    name: new_name,
                                    ..Default::default()
                                },
                            );
                            sec.selected_section = self.config.sections.len() - 1;
                            self.save_sections_config().await;
                        }
                        sec.editing = None;
                        self.ui_state.modal = Modal::Settings(state);
                    }
                    KeyCode::Esc => {
                        sec.editing = None;
                        self.ui_state.modal = Modal::Settings(state);
                    }
                    _ => {
                        super::edit_text_input(value, key);
                        self.ui_state.modal = Modal::Settings(state);
                    }
                },
            }
            return;
        }

        // --- Navigation mode ---
        let sections_len = self.config.sections.len();

        match sec.focus {
            SectionsFocus::List => {
                use claude_commander_core::config::keybindings::BindableAction;

                match self.config.keybindings.resolve(&key) {
                    Some(BindableAction::NavigateDown) => {
                        if sections_len > 0 {
                            sec.selected_section = (sec.selected_section + 1) % sections_len;
                            sec.pred_selected = 0;
                        }
                        self.ui_state.modal = Modal::Settings(state);
                    }
                    Some(BindableAction::NavigateUp) => {
                        if sections_len > 0 {
                            sec.selected_section = if sec.selected_section == 0 {
                                sections_len - 1
                            } else {
                                sec.selected_section - 1
                            };
                            sec.pred_selected = 0;
                        }
                        self.ui_state.modal = Modal::Settings(state);
                    }
                    Some(BindableAction::Quit) => {
                        self.ui_state.modal = Modal::None;
                    }
                    _ => match key.code {
                        KeyCode::Esc => {
                            self.ui_state.modal = Modal::None;
                        }
                        KeyCode::Tab => {
                            self.switch_settings_tab(&mut state, true);
                            self.ui_state.modal = Modal::Settings(state);
                        }
                        KeyCode::BackTab => {
                            self.switch_settings_tab(&mut state, false);
                            self.ui_state.modal = Modal::Settings(state);
                        }
                        KeyCode::Right | KeyCode::Enter => {
                            if sections_len > 0 {
                                sec.focus = SectionsFocus::Predicates;
                                sec.pred_selected = 0;
                            }
                            self.ui_state.modal = Modal::Settings(state);
                        }
                        KeyCode::Char('n') => {
                            sec.editing = Some(SectionsEditing::CreatingSection {
                                value: super::Input::default(),
                            });
                            self.ui_state.modal = Modal::Settings(state);
                        }
                        KeyCode::Char('r') => {
                            if sec.selected_section < sections_len {
                                let current =
                                    self.config.sections[sec.selected_section].name.clone();
                                sec.editing = Some(SectionsEditing::RenamingSection {
                                    value: current.into(),
                                });
                            }
                            self.ui_state.modal = Modal::Settings(state);
                        }
                        KeyCode::Char('d') => {
                            if sec.selected_section < sections_len {
                                self.config.sections.remove(sec.selected_section);
                                if sec.selected_section >= self.config.sections.len()
                                    && sec.selected_section > 0
                                {
                                    sec.selected_section -= 1;
                                }
                                self.save_sections_config().await;
                            }
                            self.ui_state.modal = Modal::Settings(state);
                        }
                        KeyCode::Char('J') => {
                            if sec.selected_section + 1 < sections_len {
                                self.config
                                    .sections
                                    .swap(sec.selected_section, sec.selected_section + 1);
                                sec.selected_section += 1;
                                self.save_sections_config().await;
                            }
                            self.ui_state.modal = Modal::Settings(state);
                        }
                        KeyCode::Char('K') => {
                            if sec.selected_section > 0 && sections_len > 0 {
                                self.config
                                    .sections
                                    .swap(sec.selected_section, sec.selected_section - 1);
                                sec.selected_section -= 1;
                                self.save_sections_config().await;
                            }
                            self.ui_state.modal = Modal::Settings(state);
                        }
                        _ => {
                            self.ui_state.modal = Modal::Settings(state);
                        }
                    },
                }
            }
            SectionsFocus::Predicates => {
                use claude_commander_core::config::keybindings::BindableAction;

                let pred_count = if sec.selected_section < sections_len {
                    predicate_rows(&self.config.sections[sec.selected_section]).len()
                } else {
                    0
                };

                match self.config.keybindings.resolve(&key) {
                    Some(BindableAction::NavigateDown) => {
                        if pred_count > 0 {
                            sec.pred_selected = (sec.pred_selected + 1) % pred_count;
                        }
                        self.ui_state.modal = Modal::Settings(state);
                    }
                    Some(BindableAction::NavigateUp) => {
                        if pred_count > 0 {
                            sec.pred_selected = if sec.pred_selected == 0 {
                                pred_count - 1
                            } else {
                                sec.pred_selected - 1
                            };
                        }
                        self.ui_state.modal = Modal::Settings(state);
                    }
                    Some(BindableAction::Quit) => {
                        self.ui_state.modal = Modal::None;
                    }
                    _ => match key.code {
                        KeyCode::Esc | KeyCode::Left => {
                            sec.focus = SectionsFocus::List;
                            self.ui_state.modal = Modal::Settings(state);
                        }
                        KeyCode::Tab => {
                            self.switch_settings_tab(&mut state, true);
                            self.ui_state.modal = Modal::Settings(state);
                        }
                        KeyCode::BackTab => {
                            self.switch_settings_tab(&mut state, false);
                            self.ui_state.modal = Modal::Settings(state);
                        }
                        KeyCode::Enter => {
                            if sec.selected_section < sections_len && pred_count > 0 {
                                let rows =
                                    predicate_rows(&self.config.sections[sec.selected_section]);
                                let (_, current_val) = &rows[sec.pred_selected];
                                let initial = if current_val == "(not set)" {
                                    String::new()
                                } else {
                                    current_val.clone()
                                };
                                sec.editing = Some(SectionsEditing::EditingPredicate {
                                    value: initial.into(),
                                });
                            }
                            self.ui_state.modal = Modal::Settings(state);
                        }
                        _ => {
                            self.ui_state.modal = Modal::Settings(state);
                        }
                    },
                }
            }
        }
    }

    /// Rename a section, keeping its sessions in it.
    ///
    /// The rename is a single service operation rather than a config edit
    /// followed by a save, because sessions store their section by name and
    /// both halves must move together (see
    /// [`CommanderService::rename_section`](claude_commander_core::api::CommanderService::rename_section)).
    /// The service is the authority on whether the new name is acceptable, so
    /// the local config mirror is re-read from it rather than assumed.
    async fn rename_section(&mut self, old: &str, new: &str) {
        match self.service.rename_section(old, new).await {
            Ok(true) => {
                self.config = self.service.read_config();
                // Collapse state is keyed by name too, so a collapsed section
                // would silently re-expand under its new name.
                if self.ui_state.collapsed_sections.remove(old) {
                    self.ui_state
                        .collapsed_sections
                        .insert(new.trim().to_string());
                }
                self.refresh_local_view().await;
                self.refresh_list_items().await;
            }
            Ok(false) => {}
            Err(e) => warn!("Failed to rename section: {}", e),
        }
    }

    /// Persist the current sections config to disk and reconcile session assignments.
    async fn save_sections_config(&mut self) {
        let updated = self.config.clone();
        if let Err(e) = self.service.update_config(updated) {
            warn!("Failed to save sections config: {}", e);
        }
        self.reconcile_section_assignments().await;
    }

    /// The backend whose programs the `EditServerPrograms` command targets:
    /// the selected session/project's backend, else local. (Sidebar server
    /// headings aren't selectable — a heading click passes its backend to
    /// `open_settings_on_programs` directly instead of routing through here.)
    pub(super) fn selected_backend_id(&self) -> claude_commander_core::backend::BackendId {
        if let Some(sref) = self.ui_state.selected_session_id {
            return sref.backend;
        }
        if let Some((backend, _)) = self.ui_state.selected_project_id {
            return backend;
        }
        claude_commander_core::backend::LOCAL_BACKEND_ID
    }

    /// Open the settings modal on the Programs tab, targeting `target`'s program
    /// list (loading it — synchronously for local, asynchronously for a remote).
    pub(super) fn open_settings_on_programs(
        &mut self,
        target: claude_commander_core::backend::BackendId,
    ) {
        // Cache mic devices so the STT Microphone row shows a friendly label if
        // the user tabs over to the Conversation tab.
        self.refresh_input_devices();
        let rows = self.build_settings_rows(SettingsTab::Programs);
        let selected_row = first_selectable_from(&rows, 0);
        let mut programs_state = ProgramsState {
            target,
            ..ProgramsState::default()
        };
        self.load_programs_target(&mut programs_state);
        self.ui_state.modal = Modal::Settings(SettingsState {
            tab: SettingsTab::Programs,
            selected_row,
            editing: None,
            rows,
            sections_state: SectionsState::default(),
            workspaces_state: WorkspacesState::default(),
            programs_state,
            theme_scope: self.default_theme_scope(),
            search: None,
        });
    }

    /// (Re)load the program list for the Programs tab's current target. Local is
    /// read synchronously from config; a remote server is fetched on a background
    /// task (bumping `load_gen` so a superseded response is dropped), delivering
    /// [`StateUpdate::ServerProgramsLoaded`].
    fn load_programs_target(&self, prog: &mut ProgramsState) {
        prog.editing = None;
        prog.focus = ProgramsFocus::List;
        prog.field_selected = 0;
        prog.selected = 0;
        prog.save_error = None;
        if prog.target == claude_commander_core::backend::LOCAL_BACKEND_ID {
            prog.entries = self.config.programs.clone();
            prog.loading = false;
            prog.load_error = None;
            return;
        }
        // Unknown/removed target: surface it rather than showing the local list
        // (which `backend_arc` would fall back to).
        let Some(backend) = self.backend(prog.target).map(|h| h.backend.clone()) else {
            prog.loading = false;
            prog.entries.clear();
            prog.load_error = Some("server no longer configured".to_string());
            return;
        };
        prog.load_gen = prog.load_gen.wrapping_add(1);
        prog.loading = true;
        prog.load_error = None;
        prog.entries.clear();
        let target = prog.target;
        let generation = prog.load_gen;
        let tx = self.event_loop.sender();
        tokio::spawn(async move {
            let result = backend
                .create_options()
                .await
                .map(|opts| opts.programs.into_iter().map(Into::into).collect())
                .map_err(|e| e.to_string());
            let _ = tx
                .send(crate::event::AppEvent::StateUpdate(
                    crate::event::StateUpdate::ServerProgramsLoaded {
                        backend: target,
                        generation,
                        result,
                    },
                ))
                .await;
        });
    }

    /// Push the working copy to the current target after a committed edit. Local
    /// writes the config and persists (keeping `App.config` authoritative); a
    /// remote target is queued for an ordered background PUT (see
    /// [`ProgramCommitQueue`]). If the target backend no longer exists (its
    /// server was removed while the tab was open), the edit is dropped with an
    /// in-tab error rather than silently rewriting the *local* config — the
    /// `backend_arc` fallback would otherwise route it there.
    fn commit_programs(&mut self, prog: &mut ProgramsState) {
        // A fresh edit supersedes any earlier save failure note.
        prog.save_error = None;
        if prog.target == claude_commander_core::backend::LOCAL_BACKEND_ID {
            self.config.programs = prog.entries.clone();
            self.persist_config();
            return;
        }
        let Some(backend) = self.backend(prog.target).map(|h| h.backend.clone()) else {
            prog.load_error = Some("server no longer configured".to_string());
            return;
        };
        let programs: Vec<claude_commander_core::api::ProgramInfo> =
            prog.entries.iter().map(Into::into).collect();
        let tx = self.event_loop.sender();
        self.program_commit
            .submit(prog.target, backend, tx, programs);
    }

    /// Cycle the Programs tab to the next configured backend (local → each remote
    /// server → local) and load its program list. No-op with a single backend.
    fn cycle_programs_target(&self, prog: &mut ProgramsState) {
        let ids: Vec<claude_commander_core::backend::BackendId> =
            self.backends.iter().map(|h| h.id).collect();
        if ids.len() <= 1 {
            return;
        }
        let cur = ids.iter().position(|id| *id == prog.target).unwrap_or(0);
        prog.target = ids[(cur + 1) % ids.len()];
        self.load_programs_target(prog);
    }

    /// Handle a keypress while the Programs tab is active. Edits mutate the
    /// working copy (`entries`) and are committed per edit to the current target.
    fn handle_programs_key(&mut self, key: crossterm::event::KeyEvent, mut state: SettingsState) {
        use claude_commander_core::config::keybindings::BindableAction;
        use crossterm::event::KeyCode;

        let prog = &mut state.programs_state;
        // Whether a committed edit needs pushing to the target at the end.
        let mut changed = false;
        // Whether to close the settings modal (Esc/quit) instead of keeping it.
        let mut close = false;
        // Deferred tab switch: `switch_settings_tab` needs `&mut state`, which
        // can't coexist with the `prog` borrow, so record the direction and apply
        // it after `prog` is released.
        let mut switch_tab: Option<bool> = None;

        // --- Editing mode ---
        if let Some(ref mut editing) = prog.editing {
            match editing {
                ProgramsEditing::RenamingLabel { value } => match key.code {
                    KeyCode::Enter => {
                        let new_label = value.value().trim().to_string();
                        if !new_label.is_empty() && prog.selected < prog.entries.len() {
                            let has_dup = prog
                                .entries
                                .iter()
                                .enumerate()
                                .any(|(i, p)| i != prog.selected && p.label == new_label);
                            if !has_dup {
                                prog.entries[prog.selected].label = new_label;
                                changed = true;
                            }
                        }
                        prog.editing = None;
                    }
                    KeyCode::Esc => prog.editing = None,
                    _ => {
                        super::edit_text_input(value, key);
                    }
                },
                ProgramsEditing::EditingField { value } => match key.code {
                    KeyCode::Enter => {
                        let val = value.value().trim().to_string();
                        if prog.selected < prog.entries.len() {
                            if prog.field_selected == 0 {
                                // Label: reject empty and duplicates.
                                let has_dup = prog
                                    .entries
                                    .iter()
                                    .enumerate()
                                    .any(|(i, p)| i != prog.selected && p.label == val);
                                if !val.is_empty() && !has_dup {
                                    prog.entries[prog.selected].label = val;
                                    changed = true;
                                }
                            } else if !val.is_empty() {
                                // Command: reject empty.
                                prog.entries[prog.selected].command = val;
                                changed = true;
                            }
                        }
                        prog.editing = None;
                    }
                    KeyCode::Esc => prog.editing = None,
                    _ => {
                        super::edit_text_input(value, key);
                    }
                },
                ProgramsEditing::CreatingLabel { value } => match key.code {
                    KeyCode::Enter => {
                        // The entry already exists (added on `n`); apply the typed
                        // label to it, keeping the auto-generated default when the
                        // input is empty or clashes with another entry.
                        if prog.selected < prog.entries.len() {
                            let label = value.value().trim().to_string();
                            let has_dup = prog
                                .entries
                                .iter()
                                .enumerate()
                                .any(|(i, p)| i != prog.selected && p.label == label);
                            if !label.is_empty() && !has_dup {
                                prog.entries[prog.selected].label = label;
                                changed = true;
                            }
                            // Advance to the command step, seeded from the label
                            // (the common case is command == label).
                            let seed = prog.entries[prog.selected].label.clone();
                            prog.editing =
                                Some(ProgramsEditing::CreatingCommand { value: seed.into() });
                        } else {
                            prog.editing = None;
                        }
                    }
                    // Backing out keeps the added entry (delete with `d`).
                    KeyCode::Esc => prog.editing = None,
                    _ => {
                        super::edit_text_input(value, key);
                    }
                },
                ProgramsEditing::CreatingCommand { value } => match key.code {
                    KeyCode::Enter => {
                        let command = value.value().trim().to_string();
                        if !command.is_empty() && prog.selected < prog.entries.len() {
                            prog.entries[prog.selected].command = command;
                            changed = true;
                        }
                        prog.editing = None;
                    }
                    // Backing out keeps the added entry (delete with `d`).
                    KeyCode::Esc => prog.editing = None,
                    _ => {
                        super::edit_text_input(value, key);
                    }
                },
            }
            if changed {
                self.commit_programs(prog);
            }
            self.ui_state.modal = Modal::Settings(state);
            return;
        }

        // --- Navigation mode ---
        // A remote target that is still loading (or failed to load) can be
        // navigated/retargeted/closed, but not edited.
        let editable = !prog.loading && prog.load_error.is_none();
        let programs_len = prog.entries.len();
        let resolved = self.config.keybindings.resolve(&key);

        match prog.focus {
            ProgramsFocus::List => match resolved {
                Some(BindableAction::NavigateDown) => {
                    if programs_len > 0 {
                        prog.selected = (prog.selected + 1) % programs_len;
                        prog.field_selected = 0;
                    }
                }
                Some(BindableAction::NavigateUp) => {
                    if programs_len > 0 {
                        prog.selected = if prog.selected == 0 {
                            programs_len - 1
                        } else {
                            prog.selected - 1
                        };
                        prog.field_selected = 0;
                    }
                }
                Some(BindableAction::Quit) => close = true,
                _ => match key.code {
                    KeyCode::Esc => close = true,
                    KeyCode::Tab => switch_tab = Some(true),
                    KeyCode::BackTab => switch_tab = Some(false),
                    // `t` cycles which backend's program list is being edited.
                    KeyCode::Char('t') => self.cycle_programs_target(prog),
                    KeyCode::Right | KeyCode::Enter if programs_len > 0 => {
                        prog.focus = ProgramsFocus::Fields;
                        prog.field_selected = 0;
                    }
                    KeyCode::Char('n') if editable => {
                        // Add the entry immediately so it shows in the list while
                        // being named; backing out leaves it in place (delete with
                        // `d`). Defaults are a unique label and a runnable `claude`
                        // command, both editable in the guided label → command
                        // steps that follow. `changed` commits it to the target.
                        prog.entries
                            .push(claude_commander_core::config::ProgramEntry {
                                label: unique_program_label(&prog.entries),
                                command: "claude".to_string(),
                            });
                        prog.selected = prog.entries.len() - 1;
                        changed = true;
                        prog.editing = Some(ProgramsEditing::CreatingLabel {
                            value: super::Input::default(),
                        });
                    }
                    KeyCode::Char('r') if editable && prog.selected < programs_len => {
                        let current = prog.entries[prog.selected].label.clone();
                        prog.editing = Some(ProgramsEditing::RenamingLabel {
                            value: current.into(),
                        });
                    }
                    KeyCode::Char('d') if editable && prog.selected < programs_len => {
                        prog.entries.remove(prog.selected);
                        if prog.selected >= prog.entries.len() && prog.selected > 0 {
                            prog.selected -= 1;
                        }
                        changed = true;
                    }
                    KeyCode::Char('J') if editable && prog.selected + 1 < programs_len => {
                        prog.entries.swap(prog.selected, prog.selected + 1);
                        prog.selected += 1;
                        changed = true;
                    }
                    KeyCode::Char('K') if editable && prog.selected > 0 && programs_len > 0 => {
                        prog.entries.swap(prog.selected, prog.selected - 1);
                        prog.selected -= 1;
                        changed = true;
                    }
                    _ => {}
                },
            },
            ProgramsFocus::Fields => match resolved {
                Some(BindableAction::NavigateDown | BindableAction::NavigateUp) => {
                    // Only two fields (label / command): toggle between them.
                    prog.field_selected = 1 - prog.field_selected;
                }
                Some(BindableAction::Quit) => close = true,
                _ => match key.code {
                    KeyCode::Esc | KeyCode::Left => prog.focus = ProgramsFocus::List,
                    KeyCode::Tab => switch_tab = Some(true),
                    KeyCode::BackTab => switch_tab = Some(false),
                    KeyCode::Enter if editable && prog.selected < programs_len => {
                        let entry = &prog.entries[prog.selected];
                        let current = if prog.field_selected == 0 {
                            entry.label.clone()
                        } else {
                            entry.command.clone()
                        };
                        prog.editing = Some(ProgramsEditing::EditingField {
                            value: current.into(),
                        });
                    }
                    _ => {}
                },
            },
        }

        if changed {
            self.commit_programs(prog);
        }
        // `prog` is released past this point, so `state` can be borrowed whole.
        if let Some(forward) = switch_tab {
            self.switch_settings_tab(&mut state, forward);
        }
        if close {
            self.ui_state.modal = Modal::None;
        } else {
            self.ui_state.modal = Modal::Settings(state);
        }
    }

    fn render_programs_tab(
        &self,
        frame: &mut Frame,
        body_area: Rect,
        footer_area: Rect,
        prog: &ProgramsState,
    ) {
        // When more than one backend is configured, the tab edits a *chosen*
        // backend's list — show which, and how to switch. A lone-local setup
        // keeps the original single-pane layout with no header noise.
        let body_area = if self.backends.len() > 1 {
            let name = self
                .backends
                .iter()
                .find(|h| h.id == prog.target)
                .map(|h| h.backend.descriptor().name)
                .unwrap_or_else(|| "local".to_string());
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("Target: ", Style::default().fg(self.theme.text_secondary)),
                    Span::styled(
                        name,
                        Style::default()
                            .fg(self.theme.text_primary)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        "  (t to switch)",
                        Style::default().fg(self.theme.text_secondary),
                    ),
                ])),
                Rect {
                    height: 1,
                    ..body_area
                },
            );
            // Leave a blank spacer row below the header.
            Rect {
                y: body_area.y + 2,
                height: body_area.height.saturating_sub(2),
                ..body_area
            }
        } else {
            body_area
        };

        // A remote target that is still loading (or failed) has no list to edit;
        // keep a footer so `t` (switch target) / Esc remain discoverable.
        let dead_footer = |this: &Self| {
            let hint = if this.backends.len() > 1 {
                "t: switch target  Tab: switch tab  Esc: close"
            } else {
                "Tab: switch tab  Esc: close"
            };
            (hint, footer_area)
        };
        if prog.loading {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    "Loading…",
                    Style::default().fg(self.theme.text_secondary),
                )),
                Rect {
                    height: 1,
                    ..body_area
                },
            );
            let (hint, area) = dead_footer(self);
            frame.render_widget(
                Paragraph::new(Span::styled(
                    hint,
                    Style::default().fg(self.theme.text_secondary),
                )),
                area,
            );
            return;
        }
        if let Some(err) = &prog.load_error {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    format!("Unavailable: {err}"),
                    Style::default().fg(self.theme.modal_error),
                ))
                .wrap(ratatui::widgets::Wrap { trim: true }),
                body_area,
            );
            let (hint, area) = dead_footer(self);
            frame.render_widget(
                Paragraph::new(Span::styled(
                    hint,
                    Style::default().fg(self.theme.text_secondary),
                )),
                area,
            );
            return;
        }

        let programs = &prog.entries;
        let list_width = body_area.width.clamp(16, 28);
        let divider_width = 1_u16;
        let detail_width = body_area
            .width
            .saturating_sub(list_width + divider_width + 1);

        let list_area = Rect {
            width: list_width,
            ..body_area
        };
        let divider_area = Rect {
            x: body_area.x + list_width,
            width: divider_width,
            ..body_area
        };
        let detail_area = Rect {
            x: body_area.x + list_width + divider_width + 1,
            width: detail_width,
            ..body_area
        };

        // --- Divider ---
        self.render_settings_divider(frame, divider_area);

        // --- Program list ---
        // Both list-focus label editors — renaming an existing entry (`r`) and
        // naming a just-added one (`n` → CreatingLabel) — render the selected row
        // as a live caret input over the (already-present) entry.
        {
            let visible = list_area.height as usize;
            let scroll = list_scroll_offset(prog.selected, visible);
            for (i, entry) in programs.iter().enumerate().skip(scroll).take(visible) {
                let y = list_area.y + (i - scroll) as u16;
                let is_selected = i == prog.selected;
                let is_focused = prog.focus == ProgramsFocus::List;

                let style = if is_selected && is_focused {
                    self.theme.selection()
                } else if is_selected {
                    Style::default()
                        .fg(self.theme.text_primary)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(self.theme.text_secondary)
                };

                let prefix = if is_selected { "▸ " } else { "  " };
                let is_renaming = is_selected
                    && matches!(
                        prog.editing,
                        Some(
                            ProgramsEditing::RenamingLabel { .. }
                                | ProgramsEditing::CreatingLabel { .. }
                        )
                    );
                let label = if is_renaming {
                    if let Some(
                        ProgramsEditing::RenamingLabel { value }
                        | ProgramsEditing::CreatingLabel { value },
                    ) = &prog.editing
                    {
                        format!("{prefix}{}", super::input_with_caret(value))
                    } else {
                        String::new()
                    }
                } else {
                    // Reserve room for the ` (default)` suffix on the first entry.
                    let suffix_len = if i == 0 { " (default)".len() } else { 0 };
                    let avail = (list_width as usize).saturating_sub(2 + suffix_len);
                    let l = truncate_str(&entry.label, avail);
                    format!("{prefix}{l}")
                };

                let row_style = if is_renaming {
                    style.add_modifier(Modifier::UNDERLINED)
                } else {
                    style
                };

                frame.render_widget(
                    Paragraph::new(Span::styled(label.clone(), row_style)),
                    Rect {
                        y,
                        height: 1,
                        ..list_area
                    },
                );

                // Dim ` (default)` suffix on the first entry (not while renaming).
                if i == 0 && !is_renaming {
                    let used = label.chars().count() as u16;
                    if used < list_width {
                        frame.render_widget(
                            Paragraph::new(Span::styled(
                                " (default)",
                                Style::default().fg(self.theme.text_secondary),
                            )),
                            Rect {
                                x: list_area.x + used,
                                y,
                                width: list_width - used,
                                height: 1,
                            },
                        );
                    }
                }
            }

            if programs.is_empty() {
                frame.render_widget(
                    Paragraph::new(Span::styled(
                        "  (none — built-in claude)",
                        Style::default().fg(self.theme.text_secondary),
                    )),
                    Rect {
                        height: 1,
                        ..list_area
                    },
                );
            }
        }

        // --- Detail pane (right side) ---
        // The selected entry is always a real list element (creation adds it up
        // front), so the detail pane just reflects it; the command field renders
        // as a caret input while it is being edited (see `editing_this` below).
        let detail_rows: Vec<(&str, String)> =
            if !programs.is_empty() && prog.selected < programs.len() {
                let entry = &programs[prog.selected];
                vec![
                    ("label", entry.label.clone()),
                    ("command", entry.command.clone()),
                ]
            } else {
                Vec::new()
            };

        let is_fields_focused = prog.focus == ProgramsFocus::Fields;
        for (i, (label, value)) in detail_rows.iter().enumerate() {
            if i as u16 >= detail_area.height {
                break;
            }
            let y = detail_area.y + i as u16;
            let is_selected = is_fields_focused && i == prog.field_selected;

            let style = if is_selected {
                self.theme.selection()
            } else {
                Style::default()
            };

            let label_w = 10_u16.min(detail_area.width);
            frame.render_widget(
                Paragraph::new(Span::styled(
                    format!("{:<w$}", label, w = label_w as usize),
                    style,
                )),
                Rect {
                    x: detail_area.x,
                    y,
                    width: label_w,
                    height: 1,
                },
            );

            if detail_area.width > label_w + 1 {
                let val_x = detail_area.x + label_w + 1;
                let val_w = detail_area.width.saturating_sub(label_w + 1);

                // Which field, if any, is currently in an inline edit.
                let editing_this = match &prog.editing {
                    Some(ProgramsEditing::EditingField { value: v }) if is_selected => Some(v),
                    Some(ProgramsEditing::CreatingCommand { value: v }) if i == 1 => Some(v),
                    _ => None,
                };

                let display_val = if let Some(v) = editing_this {
                    super::input_with_caret(v)
                } else {
                    value.clone()
                };

                let val_style = if editing_this.is_some() {
                    style.add_modifier(Modifier::UNDERLINED)
                } else if is_selected {
                    style
                } else {
                    style.fg(self.theme.text_accent)
                };

                frame.render_widget(
                    Paragraph::new(Span::styled(display_val, val_style)),
                    Rect {
                        x: val_x,
                        y,
                        width: val_w,
                        height: 1,
                    },
                );
            }
        }

        // Dim hint below the fields.
        let hint_y = detail_area.y + detail_rows.len() as u16 + 1;
        if hint_y < detail_area.y + detail_area.height {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    "First entry is the default for new sessions.",
                    Style::default().fg(self.theme.text_secondary),
                )),
                Rect {
                    x: detail_area.x,
                    y: hint_y,
                    width: detail_area.width,
                    height: 1,
                },
            );
        }

        // --- Footer ---
        // A background save failure takes over the footer as a non-blocking
        // warning (the list stays editable).
        if let Some(err) = &prog.save_error {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    err.clone(),
                    Style::default().fg(self.theme.modal_error),
                )),
                footer_area,
            );
            return;
        }
        let switch = if self.backends.len() > 1 {
            "  t: switch target"
        } else {
            ""
        };
        let footer_text = match &prog.editing {
            Some(ProgramsEditing::RenamingLabel { .. } | ProgramsEditing::EditingField { .. }) => {
                "Enter: save  Esc: cancel".to_string()
            }
            Some(ProgramsEditing::CreatingLabel { .. }) => {
                "Enter: next (command)  Esc: keep (delete with d)".to_string()
            }
            Some(ProgramsEditing::CreatingCommand { .. }) => {
                "Enter: save  Esc: keep (delete with d)".to_string()
            }
            None if prog.focus == ProgramsFocus::List => {
                format!(
                    "n: new  r: rename  d: delete  J/K: reorder  →/Enter: fields{switch}  Tab: tab"
                )
            }
            None => "Enter: edit  ←: back to list  Tab: switch tab".to_string(),
        };
        frame.render_widget(
            Paragraph::new(Span::styled(
                footer_text,
                Style::default().fg(self.theme.text_secondary),
            )),
            footer_area,
        );
    }
}

/// A placeholder label for a freshly-added program that does not clash with any
/// existing entry (`"New program"`, then `"New program 2"`, …). New entries are
/// added to the working copy up front, so they need a unique default before the
/// user renames them.
fn unique_program_label(programs: &[claude_commander_core::config::ProgramEntry]) -> String {
    const BASE: &str = "New program";
    let taken = |label: &str| programs.iter().any(|p| p.label == label);
    if !taken(BASE) {
        return BASE.to_string();
    }
    (2..)
        .map(|n| format!("{BASE} {n}"))
        .find(|candidate| !taken(candidate))
        .expect("an unused label exists in an unbounded sequence")
}

/// Build displayable rows for a section's predicates.
fn predicate_rows(
    section: &claude_commander_core::session::SectionConfig,
) -> Vec<(String, String)> {
    use claude_commander_core::session::section::{
        DecisionPredicate, LabelPredicate, ReviewerPredicate, StatePredicate,
    };

    let fmt_state = |p: &StatePredicate| match p {
        claude_commander_core::session::section::OneOrMany::One(v) => {
            format!("{v:?}").to_lowercase()
        }
        claude_commander_core::session::section::OneOrMany::Any(vs) => vs
            .iter()
            .map(|v| format!("{v:?}").to_lowercase())
            .collect::<Vec<_>>()
            .join(", "),
    };

    let fmt_decision = |p: &DecisionPredicate| match p {
        claude_commander_core::session::section::OneOrMany::One(v) => {
            format!("{v:?}").to_lowercase()
        }
        claude_commander_core::session::section::OneOrMany::Any(vs) => vs
            .iter()
            .map(|v| format!("{v:?}").to_lowercase())
            .collect::<Vec<_>>()
            .join(", "),
    };

    let fmt_label = |p: &LabelPredicate| match p {
        LabelPredicate::One(s) => s.clone(),
        LabelPredicate::Any(vs) => vs.join(", "),
    };

    let fmt_reviewer = |p: &ReviewerPredicate| match p {
        ReviewerPredicate::Bool(b) => b.to_string(),
        ReviewerPredicate::One(s) => s.clone(),
        ReviewerPredicate::Any(vs) => vs.join(", "),
    };

    let not_set = "(not set)".to_string();
    vec![
        (
            "pr_state".into(),
            section
                .pr_state
                .as_ref()
                .map_or_else(|| not_set.clone(), fmt_state),
        ),
        (
            "is_draft".into(),
            section
                .is_draft
                .map_or_else(|| not_set.clone(), |b| b.to_string()),
        ),
        (
            "has_label".into(),
            section
                .has_label
                .as_ref()
                .map_or_else(|| not_set.clone(), fmt_label),
        ),
        (
            "has_pr".into(),
            section
                .has_pr
                .map_or_else(|| not_set.clone(), |b| b.to_string()),
        ),
        (
            "review_decision".into(),
            section
                .review_decision
                .as_ref()
                .map_or_else(|| not_set.clone(), fmt_decision),
        ),
        (
            "has_reviewer".into(),
            section
                .has_reviewer
                .as_ref()
                .map_or_else(|| not_set.clone(), fmt_reviewer),
        ),
        (
            "max_sessions".into(),
            section
                .max_sessions
                .map_or_else(|| not_set.clone(), |n| n.to_string()),
        ),
    ]
}

/// Apply a user-edited predicate value string to a SectionConfig field.
fn apply_predicate_edit(
    section: &mut claude_commander_core::session::SectionConfig,
    pred_idx: usize,
    value: &str,
) {
    use claude_commander_core::git::{PrState, ReviewDecision};
    use claude_commander_core::session::section::{LabelPredicate, OneOrMany, ReviewerPredicate};

    let trimmed = value.trim();

    match pred_idx {
        // pr_state
        0 => {
            if trimmed.is_empty() {
                section.pr_state = None;
            } else {
                let parts: Vec<&str> = trimmed.split(',').map(str::trim).collect();
                let parsed: Vec<PrState> = parts.iter().filter_map(|s| parse_pr_state(s)).collect();
                section.pr_state = match parsed.len() {
                    0 => None,
                    1 => Some(OneOrMany::One(parsed[0])),
                    _ => Some(OneOrMany::Any(parsed)),
                };
            }
        }
        // is_draft
        1 => {
            section.is_draft = match trimmed {
                "true" => Some(true),
                "false" => Some(false),
                _ => None,
            };
        }
        // has_label
        2 => {
            if trimmed.is_empty() {
                section.has_label = None;
            } else {
                let labels: Vec<String> = trimmed
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                section.has_label = match labels.len() {
                    0 => None,
                    1 => Some(LabelPredicate::One(labels.into_iter().next().unwrap())),
                    _ => Some(LabelPredicate::Any(labels)),
                };
            }
        }
        // has_pr
        3 => {
            section.has_pr = match trimmed {
                "true" => Some(true),
                "false" => Some(false),
                _ => None,
            };
        }
        // review_decision
        4 => {
            if trimmed.is_empty() {
                section.review_decision = None;
            } else {
                let parts: Vec<&str> = trimmed.split(',').map(str::trim).collect();
                let parsed: Vec<ReviewDecision> = parts
                    .iter()
                    .filter_map(|s| parse_review_decision(s))
                    .collect();
                section.review_decision = match parsed.len() {
                    0 => None,
                    1 => Some(OneOrMany::One(parsed[0])),
                    _ => Some(OneOrMany::Any(parsed)),
                };
            }
        }
        // has_reviewer
        5 => {
            if trimmed.is_empty() {
                section.has_reviewer = None;
            } else {
                match trimmed {
                    "true" => section.has_reviewer = Some(ReviewerPredicate::Bool(true)),
                    "false" => section.has_reviewer = Some(ReviewerPredicate::Bool(false)),
                    _ => {
                        let logins: Vec<String> = trimmed
                            .split(',')
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect();
                        section.has_reviewer = match logins.len() {
                            0 => None,
                            1 => Some(ReviewerPredicate::One(logins.into_iter().next().unwrap())),
                            _ => Some(ReviewerPredicate::Any(logins)),
                        };
                    }
                }
            }
        }
        // max_sessions
        6 => {
            section.max_sessions = if trimmed.is_empty() {
                None
            } else {
                trimmed.parse::<u32>().ok().filter(|&n| n > 0)
            };
        }
        _ => {}
    }
}

fn parse_pr_state(s: &str) -> Option<claude_commander_core::git::PrState> {
    match s.to_lowercase().as_str() {
        "open" => Some(claude_commander_core::git::PrState::Open),
        "merged" => Some(claude_commander_core::git::PrState::Merged),
        "closed" => Some(claude_commander_core::git::PrState::Closed),
        _ => None,
    }
}

fn parse_review_decision(s: &str) -> Option<claude_commander_core::git::ReviewDecision> {
    match s.to_lowercase().replace(['-', '_'], "").as_str() {
        "approved" => Some(claude_commander_core::git::ReviewDecision::Approved),
        "changesrequested" => Some(claude_commander_core::git::ReviewDecision::ChangesRequested),
        "reviewrequired" => Some(claude_commander_core::git::ReviewDecision::ReviewRequired),
        _ => None,
    }
}

/// Width of the label column for the settings rows.
///
/// Sized to fit the longest label (plus a trailing space) so descriptions are
/// not clipped — the Keybindings tab has labels well over 24 columns — while
/// keeping at least a usable minimum value column on narrow terminals and a
/// sensible floor so short-labelled tabs still align tidily.
fn settings_label_width(rows: &[SettingsRow], area_width: u16) -> u16 {
    const MIN_LABEL: u16 = 24;
    const MIN_VALUE: u16 = 16;
    // 2-column gap between the label and value columns (see `val_x`).
    const GAP: u16 = 2;

    let longest = rows
        .iter()
        // Section headers span the full width, so they don't drive the label
        // column; only value-bearing rows do.
        .filter(|r| r.is_selectable())
        .map(|r| r.label.chars().count())
        .max()
        .unwrap_or(0) as u16;
    let desired = longest.saturating_add(1).max(MIN_LABEL);

    // Cap so the value column keeps at least MIN_VALUE columns, but never force
    // the label below its floor (on a very narrow terminal labels truncate).
    let cap = area_width.saturating_sub(MIN_VALUE + GAP).max(MIN_LABEL);
    desired.min(cap)
}

/// Where the settings modal draws within a frame `area`. Shared by the
/// renderer and by state that must agree with what it draws (the colour
/// picker's grid width).
pub(super) struct SettingsAreas {
    pub modal: Rect,
    /// Inside the border and a one-column side margin.
    pub content: Rect,
    /// Between the tab bar's separator and the footer.
    pub body: Rect,
    pub footer: Rect,
}

pub(super) fn settings_areas(area: Rect) -> SettingsAreas {
    let modal = modals::centered_rect(75, 85, area);
    let content = Block::default()
        .borders(Borders::ALL)
        .inner(modal)
        .inner(Margin {
            horizontal: 1,
            vertical: 0,
        });
    SettingsAreas {
        modal,
        content,
        body: Rect {
            y: content.y + 2,
            height: content.height.saturating_sub(4),
            ..content
        },
        footer: Rect {
            y: content.y + content.height.saturating_sub(1),
            height: 1,
            ..content
        },
    }
}

/// Top scroll offset that keeps `selected` visible in a window `visible` rows
/// tall: scroll only once the selection passes the bottom edge.
pub(super) fn list_scroll_offset(selected: usize, visible: usize) -> usize {
    if selected >= visible {
        selected - visible + 1
    } else {
        0
    }
}

/// Index of the first selectable row at or after `from`, wrapping to the start
/// if none is found below. Returns `from` when there are no selectable rows.
pub(super) fn first_selectable_from(rows: &[SettingsRow], from: usize) -> usize {
    rows.iter()
        .enumerate()
        .skip(from)
        .find(|(_, r)| r.is_selectable())
        .or_else(|| rows.iter().enumerate().find(|(_, r)| r.is_selectable()))
        .map(|(i, _)| i)
        .unwrap_or(from)
}

/// Next selectable row from `current` in the given direction, wrapping around
/// and skipping non-selectable header rows. Returns `current` when there is no
/// other selectable row.
fn step_selectable(rows: &[SettingsRow], current: usize, forward: bool) -> usize {
    let n = rows.len();
    if n == 0 {
        return current;
    }
    let mut idx = current;
    for _ in 0..n {
        idx = if forward {
            (idx + 1) % n
        } else {
            (idx + n - 1) % n
        };
        if rows[idx].is_selectable() {
            return idx;
        }
    }
    current
}

/// A blank spacer row (an empty-label header) drawn between sections.
fn is_spacer(row: &SettingsRow) -> bool {
    matches!(row.kind, SettingsRowKind::Header) && row.label.is_empty()
}

/// Insert a blank spacer row before every section header except the first, so
/// the grouped list breathes. Expects a spacer-free list of headers + rows.
fn with_section_spacers(rows: Vec<SettingsRow>) -> Vec<SettingsRow> {
    let mut out: Vec<SettingsRow> = Vec::with_capacity(rows.len() + 8);
    for row in rows {
        if matches!(row.kind, SettingsRowKind::Header) && !out.is_empty() {
            out.push(SettingsRow::header(""));
        }
        out.push(row);
    }
    out
}

/// Filter grouped keybinding rows to those matching `query` (case-insensitive
/// fuzzy match against the description and the bound keys). Section headers are
/// kept only when at least one of their actions survives the filter, and blank
/// spacers are re-inserted between the surviving sections. An empty query
/// returns every binding (still grouped and spaced).
pub(super) fn filter_keybinding_rows(rows: Vec<SettingsRow>, query: &str) -> Vec<SettingsRow> {
    let query = query.trim();
    // Work on the spacer-free list so the grouping logic stays simple; spacers
    // are re-added at the end based on the surviving headers.
    let rows: Vec<SettingsRow> = rows.into_iter().filter(|r| !is_spacer(r)).collect();
    if query.is_empty() {
        return with_section_spacers(rows);
    }
    let matches = |row: &SettingsRow| {
        claude_commander_viewmodel::fuzzy_score(&row.label, query).is_some()
            || claude_commander_viewmodel::fuzzy_score(row.text_value(), query).is_some()
    };

    let mut out: Vec<SettingsRow> = Vec::new();
    for row in rows {
        if row.is_selectable() {
            if matches(&row) {
                out.push(row);
            }
        } else {
            // Drop the previous header if it ended up with no matching children.
            if out.last().is_some_and(|r| !r.is_selectable()) {
                out.pop();
            }
            out.push(row);
        }
    }
    // A trailing header with no children (last section didn't match) is dropped.
    if out.last().is_some_and(|r| !r.is_selectable()) {
        out.pop();
    }
    with_section_spacers(out)
}

pub(super) fn truncate_str(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else if max > 1 {
        let kept: String = s.chars().take(max - 1).collect();
        format!("{kept}…")
    } else {
        "…".to_string()
    }
}

/// Where `scope`'s rows inherit from when they have no value of their own:
/// a workspace without a preset of its own layers over the usual theme; the
/// usual theme, and a workspace with its own preset, over that preset.
fn inherited_from(scope: &ThemeScope, overrides: &ThemeOverrides) -> &'static str {
    match scope {
        ThemeScope::Workspace(_) if !crate::theme::entry_sets_preset(overrides) => "usual",
        _ => "preset",
    }
}

/// How the scope row reads.
fn theme_scope_label(
    scope: &ThemeScope,
    merged: &[claude_commander_viewmodel::workspace::MergedWorkspace],
) -> String {
    match scope {
        ThemeScope::Usual => "Usual theme (all workspaces)".to_string(),
        ThemeScope::Workspace(name) => merged
            .iter()
            .find(|w| &w.name == name)
            .map_or_else(|| name.clone().unwrap_or_default(), |w| w.label.clone()),
    }
}

/// A scope as a picker value. The `@` spellings cannot collide with the
/// `ws:`-prefixed names.
fn theme_scope_value(scope: &ThemeScope) -> String {
    match scope {
        ThemeScope::Usual => "@usual".to_string(),
        ThemeScope::Workspace(None) => "@main".to_string(),
        ThemeScope::Workspace(Some(name)) => format!("ws:{name}"),
    }
}

/// The inverse of [`theme_scope_value`].
fn parse_theme_scope(value: &str) -> Option<ThemeScope> {
    match value {
        "@usual" => Some(ThemeScope::Usual),
        "@main" => Some(ThemeScope::Workspace(None)),
        other => other
            .strip_prefix("ws:")
            .map(|name| ThemeScope::Workspace(Some(name.to_string()))),
    }
}

/// The colour field of `overrides` a Theme-tab row edits, by its field key.
/// `None` for a key that is not a colour row (`preset`, `appearance`, …).
fn theme_colour_slot<'a>(
    overrides: &'a mut ThemeOverrides,
    field_key: &str,
) -> Option<&'a mut Option<claude_commander_core::config::theme::ColorValue>> {
    macro_rules! slot {
        ($($name:ident),* $(,)?) => {
            match field_key {
                $(stringify!($name) => Some(&mut overrides.$name),)*
                _ => None,
            }
        };
    }
    slot!(
        border_focused,
        border_unfocused,
        selection_bg,
        selection_fg,
        status_creating,
        status_running,
        status_stopped,
        status_pr,
        status_pr_merged,
        pr_open,
        pr_draft,
        pr_closed,
        text_primary,
        text_secondary,
        text_accent,
        diff_added,
        diff_removed,
        diff_hunk_header,
        diff_file_header,
        diff_context,
        modal_info,
        modal_warning,
        modal_error,
        status_bar_bg,
        status_bar_fg,
        status_bar_accent,
    )
}

/// How wide the colour picker's pane is when the settings modal is drawn in a
/// frame `area`: the whole settings body.
pub(super) fn colour_picker_width(area: Rect) -> u16 {
    settings_areas(area).body.width
}

/// Format a ratatui Color for display in the settings modal.
fn format_color(color: ratatui::style::Color) -> String {
    use ratatui::style::Color;
    match color {
        Color::Reset => "reset".into(),
        Color::Black => "black".into(),
        Color::Red => "red".into(),
        Color::Green => "green".into(),
        Color::Yellow => "yellow".into(),
        Color::Blue => "blue".into(),
        Color::Magenta => "magenta".into(),
        Color::Cyan => "cyan".into(),
        Color::Gray => "gray".into(),
        Color::DarkGray => "dark_gray".into(),
        Color::LightRed => "light_red".into(),
        Color::LightGreen => "light_green".into(),
        Color::LightYellow => "light_yellow".into(),
        Color::LightBlue => "light_blue".into(),
        Color::LightMagenta => "light_magenta".into(),
        Color::LightCyan => "light_cyan".into(),
        Color::White => "white".into(),
        Color::Indexed(i) => format!("{i}"),
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows_with_labels(labels: &[&str]) -> Vec<SettingsRow> {
        labels
            .iter()
            .map(|l| SettingsRow::text(*l, "", "key"))
            .collect()
    }

    #[test]
    fn label_width_grows_to_fit_long_labels() {
        // A long keybinding-style description must not be clipped to the old
        // fixed 24-column label width.
        let rows = rows_with_labels(&["Cascade merge main through stack"]); // 32 chars
        let w = settings_label_width(&rows, 120);
        assert_eq!(
            w, 33,
            "label column should fit the longest label plus a space"
        );
    }

    #[test]
    fn label_width_has_a_floor_for_short_labels() {
        let rows = rows_with_labels(&["Quit", "Editor"]);
        assert_eq!(settings_label_width(&rows, 120), 24);
    }

    #[test]
    fn label_width_caps_to_keep_value_column_usable() {
        // On a narrow terminal a very long label must not starve the value column.
        let rows = rows_with_labels(&["A really really really long label here"]);
        let w = settings_label_width(&rows, 50);
        assert_eq!(w, 50 - (16 + 2), "value column keeps its minimum width");
    }

    #[test]
    fn label_width_never_below_floor_even_when_very_narrow() {
        let rows = rows_with_labels(&["Some long label that won't fit"]);
        // Cap would be negative; floor wins and labels simply truncate.
        assert_eq!(settings_label_width(&rows, 10), 24);
    }

    #[test]
    fn truncate_str_truncates_multibyte_names_without_panicking() {
        // Regression: byte-index slicing panicked on non-ASCII section names
        // ("byte index is not a char boundary") when the Sections tab rendered.
        assert_eq!(truncate_str("ありがとうございます", 6), "ありがとう…");
        assert_eq!(truncate_str("ありがとう", 5), "ありがとう");
    }

    #[test]
    fn truncate_str_ascii_behaviour() {
        assert_eq!(truncate_str("short", 10), "short");
        assert_eq!(truncate_str("longer-name", 7), "longer…");
        assert_eq!(truncate_str("ab", 1), "…");
    }

    fn keybinding_rows() -> Vec<SettingsRow> {
        vec![
            SettingsRow::header("Navigation"),
            SettingsRow::text("Navigate up", "k, Up", "navigate_up"),
            SettingsRow::text("Navigate down", "j, Down", "navigate_down"),
            SettingsRow::header("Sessions"),
            SettingsRow::text("Delete/kill session", "d", "delete_session"),
        ]
    }

    #[test]
    fn first_selectable_skips_leading_header() {
        let rows = keybinding_rows();
        // Row 0 is a header; the first selectable row is index 1.
        assert_eq!(first_selectable_from(&rows, 0), 1);
        // From a header index, the next selectable is found forward.
        assert_eq!(first_selectable_from(&rows, 3), 4);
    }

    #[test]
    fn step_selectable_skips_headers_and_wraps() {
        let rows = keybinding_rows();
        // From "Navigate down" (2), forward skips the "Sessions" header (3).
        assert_eq!(step_selectable(&rows, 2, true), 4);
        // Forward from the last row wraps past the leading header to row 1.
        assert_eq!(step_selectable(&rows, 4, true), 1);
        // Backward from row 1 wraps to the last selectable row, skipping headers.
        assert_eq!(step_selectable(&rows, 1, false), 4);
    }

    #[test]
    fn filter_matches_description_and_keys() {
        // Matches on the description text.
        let out = filter_keybinding_rows(keybinding_rows(), "delete");
        assert_eq!(out.len(), 2); // "Sessions" header + the delete row
        assert!(matches!(out[0].kind, SettingsRowKind::Header));
        assert_eq!(out[0].label, "Sessions");
        assert_eq!(out[1].field_key, "delete_session");

        // Matches on the bound keys ("Up" belongs to navigate_up).
        let out = filter_keybinding_rows(keybinding_rows(), "Up");
        assert_eq!(out[0].label, "Navigation");
        assert!(out.iter().any(|r| r.field_key == "navigate_up"));
    }

    #[test]
    fn filter_drops_empty_section_headers() {
        // A query that hits nothing yields no rows (no orphan headers).
        let out = filter_keybinding_rows(keybinding_rows(), "zzzznomatch");
        assert!(out.is_empty());

        // A query hitting only Navigation drops the Sessions header entirely.
        let out = filter_keybinding_rows(keybinding_rows(), "navigate");
        assert!(out.iter().all(|r| r.label != "Sessions"));
        assert_eq!(out[0].label, "Navigation");
    }

    #[test]
    fn filter_empty_query_returns_all_rows() {
        // An empty query keeps every binding; only presentational spacers are
        // added between sections.
        let rows = keybinding_rows();
        let selectable = rows.iter().filter(|r| r.is_selectable()).count();
        let out = filter_keybinding_rows(rows, "   ");
        assert_eq!(out.iter().filter(|r| r.is_selectable()).count(), selectable);
        // Two sections → exactly one blank spacer between them.
        assert_eq!(out.iter().filter(|r| is_spacer(r)).count(), 1);
    }

    #[test]
    fn spacers_separate_sections_but_not_the_first() {
        let rows = with_section_spacers(keybinding_rows());
        // No leading spacer.
        assert!(!is_spacer(&rows[0]));
        assert_eq!(rows[0].label, "Navigation");
        // Exactly one spacer, and it sits immediately before the second header.
        let spacer_positions: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, r)| is_spacer(r))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(spacer_positions.len(), 1);
        let s = spacer_positions[0];
        assert!(matches!(rows[s + 1].kind, SettingsRowKind::Header));
        assert_eq!(rows[s + 1].label, "Sessions");
    }

    #[test]
    fn unique_program_label_avoids_clashes() {
        use claude_commander_core::config::ProgramEntry;
        let entry = |label: &str| ProgramEntry {
            label: label.to_string(),
            command: "claude".to_string(),
        };

        // Empty list → the base label.
        assert_eq!(unique_program_label(&[]), "New program");

        // Base taken → first numbered suffix.
        assert_eq!(
            unique_program_label(&[entry("New program")]),
            "New program 2"
        );

        // Base + first suffix taken → skips to the next free number.
        assert_eq!(
            unique_program_label(&[entry("New program"), entry("New program 2")]),
            "New program 3"
        );

        // Unrelated labels don't affect the base.
        assert_eq!(unique_program_label(&[entry("Claude")]), "New program");
    }

    #[test]
    fn settings_tab_cycle_includes_voice() {
        assert_eq!(SettingsTab::ALL.len(), 8);
        assert!(SettingsTab::ALL.contains(&SettingsTab::Workspaces));
        assert!(SettingsTab::ALL.contains(&SettingsTab::Voice));
        assert!(SettingsTab::ALL.contains(&SettingsTab::Programs));
        assert!(SettingsTab::ALL.contains(&SettingsTab::Server));
        assert_eq!(SettingsTab::General.next(), SettingsTab::Voice);
        assert_eq!(SettingsTab::Voice.prev(), SettingsTab::General);
        assert_eq!(SettingsTab::Voice.next(), SettingsTab::Keybindings);
        // Server sits last, after Programs, and wraps back to General.
        assert_eq!(SettingsTab::Sections.next(), SettingsTab::Workspaces);
        assert_eq!(SettingsTab::Workspaces.next(), SettingsTab::Programs);
        assert_eq!(SettingsTab::Programs.prev(), SettingsTab::Workspaces);
        assert_eq!(SettingsTab::Programs.next(), SettingsTab::Server);
        assert_eq!(SettingsTab::Server.next(), SettingsTab::General);
        assert_eq!(SettingsTab::General.prev(), SettingsTab::Server);
        assert_eq!(SettingsTab::Server.prev(), SettingsTab::Programs);
        // A full forward cycle returns to the start.
        let mut t = SettingsTab::General;
        for _ in 0..SettingsTab::ALL.len() {
            t = t.next();
        }
        assert_eq!(t, SettingsTab::General);
        // The tab is named for the feature, not for the `[conversation]` TOML
        // table it grew out of: it holds speech in as well as speech out.
        assert_eq!(SettingsTab::Voice.label(), "Voice");
        assert_eq!(SettingsTab::Programs.label(), "Programs");
    }
}
