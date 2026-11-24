//! `auto_review`（Approve for me）独立安全审阅契约。
//!
//! `pp-permission` 保持纯同步、无 LLM/网络依赖；运行时通过回调函数注入
//! 基于 `ModelRole::Review` 的独立审阅器。

use anyhow::Result;
use pp_config::config::PermissionMode;
use pp_core::cancellation::CancellationToken;
use std::path::PathBuf;

/// `REVIEW` 审阅器的二元判定结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutoReviewDecision {
    Pass,
    Deny,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AutoReviewVerdict {
    pub decision: AutoReviewDecision,
    pub reason: String,
}

/// 传递给 `REVIEW` 审阅器的最小脱敏请求载荷。
#[derive(Clone, Debug)]
pub struct AutoReviewRequest {
    pub tool: String,
    pub agent: Option<String>,
    pub mode: PermissionMode,
    pub cwd: PathBuf,
    pub trigger_reason: String,
    pub action_summary: String,
    pub requested_writable: Vec<PathBuf>,
    pub requested_protected: Vec<PathBuf>,
    pub requested_network: bool,
}

pub type AutoReviewCallback =
    dyn Fn(AutoReviewRequest, &CancellationToken) -> Result<AutoReviewVerdict> + Send + Sync;
