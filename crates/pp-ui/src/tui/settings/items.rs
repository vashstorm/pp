//! Single-source definition of the `General` settings page categories, field metadata and validation/step rules.
use anyhow::{Context, Result, ensure};
use pp_config::config::{
    CostDisplay, EditMode, SandboxMode, SettingPatch, Settings, StatsStyle, WebSearchProvider,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Category {
    EditingTools,
    ContextModel,
    Lsp,
    Subagents,
    DisplaySystem,
}

impl Category {
    pub(super) const ALL: [Self; 5] = [
        Self::EditingTools,
        Self::ContextModel,
        Self::Lsp,
        Self::Subagents,
        Self::DisplaySystem,
    ];

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::EditingTools => "Editing & Tools",
            Self::ContextModel => "Context & Model",
            Self::Lsp => "LSP",
            Self::Subagents => "Subagents",
            Self::DisplaySystem => "Display & System",
        }
    }

    pub(super) fn items(self) -> impl Iterator<Item = &'static SettingItem> {
        ITEMS.iter().filter(move |item| item.category == self)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum ValueKind {
    Bool,
    EditMode,
    SandboxMode,
    WebSearchProvider,
    StatsStyle,
    Currency,
    Int {
        step: i128,
        min: i128,
        max: Option<i128>,
    },
    Float {
        step: f64,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SettingId {
    Edit,
    SandboxMode,
    ReadMaxLines,
    ReadMaxBytes,
    BashTimeoutSeconds,
    RgContextBefore,
    RgContextAfter,
    WebSearchEnabled,
    WebSearchProvider,
    WebSearchKeyless,
    WebSearchTimeoutSeconds,
    AutoCompact,
    AutoCompactThresholdPercent,
    RequestTimeoutSeconds,
    RateLimitRetryBudgetSeconds,
    LspEnabled,
    LspLazy,
    LspDiagnosticsOnWrite,
    LspDiagnosticsOnEdit,
    LspDiagnosticsDeduplicate,
    LspRequestTimeout,
    TaskMaxConcurrency,
    TaskMaxRecursionDepth,
    TaskIdleTtlMs,
    TaskRequestBudget,
    TuiMaxFps,
    CostCurrency,
    CostExchangeRate,
    StatsStyle,
    Debug,
}

pub(super) struct SettingItem {
    pub(super) id: SettingId,
    pub(super) category: Category,
    pub(super) key: &'static str,
    pub(super) label: &'static str,
    pub(super) description: &'static str,
    pub(super) kind: ValueKind,
}

impl SettingItem {
    pub(super) fn display_value(&self, settings: &Settings) -> String {
        match self.id {
            SettingId::Edit => settings.edit.as_str().to_owned(),
            SettingId::SandboxMode => settings.tools.sandbox.mode.as_str().to_owned(),
            SettingId::ReadMaxLines => settings.read_max_lines.to_string(),
            SettingId::ReadMaxBytes => settings.read_max_bytes.to_string(),
            SettingId::BashTimeoutSeconds => format!("{}s", settings.bash_timeout_seconds),
            SettingId::RgContextBefore => settings.rg.context_before.to_string(),
            SettingId::RgContextAfter => settings.rg.context_after.to_string(),
            SettingId::WebSearchEnabled => bool_label(settings.web_search.enabled),
            SettingId::WebSearchProvider => settings.web_search.provider.as_str().to_owned(),
            SettingId::WebSearchKeyless => bool_label(settings.web_search.keyless),
            SettingId::WebSearchTimeoutSeconds => {
                format!("{}s", settings.web_search.timeout_seconds)
            }
            SettingId::AutoCompact => bool_label(settings.auto_compact),
            SettingId::AutoCompactThresholdPercent => {
                format!("{}%", settings.auto_compact_threshold_percent)
            }
            SettingId::RequestTimeoutSeconds => format!("{}s", settings.request_timeout_seconds),
            SettingId::RateLimitRetryBudgetSeconds => {
                format!("{}s", settings.rate_limit_retry_budget_seconds)
            }
            SettingId::LspEnabled => bool_label(settings.lsp.enabled),
            SettingId::LspLazy => bool_label(settings.lsp.lazy),
            SettingId::LspDiagnosticsOnWrite => bool_label(settings.lsp.diagnostics_on_write),
            SettingId::LspDiagnosticsOnEdit => bool_label(settings.lsp.diagnostics_on_edit),
            SettingId::LspDiagnosticsDeduplicate => {
                bool_label(settings.lsp.diagnostics_deduplicate)
            }
            SettingId::LspRequestTimeout => format!("{}s", settings.lsp.request_timeout),
            SettingId::TaskMaxConcurrency => settings.task.max_concurrency.to_string(),
            SettingId::TaskMaxRecursionDepth => bool_label(settings.task.max_recursion_depth != 0),
            SettingId::TaskIdleTtlMs => format!("{}ms", settings.task.idle_ttl_ms),
            SettingId::TaskRequestBudget => settings.task.request_budget.to_string(),
            SettingId::TuiMaxFps => format!("{} fps", settings.tui.max_fps),
            SettingId::CostCurrency => settings.cost.currency.clone(),
            SettingId::CostExchangeRate => format!("{}", settings.cost.exchange_rate),
            SettingId::StatsStyle => settings.stats.style.as_str().to_owned(),
            SettingId::Debug => bool_label(settings.debug),
        }
    }

    pub(super) fn raw_numeric(&self, settings: &Settings) -> String {
        match self.id {
            SettingId::ReadMaxLines => settings.read_max_lines.to_string(),
            SettingId::ReadMaxBytes => settings.read_max_bytes.to_string(),
            SettingId::BashTimeoutSeconds => settings.bash_timeout_seconds.to_string(),
            SettingId::RgContextBefore => settings.rg.context_before.to_string(),
            SettingId::RgContextAfter => settings.rg.context_after.to_string(),
            SettingId::WebSearchTimeoutSeconds => settings.web_search.timeout_seconds.to_string(),
            SettingId::AutoCompactThresholdPercent => {
                settings.auto_compact_threshold_percent.to_string()
            }
            SettingId::RequestTimeoutSeconds => settings.request_timeout_seconds.to_string(),
            SettingId::RateLimitRetryBudgetSeconds => {
                settings.rate_limit_retry_budget_seconds.to_string()
            }
            SettingId::LspRequestTimeout => settings.lsp.request_timeout.to_string(),
            SettingId::TaskMaxConcurrency => settings.task.max_concurrency.to_string(),
            SettingId::TaskIdleTtlMs => settings.task.idle_ttl_ms.to_string(),
            SettingId::TaskRequestBudget => settings.task.request_budget.to_string(),
            SettingId::TuiMaxFps => settings.tui.max_fps.to_string(),
            SettingId::CostExchangeRate => format!("{}", settings.cost.exchange_rate),
            _ => String::new(),
        }
    }

    fn current_int(&self, settings: &Settings) -> i128 {
        match self.id {
            SettingId::ReadMaxLines => settings.read_max_lines as i128,
            SettingId::ReadMaxBytes => settings.read_max_bytes as i128,
            SettingId::BashTimeoutSeconds => settings.bash_timeout_seconds as i128,
            SettingId::RgContextBefore => settings.rg.context_before as i128,
            SettingId::RgContextAfter => settings.rg.context_after as i128,
            SettingId::WebSearchTimeoutSeconds => settings.web_search.timeout_seconds as i128,
            SettingId::AutoCompactThresholdPercent => {
                i128::from(settings.auto_compact_threshold_percent)
            }
            SettingId::RequestTimeoutSeconds => settings.request_timeout_seconds as i128,
            SettingId::RateLimitRetryBudgetSeconds => {
                settings.rate_limit_retry_budget_seconds as i128
            }
            SettingId::LspRequestTimeout => settings.lsp.request_timeout as i128,
            SettingId::TaskMaxConcurrency => settings.task.max_concurrency as i128,
            SettingId::TaskIdleTtlMs => settings.task.idle_ttl_ms as i128,
            SettingId::TaskRequestBudget => settings.task.request_budget as i128,
            SettingId::TuiMaxFps => i128::from(settings.tui.max_fps),
            _ => 0,
        }
    }

    fn patch_from_int(&self, value: i128) -> Result<SettingPatch> {
        if let ValueKind::Int { min, max, .. } = self.kind {
            ensure!(value >= min, "{} must be >= {min}", self.key);
            if let Some(max) = max {
                ensure!(value <= max, "{} must be <= {max}", self.key);
            }
        }
        Ok(match self.id {
            SettingId::ReadMaxLines => {
                SettingPatch::ReadMaxLines(usize::try_from(value).context("value too large")?)
            }
            SettingId::ReadMaxBytes => {
                SettingPatch::ReadMaxBytes(usize::try_from(value).context("value too large")?)
            }
            SettingId::BashTimeoutSeconds => {
                SettingPatch::BashTimeoutSeconds(u64::try_from(value).context("value too large")?)
            }
            SettingId::RgContextBefore => {
                SettingPatch::RgContextBefore(usize::try_from(value).context("value too large")?)
            }
            SettingId::RgContextAfter => {
                SettingPatch::RgContextAfter(usize::try_from(value).context("value too large")?)
            }
            SettingId::WebSearchTimeoutSeconds => SettingPatch::WebSearchTimeoutSeconds(
                u64::try_from(value).context("value too large")?,
            ),
            SettingId::AutoCompactThresholdPercent => SettingPatch::AutoCompactThresholdPercent(
                u8::try_from(value).context("value out of range")?,
            ),
            SettingId::RequestTimeoutSeconds => SettingPatch::RequestTimeoutSeconds(
                u64::try_from(value).context("value too large")?,
            ),
            SettingId::RateLimitRetryBudgetSeconds => SettingPatch::RateLimitRetryBudgetSeconds(
                u64::try_from(value).context("value too large")?,
            ),
            SettingId::LspRequestTimeout => {
                SettingPatch::LspRequestTimeout(u64::try_from(value).context("value too large")?)
            }
            SettingId::TaskMaxConcurrency => {
                SettingPatch::TaskMaxConcurrency(usize::try_from(value).context("value too large")?)
            }
            SettingId::TaskIdleTtlMs => {
                SettingPatch::TaskIdleTtlMs(u64::try_from(value).context("value too large")?)
            }
            SettingId::TaskRequestBudget => {
                SettingPatch::TaskRequestBudget(usize::try_from(value).context("value too large")?)
            }
            SettingId::TuiMaxFps => {
                SettingPatch::TuiMaxFps(u16::try_from(value).context("value out of range")?)
            }
            _ => unreachable!("non-integer setting id"),
        })
    }

    pub(super) fn step_or_cycle(&self, settings: &Settings, forward: bool) -> Result<SettingPatch> {
        match self.kind {
            ValueKind::Bool => Ok(match self.id {
                SettingId::WebSearchEnabled => {
                    SettingPatch::WebSearchEnabled(!settings.web_search.enabled)
                }
                SettingId::WebSearchKeyless => {
                    SettingPatch::WebSearchKeyless(!settings.web_search.keyless)
                }
                SettingId::AutoCompact => SettingPatch::AutoCompact(!settings.auto_compact),
                SettingId::LspEnabled => SettingPatch::LspEnabled(!settings.lsp.enabled),
                SettingId::LspLazy => SettingPatch::LspLazy(!settings.lsp.lazy),
                SettingId::LspDiagnosticsOnWrite => {
                    SettingPatch::LspDiagnosticsOnWrite(!settings.lsp.diagnostics_on_write)
                }
                SettingId::LspDiagnosticsOnEdit => {
                    SettingPatch::LspDiagnosticsOnEdit(!settings.lsp.diagnostics_on_edit)
                }
                SettingId::LspDiagnosticsDeduplicate => {
                    SettingPatch::LspDiagnosticsDeduplicate(!settings.lsp.diagnostics_deduplicate)
                }
                SettingId::TaskMaxRecursionDepth => SettingPatch::TaskMaxRecursionDepth(
                    usize::from(settings.task.max_recursion_depth == 0),
                ),
                SettingId::Debug => SettingPatch::Debug(!settings.debug),
                _ => unreachable!("non-bool setting id"),
            }),
            ValueKind::EditMode => Ok(SettingPatch::Edit(match settings.edit {
                EditMode::Replace => EditMode::Hashline,
                EditMode::Hashline => EditMode::Replace,
            })),
            ValueKind::SandboxMode => {
                let modes = SandboxMode::ALL;
                let idx = modes
                    .iter()
                    .position(|mode| *mode == settings.tools.sandbox.mode)
                    .unwrap_or(0);
                let next = if forward {
                    (idx + 1) % modes.len()
                } else {
                    (idx + modes.len() - 1) % modes.len()
                };
                Ok(SettingPatch::SandboxMode(modes[next]))
            }
            ValueKind::WebSearchProvider => {
                let providers = WebSearchProvider::ALL;
                let idx = providers
                    .iter()
                    .position(|provider| *provider == settings.web_search.provider)
                    .unwrap_or(0);
                let next = if forward {
                    (idx + 1) % providers.len()
                } else {
                    (idx + providers.len() - 1) % providers.len()
                };
                Ok(SettingPatch::WebSearchProvider(providers[next]))
            }
            ValueKind::StatsStyle => Ok(SettingPatch::StatsStyle(match settings.stats.style {
                StatsStyle::Default => StatsStyle::Modern,
                StatsStyle::Modern => StatsStyle::Default,
            })),
            ValueKind::Currency => {
                let codes: Vec<_> = CostDisplay::currency_codes().collect();
                let idx = codes
                    .iter()
                    .position(|code| *code == settings.cost.currency)
                    .unwrap_or(0);
                let next = if forward {
                    (idx + 1) % codes.len()
                } else {
                    (idx + codes.len() - 1) % codes.len()
                };
                Ok(SettingPatch::CostCurrency(codes[next].to_owned()))
            }
            ValueKind::Int { step, min, max } => {
                let current = self.current_int(settings);
                let delta = if forward { step } else { -step };
                let mut next = current.saturating_add(delta).max(min);
                if let Some(max) = max {
                    next = next.min(max);
                }
                self.patch_from_int(next)
            }
            ValueKind::Float { step } => {
                let current = settings.cost.exchange_rate;
                let next = if forward {
                    ((current + step) * 100.0).round() / 100.0
                } else {
                    ((current - step).max(0.1) * 100.0).round() / 100.0
                };
                Ok(SettingPatch::CostExchangeRate(next))
            }
        }
    }

    pub(super) fn parse_input(&self, raw: &str) -> Result<SettingPatch> {
        let trimmed = raw.trim();
        ensure!(!trimmed.is_empty(), "{} value cannot be empty", self.key);
        match self.kind {
            ValueKind::Int { .. } => {
                let parsed = trimmed
                    .parse::<i128>()
                    .with_context(|| format!("invalid integer for {}", self.key))?;
                self.patch_from_int(parsed)
            }
            ValueKind::Float { .. } => {
                let parsed = trimmed
                    .parse::<f64>()
                    .with_context(|| format!("invalid number for {}", self.key))?;
                ensure!(
                    parsed.is_finite() && parsed > 0.0,
                    "{} must be positive and finite",
                    self.key
                );
                Ok(SettingPatch::CostExchangeRate(parsed))
            }
            _ => unreachable!("non-numeric edit"),
        }
    }
}

fn bool_label(value: bool) -> String {
    if value { "on" } else { "off" }.to_owned()
}

pub(super) const ITEMS: &[SettingItem] = &[
    // Editing & Tools
    SettingItem {
        id: SettingId::Edit,
        category: Category::EditingTools,
        key: "edit",
        label: "Edit mode",
        description: "File edit strategy: replace (text match) or hashline (line anchor); next task",
        kind: ValueKind::EditMode,
    },
    SettingItem {
        id: SettingId::SandboxMode,
        category: Category::EditingTools,
        key: "tools.sandbox.mode",
        label: "Sandbox mode",
        description: "OS-level isolation (Seatbelt on macOS, Bubblewrap on Linux): auto, workspace-write, read-only, or off",
        kind: ValueKind::SandboxMode,
    },
    SettingItem {
        id: SettingId::ReadMaxLines,
        category: Category::EditingTools,
        key: "read_max_lines",
        label: "Read max lines",
        description: "Default line limit per read call (>= 1); applies on next launch",
        kind: ValueKind::Int {
            step: 500,
            min: 1,
            max: None,
        },
    },
    SettingItem {
        id: SettingId::ReadMaxBytes,
        category: Category::EditingTools,
        key: "read_max_bytes",
        label: "Read max bytes",
        description: "Default byte limit per read call (>= 1); applies on next launch",
        kind: ValueKind::Int {
            step: 8192,
            min: 1,
            max: None,
        },
    },
    SettingItem {
        id: SettingId::BashTimeoutSeconds,
        category: Category::EditingTools,
        key: "bash_timeout_seconds",
        label: "Bash timeout (s)",
        description: "Default timeout for foreground bash tool calls (>= 1s); applies live",
        kind: ValueKind::Int {
            step: 30,
            min: 1,
            max: None,
        },
    },
    SettingItem {
        id: SettingId::RgContextBefore,
        category: Category::EditingTools,
        key: "rg.contextBefore",
        label: "Grep context before",
        description: "Default lines shown before rg matches (>= 0); next launch",
        kind: ValueKind::Int {
            step: 1,
            min: 0,
            max: None,
        },
    },
    SettingItem {
        id: SettingId::RgContextAfter,
        category: Category::EditingTools,
        key: "rg.contextAfter",
        label: "Grep context after",
        description: "Default lines shown after rg matches (>= 0); next launch",
        kind: ValueKind::Int {
            step: 1,
            min: 0,
            max: None,
        },
    },
    SettingItem {
        id: SettingId::WebSearchEnabled,
        category: Category::EditingTools,
        key: "webSearch.enabled",
        label: "Web search",
        description: "Expose built-in web_search tool for public web discovery; applies live",
        kind: ValueKind::Bool,
    },
    SettingItem {
        id: SettingId::WebSearchProvider,
        category: Category::EditingTools,
        key: "webSearch.provider",
        label: "Search provider",
        description: "Default backend: auto, public, duckduckgo, startpage, hackernews, or searxng; applies live",
        kind: ValueKind::WebSearchProvider,
    },
    SettingItem {
        id: SettingId::WebSearchKeyless,
        category: Category::EditingTools,
        key: "webSearch.keyless",
        label: "Keyless search",
        description: "Allow credential-free public engines (Startpage/DuckDuckGo/HN); applies live",
        kind: ValueKind::Bool,
    },
    SettingItem {
        id: SettingId::WebSearchTimeoutSeconds,
        category: Category::EditingTools,
        key: "webSearch.timeoutSeconds",
        label: "Search timeout (s)",
        description: "Hard timeout per web_search request (1..=300s); applies live",
        kind: ValueKind::Int {
            step: 10,
            min: 1,
            max: Some(300),
        },
    },
    // Context & Model
    SettingItem {
        id: SettingId::AutoCompact,
        category: Category::ContextModel,
        key: "auto_compact",
        label: "Auto compact",
        description: "Automatically summarize older history before reaching threshold; next task",
        kind: ValueKind::Bool,
    },
    SettingItem {
        id: SettingId::AutoCompactThresholdPercent,
        category: Category::ContextModel,
        key: "auto_compact_threshold_percent",
        label: "Compact threshold (%)",
        description: "Context usage percentage that triggers auto compaction (1..=99); next task",
        kind: ValueKind::Int {
            step: 5,
            min: 1,
            max: Some(99),
        },
    },
    SettingItem {
        id: SettingId::RequestTimeoutSeconds,
        category: Category::ContextModel,
        key: "request_timeout_seconds",
        label: "Request timeout (s)",
        description: "Connect and idle timeout per model stream (>= 1s); next launch",
        kind: ValueKind::Int {
            step: 30,
            min: 1,
            max: None,
        },
    },
    SettingItem {
        id: SettingId::RateLimitRetryBudgetSeconds,
        category: Category::ContextModel,
        key: "rate_limit_retry_budget_seconds",
        label: "Retry budget (s)",
        description: "Cumulative wait budget per model on rate limits (0 disables); next launch",
        kind: ValueKind::Int {
            step: 60,
            min: 0,
            max: None,
        },
    },
    // LSP
    SettingItem {
        id: SettingId::LspEnabled,
        category: Category::Lsp,
        key: "lsp.enabled",
        label: "LSP enabled",
        description: "Enable language server discovery, tools, and diagnostics; next launch",
        kind: ValueKind::Bool,
    },
    SettingItem {
        id: SettingId::LspLazy,
        category: Category::Lsp,
        key: "lsp.lazy",
        label: "LSP lazy start",
        description: "Start language servers on first use instead of background warmup; next launch",
        kind: ValueKind::Bool,
    },
    SettingItem {
        id: SettingId::LspDiagnosticsOnWrite,
        category: Category::Lsp,
        key: "lsp.diagnosticsOnWrite",
        label: "Diagnostics on write",
        description: "Collect LSP diagnostics automatically after write tool; next launch",
        kind: ValueKind::Bool,
    },
    SettingItem {
        id: SettingId::LspDiagnosticsOnEdit,
        category: Category::Lsp,
        key: "lsp.diagnosticsOnEdit",
        label: "Diagnostics on edit",
        description: "Collect LSP diagnostics automatically after edit tool; next launch",
        kind: ValueKind::Bool,
    },
    SettingItem {
        id: SettingId::LspDiagnosticsDeduplicate,
        category: Category::Lsp,
        key: "lsp.diagnosticsDeduplicate",
        label: "Deduplicate diagnostics",
        description: "Suppress unchanged duplicate diagnostics across turns; next launch",
        kind: ValueKind::Bool,
    },
    SettingItem {
        id: SettingId::LspRequestTimeout,
        category: Category::Lsp,
        key: "lsp.requestTimeout",
        label: "LSP timeout (s)",
        description: "Single LSP RPC request timeout in seconds (5..=300); next launch",
        kind: ValueKind::Int {
            step: 5,
            min: 5,
            max: Some(300),
        },
    },
    // Subagents
    SettingItem {
        id: SettingId::TaskMaxConcurrency,
        category: Category::Subagents,
        key: "task.max_concurrency",
        label: "Max concurrency",
        description: "Maximum parallel subagents running simultaneously (1..=32); next launch",
        kind: ValueKind::Int {
            step: 1,
            min: 1,
            max: Some(32),
        },
    },
    SettingItem {
        id: SettingId::TaskMaxRecursionDepth,
        category: Category::Subagents,
        key: "task.max_recursion_depth",
        label: "Max recursion depth",
        description: "Allow nested subagent spawning (off = 0, on = 1); next launch",
        kind: ValueKind::Bool,
    },
    SettingItem {
        id: SettingId::TaskIdleTtlMs,
        category: Category::Subagents,
        key: "task.idle_ttl_ms",
        label: "Idle TTL (ms)",
        description: "Idle timeout before retiring background subagent workers; next launch",
        kind: ValueKind::Int {
            step: 60_000,
            min: 0,
            max: None,
        },
    },
    SettingItem {
        id: SettingId::TaskRequestBudget,
        category: Category::Subagents,
        key: "task.request_budget",
        label: "Request budget",
        description: "Maximum model requests per subagent run; next launch",
        kind: ValueKind::Int {
            step: 50,
            min: 0,
            max: None,
        },
    },
    // Display & System
    SettingItem {
        id: SettingId::TuiMaxFps,
        category: Category::DisplaySystem,
        key: "tui.max_fps",
        label: "TUI max FPS",
        description: "Target TUI render frame cap (1..=120); applies immediately",
        kind: ValueKind::Int {
            step: 15,
            min: 1,
            max: Some(120),
        },
    },
    SettingItem {
        id: SettingId::CostCurrency,
        category: Category::DisplaySystem,
        key: "cost.currency",
        label: "Cost currency",
        description: "Status bar cost display currency (USD/EUR/JPY/KRW/CNY/BRL); applies live",
        kind: ValueKind::Currency,
    },
    SettingItem {
        id: SettingId::CostExchangeRate,
        category: Category::DisplaySystem,
        key: "cost.exchange_rate",
        label: "Exchange rate",
        description: "Display conversion rate per 1 USD (> 0); applies live",
        kind: ValueKind::Float { step: 0.5 },
    },
    SettingItem {
        id: SettingId::StatsStyle,
        category: Category::DisplaySystem,
        key: "stats.style",
        label: "Stats page style",
        description: "Visual style for /stats local web page: default or modern",
        kind: ValueKind::StatsStyle,
    },
    SettingItem {
        id: SettingId::Debug,
        category: Category::DisplaySystem,
        key: "debug",
        label: "Debug mode",
        description: "Enable verbose debug diagnostics in logs; next launch",
        kind: ValueKind::Bool,
    },
];
