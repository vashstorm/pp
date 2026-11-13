use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalMode {
    #[default]
    Yolo,
    Write,
    #[serde(rename = "always-ask")]
    AlwaysAsk,
}

impl ApprovalMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Yolo => "yolo",
            Self::Write => "write",
            Self::AlwaysAsk => "always-ask",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalPolicy {
    Allow,
    Prompt,
    Deny,
}

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TaskSettings {
    pub max_concurrency: usize,
    pub max_recursion_depth: usize,
    pub idle_ttl_ms: u64,
    pub request_budget: usize,
    pub disabled_agents: Vec<String>,
    pub agent_models: std::collections::BTreeMap<String, String>,
}
impl Default for TaskSettings {
    fn default() -> Self {
        Self {
            max_concurrency: 4,
            max_recursion_depth: 2,
            idle_ttl_ms: 420_000,
            request_budget: 200,
            disabled_agents: vec![],
            agent_models: std::collections::BTreeMap::new(),
        }
    }
}

/// `settings.yml` 的 `lsp:` 小节；字段名沿用 oh-my-pi 的 camelCase。
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct LspSettings {
    /// 总开关：false 时不发现、不预热、不调用自动钩子、不投递迟到诊断、不暴露工具。
    pub enabled: bool,
    /// true 时首次实际动作才启动服务器；false 时 TUI 会话在后台预热。
    pub lazy: bool,
    pub format_on_write: bool,
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
            format_on_write: false,
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
            !self.format_on_write,
            "lsp.formatOnWrite=true is no longer supported; use lsp action=format and edit/write/bash to apply suggestions"
        );
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
