//! 项目级计划管理：三个生命周期目录的列表、纳管、重命名与确认删除。
//!
//! 所有 I/O 在 worker 侧完成；正文仍是唯一的内容/进度来源，登记表只提供
//! 稳定身份与位置，绝不授予执行权限。UI 只持有 `PlanSelection` 操作令牌，
//! worker 按 ID 重查登记并核对当前路径/精确正文，不按列表下标操作磁盘。

mod list;
mod ops;
mod select;

use crate::tools::plan_catalog::{PlanFileIdentity, PlanLease};
use crate::tools::save::Workspace;
use anyhow::{Result, anyhow, ensure};
use pp_core::plan::PLAN_MAX_BYTES;
use pp_core::plan::PlanLocation;
use rustix::io::Errno;
use std::path::Path;
use std::time::SystemTime;

/// 列表中的一个计划条目；`error` 非空时禁用进入/重命名/删除。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanListEntry {
    pub id: String,
    pub file_name: String,
    pub location: PlanLocation,
    /// 首个 H1 文本，缺失时为文件名；约束为合法的 1..120 字符显示标题。
    pub title: String,
    pub completed: usize,
    pub total: usize,
    pub modified: SystemTime,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PlanListing {
    pub entries: Vec<PlanListEntry>,
    pub diagnostics: Vec<String>,
}

/// 操作令牌：ID、当时的项目相对路径与精确正文。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanSelection {
    pub id: String,
    pub path: String,
    pub content: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanOpenIntent {
    ContinueDraft,
    Execute,
}

const DELETED_PLAN: &str = "Plan was deleted; choose another plan from /plan-list.";
const CHANGED: &str = "Plan changed; reopen it from /plan-list.";
const NOT_FOUND: &str = "Plan was not found; refresh the list.";
const NAME_EXISTS: &str = "Plan file name already exists; choose another file_name.";
const REGISTER_CONFLICT: &str =
    "Plan file conflicts with the registered plan of the same name; resolve it in /plan-list.";
const PP_DIR_NAME: &str = ".pp";

/// `verify_selection` 的核验结果：登记表解析出的稳定身份、canonical 路径与
/// 已核对字节的正文，以及调用方须持有到操作结束的计划 lease（Drop 即释放，
/// 不存回 `Agent::plan_lease`）。
pub(crate) struct VerifiedSelection {
    pub(crate) id: String,
    pub(crate) file_name: String,
    pub(crate) location: PlanLocation,
    /// 登记表解析出的 canonical 项目相对路径，已核对与 `selection.path` 一致。
    pub(crate) path: String,
    /// 当前磁盘上的精确字节（已验证与 `selection.content` 一致）。
    pub(crate) content: String,
    pub(crate) _lease: PlanLease,
}

/// 目录扫描到的单个合法 `.md` 直接子项；stat 失败（符号链接、特殊文件、
/// 身份读取错误）保留为错误，由列表降级为禁用操作的条目。
struct ScannedFile {
    file_name: String,
    location: PlanLocation,
    stat: Result<(PlanFileIdentity, SystemTime)>,
}

/// NOFOLLOW 安全读取单个计划正文；上限 `PLAN_MAX_BYTES`，要求 UTF-8。
fn read_plan_bytes(workspace: &Workspace, path: &Path) -> Result<String> {
    let target = workspace.target(path, false)?;
    let bytes = target.read_bounded(PLAN_MAX_BYTES)?;
    ensure!(bytes.len() <= PLAN_MAX_BYTES, "Plan file is too large.");
    let content =
        std::str::from_utf8(&bytes).map_err(|_| anyhow!("Plan file is not valid UTF-8."))?;
    Ok(content.to_owned())
}

pub(crate) use pp_core::plan::plan_display_title;

/// 判断错误链中是否只是“路径不存在”；生命周期目录缺失表示该分类为空。
fn is_missing(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.downcast_ref::<Errno>() == Some(&Errno::NOENT))
}

#[cfg(test)]
mod fixtures {
    use super::super::Agent;
    use super::*;
    use crate::tools::Tools;
    use crate::tools::plan_catalog::{self, PlanCatalogLock};
    use pp_config::config::EditMode;
    use pp_config::config::ModelCatalog;
    use serde_json::json;

    /// An agent journaled into the fixture's global sessions directory, as
    /// production does, with its own working directory.
    pub(super) fn fixture_agent(cwd: &std::path::Path, sessions: &std::path::Path) -> Agent {
        std::fs::create_dir_all(cwd).unwrap();
        let global = sessions.parent().unwrap();
        // 生产会在启动时建立全局 sessions 目录；fixture 先建好再写配置。
        std::fs::create_dir_all(sessions).unwrap();
        let settings = global.join("settings.yml");
        let models = global.join("models.yml");
        std::fs::write(&settings, "default_model: fixture/one\n").unwrap();
        std::fs::write(
            &models,
            json!({"providers":{"fixture": { "api_key":"fixture-key", "protocol":"completion",
                "models":{"one": {"model":"one"}}}}})
            .to_string(),
        )
        .unwrap();
        Agent::new(
            ModelCatalog::from_files(&settings, &models).unwrap(),
            Tools::new(cwd.to_owned(), EditMode::Replace).unwrap(),
            sessions.to_owned(),
        )
        .unwrap()
    }

    pub(super) fn write_plan(
        dir: &std::path::Path,
        location: PlanLocation,
        name: &str,
        content: &str,
    ) {
        let path = dir.join(".pp").join(location.dir_name()).join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    pub(super) fn entry<'a>(listing: &'a PlanListing, file_name: &str) -> &'a PlanListEntry {
        listing
            .entries
            .iter()
            .find(|entry| entry.file_name == file_name)
            .unwrap()
    }

    pub(super) fn registered(
        agent: &Agent,
        file_name: &str,
    ) -> crate::tools::plan_catalog::PlanCatalogEntry {
        let workspace = agent.tools.plan_workspace();
        let _lock = PlanCatalogLock::acquire(workspace).unwrap();
        let snapshot = plan_catalog::load_catalog(workspace).unwrap();
        snapshot
            .catalog
            .entries
            .iter()
            .find(|entry| !entry.deleted && entry.file_name == file_name)
            .cloned()
            .unwrap()
    }
}
