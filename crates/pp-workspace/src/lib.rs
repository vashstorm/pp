pub mod diff;
pub mod host_defaults;
pub mod image;
pub mod output;
pub mod save;
pub mod stream_text;

use serde::{Deserialize, Serialize};

/// Tool approval risk tier; shared by execution layers such as `pp-lsp` and `pp-tools`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    Read,
    Write,
    Exec,
}

/// Wait-job snapshot shared by background Bash tasks and subagents.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WaitJob {
    pub id: String,
    pub r#type: String,
    pub label: String,
    pub status: String,
    pub duration_ms: u64,
    pub delivered: bool,
}

impl WaitJob {
    pub fn pending(&self) -> bool {
        matches!(self.status.as_str(), "queued" | "running")
    }
}
