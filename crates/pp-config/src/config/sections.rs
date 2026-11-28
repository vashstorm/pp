use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

/// MCP tool proxy mode:
/// - `auto` (default): switch to the fixed two-tool proxy `mcp_search` + `mcp_call` once the available MCP tools exceed `proxyThreshold` (default 12), so a large MCP schema neither bloats the context nor breaks the cache;
/// - `always`: always use the fixed two-tool proxy whenever available MCP tools exist;
/// - `never`: always expose every `mcp__*` tool schema directly.
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

/// Selects both the tool permission policy and the sandbox boundary (in-process state, no longer persisted to the configuration file).
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

/// The `lsp:` section of `settings.yml`; field names keep oh-my-pi's camelCase.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct LspSettings {
    /// Master switch: when false there is no discovery, no warm-up, no automatic hooks, no late diagnostics and no exposed tools.
    pub enabled: bool,
    /// When true the server starts on the first real action; when false the TUI session warms it up in the background.
    pub lazy: bool,
    pub diagnostics_on_write: bool,
    pub diagnostics_on_edit: bool,
    pub diagnostics_deduplicate: bool,
    /// Wait budget for a single RPC in seconds, 5..=300.
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

/// The `cost` section of `settings.yml` defines the display currency and the exchange amount for 1 USD.
/// Icons are derived from `currency`; the configuration no longer accepts hand-written icons.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CostDisplay {
    pub currency: String,
    pub exchange_rate: f64,
}

impl CostDisplay {
    /// Supported display currencies and their Nerd Font / ASCII icons:
    /// United States `f01c1` (`$`), Europe `f01ad` (`E`), Japan `f07bc` (`Y`), Korea `f07bd` (`W`), China `f07ba` (`Y`), Brazil `f0b85` (`$`).
    const CURRENCY_ICONS: [(&'static str, char, char); 6] = [
        ("USD", '\u{f01c1}', '$'),
        ("EUR", '\u{f01ad}', 'E'),
        ("JPY", '\u{f07bc}', 'Y'),
        ("KRW", '\u{f07bd}', 'W'),
        ("CNY", '\u{f07ba}', 'Y'),
        ("BRL", '\u{f0b85}', '$'),
    ];

    /// Nerd Font icon for the currency; unsupported currencies return `None` (configuration validation already rejects that value).
    pub fn icon(&self) -> Option<char> {
        Self::CURRENCY_ICONS
            .iter()
            .find_map(|(code, icon, _)| (*code == self.currency).then_some(*icon))
    }

    /// Plain ASCII icon for the currency; unsupported currencies return `None`.
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

/// The `eval:` section of `settings.yml`, controlling the availability and timeout window of the persistent Python / JavaScript kernels.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EvalSettings {
    #[serde(default = "default_eval_enabled")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interpreter: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub js_runtime: Option<String>,
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
            js_runtime: None,
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
        if let Some(js_runtime) = &self.js_runtime {
            ensure!(
                !js_runtime.trim().is_empty(),
                "eval.js_runtime must not be blank"
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
