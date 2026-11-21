//! 生效权限、工具门禁与用户授权。
//!
//! 交互请求经 `PermissionHandler` 交给调用方，
//! 无 handler 时拒绝调用。工具白名单先于权限检查；Execute 工具与
//! Plan 中未标为只读的 MCP 工具经过本模块。

use pp_config::config::{PermissionMode, PermissionPolicy, SandboxMode};
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
    /// MCP 工具所属服务的身份指纹；服务定义变化后不复用旧授权。
    pub server_identity: Option<String>,
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
    /// 常规工具的会话级授权（MCP 工具含服务身份指纹）。
    grants: Arc<Mutex<HashSet<rules::ToolGrant>>>,
    handler: Option<Arc<PermissionCallback>>,
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
        let default_mode = PermissionMode::Yolo;
        Self {
            default_mode,
            is_process: false,
            sandbox_override: false,
            grants: Default::default(),
            handler: None,
            modes: Arc::new(Mutex::new(ModeState {
                configured: default_mode,
                ..Default::default()
            })),
            sandbox_grants: Default::default(),
            store: None,
            workspace: None,
        }
    }

    pub fn for_process(
        agent_dir: &Path,
        workspace: &Path,
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
        }
    }

    pub fn clear_session(&self) {
        self.grants
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
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
            Ok(Some(rules)) if rules.iter().any(|rule| rule.matches_sandbox(&grant)) => {
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
        let mut reason = grant.summary();
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
                server_identity: None,
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
        if name == "bash"
            && let Some(reason) = destructive_reason(arguments)
        {
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
        let server_identity = access.server_identity;
        let is_mcp = name.starts_with("mcp__");
        let grant_key = rules::ToolGrant {
            tool: name.to_owned(),
            server: server_identity.clone(),
        };
        if (!is_mcp || server_identity.is_some())
            && self
                .grants
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(&grant_key)
        {
            return Ok(());
        }
        let load_error = match self.store.as_ref().map(rules::Store::load).transpose() {
            Ok(Some(rules))
                if rules
                    .iter()
                    .any(|rule| rule.matches_tool(name, server_identity.as_deref())) =>
            {
                return Ok(());
            }
            Ok(_) => None,
            Err(error) => {
                let message = pp_core::logging::diagnostic_text(&format!("{error:#}"));
                tracing::warn!(%message, "无法读取持久授权");
                Some(message)
            }
        };
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
        let has_identity = !is_mcp || server_identity.is_some();
        let persistent_grant = self.store.is_some() && load_error.is_none() && has_identity;
        self.prompt(
            PermissionRequest {
                tool: name.to_owned(),
                agent: None,
                reason,
                session_grant: has_identity,
                persistent_grant,
            },
            server_identity,
            load_error.as_deref(),
            cancellation,
        )
    }

    fn prompt(
        &self,
        request: PermissionRequest,
        server_identity: Option<String>,
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
                    self.grants
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(rules::ToolGrant {
                            tool: name,
                            server: server_identity,
                        });
                }
                Ok(())
            }
            PermissionDecision::ApprovePersistently if allow_persistent_grant => {
                self.store
                    .as_ref()
                    .expect("持久授权选项要求有规则存储")
                    .save_tool(&name, server_identity.as_deref())?;
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
