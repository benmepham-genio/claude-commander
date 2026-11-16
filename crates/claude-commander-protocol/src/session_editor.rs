//! Wire values for the shared session editor. Decisions live in viewmodel.
use crate::{
    api::{CreateOptions, EditSession, ProgramInfo, SessionInfo},
    session::SessionId,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SessionEditorInput {
    pub session: SessionInfo,
    pub peers: Vec<SessionInfo>,
    pub main_branch: Option<String>,
    pub options: Option<CreateOptions>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SessionEditorBase {
    pub id: String,
    pub label: String,
    pub parent_session_id: Option<SessionId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SessionEditorDraft {
    pub title: String,
    pub program: String,
    pub section: Option<String>,
    pub keep_alive: bool,
    pub base: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SessionEditorModel {
    pub original: SessionEditorDraft,
    pub live_program: String,
    pub programs: Vec<ProgramInfo>,
    pub sections: Vec<String>,
    pub bases: Vec<SessionEditorBase>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SessionEditorPlan {
    pub edit: EditSession,
    pub requires_restart: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct PlanSessionEdit {
    pub model: SessionEditorModel,
    pub draft: SessionEditorDraft,
}
