//! 生效权限、工具审批与用户授权。
//!
//! 交互请求经 `ApprovalHandler` 交给调用方，
//! 无 handler 时拒绝调用。工具白名单先于审批检查；Execute 工具与
//! Plan 中未标为只读的 MCP 工具经过本模块。

use pp_config::config::{ApprovalMode, ApprovalPolicy, PermissionMode, SandboxMode};
use std::path::{Path, PathBuf};

mod rules;
pub use rules::SandboxGrant;

use anyhow::{Result, bail};
use std::{
    collections::{BTreeMap, HashSet},
    sync::{Arc, Mutex},
};

use pp_core::cancellation::CancellationToken;

pub use pp_workspace::Tier;

/// settings.yml `tools:` 小节的运行时形态；构造在审批门内部完成。
#[derive(Clone, Debug, Default)]
struct ApprovalConfig {
    mode: ApprovalMode,
    policy: BTreeMap<String, ApprovalPolicy>,
}

impl From<pp_config::config::ToolsSettings> for ApprovalConfig {
    fn from(settings: pp_config::config::ToolsSettings) -> Self {
        Self {
            mode: settings.approval_mode,
            policy: settings.approval,
        }
    }
}

/// 审批请求：只携带工具名与脱敏 reason，不携带参数原文；
/// 参数详情在 TUI 中经既有 ToolStarted / Ctrl+O 查看。
#[derive(Clone, Debug)]
pub struct ApprovalRequest {
    pub tool: String,
    pub agent: Option<String>,
    pub reason: String,
    /// 仅当本条 prompt 允许"本会话内放行"时为 true；强制 prompt
    /// （关键命令、用户 `prompt` policy）每次都问，不提供该选项。
    pub session_grant: bool,
    pub persistent_grant: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalDecision {
    Approve,
    ApproveSession,
    ApprovePersistently,
    Deny,
}

pub type ApprovalCallback =
    dyn Fn(ApprovalRequest, &CancellationToken) -> Result<ApprovalDecision> + Send + Sync;
pub type ApprovalHandler = Box<ApprovalCallback>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EffectivePermissions {
    Legacy(ApprovalMode),
    Preset(PermissionMode),
}

impl EffectivePermissions {
    pub fn preset(self) -> Option<PermissionMode> {
        match self {
            Self::Legacy(_) => None,
            Self::Preset(mode) => Some(mode),
        }
    }

    pub fn sandbox_mode(self) -> Option<SandboxMode> {
        match self {
            Self::Legacy(_) => None,
            Self::Preset(PermissionMode::Yolo) => Some(SandboxMode::Off),
            Self::Preset(_) => Some(SandboxMode::WorkspaceWrite),
        }
    }

    pub fn approval_mode(self) -> ApprovalMode {
        match self {
            Self::Legacy(mode) => mode,
            Self::Preset(PermissionMode::AlwaysAsk) => ApprovalMode::AlwaysAsk,
            Self::Preset(PermissionMode::AcceptEdits) => ApprovalMode::Write,
            Self::Preset(PermissionMode::Yolo) => ApprovalMode::Yolo,
        }
    }
}

pub struct ToolAccess {
    pub tier: Tier,
    pub sandboxed: bool,
}

#[derive(Default)]
struct ModeState {
    configured: Option<PermissionMode>,
    startup: Option<PermissionMode>,
    force_legacy: bool,
    selected: Option<PermissionMode>,
}

impl ModeState {
    fn current(&self) -> Option<PermissionMode> {
        self.selected
            .or(self.startup)
            .or_else(|| self.configured.filter(|_| !self.force_legacy))
    }
}

#[derive(Clone)]
pub struct Gate {
    config: ApprovalConfig,
    /// `--approval-mode` flag 覆盖；`None` 时使用配置值。
    override_mode: Option<ApprovalMode>,
    /// 常规工具的会话级授权。
    grants: Arc<Mutex<HashSet<String>>>,
    handler: Option<Arc<ApprovalCallback>>,
    modes: Arc<Mutex<ModeState>>,
    sandbox_grants: Arc<Mutex<HashSet<SandboxGrant>>>,
    store: Option<rules::Store>,
    workspace: Option<PathBuf>,
}

impl Default for Gate {
    fn default() -> Self {
        Self::new()
    }
}

impl Gate {
    pub fn new() -> Self {
        Self {
            config: ApprovalConfig::default(),
            override_mode: None,
            grants: Default::default(),
            handler: None,
            modes: Default::default(),
            sandbox_grants: Default::default(),
            store: None,
            workspace: None,
        }
    }

    pub fn for_process(
        agent_dir: &Path,
        workspace: &Path,
        startup: Option<PermissionMode>,
        legacy_approval: Option<ApprovalMode>,
        legacy_sandbox_override: bool,
    ) -> Self {
        Self {
            modes: Arc::new(Mutex::new(ModeState {
                startup,
                force_legacy: legacy_approval.is_some() || legacy_sandbox_override,
                ..Default::default()
            })),
            override_mode: legacy_approval,
            store: Some(rules::Store::new(agent_dir)),
            workspace: Some(
                workspace
                    .canonicalize()
                    .unwrap_or_else(|_| workspace.to_owned()),
            ),
            ..Self::new()
        }
    }

    pub fn workspace<'a>(&'a self, cwd: &'a Path) -> &'a Path {
        self.workspace.as_deref().unwrap_or(cwd)
    }

    pub fn protects_path(&self, path: &Path) -> bool {
        self.store
            .as_ref()
            .is_some_and(|store| store.protects_path(path))
    }

    pub fn snapshot(&self) -> EffectivePermissions {
        let state = self.modes.lock().unwrap_or_else(|error| error.into_inner());
        state.current().map_or(
            EffectivePermissions::Legacy(self.override_mode.unwrap_or(self.config.mode)),
            EffectivePermissions::Preset,
        )
    }

    pub fn select(&self, requested: Option<PermissionMode>) {
        let mut state = self.modes.lock().unwrap_or_else(|error| error.into_inner());
        let current = state.current();
        let selected = requested
            .unwrap_or_else(|| current.map_or(PermissionMode::AcceptEdits, PermissionMode::next));
        state.selected = Some(selected);
        if current != Some(selected) {
            self.grants
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clear();
        }
    }

    pub fn clear_session(&self) {
        if self.snapshot().preset().is_some() {
            self.grants
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clear();
        }
        self.sandbox_grants
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
    }

    pub fn list_rules(&self) -> Result<String> {
        self.store.as_ref().map_or_else(
            || Ok("No saved permission rules.".into()),
            rules::Store::list,
        )
    }

    pub fn revoke_rule(&self, id: &str) -> Result<()> {
        let Some(store) = &self.store else {
            bail!("permission rule store unavailable");
        };
        store.revoke(id)
    }

    pub fn authorize_sandbox(
        &self,
        grant: SandboxGrant,
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
            Ok(Some(rules)) if rules.iter().any(|rule| rule.grant == grant) => return Ok(()),
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
                    "Bash requires sandbox approval and no interactive UI is available. {message}"
                );
            }
            bail!("Bash requires sandbox approval and no interactive UI is available.");
        };
        let mut reason = grant.summary();
        if let Some(message) = &load_error {
            reason.push_str(&format!("\nSaved permissions unavailable: {message}"));
        }
        let persistent_grant = self.store.is_some() && load_error.is_none();
        let decision = handler(
            ApprovalRequest {
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
            ApprovalDecision::Approve => Ok(()),
            ApprovalDecision::ApproveSession => {
                self.sandbox_grants
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .insert(grant);
                Ok(())
            }
            ApprovalDecision::ApprovePersistently if persistent_grant => {
                self.store
                    .as_ref()
                    .expect("批准选项要求有规则存储")
                    .save(grant)?;
                pp_core::cancellation::check(cancellation)?;
                Ok(())
            }
            ApprovalDecision::ApprovePersistently => bail!("persistent permission is unavailable"),
            ApprovalDecision::Deny => bail!("Bash sandbox permission was denied by the user."),
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
    pub fn set_config(&mut self, settings: pp_config::config::ToolsSettings) {
        self.modes
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .configured = settings.permission_mode;
        self.config = ApprovalConfig::from(settings);
    }

    pub fn set_override(&mut self, mode: ApprovalMode) {
        tracing::debug!(mode = mode.as_str(), "设置工具审批模式");
        self.override_mode = Some(mode);
    }

    pub fn set_handler(&mut self, handler: ApprovalHandler) {
        self.handler = Some(Arc::from(handler));
    }

    /// 生效审批档位（flag 覆盖优先）；只读查询，不另存权限状态。
    pub fn effective_mode(&self) -> ApprovalMode {
        self.snapshot().approval_mode()
    }

    /// 单次工具调用的审批决策。决策顺序：
    /// 1) YOLO 无条件跳过审批，包括显式 policy 和关键命令检查
    /// 2) 用户 policy（Deny 拒绝、Allow 旁路一切后续检查）
    /// 3) bash 内置关键命令强制 prompt（无会话级选项）
    /// 4) 用户 policy `Prompt` 强制 prompt（无会话级选项）
    /// 5) 生效模式按 tier 放行或进入 prompt（`lsp` 的 tier 结合动作参数）
    /// 6) 会话级授权命中放行，否则 prompt（无 handler 时 fail-closed）
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
        permissions: EffectivePermissions,
        name: &str,
        arguments: &str,
        access: ToolAccess,
        hook_ask: Option<&str>,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        let mode = permissions.approval_mode();
        let tier = access.tier;
        if let Some(reason) = hook_ask {
            // hook 的 ask 强制一次审批：YOLO 与已有会话授权都不能跳过，
            // 但非 YOLO 下用户的显式 deny 仍然优先。
            if mode != ApprovalMode::Yolo
                && matches!(self.config.policy.get(name), Some(ApprovalPolicy::Deny))
            {
                bail!("{name} is blocked by tools.approval policy.")
            }
            return self.prompt(
                ApprovalRequest {
                    tool: name.to_owned(),
                    agent: None,
                    reason: reason.to_owned(),
                    session_grant: false,
                    persistent_grant: false,
                },
                cancellation,
            );
        }
        if mode == ApprovalMode::Yolo {
            return Ok(());
        }
        match self.config.policy.get(name) {
            Some(ApprovalPolicy::Allow) => return Ok(()),
            Some(ApprovalPolicy::Deny) => {
                bail!("{name} is blocked by tools.approval policy.")
            }
            _ => {}
        }
        if name == "bash"
            && let Some(reason) = destructive_reason(arguments)
        {
            return self.prompt(
                ApprovalRequest {
                    tool: name.to_owned(),
                    agent: None,
                    reason: reason.to_owned(),
                    session_grant: false,
                    persistent_grant: false,
                },
                cancellation,
            );
        }
        if matches!(self.config.policy.get(name), Some(ApprovalPolicy::Prompt)) {
            return self.prompt(
                ApprovalRequest {
                    tool: name.to_owned(),
                    agent: None,
                    reason: "required by the tools.approval policy for this tool.".to_owned(),
                    session_grant: false,
                    persistent_grant: false,
                },
                cancellation,
            );
        }
        let needs_prompt = match (permissions, tier) {
            (EffectivePermissions::Preset(PermissionMode::AcceptEdits), _) if access.sandboxed => {
                false
            }
            (
                EffectivePermissions::Preset(PermissionMode::AcceptEdits),
                Tier::Write | Tier::Exec,
            ) => true,
            _ => match (mode, tier) {
                (ApprovalMode::Yolo, _) => false,
                (ApprovalMode::Write, Tier::Read | Tier::Write) => false,
                (ApprovalMode::AlwaysAsk, Tier::Read) => false,
                (ApprovalMode::Write, Tier::Exec) => true,
                (ApprovalMode::AlwaysAsk, Tier::Write | Tier::Exec) => true,
            },
        };
        if !needs_prompt {
            return Ok(());
        }
        if self
            .grants
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(name)
        {
            return Ok(());
        }
        let reason = match permissions {
            EffectivePermissions::Preset(PermissionMode::AcceptEdits) => {
                "tool runs outside the workspace sandbox and requires approval."
            }
            EffectivePermissions::Legacy(ApprovalMode::Write) => {
                "exec-tier tool requires approval in write mode."
            }
            _ => "tool requires approval in always-ask mode.",
        }
        .to_owned();
        self.prompt(
            ApprovalRequest {
                tool: name.to_owned(),
                agent: None,
                reason,
                session_grant: true,
                persistent_grant: false,
            },
            cancellation,
        )
    }

    fn prompt(&self, request: ApprovalRequest, cancellation: &CancellationToken) -> Result<()> {
        let Some(handler) = self.handler.as_ref() else {
            bail!(
                "{} requires approval and no interactive UI is available; run in an interactive TUI or adjust tools.approvalMode.",
                request.tool
            );
        };
        let name = request.tool.clone();
        let allow_session_grant = request.session_grant;
        let decision = handler(request, cancellation)?;
        pp_core::cancellation::check(cancellation)?;
        match decision {
            ApprovalDecision::Approve => Ok(()),
            ApprovalDecision::ApproveSession => {
                if allow_session_grant {
                    self.grants
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(name);
                }
                Ok(())
            }
            ApprovalDecision::Deny | ApprovalDecision::ApprovePersistently => {
                bail!("{name} was denied by the user.")
            }
        }
    }
}

// ---- bash 内置关键命令检查 ----
// 匹配前对命令做 trim + 连续空白折叠；reason 脱敏，不回显命令原文。
// 对齐 OMP 关键清单的最小子集；只对非 YOLO 模式的 `bash` 工具生效。

type DestructiveMatcher = fn(&str) -> bool;

const DESTRUCTIVE_PATTERNS: &[(&str, DestructiveMatcher)] = &[
    (
        "recursive force delete targeting a root path",
        is_recursive_rm_root,
    ),
    ("fork bomb pattern", is_fork_bomb),
    ("remote script piped into a shell", is_piped_remote_script),
    ("write to system authentication files", is_auth_file_write),
    (
        "system shutdown, reboot, halt or poweroff",
        is_power_command,
    ),
    (
        "disk destruction via mkfs or raw-device dd",
        is_disk_destruction,
    ),
];

/// 宽松解析 bash 参数中的 `command` 字符串；解析失败返回 `None`
///（后续正式参数解析会报错），不在此处重复校验。
fn destructive_reason(arguments: &str) -> Option<&'static str> {
    let value: serde_json::Value = serde_json::from_str(arguments).ok()?;
    let command = value.get("command")?.as_str()?;
    let normalized = command.split_whitespace().collect::<Vec<_>>().join(" ");
    DESTRUCTIVE_PATTERNS
        .iter()
        .find(|(_, matches)| matches(&normalized))
        .map(|(reason, _)| *reason)
}

fn tokens(command: &str) -> impl Iterator<Item = &str> {
    command.split_whitespace()
}

fn is_recursive_rm_root(command: &str) -> bool {
    let tokens: Vec<&str> = tokens(command).collect();
    let Some(position) = tokens.iter().position(|token| {
        let base = token.rsplit('/').next().unwrap_or(token);
        base == "rm"
    }) else {
        return false;
    };
    let args = &tokens[position + 1..];
    let recursive = args.iter().any(|token| {
        token.starts_with('-') && (token[1..].contains(['r', 'R']) || *token == "--recursive")
    });
    recursive
        && args.iter().any(|token| {
            !token.starts_with('-') && (token.starts_with('/') || token.starts_with('~'))
        })
}

fn is_fork_bomb(command: &str) -> bool {
    let compact: String = command.chars().filter(|c| !c.is_whitespace()).collect();
    compact.contains(":(){:|:&};:")
}

fn is_piped_remote_script(command: &str) -> bool {
    let mut fetch = false;
    for segment in command.split('|') {
        let segment_tokens: Vec<&str> = tokens(segment).collect();
        if segment_tokens
            .iter()
            .any(|token| *token == "curl" || *token == "wget")
        {
            fetch = true;
            continue;
        }
        if fetch {
            let stripped = segment_tokens
                .iter()
                .copied()
                .find(|token| *token != "sudo");
            if matches!(stripped, Some("sh" | "bash" | "/bin/sh" | "/bin/bash")) {
                return true;
            }
        }
    }
    false
}

fn is_auth_file_write(command: &str) -> bool {
    const AUTH_FILES: &[&str] = &["/etc/passwd", "/etc/shadow", "/etc/sudoers"];
    AUTH_FILES.iter().any(|path| command.contains(path))
        && (command.contains('>') || tokens(command).any(|token| token == "tee"))
}

fn is_power_command(command: &str) -> bool {
    tokens(command).any(|token| {
        let base = token.rsplit('/').next().unwrap_or(token);
        matches!(base, "shutdown" | "reboot" | "halt" | "poweroff")
    })
}

fn is_disk_destruction(command: &str) -> bool {
    let tokens: Vec<&str> = tokens(command).collect();
    tokens.iter().any(|token| token.starts_with("mkfs"))
        || (tokens.iter().any(|token| {
            let base = token.rsplit('/').next().unwrap_or(token);
            base == "dd"
        }) && tokens.iter().any(|token| token.starts_with("of=/dev/")))
}

#[cfg(test)]
mod tests;
