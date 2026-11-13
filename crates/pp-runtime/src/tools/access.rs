//! `Tools` 的构造、拆除与工具侧句柄访问器。
use anyhow::Result;
use pp_config::config::EditMode;
use pp_config::config::ToolLimits;
use pp_core::plan::PlanDocument;
use pp_core::plan::WorkMode;
use serde_json::Value;
use std::{
    cell::RefCell,
    path::{Path, PathBuf},
};

use super::{BackgroundCleanup, PipeStore, Tools, approval, plan, save};

/// 仅提取模型参数中的显示路径，不访问文件系统。
pub fn edit_path(arguments: &Value) -> Option<&str> {
    let input = arguments["input"].as_str()?.trim_start_matches('\n');
    let input = input.strip_prefix("*** Begin Patch\n").unwrap_or(input);
    input
        .lines()
        .next()?
        .strip_prefix('[')?
        .strip_suffix(']')?
        .rsplit_once('#')
        .map(|(path, _)| path)
}

impl Tools {
    /// 测试与 mock 专用构造：空 `McpManager`，不读取真实用户 MCP 定义。
    pub(crate) fn new(cwd: PathBuf, edit_mode: EditMode) -> Result<Self> {
        let workspace = save::Workspace::open(&cwd)?;
        Ok(Self {
            artifacts: Default::default(),
            ask_handler: None,
            mcp: std::sync::Arc::new(crate::mcp::McpManager::empty()),
            mcp_shared: false,
            mcp_readonly: false,
            lsp: None,
            lsp_cleanup: None,
            lsp_readonly: false,
            lsp_owner: crate::lsp::writethrough::Owner::new(),
            subagent: None,
            allowed_tools: None,
            yield_schema: None,
            yielded: RefCell::new(None),
            approval: approval::Gate::new(),
            skills: pp_config::skills::Skills::default().shared(),
            cwd: workspace.path.clone(),
            edit_mode,
            workspace,
            changes: Default::default(),
            write_lock: Default::default(),
            web_cache: RefCell::default(),
            snapshots: RefCell::default(),
            project_instructions: RefCell::default(),
            background_tasks: Default::default(),
            wait_state: RefCell::default(),
            limits: ToolLimits::default(),
            host_defaults: crate::host_defaults::current(),
            pipe_store: PipeStore::new(),
        })
    }

    /// 生产与子 Agent 构造入口：注入进程级共享的 `McpManager`。
    pub fn with_mcp(
        cwd: PathBuf,
        edit_mode: EditMode,
        mcp: std::sync::Arc<crate::mcp::McpManager>,
    ) -> Result<Self> {
        let mut tools = Self::new(cwd, edit_mode)?;
        tools.mcp = mcp;
        Ok(tools)
    }

    pub(crate) fn pipe_store(&self) -> PipeStore {
        self.pipe_store.clone()
    }

    /// 换根时替换 registry；旧会话的临时 URI 不跨根复用。
    pub(crate) fn reset_pipe_store(&mut self) {
        self.pipe_store = PipeStore::new();
    }

    /// 子 Agent 的 run 与根会话共用临时正文配额和生命周期。
    pub(crate) fn share_pipe_store(&mut self, store: &PipeStore) {
        self.pipe_store = store.clone();
    }

    pub(crate) fn workspace(&self) -> &save::Workspace {
        &self.workspace
    }

    pub(crate) fn stop_background_tasks(&self) -> Result<()> {
        self.background_tasks.stop_all()?;
        self.wait_state.borrow_mut().clear_background();
        Ok(())
    }

    pub(crate) fn background_cleanup(&self) -> BackgroundCleanup {
        self.background_tasks.cleanup()
    }

    pub(crate) fn cwd(&self) -> &Path {
        &self.cwd
    }
    pub(crate) fn edit_mode(&self) -> EditMode {
        self.edit_mode
    }

    /// Safe read of the current plan document through the NOFOLLOW workspace
    /// handle. Oversize, non-UTF-8, symlink, missing and special-type targets
    /// are errors; a logged snapshot is never substituted for the file. A
    /// document without a saved lifecycle path reports `No saved plan.`
    pub(crate) fn read_plan(&self, document: &PlanDocument) -> Result<String> {
        plan::read_plan(&self.workspace, document)
    }

    /// True only when both workspace-normalized paths are exactly equal; the
    /// expected path comes from the caller's bound plan document.
    pub(crate) fn is_plan_path(&self, path: &str, expected_path: &str) -> bool {
        plan::is_plan_path(&self.workspace, path, expected_path)
    }

    /// 登记表/登记事务复用的安全工作区句柄；不扩大任何文件权限。
    pub(crate) fn plan_workspace(&self) -> &save::Workspace {
        &self.workspace
    }

    pub(super) fn mcp_access(&self, mode: WorkMode) -> crate::mcp::Access {
        if self.mcp_readonly {
            crate::mcp::Access::ReadOnly
        } else {
            crate::mcp::Access::for_mode(mode)
        }
    }
}

impl Drop for Tools {
    fn drop(&mut self) {
        if let Some(binding) = self.subagent.take().filter(|b| b.depth == 0)
            && let Err(error) = binding.shutdown()
        {
            tracing::error!(error=%error,"subagent exit cleanup failed");
        }
        // 只有根/独立 Registry 的 owner 持有该句柄；显式 stop 由调用方负责
        // 上报错误，这里只做兜底回收。
        if let Some(mut cleanup) = self.lsp_cleanup.take()
            && let Err(error) = cleanup.stop()
        {
            tracing::error!(error=%error,"LSP exit cleanup failed");
        }
    }
}
