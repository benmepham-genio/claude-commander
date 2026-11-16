use super::*;
use claude_commander_protocol::api::{CreateOptions, EditSession, ProgramInfo};
use crossterm::event::{KeyCode, KeyEvent};

#[derive(Debug, Clone)]
pub struct SessionEditor {
    pub session: SessionRef,
    pub name: Input,
    pub program: Input,
    pub model: claude_commander_protocol::session_editor::SessionEditorModel,
    pub sections: Vec<Option<String>>,
    pub section: usize,
    pub bases: Vec<(Option<SessionId>, String)>,
    pub base: usize,
    pub keep_alive: bool,
    pub focus: usize,
    pub programs: Vec<ProgramInfo>,
    pub restart_now: bool,
}

impl SessionEditor {
    pub fn new(
        session: SessionRef,
        info: &claude_commander_protocol::api::SessionInfo,
        peers: &[claude_commander_protocol::api::SessionInfo],
        main_branch: &str,
    ) -> Self {
        let model =
            claude_commander_viewmodel::session_editor::model(info, peers, Some(main_branch), None);
        let section = usize::from(model.original.section.is_some());
        let base = model
            .bases
            .iter()
            .position(|b| b.id == model.original.base)
            .unwrap_or(0);
        let mut sections = vec![None];
        sections.extend(model.sections.iter().cloned().map(Some));
        Self {
            session,
            name: model.original.title.clone().into(),
            program: model.original.program.clone().into(),
            sections,
            section,
            bases: model
                .bases
                .iter()
                .map(|b| (b.parent_session_id, b.label.clone()))
                .collect(),
            base,
            keep_alive: model.original.keep_alive,
            focus: 0,
            programs: model.programs.clone(),
            model,
            restart_now: false,
        }
    }
    pub fn load_options(&mut self, opts: CreateOptions) {
        let section = self.sections[self.section].clone();
        claude_commander_viewmodel::session_editor::load_options(&mut self.model, &opts);
        self.sections = vec![None];
        self.sections
            .extend(self.model.sections.iter().cloned().map(Some));
        self.section = self
            .sections
            .iter()
            .position(|s| *s == section)
            .unwrap_or(0);
        self.programs = self.model.programs.clone();
    }
    fn plan(
        &self,
    ) -> std::result::Result<claude_commander_protocol::session_editor::SessionEditorPlan, String>
    {
        claude_commander_viewmodel::session_editor::plan(
            &self.model,
            claude_commander_protocol::session_editor::SessionEditorDraft {
                title: self.name.value().into(),
                program: self.program.value().into(),
                section: self.sections[self.section].clone(),
                keep_alive: self.keep_alive,
                base: self.model.bases[self.base].id.clone(),
            },
        )
    }
    fn program_label(&self) -> &str {
        self.programs
            .iter()
            .find(|p| p.command == self.program.value())
            .map_or(self.program.value(), |p| p.label.as_str())
    }
    pub fn edit(&self, restart: bool) -> std::result::Result<EditSession, String> {
        let mut edit = self.plan()?.edit;
        edit.restart = restart;
        Ok(edit)
    }
    fn cycle(&mut self, forward: bool) {
        fn step(index: usize, len: usize, forward: bool) -> usize {
            if forward {
                (index + 1) % len
            } else {
                (index + len - 1) % len
            }
        }
        match self.focus {
            1 => {
                let index = self
                    .programs
                    .iter()
                    .position(|s| s.command == self.program.value())
                    .unwrap_or(0);
                self.program = self.programs[step(index, self.programs.len(), forward)]
                    .command
                    .clone()
                    .into();
            }
            2 => self.section = step(self.section, self.sections.len(), forward),
            3 => self.base = step(self.base, self.bases.len(), forward),
            4 => self.keep_alive = !self.keep_alive,
            _ => {}
        }
    }
}

impl App {
    pub(super) fn handle_edit_session(&mut self) {
        let Some(session) = self.ui_state.selected_session_id else {
            return;
        };
        let Some(info) = self.session(session) else {
            return;
        };
        let Some(handle) = self.backend(session.backend) else {
            return;
        };
        let main = handle
            .view
            .snapshot
            .projects
            .iter()
            .find(|p| p.id == info.project_id)
            .map(|p| p.main_branch.as_str())
            .unwrap_or("main");
        let editor = SessionEditor::new(session, info, &handle.view.snapshot.sessions, main);
        self.ui_state.modal = Modal::EditSession(editor);
        let backend = self.backend_arc(session.backend);
        let tx = self.event_loop.sender();
        tokio::spawn(async move {
            let result = backend.create_options().await.map_err(|e| e.to_string());
            let _ = tx
                .send(AppEvent::StateUpdate(
                    StateUpdate::SessionEditOptionsLoaded { session, result },
                ))
                .await;
        });
    }
    pub(super) fn handle_edit_session_key(&mut self, key: KeyEvent) {
        let modal = std::mem::replace(&mut self.ui_state.modal, Modal::None);
        let (mut editor, warning) = match modal {
            Modal::EditSession(editor) => (editor, false),
            Modal::EditSessionRestart(editor) => (editor, true),
            other => {
                self.ui_state.modal = other;
                return;
            }
        };
        if warning {
            match key.code {
                KeyCode::Esc => self.ui_state.modal = Modal::EditSession(editor),
                KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab => {
                    editor.restart_now = !editor.restart_now;
                    self.ui_state.modal = Modal::EditSessionRestart(editor);
                }
                KeyCode::Char('y') => self.save_session_edit(editor, true),
                KeyCode::Char('n') => self.save_session_edit(editor, false),
                KeyCode::Enter => {
                    let restart = editor.restart_now;
                    self.save_session_edit(editor, restart);
                }
                _ => self.ui_state.modal = Modal::EditSessionRestart(editor),
            }
            return;
        }
        match key.code {
            KeyCode::Esc => return,
            KeyCode::Tab | KeyCode::Down => editor.focus = (editor.focus + 1) % 5,
            KeyCode::BackTab | KeyCode::Up => editor.focus = (editor.focus + 4) % 5,
            KeyCode::Enter => {
                if editor.name.value().trim().is_empty() || editor.program.value().trim().is_empty()
                {
                    self.ui_state.status_message = Some((
                        "Name and program cannot be empty".into(),
                        Instant::now() + Duration::from_secs(4),
                    ));
                } else if editor.plan().is_ok_and(|p| p.requires_restart) {
                    self.ui_state.modal = Modal::EditSessionRestart(editor);
                    return;
                } else {
                    self.save_session_edit(editor, false);
                    return;
                }
            }
            KeyCode::Left | KeyCode::Right if editor.focus >= 1 => {
                editor.cycle(key.code == KeyCode::Right)
            }
            KeyCode::Char(' ') if editor.focus >= 1 => editor.cycle(true),
            _ if editor.focus == 0 => {
                edit_text_input(&mut editor.name, key);
            }
            _ => {}
        }
        self.ui_state.modal = Modal::EditSession(editor);
    }
    fn save_session_edit(&mut self, editor: SessionEditor, restart: bool) {
        let edit = match editor.edit(restart) {
            Ok(edit) => edit,
            Err(message) => {
                self.ui_state.status_message =
                    Some((message, Instant::now() + Duration::from_secs(5)));
                self.ui_state.modal = Modal::EditSession(editor);
                return;
            }
        };
        let backend = self.backend_arc(editor.session.backend);
        let tx = self.event_loop.sender();
        tokio::spawn(async move {
            match backend.edit_session(editor.session.id, edit).await {
                Ok(outcome) => {
                    if !outcome.warnings.is_empty() {
                        let _ = tx
                            .send(AppEvent::StateUpdate(StateUpdate::Error {
                                message: outcome.warnings.join("\n"),
                            }))
                            .await;
                    }
                    let _ = tx
                        .send(AppEvent::StateUpdate(StateUpdate::SessionMutationApplied {
                            backend_id: editor.session.backend.0,
                            session_id: editor.session.id,
                        }))
                        .await;
                }
                Err(e) => {
                    let _ = tx
                        .send(AppEvent::StateUpdate(StateUpdate::Error {
                            message: format!("Failed to edit session: {e}"),
                        }))
                        .await;
                }
            }
        });
    }
}

pub(super) fn render(
    frame: &mut Frame,
    area: Rect,
    editor: &SessionEditor,
    warning: bool,
    theme: &Theme,
) {
    let area = super::modals::centered_rect(75, 14, area);
    frame.render_widget(Clear, area);
    let title = if warning {
        " Restart session? "
    } else {
        " Edit session "
    };
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border_focused));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let lines = if warning {
        vec![
            Line::from("Changing the program requires a fresh restart."),
            Line::from("Restarting stops the current agent and clears its conversation."),
            Line::from("Saving for later keeps the current agent running."),
            Line::from("The new program starts fresh on the next restart."),
            Line::from(""),
            Line::from(if editor.restart_now {
                "[Yes, restart and clear]    No, save for next restart"
            } else {
                " Yes, restart and clear    [No, save for next restart]"
            }),
            Line::from("←/→ choose · Enter confirm · Esc back"),
        ]
    } else {
        let values = [
            format!(
                "Name: {}",
                if editor.focus == 0 {
                    input_with_caret(&editor.name)
                } else {
                    editor.name.value().into()
                }
            ),
            format!("Program: {}", editor.program_label()),
            format!(
                "Section: {}",
                editor.sections[editor.section]
                    .as_deref()
                    .unwrap_or("Automatic")
            ),
            format!("Stack base: {}", editor.bases[editor.base].1),
            format!(
                "Keep alive: {}",
                if editor.keep_alive { "Yes" } else { "No" }
            ),
        ];
        let mut lines: Vec<Line> = values
            .into_iter()
            .enumerate()
            .map(|(i, text)| {
                Line::styled(
                    format!("{} {text}", if editor.focus == i { ">" } else { " " }),
                    Style::default().fg(if editor.focus == i {
                        theme.text_primary
                    } else {
                        theme.text_secondary
                    }),
                )
            })
            .collect();
        lines.extend([
            Line::from(""),
            Line::from("Tab/↑/↓ field · ←/→ or Space choose option"),
            Line::from("Enter save · Esc cancel"),
            Line::from("Stack base changes metadata and PR target; git history is unchanged."),
        ]);
        lines
    };
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}
