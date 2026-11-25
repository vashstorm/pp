//! `Tools` 的构造、拆除与工具侧句柄访问器。
use anyhow::{Result, bail, ensure};
use pp_config::config::EditMode;
use pp_config::config::PermissionMode;
use pp_config::config::ToolLimits;
use pp_core::plan::PlanDocument;
use pp_permission::ToolAccess;
use serde_json::Value;
use std::{
    cell::RefCell,
    path::{Path, PathBuf},
};

use super::{
    ExecuteCall, PLAN_BASE_TOOLS, PLAN_SEARCH_TOOLS, ToolPolicy, Tools, is_plan_tool, permission,
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
    /// `eval` 可用性保守关闭，只由 `configure_eval` 按配置唯一赋值，
    /// 因此这里不探测 Python（避免构造期多起一次子进程）。
    pub fn new(cwd: PathBuf, edit_mode: EditMode) -> Result<Self> {
        let workspace = save::Workspace::open(&cwd)?;
        Ok(Self {
            artifacts: Default::default(),
            ask_handler: None,
            mcp: std::sync::Arc::new(pp_mcp::McpManager::empty()),
            mcp_shared: false,
            mcp_readonly: false,
            mcp_settings: pp_config::config::McpSettings::default(),
            lsp: None,
            lsp_cleanup: None,
            lsp_readonly: false,
            lsp_owner: pp_lsp::writethrough::Owner::new(),
            subagent: None,
            allowed_tools: None,
            yield_schema: None,
            yielded: RefCell::new(None),
            permission: permission::Gate::new(),
            skills: pp_config::skills::Skills::default().shared(),
            cwd: workspace.path.clone(),
            edit_mode,
            workspace,
            write_lock: Default::default(),
            web_cache: RefCell::default(),
            read_dedup: RefCell::default(),
            snapshots: RefCell::default(),
            project_instructions: RefCell::default(),
            background_tasks: Default::default(),
            eval_available: false,
            eval: pp_eval::Session::new(),
            eval_enabled: true,
            eval_interpreter: None,
            wait_state: RefCell::default(),
            limits: ToolLimits::default(),
            host_defaults: pp_workspace::host_defaults::current(),
            pipe_store: PipeStore::new(),
            sandbox: std::sync::Arc::new(pp_sandbox::Sandbox::disabled()),
            web_search: pp_config::config::WebSearchSettings::default(),
        })
    }

    pub fn configure_web_search(&mut self, settings: pp_config::config::WebSearchSettings) {
        self.web_search = settings;
    }

    pub fn configure_mcp(&mut self, settings: pp_config::config::McpSettings) {
        self.mcp_settings = settings;
    }

    pub(super) fn uses_mcp_proxy(&self, access: pp_mcp::Access) -> bool {
        let count = self.mcp.available_tool_count(access, |name| {
            self.allowed_tools
                .as_ref()
                .is_none_or(|allowed| allowed.allows(name))
        });
        if count == 0 {
            return false;
        }
        match self.mcp_settings.proxy {
            pp_config::config::McpProxyMode::Never => false,
            pp_config::config::McpProxyMode::Always => true,
            pp_config::config::McpProxyMode::Auto => count > self.mcp_settings.proxy_threshold,
        }
    }

    pub fn set_sandbox(&mut self, sandbox: std::sync::Arc<pp_sandbox::Sandbox>) {
        self.sandbox = sandbox;
    }

    pub fn sandbox(&self) -> &std::sync::Arc<pp_sandbox::Sandbox> {
        &self.sandbox
    }

    pub fn sandbox_status(&self) -> pp_sandbox::SandboxStatus {
        self.sandbox.view(self.permission.sandbox_mode()).status()
    }

    pub fn allows_any_mcp(&self) -> bool {
        self.allowed_tools
            .as_ref()
            .is_none_or(crate::AllowedTools::allows_any_mcp)
    }

    pub fn allows_lsp_lifecycle(&self) -> bool {
        self.allowed_tools
            .as_ref()
            .is_none_or(crate::AllowedTools::allows_lsp_lifecycle)
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

    /// `eval` 可用性的唯一赋值点：按 `settings.yml` 的 `eval` 节探测一次解释器。
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
        {
            let mut dedup = self.read_dedup.borrow_mut();
            dedup.invalidate_path(&path);
            if !path.is_absolute() {
                dedup.invalidate_path(&self.cwd.join(&path));
            }
        }
        emit(pp_core::events::AgentEvent::FileChanged {
            id: call_id.to_owned(),
            path: path.display().to_string(),
            diff,
        });
    }

    pub fn invalidate_read_dedup(&self) {
        self.read_dedup.borrow_mut().clear();
    }

    pub fn clear_caches(&self) {
        self.web_cache.borrow_mut().clear();
        self.read_dedup.borrow_mut().clear();
        self.snapshots.borrow_mut().clear();
    }

    pub fn mcp_access(&self) -> pp_mcp::Access {
        if self.mcp_readonly {
            pp_mcp::Access::ReadOnly
        } else {
            pp_mcp::Access::Full
        }
    }

    pub(super) fn authorize_call<'a>(
        &'a self,
        call: &ExecuteCall<'_, '_>,
        hook_ask: Option<&'a str>,
    ) -> Result<AuthorizedAccess<'a>> {
        // 在进入任何审批等待前同时冻结权限档位与实际沙盒快照（含 /sandbox 覆盖值），
        // 避免等待期间切换档位或清除覆盖影响当前已进入审批的调用。
        self.sandbox
            .set_permission_overlay(self.permission.sandbox_overlay(&self.cwd));
        let (permissions, sandbox_mode) = self.permission.snapshot_with_sandbox();
        let sandbox = self.sandbox.view(sandbox_mode);
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
        if let Some(allowed) = &self.allowed_tools
            && !(name == "yield" && self.yield_schema.is_some())
        {
            allowed.authorize_call(name, arguments)?;
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
        let is_mcp_search = name == super::mcp_proxy::MCP_SEARCH_TOOL;
        let is_mcp_call = name == super::mcp_proxy::MCP_CALL_TOOL;
        let resolved_mcp_call = if is_mcp_call {
            Some(super::mcp_proxy::resolve_call(&self.mcp, arguments)?)
        } else {
            None
        };
        if let Some(resolved) = &resolved_mcp_call
            && let Some(allowed) = &self.allowed_tools
        {
            allowed.authorize_call(&resolved.exposed_name, &resolved.inner_arguments)?;
        }
        let mcp_target_name = resolved_mcp_call
            .as_ref()
            .map_or(name, |resolved| resolved.exposed_name.as_str());
        let is_mcp = is_mcp_call || name.starts_with("mcp__");
        let mcp_readonly_tool = is_mcp && self.mcp.is_read_only(mcp_target_name);
        let mcp_access = match call.policy {
            ToolPolicy::Review => pp_mcp::Access::ReadOnly,
            _ => self.mcp_access(),
        };
        let allowed = match &call.policy {
            ToolPolicy::Execute { .. } => {
                !is_plan_tool(name)
                    && (is_mcp_search
                        || mcp_access == pp_mcp::Access::Full
                        || !is_mcp
                        || mcp_readonly_tool)
            }
            ToolPolicy::Review => {
                PLAN_BASE_TOOLS.contains(&name)
                    || PLAN_SEARCH_TOOLS.contains(&name)
                    || name == "lsp" && self.lsp.is_some()
                    || is_mcp_search
                    || mcp_readonly_tool
            }
            ToolPolicy::Plan { .. } => {
                PLAN_BASE_TOOLS.contains(&name)
                    || PLAN_SEARCH_TOOLS.contains(&name)
                    || name == "lsp" && self.lsp.is_some()
                    || is_mcp_search
                    || is_mcp
                        && (mcp_access == pp_mcp::Access::Full || mcp_readonly_tool)
                        && self.mcp.has_available_tool(mcp_target_name)
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
        if name == "web_search" && !self.web_search.enabled {
            bail!("web_search is disabled in settings.");
        }
        // 未安装（enabled=false 或 Agent 未初始化 LSP）时显式调用直接失败，
        // 既不进入审批也不尝试发现服务器。
        if name == "lsp" && self.lsp.is_none() {
            bail!("lsp is not available; LSP is disabled or not installed.");
        }
        // LSP 的动作级授权在执行层完成：受限模式（Plan/Review/readonly
        // 子 Agent）连写动作的参数都不解析就拒绝，且必须先于权限门，
        // 避免受限 Agent 通过权限提示绕过白名单。schema 裁剪只是提示。
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
        // Plan 中未标为只读的 MCP 工具沿用 Execute 的写档权限规则。
        let needs_permission = match call.policy {
            ToolPolicy::Execute { .. } => {
                !(mcp_readonly_tool && self.subagent.as_ref().is_some_and(|b| b.plan_mode()))
            }
            ToolPolicy::Plan { .. } => is_mcp && !mcp_readonly_tool,
            ToolPolicy::Review => false,
        };
        if needs_permission {
            let perm_name = mcp_target_name;
            let perm_args = resolved_mcp_call
                .as_ref()
                .map_or(arguments, |resolved| resolved.inner_arguments.as_str());
            let tier = if mcp_readonly_tool {
                permission::Tier::Read
            } else {
                permission::tier_of_arguments(perm_name, perm_args)
            };
            // 审批必须反映当次冻结的真实沙盒策略：仅 Execute 下受沙盒约束的
            // 工具在策略真实存在时才算 sandboxed。off / auto 降级（policy 为
            // None）回退到普通审批；显式开启但平台不可用则 fail-closed，
            // 在进入审批或产生任何副作用之前返回错误。
            let sandbox_checked_tool = matches!(call.policy, ToolPolicy::Execute { .. })
                && matches!(name, "write" | "edit" | "bash" | "bash_task" | "eval");
            let policy = if sandbox_checked_tool
                || (matches!(call.policy, ToolPolicy::Execute { .. })
                    && matches!(name, "web_search" | "read"))
            {
                sandbox.policy()?
            } else {
                None
            };
            let sandboxed = sandbox_checked_tool && policy.is_some();
            let network_enabled = policy.as_ref().is_none_or(|p| p.network());
            let read_is_http = name == "read"
                && serde_json::from_str::<super::ReadArgs>(arguments)
                    .ok()
                    .and_then(|args| super::read::is_http_url(&args.path, &self.cwd).ok())
                    .unwrap_or(false);
            let deferred_to_dispatch = matches!(call.policy, ToolPolicy::Execute { .. })
                && (matches!(name, "bash" | "write" | "edit" | "web_search") || read_is_http);
            if !deferred_to_dispatch {
                self.permission.enforce_for(
                    permissions,
                    perm_name,
                    perm_args,
                    ToolAccess {
                        tier,
                        sandboxed,
                        network_enabled,
                    },
                    hook_ask,
                    call.cancellation,
                )?;
            }
        }
        Ok(AuthorizedAccess {
            mcp_access,
            lsp_access,
            permissions,
            sandbox,
            hook_ask,
            resolved_mcp_call,
        })
    }
}

pub(super) struct AuthorizedAccess<'a> {
    pub(super) mcp_access: pp_mcp::Access,
    pub(super) lsp_access: pp_lsp::tool::Access,
    pub(super) permissions: PermissionMode,
    pub(super) sandbox: pp_sandbox::SandboxView<'a>,
    pub(super) hook_ask: Option<&'a str>,
    pub(super) resolved_mcp_call: Option<super::mcp_proxy::ResolvedMcpCall>,
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
