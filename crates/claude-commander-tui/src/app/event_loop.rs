//! Main event loop: tick dispatch, event processing, and config hot-reload.

use super::*;
use crate::widgets::status_glyph::status_spins;

impl App {
    /// Main event loop
    pub(super) async fn main_loop(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> Result<()> {
        // Sync selection ids from the restored board cursor.
        self.update_selection();
        self.spawn_preview_update();

        let mut dirty = true;
        loop {
            // Honour a quit that was requested *before* this loop started. The
            // in-session switcher runs the palette while an attach is suspended,
            // so a command picked there (Quit, say) lands here already decided;
            // without this the loop would draw a frame and sit waiting for an
            // unrelated keystroke before acting on it. The check at the bottom
            // covers quits raised by events we process ourselves.
            if self.ui_state.should_quit {
                break;
            }

            // Full-screen-takeover clearing happens inside `render` via the
            // `Clear` widget (see the `force_clear`/`leaving_fullscreen`
            // handling there). We must not
            // call `terminal.clear()`: since ratatui 0.30 it reads the cursor
            // position from stdin, which races our background input reader,
            // times out, and kills the loop.

            // Render with whatever data we have — never blocks on I/O
            if dirty {
                terminal
                    .draw(|f| self.render(f))
                    .map_err(|e| TuiError::RenderError(e.to_string()))?;
            }

            // Wait for at least one event
            let Some(event) = self.event_loop.next().await else {
                break;
            };

            // Process first event, then drain all pending events.
            // This ensures rapid keypresses are handled immediately
            // without waiting for the next render cycle.
            dirty = self.process_event(event).await;
            let batch_started = Instant::now();
            for _ in 0..128 {
                if batch_started.elapsed() >= Duration::from_millis(8) {
                    break;
                }
                let Some(event) = self.event_loop.try_next() else {
                    break;
                };
                dirty |= self.process_event(event).await;
            }

            if self.ui_state.should_quit {
                break;
            }
        }

        Ok(())
    }

    /// Process a single event, returning whether the frame needs a redraw.
    pub(super) async fn process_event(&mut self, event: AppEvent) -> bool {
        match event {
            AppEvent::Input(input) => {
                let old_session = self.ui_state.selected_session_id;
                let old_project = self.ui_state.selected_project_id;
                let old_pane = self.ui_state.right_pane_view;
                let old_info = self.is_info_open();

                self.handle_input(input).await;
                // Keep selection IDs in sync after input (needed for
                // correct behavior when draining multiple events)
                self.update_selection();

                if self.ui_state.selected_session_id != old_session
                    || self.ui_state.selected_project_id != old_project
                {
                    self.ui_state.pending_selection = None;
                    self.ui_state.pending_project = None;
                }
                // Re-capture immediately when the selection changes, so the
                // pane doesn't keep showing the previous session until the next
                // tick.
                if self.ui_state.selected_session_id != old_session
                    || self.ui_state.selected_project_id != old_project
                    || self.ui_state.right_pane_view != old_pane
                    || self.is_info_open() != old_info
                {
                    // Cancel any in-flight fetch for the old selection
                    if let Some(task) = self.ui_state.preview_task.take() {
                        task.abort();
                    }
                    self.ui_state.preview_update_spawned_at = None;
                    self.spawn_preview_update();
                }
            }
            AppEvent::StateUpdate(update) => {
                let changed = match &update {
                    StateUpdate::PreviewReady {
                        preview_content,
                        shell_content,
                        diff_info,
                        ..
                    } => {
                        &self.ui_state.preview_content != preview_content
                            || &self.ui_state.shell_content != shell_content
                            || self.ui_state.diff_info.diff != diff_info.diff
                            || self.ui_state.diff_info.files_changed != diff_info.files_changed
                            || self.ui_state.diff_info.lines_added != diff_info.lines_added
                            || self.ui_state.diff_info.lines_removed != diff_info.lines_removed
                    }
                    _ => true,
                };
                self.handle_state_update(update).await;
                return changed;
            }
            AppEvent::Tick => {
                self.ui_state.tick_count = self.ui_state.tick_count.wrapping_add(1);
                let recording = self.conversation.is_recording();
                let mut dirty = self.drain_dictations()
                    || matches!(self.ui_state.modal, Modal::Conversation { .. })
                    || recording != self.ui_state.last_recording;
                self.ui_state.last_recording = recording;
                if self.ui_state.last_animation.elapsed() >= Duration::from_millis(100) {
                    self.ui_state.last_animation = Instant::now();
                    self.ui_state.throbber_state.calc_next();
                    dirty |= matches!(self.ui_state.modal, Modal::Loading { .. })
                        || self.backends.iter().any(|h| {
                            h.view
                                .snapshot
                                .sessions
                                .iter()
                                .any(|s| status_spins(s.status))
                                || h.view
                                    .agent_states
                                    .states
                                    .values()
                                    .any(|s| *s == AgentState::Working)
                        });
                }
                if let Some(crate::digit_accumulator::DigitResult::Jump(n)) =
                    self.digit_accumulator.tick()
                {
                    self.jump_to_session_number(n);
                    dirty = true;
                }
                let preview_interval = if self.is_info_open()
                    || self.ui_state.right_pane_view == RightPaneView::Info
                {
                    Duration::from_secs(1)
                } else {
                    Duration::from_millis(100)
                };
                if self.ui_state.last_preview_refresh.elapsed() >= preview_interval {
                    self.ui_state.last_preview_refresh = Instant::now();
                    self.spawn_preview_update();
                }
                if self.ui_state.tick_count.is_multiple_of(30) {
                    self.spawn_reviewed_marks_poll(self.ui_state.tick_count / 30);
                }
                if self.ui_state.last_ui_maintenance.elapsed() >= Duration::from_secs(1) {
                    self.ui_state.last_ui_maintenance = Instant::now();
                    if self.check_config_reload() {
                        self.refresh_list_items().await;
                    }
                    // Age labels and expiring status messages use wall-clock time.
                    dirty = true;
                }
                return dirty;
            }
            AppEvent::Quit => {
                self.ui_state.should_quit = true;
            }
        }
        true
    }

    /// Check if `config.toml` has been modified externally and refresh the local cache.
    pub(super) fn check_config_reload(&mut self) -> bool {
        if self.ui_state.config_reload_in_flight {
            return false;
        }
        self.ui_state.config_reload_in_flight = true;
        let service = self.service.clone();
        let tx = self.event_loop.sender();
        // reload_config takes the provider-transition lock, which review polls,
        // deletion and retargeting can hold across slow I/O. Awaiting it on a
        // tick would stop input and rendering even when config is unchanged.
        tokio::spawn(async move {
            let result = service.reload_config().await.map_err(|e| e.to_string());
            let _ = tx
                .send(AppEvent::StateUpdate(StateUpdate::ConfigReloaded {
                    result,
                }))
                .await;
        });
        false
    }

    pub(super) fn apply_config_reload(
        &mut self,
        result: std::result::Result<bool, String>,
    ) -> bool {
        self.ui_state.config_reload_in_flight = false;
        match result {
            Ok(true) => {
                debug!("Config hot-reloaded from disk");
                // Also reconciles the live backends against a changed
                // remote-server list.
                self.sync_config_from_service();
                self.reload_theme();
                true
            }
            Ok(false) => false,
            Err(e) => {
                debug!("Config reload check failed: {}", e);
                false
            }
        }
    }
}
