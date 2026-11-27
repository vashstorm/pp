//! 生效权限、工具门禁、自动审阅回调与用户授权。
//!
//! 交互请求经 `PermissionHandler` 交给调用方，
//! 无 handler 时拒绝调用。工具白名单先于权限检查；Execute 工具与
//! Plan 中未标为只读的 MCP 工具经过本模块。

use pp_config::config::{PermissionMode, PermissionPolicy, SandboxMode};
use pp_sandbox::SandboxOverlay;
use std::path::{Path, PathBuf};

mod auto_review;
mod bash_ast;
mod enforce;
mod preapproved;
mod rules;

pub use auto_review::{
    AutoReviewCallback, AutoReviewDecision, AutoReviewRequest, AutoReviewVerdict,
};
pub use enforce::{ApprovedBashScope, PathWriteKind};
use preapproved::PreapprovedTools;
pub use preapproved::{BashCommandPattern, check_bash_command_rules};
pub use rules::{BashRule, FsAccess, MatchMode, SandboxGrant};

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
    pub network_enabled: bool,
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
            PermissionMode::DangerFullAccess => SandboxMode::Off,
            PermissionMode::WorkspaceWrite | PermissionMode::AutoReview => {
                SandboxMode::WorkspaceWrite
            }
            PermissionMode::ReadOnly => SandboxMode::ReadOnly,
        })
    }
}

#[derive(Clone)]
pub struct Gate {
    default_mode: PermissionMode,
    /// 常规工具的会话级授权。
    grants: Arc<Mutex<HashSet<String>>>,
    /// Bash 命令规则的会话级授权（精确到参数/文件）。
    bash_grants: Arc<Mutex<HashSet<BashRule>>>,
    /// 具体文件路径的会话级写授权（用于 .git/.agents/工作区外/read-only）。
    path_grants: Arc<Mutex<HashSet<PathBuf>>>,
    /// 工作区外文件/目录和受保护会话路径的会话级只读授权。
    read_path_grants: Arc<Mutex<HashSet<PathBuf>>>,
    /// 会话级互联网访问授权。
    network_grant: Arc<Mutex<bool>>,
    /// CLI `--allowed-tools` 赋予本次运行的工具预批准集合；不随会话重置清空。
    preapproved: Arc<Mutex<PreapprovedTools>>,
    handler: Option<Arc<PermissionCallback>>,
    auto_review: Arc<Mutex<Option<Arc<AutoReviewCallback>>>>,
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
        let default_mode = PermissionMode::DangerFullAccess;
        Self {
            default_mode,
            grants: Default::default(),
            bash_grants: Default::default(),
            path_grants: Default::default(),
            read_path_grants: Default::default(),
            network_grant: Default::default(),
            preapproved: Default::default(),
            handler: None,
            auto_review: Default::default(),
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
        _sandbox_override: bool,
    ) -> Self {
        let default_mode = PermissionMode::WorkspaceWrite;
        Self {
            default_mode,
            modes: Arc::new(Mutex::new(ModeState {
                config: PermissionConfig::default(),
                configured: default_mode,
                startup,
                selected: None,
                manage_sandbox: startup.is_some(),
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

    /// 导出当前 `permission.toml` 激活 profile 的文件系统与网络叠加配置供沙盒同步。
    pub fn sandbox_overlay(&self, cwd: &Path) -> SandboxOverlay {
        let Some(store) = &self.store else {
            return SandboxOverlay::default();
        };
        let home = std::env::var_os("HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from);
        store
            .load()
            .ok()
            .and_then(|doc| doc.sandbox_overlay(cwd, home.as_deref()).ok())
            .unwrap_or_default()
    }

    pub fn select(&self, requested: Option<PermissionMode>) {
        let mut state = self.modes.lock().unwrap_or_else(|error| error.into_inner());
        let current = state.current();
        let selected = requested.unwrap_or_else(|| current.next());
        state.selected = Some(selected);
        state.manage_sandbox = true;
        if current != selected {
            self.clear_session();
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
        self.path_grants
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
        self.read_path_grants
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
        *self
            .network_grant
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = false;
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

    pub(crate) fn is_preapproved(&self, name: &str) -> bool {
        self.preapproved
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .contains(name)
    }

    pub(crate) fn matches_preapproved_bash_leaf(&self, leaf: &bash_ast::LeafCommand) -> bool {
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
        let mut reason = grant.summary(cwd);
        if let Some(message) = &load_error {
            reason.push_str(&format!("\nSaved permissions unavailable: {message}"));
        }
        let persistent_grant = self.store.is_some() && load_error.is_none();
        let mode = self.snapshot();
        if mode == PermissionMode::AutoReview
            && let Some(reviewer) = self
                .auto_review
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        {
            let review_req = AutoReviewRequest {
                tool: "bash".into(),
                agent: None,
                mode,
                cwd: cwd
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| PathBuf::from("/")),
                trigger_reason: grant.summary(cwd),
                action_summary: pp_core::logging::diagnostic_text(&grant.command),
                requested_writable: grant.writable.clone(),
                requested_protected: grant.allow_protected.clone(),
                requested_network: grant.network,
            };
            match reviewer(review_req, cancellation) {
                Ok(verdict) if verdict.decision == AutoReviewDecision::Pass => return Ok(()),
                Ok(verdict) => {
                    reason = format!(
                        "Reviewer flagged as potentially unsafe: {}\n{reason}",
                        pp_core::text::display_text(&verdict.reason)
                    );
                }
                Err(error) => {
                    if error
                        .chain()
                        .any(|cause| cause.is::<pp_core::cancellation::Cancelled>())
                    {
                        return Err(error);
                    }
                    let msg = pp_core::logging::diagnostic_text(&format!("{error:#}"));
                    reason = format!(
                        "Auto-review unavailable ({msg}); manual approval required.\n{reason}"
                    );
                }
            }
        }

        let Some(handler) = &self.handler else {
            if let Some(message) = load_error {
                bail!(
                    "Bash requires sandbox permission and no interactive UI is available. {message}"
                );
            }
            bail!("Bash requires sandbox permission and no interactive UI is available.");
        };
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
        state.configured = self.default_mode;
        state.manage_sandbox = state.selected.is_some() || state.startup.is_some();
        state.config = PermissionConfig::from(settings);
    }

    pub fn set_handler(&mut self, handler: PermissionHandler) {
        self.handler = Some(Arc::from(handler));
    }

    pub fn set_auto_review_handler(&self, handler: Arc<AutoReviewCallback>) {
        *self
            .auto_review
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(handler);
    }

    /// 生效权限档位；只读查询，不另存状态。
    pub fn effective_mode(&self) -> PermissionMode {
        self.snapshot()
    }

    pub(crate) fn policy_for(&self, name: &str) -> Option<PermissionPolicy> {
        self.modes
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .config
            .policy
            .get(name)
            .copied()
    }

    pub(crate) fn load_store(&self) -> (Option<Arc<rules::Document>>, Option<String>) {
        match self.store.as_ref().map(rules::Store::load).transpose() {
            Ok(doc) => (doc, None),
            Err(error) => {
                let message = pp_core::logging::diagnostic_text(&format!("{error:#}"));
                tracing::warn!(%message, "无法读取持久授权");
                (None, Some(message))
            }
        }
    }

    pub(crate) fn prompt_user(
        &self,
        request: PermissionRequest,
        target: Option<GrantTarget>,
        load_error: Option<&str>,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        let Some(handler) = self.handler.as_ref() else {
            let reason_note = request
                .reason
                .lines()
                .next()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(|line| format!(" ({line})"))
                .unwrap_or_default();
            if let Some(message) = load_error {
                bail!(
                    "{} requires permission and no interactive UI is available{reason_note}. {message}",
                    request.tool
                );
            }
            bail!(
                "{} requires permission and no interactive UI is available{reason_note}; run in an interactive TUI or use --permission-mode / --yolo.",
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
                        Some(GrantTarget::BashRules(rules)) => {
                            let mut bash_grants =
                                self.bash_grants.lock().unwrap_or_else(|e| e.into_inner());
                            for rule in rules {
                                bash_grants.insert(rule);
                            }
                        }
                        Some(GrantTarget::Sandbox(grant)) => {
                            self.sandbox_grants
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .insert(grant);
                        }
                        Some(GrantTarget::BashAndSandbox { rules, sandbox }) => {
                            let mut bash_grants =
                                self.bash_grants.lock().unwrap_or_else(|e| e.into_inner());
                            for rule in rules {
                                bash_grants.insert(rule);
                            }
                            drop(bash_grants);
                            self.sandbox_grants
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .insert(sandbox);
                        }
                        Some(GrantTarget::WorkspacePath { abs, .. })
                        | Some(GrantTarget::HostPath(abs)) => {
                            self.path_grants
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .insert(abs);
                        }
                        Some(GrantTarget::HostReadPaths(paths)) => {
                            let mut read_grants = self
                                .read_path_grants
                                .lock()
                                .unwrap_or_else(|e| e.into_inner());
                            for path in paths {
                                read_grants.insert(path);
                            }
                        }
                        Some(GrantTarget::BashAndOutsideReads {
                            rules,
                            sandbox,
                            read_paths,
                        }) => {
                            if let Some(rules) = rules {
                                let mut bash_grants =
                                    self.bash_grants.lock().unwrap_or_else(|e| e.into_inner());
                                for rule in rules {
                                    bash_grants.insert(rule);
                                }
                            }
                            if let Some(sandbox) = sandbox {
                                self.sandbox_grants
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .insert(sandbox);
                            }
                            let mut read_grants = self
                                .read_path_grants
                                .lock()
                                .unwrap_or_else(|e| e.into_inner());
                            for path in read_paths {
                                read_grants.insert(path);
                            }
                        }
                        Some(GrantTarget::Network) => {
                            *self.network_grant.lock().unwrap_or_else(|e| e.into_inner()) = true;
                        }
                        None => {}
                    }
                }
                Ok(())
            }
            PermissionDecision::ApprovePersistently if allow_persistent_grant => {
                let store = self.store.as_ref().expect("持久授权选项要求有规则存储");
                match target {
                    Some(GrantTarget::Tool(tool_name)) => {
                        store.save_tool(&tool_name, PermissionPolicy::Allow)?
                    }
                    Some(GrantTarget::BashRules(rules)) => store.save_bash_batch(rules)?,
                    Some(GrantTarget::Sandbox(grant)) => store.save(grant)?,
                    Some(GrantTarget::BashAndSandbox { rules, sandbox }) => {
                        store.save_bash_batch(rules)?;
                        store.save(sandbox)?;
                    }
                    Some(GrantTarget::WorkspacePath { rel, .. }) => {
                        store.save_workspace_fs(&rel, FsAccess::Write)?
                    }
                    Some(GrantTarget::HostPath(abs)) => {
                        let raw = abs
                            .to_str()
                            .ok_or_else(|| anyhow::anyhow!("non-UTF-8 path cannot be saved"))?;
                        store.save_host_fs(raw, FsAccess::Write)?;
                    }
                    Some(GrantTarget::HostReadPaths(paths)) => {
                        store.save_host_reads(&paths)?;
                    }
                    Some(GrantTarget::BashAndOutsideReads {
                        rules,
                        sandbox,
                        read_paths,
                    }) => {
                        if let Some(rules) = rules {
                            store.save_bash_batch(rules)?;
                        }
                        if let Some(sandbox) = sandbox {
                            store.save(sandbox)?;
                        }
                        for abs in read_paths {
                            let raw = abs
                                .to_str()
                                .ok_or_else(|| anyhow::anyhow!("non-UTF-8 path cannot be saved"))?;
                            store.save_host_fs(raw, FsAccess::Read)?;
                        }
                    }
                    Some(GrantTarget::Network) => store.save_network_enabled(true)?,
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
pub(crate) enum GrantTarget {
    Tool(String),
    BashRules(Vec<BashRule>),
    Sandbox(SandboxGrant),
    BashAndSandbox {
        rules: Vec<BashRule>,
        sandbox: SandboxGrant,
    },
    WorkspacePath {
        abs: PathBuf,
        rel: String,
    },
    HostPath(PathBuf),
    HostReadPaths(Vec<PathBuf>),
    BashAndOutsideReads {
        rules: Option<Vec<BashRule>>,
        sandbox: Option<SandboxGrant>,
        read_paths: Vec<PathBuf>,
    },
    Network,
}

#[cfg(test)]
fn destructive_reason(arguments: &str) -> Option<&'static str> {
    bash_ast::analyze_bash_arguments(arguments)?.critical_reason
}

#[cfg(test)]
#[path = "bash_ast_tests.rs"]
mod bash_ast_tests;
#[cfg(test)]
#[path = "tests.rs"]
mod tests;
