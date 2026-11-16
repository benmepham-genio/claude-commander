//! JSON preserves omitted/null patch fields across FRB. All decisions are in viewmodel.
use crate::api::mirrors::{CreateOptions, SessionInfo};
use anyhow::{anyhow, Result};
use flutter_rust_bridge::frb;

#[frb(sync)]
pub fn session_editor_model(
    session: SessionInfo,
    peers: Vec<SessionInfo>,
    main_branch: Option<String>,
    options: Option<CreateOptions>,
) -> Result<String> {
    Ok(serde_json::to_string(
        &claude_commander_viewmodel::session_editor::model(
            &session,
            &peers,
            main_branch.as_deref(),
            options.as_ref(),
        ),
    )?)
}

#[frb(sync)]
pub fn plan_session_edit(model_json: String, draft_json: String) -> Result<String> {
    let model = serde_json::from_str(&model_json)?;
    let draft = serde_json::from_str(&draft_json)?;
    let plan =
        claude_commander_viewmodel::session_editor::plan(&model, draft).map_err(|e| anyhow!(e))?;
    Ok(serde_json::to_string(&plan)?)
}
