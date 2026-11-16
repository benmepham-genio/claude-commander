//! Shared, pure editor initialization and changed-field planning.
use claude_commander_protocol::{
    api::{CreateOptions, SessionInfo, SetSessionBase},
    session_editor::*,
};

pub fn model(
    info: &SessionInfo,
    peers: &[SessionInfo],
    main: Option<&str>,
    options: Option<&CreateOptions>,
) -> SessionEditorModel {
    let mut bases = vec![SessionEditorBase {
        id: "project".into(),
        label: "Project base".into(),
        parent_session_id: None,
    }];
    let peers: Vec<_> = peers
        .iter()
        .filter(|s| s.project_id == info.project_id && s.session_id != info.session_id)
        .collect();
    bases.extend(peers.iter().map(|s| SessionEditorBase {
        id: format!("session:{}", s.session_id.as_uuid()),
        label: format!("{} ({})", s.title, s.branch),
        parent_session_id: Some(s.session_id),
    }));
    let base = if let Some(branch) = info.pr_base_branch.as_deref() {
        if Some(branch) == main {
            "project".into()
        } else if let Some(peer) = peers.iter().find(|s| s.branch == branch) {
            format!("session:{}", peer.session_id.as_uuid())
        } else {
            "current".into()
        }
    } else if let Some(parent) = info.stack_parent_session_id {
        let key = format!("session:{}", parent.as_uuid());
        if bases.iter().any(|b| b.id == key) {
            key
        } else {
            "current".into()
        }
    } else {
        "project".into()
    };
    if base == "current" {
        bases.push(SessionEditorBase {
            id: base.clone(),
            label: format!(
                "Current base ({})",
                info.pr_base_branch
                    .as_deref()
                    .unwrap_or("session unavailable")
            ),
            parent_session_id: None,
        });
    }
    let original = SessionEditorDraft {
        title: info.title.clone(),
        program: info
            .pending_program
            .as_ref()
            .unwrap_or(&info.program)
            .clone(),
        section: info.section_override.clone(),
        keep_alive: info.keep_alive,
        base,
    };
    let mut model = SessionEditorModel {
        original,
        live_program: info.program.clone(),
        programs: vec![],
        sections: vec![],
        bases,
    };
    let empty = CreateOptions {
        default_program: String::new(),
        programs: vec![],
        sections: vec![],
    };
    load_options(&mut model, options.unwrap_or(&empty));
    model
}

pub fn load_options(model: &mut SessionEditorModel, options: &CreateOptions) {
    model.programs.clear();
    for p in &options.programs {
        if !model.programs.iter().any(|v| v.command == p.command) {
            model.programs.push(p.clone());
        }
    }
    for command in [&model.original.program, &model.live_program] {
        if !model.programs.iter().any(|p| &p.command == command) {
            model
                .programs
                .push(claude_commander_protocol::api::ProgramInfo {
                    label: command.clone(),
                    command: command.clone(),
                });
        }
    }
    model.sections.clear();
    for section in &options.sections {
        if !model.sections.contains(section) {
            model.sections.push(section.clone());
        }
    }
    if let Some(section) = &model.original.section
        && !model.sections.contains(section)
    {
        model.sections.push(section.clone());
    }
}

pub fn plan(
    model: &SessionEditorModel,
    mut draft: SessionEditorDraft,
) -> Result<SessionEditorPlan, String> {
    draft.title = draft.title.trim().into();
    if draft.title.is_empty() {
        return Err("Session name cannot be empty".into());
    }
    if !model.programs.iter().any(|p| p.command == draft.program) {
        return Err("Choose a configured or retained program".into());
    }
    if draft
        .section
        .as_ref()
        .is_some_and(|s| !model.sections.contains(s))
    {
        return Err("Choose an available section".into());
    }
    let base = model
        .bases
        .iter()
        .find(|b| b.id == draft.base)
        .ok_or("Choose an available base")?;
    let original = &model.original;
    let changed_program = draft.program != original.program;
    let requires_restart = changed_program && draft.program != model.live_program;
    let edit = claude_commander_protocol::api::EditSession {
        title: (draft.title != original.title).then_some(draft.title),
        program: changed_program.then_some(draft.program),
        section: (draft.section != original.section).then_some(draft.section),
        keep_alive: (draft.keep_alive != original.keep_alive).then_some(draft.keep_alive),
        base: (draft.base != original.base).then_some(SetSessionBase {
            parent_session_id: base.parent_session_id,
        }),
        restart: false,
    };
    Ok(SessionEditorPlan {
        edit,
        requires_restart,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use claude_commander_protocol::{
        api::ProgramInfo,
        session::{ProjectId, SessionId, SessionStatus},
    };
    fn session(title: &str) -> SessionInfo {
        let sid = SessionId::new();
        serde_json::from_value(serde_json::json!({
            "id": sid.to_string(),
            "session_id": sid,
            "title": title,
            "branch": title,
            "status": SessionStatus::Running,
            "program": "claude",
            "project_id": ProjectId::new(),
            "project_name": "repo",
            "pr_number": null,
            "pr_url": null,
            "pr_state": "open",
            "pr_draft": false,
            "pr_labels": [],
            "review_decision": null,
            "pr_reviewers": [],
            "created_at": "2026-01-01T00:00:00Z",
        }))
        .unwrap()
    }

    #[test]
    fn unchanged_form_preserves_concurrent_fields_and_main_pr_base() {
        let mut info = session("task");
        info.pr_base_branch = Some("main".into());
        let model = model(&info, &[], Some("main"), None);
        assert_eq!(model.original.base, "project");
        assert_eq!(model.bases.len(), 1);
        let plan = plan(&model, model.original.clone()).unwrap();
        assert_eq!(
            serde_json::to_value(plan.edit).unwrap(),
            serde_json::json!({"restart":false})
        );
        assert!(!plan.requires_restart);
    }
    #[test]
    fn cancel_pending_program_needs_no_restart_warning() {
        let mut info = session("task");
        info.pending_program = Some("codex".into());
        let model = model(&info, &[], Some("main"), None);
        let mut draft = model.original.clone();
        draft.program = info.program.clone();
        let plan = plan(&model, draft).unwrap();
        assert_eq!(plan.edit.program.as_deref(), Some(info.program.as_str()));
        assert!(!plan.requires_restart);
    }
    #[test]
    fn retains_aliases_live_and_pending_programs_without_free_text() {
        let mut info = session("task");
        info.pending_program = Some("codex".into());
        let options = CreateOptions {
            default_program: "codex".into(),
            programs: vec![ProgramInfo {
                label: "Code".into(),
                command: "codex".into(),
            }],
            sections: vec!["Review".into()],
        };
        let model = model(&info, &[], Some("main"), Some(&options));
        assert_eq!(model.programs.len(), 2);
        assert_eq!(model.programs[0].label, "Code");
        let mut draft = model.original.clone();
        draft.program = "arbitrary".into();
        assert!(plan(&model, draft).is_err());
    }
    #[test]
    fn edits_are_deltas_and_explicit_section_clear_survives_json() {
        let mut info = session("task");
        info.section_override = Some("Review".into());
        let mut model = model(&info, &[], Some("main"), None);
        model.programs.push(ProgramInfo {
            label: "Code".into(),
            command: "codex".into(),
        });
        let mut draft = model.original.clone();
        draft.title = " renamed ".into();
        draft.section = None;
        draft.program = "codex".into();
        let plan = plan(&model, draft).unwrap();
        assert!(plan.requires_restart);
        let json = serde_json::to_value(&plan.edit).unwrap();
        assert_eq!(json["title"], "renamed");
        assert!(json.get("keep_alive").is_none());
        assert!(json.get("base").is_none());
        assert!(json.get("section").unwrap().is_null());
        let decoded: claude_commander_protocol::api::EditSession =
            serde_json::from_value(json).unwrap();
        assert_eq!(decoded.section, Some(None));
    }
    #[test]
    fn unknown_pr_base_is_preserved_until_explicit_selection() {
        let mut info = session("task");
        info.pr_base_branch = Some("remote-base".into());
        let model = model(&info, &[], Some("main"), None);
        assert_eq!(model.original.base, "current");
        assert!(
            plan(&model, model.original.clone())
                .unwrap()
                .edit
                .base
                .is_none()
        );
        let mut draft = model.original.clone();
        draft.base = "project".into();
        assert!(
            plan(&model, draft)
                .unwrap()
                .edit
                .base
                .unwrap()
                .parent_session_id
                .is_none()
        );
    }
    #[test]
    fn aliases_deduplicate_commands_and_peer_bases_use_stable_ids() {
        let info = session("task");
        let mut peer = session("parent");
        peer.project_id = info.project_id;
        let mut info = info;
        info.pr_base_branch = Some(peer.branch.clone());
        let options = CreateOptions {
            default_program: "claude".into(),
            sections: vec![],
            programs: vec![
                ProgramInfo {
                    label: "First alias".into(),
                    command: "claude".into(),
                },
                ProgramInfo {
                    label: "Duplicate".into(),
                    command: "claude".into(),
                },
            ],
        };
        let model = model(&info, &[peer.clone()], Some("main"), Some(&options));
        assert_eq!(model.programs.len(), 1);
        assert_eq!(model.programs[0].label, "First alias");
        assert_eq!(
            model.original.base,
            format!("session:{}", peer.session_id.as_uuid())
        );
        assert!(
            plan(&model, model.original.clone())
                .unwrap()
                .edit
                .base
                .is_none()
        );
    }
    #[test]
    fn full_uuid_base_keys_distinguish_matching_short_prefixes() {
        let info = session("task");
        let mut first = session("first");
        first.project_id = info.project_id;
        first.session_id =
            serde_json::from_str("\"12345678-1111-4111-8111-111111111111\"").unwrap();
        let mut second = first.clone();
        second.session_id =
            serde_json::from_str("\"12345678-2222-4222-8222-222222222222\"").unwrap();
        let options = CreateOptions {
            default_program: "claude".into(),
            programs: vec![],
            sections: vec!["Review".into(), "Review".into()],
        };
        let model = model(
            &info,
            &[first, second.clone()],
            Some("main"),
            Some(&options),
        );
        assert_eq!(model.sections, vec!["Review"]);
        assert_ne!(model.bases[1].id, model.bases[2].id);
        let mut draft = model.original.clone();
        draft.base = model.bases[2].id.clone();
        assert_eq!(
            plan(&model, draft)
                .unwrap()
                .edit
                .base
                .unwrap()
                .parent_session_id,
            Some(second.session_id)
        );
    }
}
