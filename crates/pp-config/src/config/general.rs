use std::path::Path;

use anyhow::{Context, Result};
use serde_yaml::Value;

use super::{
    ApprovalMode, EditMode, SandboxMode, Settings, StatsStyle, WebSearchProvider,
    update_settings_fields_returning,
};

/// `/setting` 面板 `General` 页可编辑的运行参数补丁。
#[derive(Clone, Debug, PartialEq)]
pub enum SettingPatch {
    Edit(EditMode),
    ApprovalMode(ApprovalMode),
    SandboxMode(SandboxMode),
    ReadMaxLines(usize),
    ReadMaxBytes(usize),
    BashTimeoutSeconds(u64),
    RgContextBefore(usize),
    RgContextAfter(usize),
    AutoCompact(bool),
    AutoCompactThresholdPercent(u8),
    RequestTimeoutSeconds(u64),
    RateLimitRetryBudgetSeconds(u64),
    LspEnabled(bool),
    LspLazy(bool),
    LspDiagnosticsOnWrite(bool),
    LspDiagnosticsOnEdit(bool),
    LspDiagnosticsDeduplicate(bool),
    LspRequestTimeout(u64),
    WebSearchEnabled(bool),
    WebSearchProvider(WebSearchProvider),
    WebSearchKeyless(bool),
    WebSearchTimeoutSeconds(u64),
    TaskMaxConcurrency(usize),
    TaskMaxRecursionDepth(usize),
    TaskIdleTtlMs(u64),
    TaskRequestBudget(usize),
    TuiMaxFps(u16),
    CostCurrency(String),
    CostExchangeRate(f64),
    StatsStyle(StatsStyle),
    Debug(bool),
}

impl SettingPatch {
    fn apply(self, mut settings: Settings) -> Result<(&'static str, Value)> {
        let pair = match self {
            Self::Edit(value) => (
                "edit",
                serde_yaml::to_value(value).context("Failed to serialize edit")?,
            ),
            Self::ApprovalMode(value) => {
                settings.tools.approval_mode = Some(value);
                (
                    "tools",
                    serde_yaml::to_value(&settings.tools)
                        .context("Failed to serialize tools settings")?,
                )
            }
            Self::SandboxMode(value) => {
                settings.tools.sandbox.mode = value;
                (
                    "tools",
                    serde_yaml::to_value(&settings.tools)
                        .context("Failed to serialize tools settings")?,
                )
            }
            Self::ReadMaxLines(value) => ("read_max_lines", Value::Number(value.into())),
            Self::ReadMaxBytes(value) => ("read_max_bytes", Value::Number(value.into())),
            Self::BashTimeoutSeconds(value) => {
                ("bash_timeout_seconds", Value::Number(value.into()))
            }
            Self::RgContextBefore(value) => {
                settings.rg.context_before = value;
                (
                    "rg",
                    serde_yaml::to_value(settings.rg).context("Failed to serialize rg settings")?,
                )
            }
            Self::RgContextAfter(value) => {
                settings.rg.context_after = value;
                (
                    "rg",
                    serde_yaml::to_value(settings.rg).context("Failed to serialize rg settings")?,
                )
            }
            Self::AutoCompact(value) => ("auto_compact", Value::Bool(value)),
            Self::AutoCompactThresholdPercent(value) => (
                "auto_compact_threshold_percent",
                Value::Number(value.into()),
            ),
            Self::RequestTimeoutSeconds(value) => {
                ("request_timeout_seconds", Value::Number(value.into()))
            }
            Self::RateLimitRetryBudgetSeconds(value) => (
                "rate_limit_retry_budget_seconds",
                Value::Number(value.into()),
            ),
            Self::LspEnabled(value) => {
                settings.lsp.enabled = value;
                serialize_lsp(settings.lsp)?
            }
            Self::LspLazy(value) => {
                settings.lsp.lazy = value;
                serialize_lsp(settings.lsp)?
            }
            Self::LspDiagnosticsOnWrite(value) => {
                settings.lsp.diagnostics_on_write = value;
                serialize_lsp(settings.lsp)?
            }
            Self::LspDiagnosticsOnEdit(value) => {
                settings.lsp.diagnostics_on_edit = value;
                serialize_lsp(settings.lsp)?
            }
            Self::LspDiagnosticsDeduplicate(value) => {
                settings.lsp.diagnostics_deduplicate = value;
                serialize_lsp(settings.lsp)?
            }
            Self::LspRequestTimeout(value) => {
                settings.lsp.request_timeout = value;
                serialize_lsp(settings.lsp)?
            }
            Self::WebSearchEnabled(value) => {
                settings.web_search.enabled = value;
                serialize_web_search(&settings.web_search)?
            }
            Self::WebSearchProvider(value) => {
                settings.web_search.provider = value;
                serialize_web_search(&settings.web_search)?
            }
            Self::WebSearchKeyless(value) => {
                settings.web_search.keyless = value;
                serialize_web_search(&settings.web_search)?
            }
            Self::WebSearchTimeoutSeconds(value) => {
                settings.web_search.timeout_seconds = value;
                serialize_web_search(&settings.web_search)?
            }
            Self::TaskMaxConcurrency(value) => {
                settings.task.max_concurrency = value;
                serialize_task(&settings.task)?
            }
            Self::TaskMaxRecursionDepth(value) => {
                settings.task.max_recursion_depth = value;
                serialize_task(&settings.task)?
            }
            Self::TaskIdleTtlMs(value) => {
                settings.task.idle_ttl_ms = value;
                serialize_task(&settings.task)?
            }
            Self::TaskRequestBudget(value) => {
                settings.task.request_budget = value;
                serialize_task(&settings.task)?
            }
            Self::TuiMaxFps(value) => {
                settings.tui.max_fps = value;
                (
                    "tui",
                    serde_yaml::to_value(settings.tui)
                        .context("Failed to serialize tui settings")?,
                )
            }
            Self::CostCurrency(value) => {
                settings.cost.currency = value;
                serialize_cost(&settings.cost)?
            }
            Self::CostExchangeRate(value) => {
                settings.cost.exchange_rate = value;
                serialize_cost(&settings.cost)?
            }
            Self::StatsStyle(value) => {
                settings.stats.style = value;
                (
                    "stats",
                    serde_yaml::to_value(settings.stats)
                        .context("Failed to serialize stats settings")?,
                )
            }
            Self::Debug(value) => ("debug", Value::Bool(value)),
        };
        Ok(pair)
    }
}

fn serialize_lsp(lsp: super::LspSettings) -> Result<(&'static str, Value)> {
    Ok((
        "lsp",
        serde_yaml::to_value(lsp).context("Failed to serialize lsp settings")?,
    ))
}

fn serialize_web_search(web_search: &super::WebSearchSettings) -> Result<(&'static str, Value)> {
    Ok((
        "webSearch",
        serde_yaml::to_value(web_search).context("Failed to serialize webSearch settings")?,
    ))
}

fn serialize_task(task: &super::TaskSettings) -> Result<(&'static str, Value)> {
    Ok((
        "task",
        serde_yaml::to_value(task).context("Failed to serialize task settings")?,
    ))
}

fn serialize_cost(cost: &super::CostDisplay) -> Result<(&'static str, Value)> {
    Ok((
        "cost",
        serde_yaml::to_value(cost).context("Failed to serialize cost settings")?,
    ))
}

/// 以严格的 YAML 校验与原子写入更新一项通用配置，返回提交后的完整 `Settings`。
pub fn update_general_setting(path: &Path, patch: SettingPatch) -> Result<Settings> {
    update_settings_fields_returning(path, |settings| Ok(vec![patch.apply(settings)?]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::read_settings;

    #[test]
    fn general_patches_persist_preserve_comments_and_other_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.yml");
        std::fs::write(
            &path,
            "# keep header comment\ntui:\n  theme: tokyonight\n  max_fps: 60\n",
        )
        .unwrap();

        let saved =
            update_general_setting(&path, SettingPatch::ApprovalMode(ApprovalMode::Write)).unwrap();
        assert_eq!(saved.tools.approval_mode, Some(ApprovalMode::Write));
        assert_eq!(saved.tui.theme, crate::config::Theme::TokyoNight);

        let saved = update_general_setting(&path, SettingPatch::LspRequestTimeout(30)).unwrap();
        assert_eq!(saved.lsp.request_timeout, 30);

        let saved = update_general_setting(&path, SettingPatch::TaskMaxConcurrency(8)).unwrap();
        assert_eq!(saved.task.max_concurrency, 8);

        let saved =
            update_general_setting(&path, SettingPatch::CostCurrency("CNY".into())).unwrap();
        assert_eq!(saved.cost.currency, "CNY");

        let saved = update_general_setting(&path, SettingPatch::CostExchangeRate(7.25)).unwrap();
        assert!((saved.cost.exchange_rate - 7.25).abs() < f64::EPSILON);

        let saved = update_general_setting(&path, SettingPatch::Edit(EditMode::Hashline)).unwrap();
        assert_eq!(saved.edit, EditMode::Hashline);

        let saved = update_general_setting(&path, SettingPatch::TuiMaxFps(120)).unwrap();
        assert_eq!(saved.tui.max_fps, 120);
        assert_eq!(saved.tui.theme, crate::config::Theme::TokyoNight);

        let saved = update_general_setting(&path, SettingPatch::Debug(true)).unwrap();
        assert!(saved.debug);

        let reloaded = read_settings(&path).unwrap();
        assert_eq!(reloaded, saved);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# keep header comment\n"));
    }

    #[test]
    fn general_patches_reject_invalid_values_without_overwriting() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.yml");
        std::fs::write(&path, "debug: false\n").unwrap();

        for patch in [
            SettingPatch::TuiMaxFps(0),
            SettingPatch::TuiMaxFps(121),
            SettingPatch::AutoCompactThresholdPercent(0),
            SettingPatch::AutoCompactThresholdPercent(100),
            SettingPatch::RequestTimeoutSeconds(0),
            SettingPatch::ReadMaxLines(0),
            SettingPatch::ReadMaxBytes(0),
            SettingPatch::BashTimeoutSeconds(0),
            SettingPatch::LspRequestTimeout(4),
            SettingPatch::LspRequestTimeout(301),
            SettingPatch::TaskMaxConcurrency(0),
            SettingPatch::TaskMaxConcurrency(33),
            SettingPatch::TaskMaxRecursionDepth(9),
            SettingPatch::CostCurrency("GBP".into()),
            SettingPatch::CostExchangeRate(0.0),
            SettingPatch::CostExchangeRate(-1.5),
            SettingPatch::CostExchangeRate(f64::NAN),
        ] {
            assert!(
                update_general_setting(&path, patch.clone()).is_err(),
                "expected {patch:?} to be rejected"
            );
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "debug: false\n");
    }
}
