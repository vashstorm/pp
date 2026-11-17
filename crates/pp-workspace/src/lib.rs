pub mod diff;
pub mod host_defaults;
pub mod image;
pub mod output;
pub mod save;
pub mod stream_text;

use serde::{Deserialize, Serialize};

/// 工具审批风险分档；由 `pp-lsp`、`pp-tools` 等执行层共用。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    Read,
    Write,
    Exec,
}

/// 后台 Bash 与子 Agent 共用的等待任务快照。
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
