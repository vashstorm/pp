//! Construction, teardown and tool-side handle accessors for `Tools`.
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
use pp_bash::BackgroundCleanup;
use pp_workspace::{diff, save};

/// Extract only the display path from model arguments, without touching the filesystem.
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
    /// Construction for tests and mocks only: an empty `McpManager` that never reads real user MCP definitions.
    /// `eval` availability stays conservatively off and is assigned only by `configure_eval` from config,
    /// so Python is not probed here (avoiding one extra subprocess at construction time).
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
            eval_py_available: false,
            eval_js_available: None,
            eval: pp_eval::Session::new(),
            eval_enabled: true,
            eval_interpreter: None,
            eval_js_runtime: None,
            wait_state: RefCell::default(),
            limits: ToolLimits::default(),
            host_defaults: pp_workspace::host_defaults::current(),
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

    /// Production and subagent construction entry: injects the process-wide shared `McpManager`.
    pub fn with_mcp(
        cwd: PathBuf,
        edit_mode: EditMode,
        mcp: std::sync::Arc<pp_mcp::McpManager>,
    ) -> Result<Self> {
        let mut tools = Self::new(cwd, edit_mode)?;
        tools.mcp = mcp;
        Ok(tools)
    }

    pub fn stop_background_tasks(&self) -> Result<()> {
        self.background_tasks.stop_all()?;
        self.wait_state.borrow_mut().clear_background();
        Ok(())
    }

    pub fn background_cleanup(&self) -> BackgroundCleanup {
        self.background_tasks.cleanup()
    }

    /// The single assignment point for `eval` availability: probes the Python and JS runtimes from the `eval` section of `settings.yml`.
    pub fn configure_eval(&mut self, settings: &pp_config::config::EvalSettings) {
        self.eval_enabled = settings.enabled;
        self.eval_interpreter.clone_from(&settings.interpreter);
        self.eval_js_runtime.clone_from(&settings.js_runtime);
        if settings.enabled {
            self.eval_py_available =
                pp_eval::probe_python(&self.cwd, self.eval_interpreter.as_deref()).is_ok();
            self.eval_js_available = pp_eval::probe_js(&self.cwd, self.eval_js_runtime.as_deref())
                .ok()
                .map(|rt| rt.kind);
        } else {
            self.eval_py_available = false;
            self.eval_js_available = None;
        }
    }

    pub fn eval_available(&self) -> bool {
        self.eval_py_available || self.eval_js_available.is_some()
    }

    pub fn eval_py_available(&self) -> bool {
        self.eval_py_available
    }

    pub fn eval_js_available(&self) -> bool {
        self.eval_js_available.is_some()
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

    /// Safe workspace handle reused by the registry and registry transactions; it grants no extra file permissions.
    pub fn plan_workspace(&self) -> &save::Workspace {
        &self.workspace
    }

    /// The caller already committed safely and passes the canonical workspace-relative path; nothing is read here.
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
        // Freeze the permission tier and the actual sandbox snapshot (including the /sandbox override) before any approval wait,
        // so switching tiers or clearing the override during the wait cannot affect a call already in approval.
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
        // An explicit call fails outright when not installed (enabled=false or the Agent did not initialize LSP),
        // neither entering approval nor trying to discover a server.
        if name == "lsp" && self.lsp.is_none() {
            bail!("lsp is not available; LSP is disabled or not installed.");
        }
        // LSP action-level authorization happens in the execution layer: restricted modes (Plan/Review/readonly
        // subagents) reject write actions without even parsing their arguments, and it must precede the permission
        // gate so a restricted Agent cannot bypass the allowlist through an approval prompt. Schema trimming is only a hint.
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
        // MCP tools not marked read-only keep the Execute write-tier permission rules under Plan;
        // local read tools (read/rg/glob/lsp) under Execute and Plan go through sandbox read-path classification and approval,
        // while Review stays non-interactive and read-only, with the sandbox intercepting out-of-scope reads directly.
        let needs_permission = match call.policy {
            ToolPolicy::Execute { .. } => {
                !(mcp_readonly_tool && self.subagent.as_ref().is_some_and(|b| b.plan_mode()))
            }
            ToolPolicy::Plan { .. } => is_mcp && !mcp_readonly_tool,
            ToolPolicy::Review => false,
        };
        let is_read_checked_tool = matches!(name, "read" | "rg" | "glob" | "lsp");
        let mut read_policy: Option<std::sync::Arc<pp_sandbox::Policy>> = None;
        let mut approved_session_read = None;

        if matches!(call.policy, ToolPolicy::Review)
            && name == "lsp"
            && let Some(policy) = sandbox.policy()?
        {
            for target in
                pp_lsp::tool::resolve_read_targets(&self.cwd, arguments, call.cancellation)?
            {
                policy.check_read(&target)?;
            }
        }

        if needs_permission
            || (matches!(call.policy, ToolPolicy::Plan { .. }) && is_read_checked_tool)
        {
            let perm_name = mcp_target_name;
            let perm_args = resolved_mcp_call
                .as_ref()
                .map_or(arguments, |resolved| resolved.inner_arguments.as_str());
            let tier = if mcp_readonly_tool {
                permission::Tier::Read
            } else {
                permission::tier_of_arguments(perm_name, perm_args)
            };
            // Approval must reflect the real sandbox policy frozen for this call: only sandbox-constrained
            // tools under Execute count as sandboxed when the policy really exists. off / auto degradation
            // (policy is None) falls back to ordinary approval; explicitly enabled but unavailable on the
            // platform fails closed, returning an error before approval or any side effect.
            let sandbox_checked_tool = matches!(call.policy, ToolPolicy::Execute { .. })
                && matches!(name, "write" | "edit" | "bash" | "bash_task" | "eval");
            let policy = if sandbox_checked_tool
                || (matches!(call.policy, ToolPolicy::Execute { .. }) && name == "web_search")
                || (matches!(
                    call.policy,
                    ToolPolicy::Execute { .. } | ToolPolicy::Plan { .. }
                ) && is_read_checked_tool)
            {
                sandbox.policy()?
            } else {
                None
            };
            let sandboxed = sandbox_checked_tool && policy.is_some();
            let network_enabled = policy.as_ref().is_none_or(|p| p.network());
            let read_target = (name == "read")
                .then(|| super::read::parse_args(arguments, &self.cwd).ok())
                .flatten()
                .and_then(|path| super::read::Target::parse(&path, &self.cwd).ok());
            let deferred_to_dispatch = matches!(call.policy, ToolPolicy::Execute { .. })
                && (matches!(name, "bash" | "write" | "edit" | "web_search")
                    || read_target.as_ref().is_some_and(|target| target.is_url()));

            let mut path_reads_authorized = false;
            if is_read_checked_tool && let Some(policy) = policy.as_ref() {
                if permissions == PermissionMode::DangerFullAccess {
                    read_policy = Some(policy.without_home_restriction());
                } else {
                    let raw_targets: Vec<PathBuf> = match name {
                        "read" => read_target
                            .as_ref()
                            .and_then(|target| target.local_path(&self.cwd))
                            .into_iter()
                            .collect(),
                        "rg" => super::rg::inspect_roots(&self.cwd, arguments)?,
                        "glob" => super::glob::inspect_root(&self.cwd, arguments)?
                            .into_iter()
                            .collect(),
                        "lsp" => pp_lsp::tool::resolve_read_targets(
                            &self.cwd,
                            arguments,
                            call.cancellation,
                        )?,
                        _ => Vec::new(),
                    };

                    let mut session_checks = Vec::new();
                    let mut outside_checks = Vec::new();
                    let mut session_reads = Vec::new();
                    let mut outside_reads = Vec::new();
                    for local in raw_targets {
                        match policy.classify_read(&local) {
                            pp_sandbox::ReadAccessCheck::Allowed => {}
                            pp_sandbox::ReadAccessCheck::ImmutableDenied(msg) => bail!("{msg}"),
                            pp_sandbox::ReadAccessCheck::NeedsSessionApproval(path) => {
                                if name == "read" {
                                    ensure!(path.is_file(), "read requires a regular file");
                                }
                                session_checks.push((local, path.clone()));
                                if !session_reads.contains(&path) {
                                    session_reads.push(path);
                                }
                            }
                            pp_sandbox::ReadAccessCheck::NeedsOutsideApproval(path) => {
                                outside_checks.push((local, path.clone()));
                                if !outside_reads.contains(&path) {
                                    outside_reads.push(path);
                                }
                            }
                        }
                    }

                    if !session_reads.is_empty() || !outside_reads.is_empty() {
                        self.permission.authorize_path_reads(
                            permissions,
                            perm_name,
                            &self.cwd,
                            &session_reads,
                            &outside_reads,
                            hook_ask,
                            call.cancellation,
                        )?;
                        for (local, expected) in &session_checks {
                            ensure!(
                                policy.classify_read(local)
                                    == pp_sandbox::ReadAccessCheck::NeedsSessionApproval(
                                        expected.clone()
                                    ),
                                "approved session path changed during permission check"
                            );
                        }
                        for (local, expected) in &outside_checks {
                            ensure!(
                                matches!(
                                    policy.classify_read(local),
                                    pp_sandbox::ReadAccessCheck::NeedsOutsideApproval(ref current)
                                        if current == expected
                                ) || matches!(
                                    policy.classify_read(local),
                                    pp_sandbox::ReadAccessCheck::Allowed
                                ),
                                "approved read path changed during permission check"
                            );
                        }
                        let scoped = policy.with_approved_reads(&session_reads, &outside_reads)?;
                        if name == "read"
                            && let Some(target) = read_target
                            && let [only_session] = session_reads.as_slice()
                        {
                            approved_session_read = Some((target, only_session.clone()));
                        }
                        read_policy = Some(scoped);
                        path_reads_authorized = true;
                    } else {
                        read_policy = Some(std::sync::Arc::clone(policy));
                    }
                }
            }

            if needs_permission && !path_reads_authorized && !deferred_to_dispatch {
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
            read_policy,
            hook_ask,
            resolved_mcp_call,
            approved_session_read,
        })
    }
}

pub(super) struct AuthorizedAccess<'a> {
    pub(super) mcp_access: pp_mcp::Access,
    pub(super) lsp_access: pp_lsp::tool::Access,
    pub(super) permissions: PermissionMode,
    pub(super) sandbox: pp_sandbox::SandboxView<'a>,
    pub(super) read_policy: Option<std::sync::Arc<pp_sandbox::Policy>>,
    pub(super) hook_ask: Option<&'a str>,
    pub(super) resolved_mcp_call: Option<super::mcp_proxy::ResolvedMcpCall>,
    pub(super) approved_session_read: Option<(super::read::Target, PathBuf)>,
}

impl Drop for Tools {
    fn drop(&mut self) {
        if let Some(binding) = self.subagent.take().filter(|b| b.is_root())
            && let Err(error) = binding.shutdown()
        {
            tracing::error!(error=%error,"subagent exit cleanup failed");
        }
        // Only the owner of the root/standalone Registry holds this handle; explicit stop leaves error
        // reporting to the caller, so this is a safety-net reclaim.
        if let Some(mut cleanup) = self.lsp_cleanup.take()
            && let Err(error) = cleanup.stop()
        {
            tracing::error!(error=%error,"LSP exit cleanup failed");
        }
    }
}
