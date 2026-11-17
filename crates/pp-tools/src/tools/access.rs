//! `Tools` 的构造、拆除与工具侧句柄访问器。
use anyhow::{Result, bail, ensure};
use pp_config::config::EditMode;
use pp_config::config::ToolLimits;
use pp_core::plan::PlanDocument;
use pp_core::plan::WorkMode;
use serde_json::Value;
use std::{
    cell::RefCell,
    path::{Path, PathBuf},
};

use super::{
    ExecuteCall, PLAN_BASE_TOOLS, PLAN_SEARCH_TOOLS, ToolPolicy, Tools, approval, is_plan_tool,
    plan,
};
use pp_bash::{BackgroundCleanup, PipeStore};
use pp_workspace::{diff, save};

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
    pub fn new(cwd: PathBuf, edit_mode: EditMode) -> Result<Self> {
        let workspace = save::Workspace::open(&cwd)?;
        let eval_available = pp_eval::probe_python(&workspace.path, None).is_ok();
        Ok(Self {
            artifacts: Default::default(),
            ask_handler: None,
            mcp: std::sync::Arc::new(pp_mcp::McpManager::empty()),
            mcp_shared: false,
            mcp_readonly: false,
            lsp: None,
            lsp_cleanup: None,
            lsp_readonly: false,
            lsp_owner: pp_lsp::writethrough::Owner::new(),
            subagent: None,
            allowed_tools: None,
            yield_schema: None,
            yielded: RefCell::new(None),
            approval: approval::Gate::new(),
            skills: pp_config::skills::Skills::default().shared(),
            cwd: workspace.path.clone(),
            edit_mode,
            workspace,
            write_lock: Default::default(),
            web_cache: RefCell::default(),
            snapshots: RefCell::default(),
            project_instructions: RefCell::default(),
            background_tasks: Default::default(),
            eval_available,
            eval: pp_eval::Session::new(),
            eval_enabled: true,
            eval_interpreter: None,
            wait_state: RefCell::default(),
            limits: ToolLimits::default(),
            host_defaults: pp_workspace::host_defaults::current(),
            pipe_store: PipeStore::new(),
            sandbox: std::sync::Arc::new(pp_sandbox::Sandbox::disabled()),
        })
    }

    pub fn set_sandbox(&mut self, sandbox: std::sync::Arc<pp_sandbox::Sandbox>) {
        self.sandbox = sandbox;
    }

    pub fn sandbox(&self) -> &std::sync::Arc<pp_sandbox::Sandbox> {
        &self.sandbox
    }

    /// 生产与子 Agent 构造入口：注入进程级共享的 `McpManager`。
    pub fn with_mcp(
        cwd: PathBuf,
        edit_mode: EditMode,
        mcp: std::sync::Arc<pp_mcp::McpManager>,
    ) -> Result<Self> {
        let mut tools = Self::new(cwd, edit_mode)?;
        tools.mcp = mcp;
        Ok(tools)
    }

    pub fn pipe_store(&self) -> PipeStore {
        self.pipe_store.clone()
    }

    /// 换根时替换 registry；旧会话的临时 URI 不跨根复用。
    pub fn reset_pipe_store(&mut self) {
        self.pipe_store = PipeStore::new();
    }

    /// 子 Agent 的 run 与根会话共用临时正文配额和生命周期。
    pub fn share_pipe_store(&mut self, store: &PipeStore) {
        self.pipe_store = store.clone();
    }

    pub fn stop_background_tasks(&self) -> Result<()> {
        self.background_tasks.stop_all()?;
        self.wait_state.borrow_mut().clear_background();
        Ok(())
    }

    pub fn background_cleanup(&self) -> BackgroundCleanup {
        self.background_tasks.cleanup()
    }

    pub fn configure_eval(&mut self, settings: &pp_config::config::EvalSettings) {
        self.eval_enabled = settings.enabled;
        self.eval_interpreter.clone_from(&settings.interpreter);
        self.eval_available = settings.enabled
            && pp_eval::probe_python(&self.cwd, self.eval_interpreter.as_deref()).is_ok();
    }

    pub fn eval_available(&self) -> bool {
        self.eval_available
    }

    pub fn stop_eval(&self) -> Result<()> {
        self.eval.stop()
    }

    pub fn eval_cleanup(&self) -> pp_eval::EvalCleanup {
        self.eval.cleanup()
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }
    pub fn edit_mode(&self) -> EditMode {
        self.edit_mode
    }

    /// Safe read of the current plan document through the NOFOLLOW workspace
    /// handle. Oversize, non-UTF-8, symlink, missing and special-type targets
    /// are errors; a logged snapshot is never substituted for the file. A
    /// document without a saved lifecycle path reports `No saved plan.`
    pub fn read_plan(&self, document: &PlanDocument) -> Result<String> {
        plan::read_plan(&self.workspace, document)
    }

    /// True only when both workspace-normalized paths are exactly equal; the
    /// expected path comes from the caller's bound plan document.
    pub fn is_plan_path(&self, path: &str, expected_path: &str) -> bool {
        plan::is_plan_path(&self.workspace, path, expected_path)
    }

    /// 登记表/登记事务复用的安全工作区句柄；不扩大任何文件权限。
    pub fn plan_workspace(&self) -> &save::Workspace {
        &self.workspace
    }

    /// 调用者已完成安全提交，并传入工作区规范相对路径；此处不再读盘。
    pub(super) fn record_committed_file(
        &self,
        call_id: &str,
        path: PathBuf,
        before: diff::Original,
        after: &[u8],
        emit: &mut dyn FnMut(pp_core::events::AgentEvent),
    ) {
        let diff = diff::committed(&before, Some(after));
        self.snapshots.borrow_mut().invalidate(&path);
        emit(pp_core::events::AgentEvent::FileChanged {
            id: call_id.to_owned(),
            path: path.display().to_string(),
            diff,
        });
    }

    pub fn clear_caches(&self) {
        self.web_cache.borrow_mut().clear();
        self.snapshots.borrow_mut().clear();
    }

    pub(super) fn mcp_access(&self, mode: WorkMode) -> pp_mcp::Access {
        if self.mcp_readonly {
            pp_mcp::Access::ReadOnly
        } else {
            pp_mcp::Access::for_mode(mode)
        }
    }

    pub(super) fn authorize_call(
        &self,
        call: &ExecuteCall<'_, '_>,
        hook_ask: Option<&str>,
    ) -> Result<AuthorizedAccess> {
        let name = call.name;
        let arguments = call.arguments;
        ensure!(
            self.yielded.borrow().is_none(),
            "subagent task is complete; further tools were not executed"
        );
        if name == "plan_submit"
            && let Some(binding) = &self.subagent
        {
            ensure!(
                !binding.pending(),
                "wait for the subagent result to complete and commit to context before submitting a plan."
            );
        }
        if let Some(allowed) = &self.allowed_tools {
            ensure!(
                allowed.iter().any(|tool| tool == name)
                    || name == "yield" && self.yield_schema.is_some(),
                "Tool is outside this agent capability set."
            );
        }
        // A sealed batch (plan already submitted) refuses every call before
        // any parsing or dispatch, while still producing one result per call
        // ID through the ordinary ToolStarted/ToolFinished path.
        if let ToolPolicy::Plan { sealed: true, .. } = call.policy {
            bail!("Not executed: plan submitted for review.");
        }
        // Execution-layer policy gate. It runs before argument parsing, file
        // access, process creation and MCP dispatch, so denied tools have no
        // side effects even with malformed arguments, and unknown names
        // (including unseen MCP tools) cannot bypass it via the fallback.
        let mcp_readonly_tool = self.mcp.is_read_only(name);
        let mcp_access = match call.policy {
            ToolPolicy::Execute { .. } => self.mcp_access(WorkMode::Execute),
            _ => pp_mcp::Access::ReadOnly,
        };
        let allowed = match &call.policy {
            ToolPolicy::Execute { .. } => {
                !is_plan_tool(name)
                    && (mcp_access == pp_mcp::Access::Full
                        || !name.starts_with("mcp__")
                        || mcp_readonly_tool)
            }
            ToolPolicy::Review => {
                PLAN_BASE_TOOLS.contains(&name)
                    || PLAN_SEARCH_TOOLS.contains(&name)
                    || name == "lsp" && self.lsp.is_some()
                    || mcp_readonly_tool
            }
            ToolPolicy::Plan { .. } => {
                PLAN_BASE_TOOLS.contains(&name)
                    || PLAN_SEARCH_TOOLS.contains(&name)
                    || name == "lsp" && self.lsp.is_some()
                    || mcp_readonly_tool
                    || self.subagent.is_some() && matches!(name, "task" | "wait")
                    || name == "write"
                    || is_plan_tool(name)
                    || name == "ask" && self.ask_handler.is_some()
            }
        };
        if !allowed {
            bail!(
                "{}",
                match &call.policy {
                    ToolPolicy::Execute { .. } => {
                        format!("{name} is only available in Plan mode.")
                    }
                    ToolPolicy::Review => "Tool is not available during Plan review.".to_owned(),
                    ToolPolicy::Plan { .. } => "Tool is not available in Plan mode.".to_owned(),
                }
            );
        }
        if name == "eval" && !self.eval_enabled {
            bail!("eval is disabled in settings.");
        }
        // 未安装（enabled=false 或 Agent 未初始化 LSP）时显式调用直接失败，
        // 既不进入审批也不尝试发现服务器。
        if name == "lsp" && self.lsp.is_none() {
            bail!("lsp is not available; LSP is disabled or not installed.");
        }
        // LSP 的动作级授权在执行层完成：受限模式（Plan/Review/readonly
        // 子 Agent）连写动作的参数都不解析就拒绝，且必须先于审批门，
        // 避免受限 Agent 通过审批提示绕过白名单。schema 裁剪只是提示。
        let lsp_access = match &call.policy {
            ToolPolicy::Execute { .. } if !self.lsp_readonly => pp_lsp::tool::Access::Execute,
            _ => pp_lsp::tool::Access::ReadOnly,
        };
        if name == "lsp" {
            pp_lsp::tool::validate_access(arguments, lsp_access)?;
        }
        if let Some(message) = self.instruction_batch_blocked() {
            bail!("{message}");
        }
        // 审批门只作用于 Execute；Plan/Review 的白名单门禁已是完整授权边界。
        // 与 Plan/Review 门禁一样在参数解析与分发之前执行，失败由
        // execute_with_fetch 统一回填为 ok:false 的工具结果。
        if matches!(call.policy, ToolPolicy::Execute { .. })
            && !(mcp_readonly_tool && self.subagent.as_ref().is_some_and(|b| b.plan_mode()))
        {
            let tier = if mcp_readonly_tool {
                approval::Tier::Read
            } else {
                approval::tier_of_arguments(name, arguments)
            };
            self.approval
                .enforce(name, arguments, tier, hook_ask, call.cancellation)?;
        }
        Ok(AuthorizedAccess {
            mcp_access,
            lsp_access,
        })
    }
}

pub(super) struct AuthorizedAccess {
    pub(super) mcp_access: pp_mcp::Access,
    pub(super) lsp_access: pp_lsp::tool::Access,
}

impl Drop for Tools {
    fn drop(&mut self) {
        if let Some(binding) = self.subagent.take().filter(|b| b.is_root())
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
