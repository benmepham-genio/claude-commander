use serde::{Deserialize, Serialize};

/// Resources requested by a preview consumer. Older callers request all.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PreviewPart {
    #[default]
    All,
    Pane,
    Diff,
    Stats,
    Shell,
}
impl PreviewPart {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Pane => "pane",
            Self::Diff => "diff",
            Self::Stats => "stats",
            Self::Shell => "shell",
        }
    }
}
