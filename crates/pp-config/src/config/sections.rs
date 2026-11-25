use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

/// MCP 工具代理模式：
/// - `auto`（默认）：可用 MCP 工具数超过 `proxyThreshold`（默认 12）时切换为固定双工具代理 `mcp_search` + `mcp_call`，避免大量 MCP schema 膨胀上下文与破坏缓存；
/// - `always`：只要存在可用 MCP 工具即始终使用固定双工具代理；
/// - `never`：始终直接暴露全部 `mcp__*` 工具 schema。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpProxyMode {
    #[default]
    Auto,
    Always,
    Never,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct McpSettings {
    pub proxy: McpProxyMode,
    pub proxy_threshold: usize,
}

impl Default for McpSettings {
    fn default() -> Self {
        Self {
            proxy: McpProxyMode::Auto,
            proxy_threshold: 12,
        }
    }
}

impl McpSettings {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.proxy_threshold > 0,
            "mcp.proxyThreshold must be positive"
        );
        Ok(())
    }
}

#[cfg(test)]
mod mcp_settings_tests {
    use super::*;

    #[test]
    fn mcp_settings_defaults_modes_and_validation() {
        let defaults = McpSettings::default();
        assert_eq!(defaults.proxy, McpProxyMode::Auto);
        assert_eq!(defaults.proxy_threshold, 12);
        defaults.validate().unwrap();

        let parsed: McpSettings =
            serde_yaml::from_str("proxy: always\nproxyThreshold: 5\n").unwrap();
        assert_eq!(parsed.proxy, McpProxyMode::Always);
        assert_eq!(parsed.proxy_threshold, 5);
        parsed.validate().unwrap();

        let zero: McpSettings = serde_yaml::from_str("proxyThreshold: 0\n").unwrap();
        assert!(zero.validate().is_err());
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct OutputSettings {
    pub artifact_spill_threshold: usize,
    pub artifact_head_bytes: usize,
    pub output_max_columns: usize,
    pub artifact_max_bytes: usize,
}

impl Default for OutputSettings {
    fn default() -> Self {
        Self {
            artifact_spill_threshold: 50 * 1024,
            artifact_head_bytes: 20 * 1024,
            output_max_columns: 768,
            artifact_max_bytes: 16 * 1024 * 1024,
        }
    }
}

impl OutputSettings {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.artifact_spill_threshold > 0,
            "artifactSpillThreshold must be positive"
        );
        Ok(())
    }
}

/// 同时选择工具权限策略与沙盒边界（进程内状态，不再持久化到配置文件）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum PermissionMode {
    #[default]
    #[serde(rename = "workspace-write")]
    WorkspaceWrite,
    #[serde(rename = "auto_review")]
    AutoReview,
    #[serde(rename = "danger-full-access", alias = "yolo")]
    DangerFullAccess,
    #[serde(rename = "read-only")]
    ReadOnly,
}

impl PermissionMode {
    pub const ALL: [Self; 4] = [
        Self::WorkspaceWrite,
        Self::AutoReview,
        Self::DangerFullAccess,
        Self::ReadOnly,
    ];
    pub const OPTIONS: &str = "workspace-write, auto_review, danger-full-access (yolo), read-only";

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::WorkspaceWrite => "workspace-write",
            Self::AutoReview => "auto_review",
            Self::DangerFullAccess => "danger-full-access",
            Self::ReadOnly => "read-only",
        }
    }

    pub const fn title(self) -> &'static str {
        match self {
            Self::WorkspaceWrite => "Ask for approval",
            Self::AutoReview => "Approve for me",
            Self::DangerFullAccess => "Full Access",
            Self::ReadOnly => "Read Only",
        }
    }

    pub const fn short_label(self) -> &'static str {
        match self {
            Self::WorkspaceWrite => "Ask",
            Self::AutoReview => "Auto",
            Self::DangerFullAccess => "YOLO",
            Self::ReadOnly => "RO",
        }
    }

    pub const fn description(self) -> &'static str {
        match self {
            Self::WorkspaceWrite => {
                "Read and edit workspace files and run commands, with approval required for internet access or edits outside the workspace"
            }
            Self::AutoReview => "Only ask for actions detected as potentially unsafe",
            Self::DangerFullAccess => {
                "Use with caution: pp can edit files outside this workspace and access the internet without approval"
            }
            Self::ReadOnly => {
                "Read workspace files, with approval required for edits or internet access"
            }
        }
    }

    pub const fn next(self) -> Self {
        match self {
            Self::WorkspaceWrite => Self::AutoReview,
            Self::AutoReview => Self::DangerFullAccess,
            Self::DangerFullAccess => Self::ReadOnly,
            Self::ReadOnly => Self::WorkspaceWrite,
        }
    }
}

impl std::str::FromStr for PermissionMode {
    type Err = anyhow::Error;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let normalized = raw.trim().to_ascii_lowercase();
        match normalized.as_str() {
            "workspace-write" => Ok(Self::WorkspaceWrite),
            "auto_review" => Ok(Self::AutoReview),
            "danger-full-access" | "yolo" => Ok(Self::DangerFullAccess),
            "read-only" => Ok(Self::ReadOnly),
            _ => Err(anyhow::anyhow!("expected one of {}", Self::OPTIONS)),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionPolicy {
    Allow,
    Prompt,
    Deny,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TaskSettings {
    pub max_concurrency: usize,
    pub max_recursion_depth: usize,
    pub idle_ttl_ms: u64,
    pub request_budget: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_budget: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_budget_usd: Option<f64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub disabled_agents: Vec<String>,
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub agent_models: std::collections::BTreeMap<String, String>,
}
impl Default for TaskSettings {
    fn default() -> Self {
        Self {
            max_concurrency: 4,
            max_recursion_depth: 2,
            idle_ttl_ms: 420_000,
            request_budget: 200,
            token_budget: None,
            cost_budget_usd: None,
            disabled_agents: vec![],
            agent_models: std::collections::BTreeMap::new(),
        }
    }
}

/// `settings.yml` 的 `lsp:` 小节；字段名沿用 oh-my-pi 的 camelCase。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct LspSettings {
    /// 总开关：false 时不发现、不预热、不调用自动钩子、不投递迟到诊断、不暴露工具。
    pub enabled: bool,
    /// true 时首次实际动作才启动服务器；false 时 TUI 会话在后台预热。
    pub lazy: bool,
    pub diagnostics_on_write: bool,
    pub diagnostics_on_edit: bool,
    pub diagnostics_deduplicate: bool,
    /// 单次 RPC 等待预算（秒），5..=300。
    pub request_timeout: u64,
}

impl Default for LspSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            lazy: true,
            diagnostics_on_write: true,
            diagnostics_on_edit: false,
            diagnostics_deduplicate: true,
            request_timeout: 10,
        }
    }
}

impl LspSettings {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (5..=300).contains(&self.request_timeout),
            "lsp.requestTimeout must be between 5 and 300"
        );
        Ok(())
    }

    pub fn request_budget(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.request_timeout)
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum EditMode {
    #[default]
    Replace,
    Hashline,
}

impl EditMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Replace => "replace",
            Self::Hashline => "hashline",
        }
    }
}

/// `settings.yml` 的 `cost` 定义显示币种与 1 USD 的兑换金额。
/// 图标由 `currency` 派生，配置不再接受手写图标。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CostDisplay {
    pub currency: String,
    pub exchange_rate: f64,
}

impl CostDisplay {
    /// 受支持的显示币种及对应的 Nerd Font / ASCII 图标：
    /// 美国 `f01c1` (`$`)、欧洲 `f01ad` (`E`)、日本 `f07bc` (`Y`)、韩国 `f07bd` (`W`)、中国 `f07ba` (`Y`)、巴西 `f0b85` (`$`)。
    const CURRENCY_ICONS: [(&'static str, char, char); 6] = [
        ("USD", '\u{f01c1}', '$'),
        ("EUR", '\u{f01ad}', 'E'),
        ("JPY", '\u{f07bc}', 'Y'),
        ("KRW", '\u{f07bd}', 'W'),
        ("CNY", '\u{f07ba}', 'Y'),
        ("BRL", '\u{f0b85}', '$'),
    ];

    /// 币种 Nerd Font 图标；不受支持的币种返回 `None`（配置校验已拒绝该取值）。
    pub fn icon(&self) -> Option<char> {
        Self::CURRENCY_ICONS
            .iter()
            .find_map(|(code, icon, _)| (*code == self.currency).then_some(*icon))
    }

    /// 币种纯 ASCII 图标；不受支持的币种返回 `None`。
    pub fn ascii_icon(&self) -> Option<char> {
        Self::CURRENCY_ICONS
            .iter()
            .find_map(|(code, _, ascii)| (*code == self.currency).then_some(*ascii))
    }

    pub fn currency_codes() -> impl Iterator<Item = &'static str> {
        Self::CURRENCY_ICONS.iter().map(|(code, _, _)| *code)
    }
}

impl Default for CostDisplay {
    fn default() -> Self {
        Self {
            currency: "USD".into(),
            exchange_rate: 1.0,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadDisplaySettings {
    #[serde(default, rename = "toolResultPreview")]
    pub(crate) tool_result_preview: bool,
}

pub fn default_eval_enabled() -> bool {
    true
}

pub fn default_eval_timeout_seconds() -> u64 {
    30
}

pub fn default_eval_max_timeout_seconds() -> u64 {
    600
}

/// `settings.yml` 的 `eval:` 小节，控制持久 Python 内核的可用性与超时窗口。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EvalSettings {
    #[serde(default = "default_eval_enabled")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interpreter: Option<String>,
    #[serde(default = "default_eval_timeout_seconds")]
    pub timeout_seconds: u64,
    #[serde(default = "default_eval_max_timeout_seconds")]
    pub max_timeout_seconds: u64,
}

impl Default for EvalSettings {
    fn default() -> Self {
        Self {
            enabled: default_eval_enabled(),
            interpreter: None,
            timeout_seconds: default_eval_timeout_seconds(),
            max_timeout_seconds: default_eval_max_timeout_seconds(),
        }
    }
}

impl EvalSettings {
    pub fn validate(&self) -> Result<()> {
        if let Some(interpreter) = &self.interpreter {
            ensure!(
                !interpreter.trim().is_empty(),
                "eval.interpreter must not be blank"
            );
        }
        ensure!(
            self.timeout_seconds > 0,
            "eval.timeout_seconds must be greater than zero"
        );
        ensure!(
            self.max_timeout_seconds > 0,
            "eval.max_timeout_seconds must be greater than zero"
        );
        ensure!(
            self.timeout_seconds <= self.max_timeout_seconds,
            "eval.timeout_seconds must not exceed eval.max_timeout_seconds"
        );
        ensure!(
            std::time::Instant::now()
                .checked_add(std::time::Duration::from_secs(self.max_timeout_seconds))
                .is_some(),
            "eval.max_timeout_seconds is too large"
        );
        Ok(())
    }
}
