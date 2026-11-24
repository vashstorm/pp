//! 生效权限、工具门禁与用户授权。
//!
//! 交互请求经 `PermissionHandler` 交给调用方，
//! 无 handler 时拒绝调用。工具白名单先于权限检查；Execute 工具与
//! Plan 中未标为只读的 MCP 工具经过本模块。

use pp_config::config::{PermissionMode, PermissionPolicy, SandboxMode};
use std::path::Path;

mod bash_ast;
mod preapproved;
mod rules;
use preapproved::PreapprovedTools;
pub use preapproved::{BashCommandPattern, check_bash_command_rules};
pub use rules::{BashRule, SandboxGrant};

use anyhow::{Result, bail};
use std::{
    collections::{BTreeMap, HashSet},
    sync::{Arc, Mutex},
};

use pp_core::cancellation::CancellationToken;

pub use pp_workspace::Tier;

/// settings.yml `tools:` 小节的运行时形态；构造在权限门内部完成。
#[derive(Clone, Debug, Default)]
struct PermissionConfig {
    policy: BTreeMap<String, PermissionPolicy>,
}

impl From<pp_config::config::ToolsSettings> for PermissionConfig {
    fn from(settings: pp_config::config::ToolsSettings) -> Self {
        Self {
            policy: settings.permission,
        }
    }
}

/// 权限请求：只携带工具名与脱敏 reason，不携带参数原文；
/// 参数详情在 TUI 中经既有 ToolStarted / Ctrl+O 查看。
#[derive(Clone, Debug)]
pub struct PermissionRequest {
    pub tool: String,
    pub agent: Option<String>,
    pub reason: String,
    /// 仅当本条 prompt 允许"本会话内放行"时为 true；强制 prompt
    /// （关键命令、用户 `prompt` policy）每次都问，不提供该选项。
    pub session_grant: bool,
    pub persistent_grant: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionDecision {
    Approve,
    ApproveSession,
    ApprovePersistently,
    Deny,
}

pub type PermissionCallback =
    dyn Fn(PermissionRequest, &CancellationToken) -> Result<PermissionDecision> + Send + Sync;
pub type PermissionHandler = Box<PermissionCallback>;

pub struct ToolAccess {
    pub tier: Tier,
    pub sandboxed: bool,
}

#[derive(Default)]
struct ModeState {
    config: PermissionConfig,
    configured: PermissionMode,
    startup: Option<PermissionMode>,
    selected: Option<PermissionMode>,
    manage_sandbox: bool,
}

impl ModeState {
    fn current(&self) -> PermissionMode {
        self.selected.or(self.startup).unwrap_or(self.configured)
    }

    fn sandbox_mode(&self) -> Option<SandboxMode> {
        self.manage_sandbox.then(|| match self.current() {
            PermissionMode::Yolo => SandboxMode::Off,
            PermissionMode::AcceptEdits | PermissionMode::AlwaysAsk => SandboxMode::WorkspaceWrite,
        })
    }
}

#[derive(Clone)]
pub struct Gate {
    default_mode: PermissionMode,
    is_process: bool,
    sandbox_override: bool,
    /// 常规工具的会话级授权。
    grants: Arc<Mutex<HashSet<String>>>,
    /// Bash 命令前缀的会话级授权。
    bash_grants: Arc<Mutex<HashSet<Vec<String>>>>,
    /// CLI `--allowed-tools` 赋予本次运行的工具预批准集合；不随会话重置清空。
    preapproved: Arc<Mutex<PreapprovedTools>>,
    handler: Option<Arc<PermissionCallback>>,
    modes: Arc<Mutex<ModeState>>,
    sandbox_grants: Arc<Mutex<HashSet<SandboxGrant>>>,
    store: Option<rules::Store>,
}

impl Default for Gate {
    fn default() -> Self {
        Self::new()
    }
}

impl Gate {
    pub fn new() -> Self {
        let default_mode = PermissionMode::Yolo;
        Self {
            default_mode,
            is_process: false,
            sandbox_override: false,
            grants: Default::default(),
            bash_grants: Default::default(),
            preapproved: Default::default(),
            handler: None,
            modes: Arc::new(Mutex::new(ModeState {
                configured: default_mode,
                ..Default::default()
            })),
            sandbox_grants: Default::default(),
            store: None,
        }
    }

    pub fn for_process(
        agent_dir: &Path,
        startup: Option<PermissionMode>,
        sandbox_override: bool,
    ) -> Self {
        let default_mode = PermissionMode::AcceptEdits;
        Self {
            default_mode,
            is_process: true,
            sandbox_override,
            modes: Arc::new(Mutex::new(ModeState {
                config: PermissionConfig::default(),
                configured: default_mode,
                startup,
                selected: None,
                manage_sandbox: startup.is_some() || !sandbox_override,
            })),
            store: Some(rules::Store::new(agent_dir)),
            ..Self::new()
        }
    }

    pub fn protects_path(&self, path: &Path) -> bool {
        self.store
            .as_ref()
            .is_some_and(|store| store.protects_path(path))
    }

    pub fn snapshot(&self) -> PermissionMode {
        self.snapshot_with_sandbox().0
    }

    pub fn sandbox_mode(&self) -> Option<SandboxMode> {
        self.snapshot_with_sandbox().1
    }

    /// 原子读取当前生效权限档位及对应的沙盒预设，供单次工具调用入口使用。
    pub fn snapshot_with_sandbox(&self) -> (PermissionMode, Option<SandboxMode>) {
        let state = self.modes.lock().unwrap_or_else(|error| error.into_inner());
        (state.current(), state.sandbox_mode())
    }

    pub fn select(&self, requested: Option<PermissionMode>) {
        let mut state = self.modes.lock().unwrap_or_else(|error| error.into_inner());
        let current = state.current();
        let selected = requested.unwrap_or_else(|| current.next());
        state.selected = Some(selected);
        state.manage_sandbox = true;
        if current != selected {
            self.grants
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clear();
            self.bash_grants
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clear();
        }
    }

    pub fn clear_session(&self) {
        self.grants
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
        self.bash_grants
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
        self.sandbox_grants
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
    }

    /// 登记 CLI `--allowed-tools` 的进程级预批准工具与细粒度 Bash 命令规则集合。
    pub fn grant_allowed_tools(
        &self,
        exact: impl IntoIterator<Item = String>,
        prefixes: Vec<String>,
        all_standard: bool,
        bash_patterns: Vec<BashCommandPattern>,
    ) {
        let mut preapproved = self
            .preapproved
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        preapproved.exact = exact.into_iter().collect();
        preapproved.prefixes = prefixes;
        preapproved.all_standard = all_standard;
        preapproved.bash_patterns = bash_patterns;
    }

    fn is_preapproved(&self, name: &str) -> bool {
        self.preapproved
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .contains(name)
    }

    fn matches_preapproved_bash_leaf(&self, leaf: &bash_ast::LeafCommand) -> bool {
        self.preapproved
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .matches_bash_leaf(leaf)
    }

    pub fn list_rules(&self) -> Result<String> {
        self.store.as_ref().map_or_else(
            || Ok("No saved permission rules.".into()),
            rules::Store::list,
        )
    }

    pub fn save_bash_rule(&self, rule: BashRule) -> Result<()> {
        let Some(store) = &self.store else {
            bail!("permission rule store unavailable");
        };
        store.save_bash_batch(vec![rule])
    }

    pub fn revoke_rule(&self, target: &str) -> Result<()> {
        let Some(store) = &self.store else {
            bail!("permission rule store unavailable");
        };
        store.revoke(target)
    }

    pub fn authorize_sandbox(
        &self,
        grant: SandboxGrant,
        cwd: Option<&Path>,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        pp_core::cancellation::check(cancellation)?;
        if self
            .sandbox_grants
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .contains(&grant)
        {
            return Ok(());
        }
        let load_error = match self.store.as_ref().map(rules::Store::load).transpose() {
            Ok(Some(doc)) if doc.sandbox.iter().any(|rule| rule == &grant) => {
                return Ok(());
            }
            Ok(_) => None,
            Err(error) => {
                let message = pp_core::logging::diagnostic_text(&format!("{error:#}"));
                tracing::warn!(%message, "无法读取持久授权");
                Some(message)
            }
        };
        let Some(handler) = &self.handler else {
            if let Some(message) = load_error {
                bail!(
                    "Bash requires sandbox permission and no interactive UI is available. {message}"
                );
            }
            bail!("Bash requires sandbox permission and no interactive UI is available.");
        };
        let mut reason = grant.summary(cwd);
        if let Some(message) = &load_error {
            reason.push_str(&format!("\nSaved permissions unavailable: {message}"));
        }
        let persistent_grant = self.store.is_some() && load_error.is_none();
        let decision = handler(
            PermissionRequest {
                tool: "bash".into(),
                agent: None,
                reason,
                session_grant: true,
                persistent_grant,
            },
            cancellation,
        )?;
        pp_core::cancellation::check(cancellation)?;
        match decision {
            PermissionDecision::Approve => Ok(()),
            PermissionDecision::ApproveSession => {
                self.sandbox_grants
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .insert(grant);
                Ok(())
            }
            PermissionDecision::ApprovePersistently if persistent_grant => {
                self.store
                    .as_ref()
                    .expect("持久授权选项要求有规则存储")
                    .save(grant)?;
                pp_core::cancellation::check(cancellation)?;
                Ok(())
            }
            PermissionDecision::ApprovePersistently => {
                bail!("persistent permission is unavailable")
            }
            PermissionDecision::Deny => bail!("Bash sandbox permission was denied by the user."),
        }
    }

    pub fn label(&mut self, label: String, owner: String) {
        if let Some(handler) = self.handler.clone() {
            self.handler = Some(Arc::new(move |mut request, token| {
                request.agent = Some(owner.clone());
                request.reason = format!("[{label}] {}", request.reason);
                pp_core::cancellation::check(token)?;
                handler(request, token)
            }));
        }
    }

    pub fn set_config(&self, settings: pp_config::config::ToolsSettings) {
        let mut state = self.modes.lock().unwrap_or_else(|error| error.into_inner());
        state.configured = settings.permission_mode.unwrap_or(self.default_mode);
        state.manage_sandbox = state.selected.is_some()
            || state.startup.is_some()
            || settings.permission_mode.is_some()
            || (self.is_process && !self.sandbox_override && settings.sandbox.is_default());
        state.config = PermissionConfig::from(settings);
    }

    pub fn set_handler(&mut self, handler: PermissionHandler) {
        self.handler = Some(Arc::from(handler));
    }

    /// 生效权限档位；只读查询，不另存状态。
    pub fn effective_mode(&self) -> PermissionMode {
        self.snapshot()
    }

    fn policy_for(&self, name: &str) -> Option<PermissionPolicy> {
        self.modes
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .config
            .policy
            .get(name)
            .copied()
    }

    /// 单次工具调用的权限决策。决策顺序：
    /// 1) YOLO 无条件跳过常规权限检查，包括显式 policy 和关键命令检查
    /// 2) 用户 policy（Deny 拒绝、Allow 旁路一切后续检查）
    /// 3) bash 内置关键命令强制 prompt（无会话级与持久选项）
    /// 4) 用户 policy `Prompt` 强制 prompt（无会话级与持久选项）
    /// 5) 生效模式按 tier 与沙盒状态放行或进入 prompt（`lsp` 的 tier 结合动作参数）
    /// 6) 会话级授权与全局持久授权命中放行，否则 prompt（无 handler 时 fail-closed）
    pub fn enforce(
        &self,
        name: &str,
        arguments: &str,
        tier: Tier,
        hook_ask: Option<&str>,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        self.enforce_for(
            self.snapshot(),
            name,
            arguments,
            ToolAccess {
                tier,
                sandboxed: false,
            },
            hook_ask,
            cancellation,
        )
    }

    pub fn enforce_for(
        &self,
        mode: PermissionMode,
        name: &str,
        arguments: &str,
        access: ToolAccess,
        hook_ask: Option<&str>,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        let tier = access.tier;
        let policy = self.policy_for(name);
        if let Some(reason) = hook_ask {
            // hook 的 ask 强制一次确认：YOLO 与已有会话/持久授权都不能跳过，
            // 但非 YOLO 下用户的显式 deny 仍然优先。
            if mode != PermissionMode::Yolo && matches!(policy, Some(PermissionPolicy::Deny)) {
                bail!("{name} is blocked by tools.permission policy.")
            }
            return self.prompt(
                PermissionRequest {
                    tool: name.to_owned(),
                    agent: None,
                    reason: reason.to_owned(),
                    session_grant: false,
                    persistent_grant: false,
                },
                None,
                None,
                cancellation,
            );
        }
        if mode == PermissionMode::Yolo {
            return Ok(());
        }
        match policy {
            Some(PermissionPolicy::Allow) => return Ok(()),
            Some(PermissionPolicy::Deny) => {
                bail!("{name} is blocked by tools.permission policy.")
            }
            _ => {}
        }
        let bash_analysis = (name == "bash")
            .then(|| bash_ast::analyze_bash_arguments(arguments))
            .flatten();
        if let Some(reason) = bash_analysis.as_ref().and_then(|a| a.critical_reason) {
            return self.prompt(
                PermissionRequest {
                    tool: name.to_owned(),
                    agent: None,
                    reason: reason.to_owned(),
                    session_grant: false,
                    persistent_grant: false,
                },
                None,
                None,
                cancellation,
            );
        }
        if matches!(policy, Some(PermissionPolicy::Prompt)) {
            return self.prompt(
                PermissionRequest {
                    tool: name.to_owned(),
                    agent: None,
                    reason: "required by the tools.permission policy for this tool.".to_owned(),
                    session_grant: false,
                    persistent_grant: false,
                },
                None,
                None,
                cancellation,
            );
        }
        if let Some(analysis) = bash_analysis {
            return self.enforce_bash(mode, access, analysis, cancellation);
        }
        let needs_prompt = match (mode, tier) {
            (PermissionMode::Yolo, _) => false,
            (PermissionMode::AcceptEdits, _) if access.sandboxed => false,
            (PermissionMode::AcceptEdits, Tier::Read) => false,
            (PermissionMode::AcceptEdits, Tier::Write | Tier::Exec) => true,
            (PermissionMode::AlwaysAsk, Tier::Read) => false,
            (PermissionMode::AlwaysAsk, Tier::Write | Tier::Exec) => true,
        };
        if !needs_prompt {
            return Ok(());
        }
        if self.is_preapproved(name)
            || self
                .grants
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(name)
        {
            return Ok(());
        }
        let (doc, load_error) = self.load_store();
        if doc
            .as_ref()
            .is_some_and(|doc| doc.tools.iter().any(|tool| tool == name))
        {
            return Ok(());
        }
        let mut reason = match mode {
            PermissionMode::AcceptEdits => {
                "tool runs outside the workspace sandbox and requires permission."
            }
            PermissionMode::AlwaysAsk => "tool requires permission in always-ask mode.",
            PermissionMode::Yolo => unreachable!(),
        }
        .to_owned();
        if let Some(message) = &load_error {
            reason.push_str(&format!("\nSaved permissions unavailable: {message}"));
        }
        let persistent_grant = self.store.is_some() && load_error.is_none();
        self.prompt(
            PermissionRequest {
                tool: name.to_owned(),
                agent: None,
                reason,
                session_grant: true,
                persistent_grant,
            },
            Some(GrantTarget::Tool(name.to_owned())),
            load_error.as_deref(),
            cancellation,
        )
    }

    fn load_store(&self) -> (Option<Arc<rules::Document>>, Option<String>) {
        match self.store.as_ref().map(rules::Store::load).transpose() {
            Ok(doc) => (doc, None),
            Err(error) => {
                let message = pp_core::logging::diagnostic_text(&format!("{error:#}"));
                tracing::warn!(%message, "无法读取持久授权");
                (None, Some(message))
            }
        }
    }

    fn enforce_bash(
        &self,
        mode: PermissionMode,
        access: ToolAccess,
        analysis: bash_ast::ProgramAnalysis,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        if let Some(reason) = analysis.critical_reason {
            return self.prompt(
                PermissionRequest {
                    tool: "bash".to_owned(),
                    agent: None,
                    reason: reason.to_owned(),
                    session_grant: false,
                    persistent_grant: false,
                },
                None,
                None,
                cancellation,
            );
        }

        let (doc, load_error) = self.load_store();
        let saved_bash_rules = doc.as_ref().map_or(&[][..], |d| d.bash.as_slice());

        let mut forced_prompt_prefixes = Vec::new();
        for leaf in &analysis.leaves {
            if let Some(rule) = matching_bash_rule(leaf, saved_bash_rules) {
                match rule.policy {
                    PermissionPolicy::Deny => {
                        bail!(
                            "bash command is blocked by saved permission rule: {}",
                            rule.display_prefix()
                        );
                    }
                    PermissionPolicy::Prompt => {
                        let display = rule.display_prefix();
                        if !forced_prompt_prefixes.contains(&display) {
                            forced_prompt_prefixes.push(display);
                        }
                    }
                    PermissionPolicy::Allow => {}
                }
            }
        }
        if !forced_prompt_prefixes.is_empty() {
            return self.prompt(
                PermissionRequest {
                    tool: "bash".to_owned(),
                    agent: None,
                    reason: format!(
                        "Command matched a prompt permission rule ({}).",
                        forced_prompt_prefixes.join("; ")
                    ),
                    session_grant: false,
                    persistent_grant: false,
                },
                None,
                load_error.as_deref(),
                cancellation,
            );
        }

        let tool_granted = self.is_preapproved("bash")
            || self
                .grants
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains("bash")
            || doc
                .as_ref()
                .is_some_and(|d| d.tools.iter().any(|tool| tool == "bash"));

        let session_bash = self
            .bash_grants
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();

        let is_leaf_approved = |leaf: &bash_ast::LeafCommand| -> bool {
            if tool_granted {
                return true;
            }
            if !analysis.has_write_redirection && self.matches_preapproved_bash_leaf(leaf) {
                return true;
            }
            if matching_bash_rule(leaf, saved_bash_rules)
                .is_some_and(|rule| rule.policy == PermissionPolicy::Allow)
            {
                return true;
            }
            session_bash
                .iter()
                .any(|prefix| leaf.matches_prefix(prefix))
        };

        let ask_all = mode == PermissionMode::AlwaysAsk || !access.sandboxed;
        let mut unapproved_high_impact = Vec::new();
        let mut unapproved_regular = Vec::new();
        for leaf in &analysis.leaves {
            if is_leaf_approved(leaf) {
                continue;
            }
            if leaf.is_high_impact() {
                unapproved_high_impact.push(leaf);
            } else if ask_all
                && !(leaf.is_safe_read_only()
                    && !analysis.has_write_redirection
                    && !analysis.unanalyzable)
            {
                unapproved_regular.push(leaf);
            }
        }

        let needs_unanalyzable_prompt = ask_all && analysis.unanalyzable && !tool_granted;
        if unapproved_high_impact.is_empty()
            && unapproved_regular.is_empty()
            && !needs_unanalyzable_prompt
        {
            return Ok(());
        }

        let all_unapproved = unapproved_high_impact
            .iter()
            .chain(unapproved_regular.iter())
            .copied()
            .collect::<Vec<_>>();
        let suggested = if analysis.unanalyzable {
            None
        } else {
            bash_ast::ProgramAnalysis::suggested_prefixes(all_unapproved)
        };

        let mut reason = if !unapproved_high_impact.is_empty() {
            "High-impact shell command requires permission.".to_owned()
        } else if analysis.unanalyzable {
            "Dynamic or unanalyzable shell command requires one-time approval.".to_owned()
        } else if !access.sandboxed && mode == PermissionMode::AcceptEdits {
            "bash runs outside the workspace sandbox and requires permission.".to_owned()
        } else {
            "bash requires permission in always-ask mode.".to_owned()
        };

        if let Some(prefixes) = &suggested {
            let formatted = prefixes
                .iter()
                .map(|p| pp_core::text::display_text(&p.join(" ")).into_owned())
                .collect::<Vec<_>>()
                .join("; ");
            let label = if prefixes.len() == 1 {
                "Allow rule prefix"
            } else {
                "Allow rule prefixes"
            };
            reason.push_str(&format!("\n{label}: {formatted}"));
        }
        if let Some(message) = &load_error {
            reason.push_str(&format!("\nSaved permissions unavailable: {message}"));
        }

        let session_grant = suggested.is_some();
        let persistent_grant = session_grant && self.store.is_some() && load_error.is_none();
        self.prompt(
            PermissionRequest {
                tool: "bash".to_owned(),
                agent: None,
                reason,
                session_grant,
                persistent_grant,
            },
            suggested.map(GrantTarget::BashPrefixes),
            load_error.as_deref(),
            cancellation,
        )
    }

    fn prompt(
        &self,
        request: PermissionRequest,
        target: Option<GrantTarget>,
        load_error: Option<&str>,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        let Some(handler) = self.handler.as_ref() else {
            if let Some(message) = load_error {
                bail!(
                    "{} requires permission and no interactive UI is available. {message}",
                    request.tool
                );
            }
            bail!(
                "{} requires permission and no interactive UI is available; run in an interactive TUI or adjust tools.permissionMode.",
                request.tool
            );
        };
        let name = request.tool.clone();
        let allow_session_grant = request.session_grant;
        let allow_persistent_grant = request.persistent_grant;
        let decision = handler(request, cancellation)?;
        pp_core::cancellation::check(cancellation)?;
        match decision {
            PermissionDecision::Approve => Ok(()),
            PermissionDecision::ApproveSession => {
                if allow_session_grant {
                    match target {
                        Some(GrantTarget::Tool(tool_name)) => {
                            self.grants
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .insert(tool_name);
                        }
                        Some(GrantTarget::BashPrefixes(prefixes)) => {
                            let mut bash_grants =
                                self.bash_grants.lock().unwrap_or_else(|e| e.into_inner());
                            for prefix in prefixes {
                                bash_grants.insert(prefix);
                            }
                        }
                        None => {}
                    }
                }
                Ok(())
            }
            PermissionDecision::ApprovePersistently if allow_persistent_grant => {
                let store = self.store.as_ref().expect("持久授权选项要求有规则存储");
                match target {
                    Some(GrantTarget::Tool(tool_name)) => store.save_tool(&tool_name)?,
                    Some(GrantTarget::BashPrefixes(prefixes)) => {
                        let rules = prefixes
                            .into_iter()
                            .map(|prefix| BashRule {
                                prefix,
                                policy: PermissionPolicy::Allow,
                            })
                            .collect();
                        store.save_bash_batch(rules)?;
                    }
                    None => bail!("{name} was denied by the user."),
                }
                pp_core::cancellation::check(cancellation)?;
                Ok(())
            }
            PermissionDecision::ApprovePersistently => {
                bail!("{name} was denied by the user.")
            }
            PermissionDecision::Deny => bail!("{name} was denied by the user."),
        }
    }
}

#[derive(Clone, Debug)]
enum GrantTarget {
    Tool(String),
    BashPrefixes(Vec<Vec<String>>),
}

fn policy_severity(policy: PermissionPolicy) -> u8 {
    match policy {
        PermissionPolicy::Allow => 0,
        PermissionPolicy::Prompt => 1,
        PermissionPolicy::Deny => 2,
    }
}

fn matching_bash_rule<'a>(
    leaf: &bash_ast::LeafCommand,
    rules: &'a [BashRule],
) -> Option<&'a BashRule> {
    rules
        .iter()
        .filter(|rule| match rule.policy {
            PermissionPolicy::Allow => leaf.matches_prefix(&rule.prefix),
            PermissionPolicy::Prompt | PermissionPolicy::Deny => {
                leaf.matches_restrictive_prefix(&rule.prefix)
            }
        })
        .max_by_key(|rule| (rule.prefix.len(), policy_severity(rule.policy)))
}

#[cfg(test)]
fn destructive_reason(arguments: &str) -> Option<&'static str> {
    bash_ast::analyze_bash_arguments(arguments).and_then(|analysis| analysis.critical_reason)
}

#[cfg(test)]
mod bash_ast_tests;
#[cfg(test)]
mod tests;
