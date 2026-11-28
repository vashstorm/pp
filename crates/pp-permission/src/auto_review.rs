//! Independent security review contract for `auto_review` (Approve for me).
//!
//! `pp-permission` stays purely synchronous with no LLM/network dependency; at runtime a callback injects
//! an independent reviewer backed by `ModelRole::Review`.

use anyhow::Result;
use pp_config::config::PermissionMode;
use pp_core::cancellation::CancellationToken;
use std::path::PathBuf;

/// Binary verdict of the `REVIEW` reviewer.
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

/// Minimal redacted request payload passed to the `REVIEW` reviewer.
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
