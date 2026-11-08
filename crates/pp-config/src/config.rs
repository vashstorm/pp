use anyhow::{Context, Result, bail, ensure};
use pp_core::model::ThinkLevel;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use crate::{models, yaml};

mod roles;
pub use roles::{ModelRole, ModelRoles, RoleAssignment, RoleModel, RoleRoute};

// Deliberately not Debug: this owns the API credential.
pub struct Config {
    pub auth: Option<crate::auth::Config>,
    pub thinking: models::ModelThinking,
    pub protocol: models::Protocol,
    pub api_key: String,
    pub model: String,
    // 完整 provider/model 配置引用；与供应商实际 wire model 名（model 字段）不同。
    pub model_reference: String,
    pub context_window: Option<u64>,
    pub base_url: Url,
    pub agent_db_path: PathBuf,
    pub settings: Settings,
}

// Deliberately not Debug: the catalog owns all startup credentials.
#[derive(Clone)]
pub struct ModelCatalog {
    settings: Settings,
    settings_path: PathBuf,
    agent_db_path: PathBuf,
    providers: Vec<models::ValidatedProvider>,
    pricing: models::ModelPricing,
    preferences: ModelPreferences,
    default_reference: String,
    auth_generations: std::collections::BTreeMap<crate::auth::Provider, String>,
}

#[derive(Clone)]
pub struct ModelChoice {
    pub thinking: models::ModelThinking,
    pub reference: String,
    pub model: String,
    pub protocol: models::Protocol,
    pub context_window: Option<u64>,
}

/// `--model` 解析结果：完整引用 + 已解析到具体档位。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelSelection {
    pub reference: String,
    pub effort: ThinkLevel,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum EditMode {
    #[default]
    Replace,
    Hashline,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
pub struct ToolLimits {
    #[serde(default)]
    pub output: crate::config::OutputSettings,
    pub read_max_bytes: usize,
    pub read_max_lines: usize,
    pub bash_timeout_seconds: u64,
    #[serde(default)]
    pub grep: GrepSettings,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GrepSettings {
    #[serde(rename = "contextBefore")]
    pub context_before: usize,
    #[serde(rename = "contextAfter")]
    pub context_after: usize,
}

impl Default for GrepSettings {
    fn default() -> Self {
        Self {
            context_before: 1,
            context_after: 3,
        }
    }
}

/// settings.yml 的 `tools:` 小节；字段语义由 `crate::tools::approval` 解释。
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ToolsSettings {
    #[serde(flatten)]
    pub output: crate::config::OutputSettings,
    #[serde(rename = "approvalMode")]
    pub approval_mode: crate::config::ApprovalMode,
    pub approval: std::collections::BTreeMap<String, crate::config::ApprovalPolicy>,
}

impl ToolLimits {
    pub fn validate(&self) -> Result<()> {
        self.output.validate()?;
        ensure!(
            self.read_max_bytes > 0,
            "read_max_bytes must be greater than zero"
        );
        ensure!(
            self.read_max_lines > 0,
            "read_max_lines must be greater than zero"
        );
        ensure!(
            self.bash_timeout_seconds > 0,
            "bash_timeout_seconds must be greater than zero"
        );
        ensure!(
            std::time::Instant::now()
                .checked_add(std::time::Duration::from_secs(self.bash_timeout_seconds))
                .is_some(),
            "bash_timeout_seconds is too large"
        );
        Ok(())
    }
}

impl Default for ToolLimits {
    fn default() -> Self {
        Self {
            read_max_bytes: default_read_max_bytes(),
            read_max_lines: default_read_max_lines(),
            bash_timeout_seconds: default_bash_timeout_seconds(),
            grep: GrepSettings::default(),
            output: Default::default(),
        }
    }
}

fn default_read_max_bytes() -> usize {
    65_536
}
fn default_read_max_lines() -> usize {
    2_000
}
fn default_bash_timeout_seconds() -> u64 {
    300
}

fn default_notification_show_countdown() -> bool {
    true
}

fn default_notification_timeout_seconds() -> u64 {
    10
}

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TuiSettings {
    pub max_fps: u16,
}

impl Default for TuiSettings {
    fn default() -> Self {
        Self { max_fps: 60 }
    }
}

/// `settings.yml` 的 `cost` 定义显示币种与 1 USD 的兑换金额。
/// 图标由 `currency` 派生，配置不再接受手写图标。
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CostDisplay {
    pub currency: String,
    pub exchange_rate: f64,
}

impl CostDisplay {
    /// 受支持的显示币种及对应的 Nerd Font 图标（Material Design `currency_*` 系列）：
    /// 美国 `f01c1`、欧洲 `f01ad`、日本 `f07bc`、韩国 `f07bd`、中国 `f07ba`、巴西 `f0b85`。
    const CURRENCY_ICONS: [(&'static str, char); 6] = [
        ("USD", '\u{f01c1}'),
        ("EUR", '\u{f01ad}'),
        ("JPY", '\u{f07bc}'),
        ("KRW", '\u{f07bd}'),
        ("CNY", '\u{f07ba}'),
        ("BRL", '\u{f0b85}'),
    ];

    /// 币种图标；不受支持的币种返回 `None`（配置校验已拒绝该取值）。
    pub fn icon(&self) -> Option<char> {
        Self::CURRENCY_ICONS
            .iter()
            .find_map(|(code, icon)| (*code == self.currency).then_some(*icon))
    }

    fn currency_codes() -> impl Iterator<Item = &'static str> {
        Self::CURRENCY_ICONS.iter().map(|(code, _)| *code)
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

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadDisplaySettings {
    #[serde(default, rename = "toolResultPreview")]
    pub(crate) tool_result_preview: bool,
}

/// 默认/收藏/Role/effort 的持久化权威，本批自 settings.yml 四字段迁入 agent.db。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelPreferences {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) default_model: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) favorite_models: Vec<String>,
    #[serde(default)]
    pub(crate) model_roles: ModelRoles,
    #[serde(default)]
    pub(crate) think_level: ThinkLevel,
}

impl ModelPreferences {
    /// 沿用 settings.yml 四字段的既有校验规则；可用性由 ModelCatalog 在入口校验。
    pub(crate) fn validate(&self) -> Result<()> {
        for assignment in self.model_roles.values() {
            assignment.validate()?;
        }
        ensure!(
            self.favorite_models.iter().all(|reference| {
                reference.split_once('/').is_some_and(|(provider, model)| {
                    !provider.trim().is_empty() && !model.trim().is_empty()
                })
            }),
            "favorite_models must use provider/model"
        );
        if let Some(default_model) = &self.default_model {
            ensure!(
                !default_model.trim().is_empty(),
                "default_model must not be blank"
            );
            ensure!(
                default_model
                    .split_once('/')
                    .is_some_and(|(provider, model)| {
                        !provider.trim().is_empty() && !model.trim().is_empty()
                    }),
                "default_model must use provider/model"
            );
        }
        Ok(())
    }
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    #[serde(default)]
    pub tui: TuiSettings,
    #[serde(default)]
    pub cost: CostDisplay,
    #[serde(default)]
    pub task: crate::config::TaskSettings,
    #[serde(default)]
    pub grep: GrepSettings,
    #[serde(default)]
    pub read: ReadDisplaySettings,
    #[serde(default)]
    pub tools: ToolsSettings,
    #[serde(default)]
    pub lsp: crate::config::LspSettings,
    #[serde(default)]
    pub debug: bool,
    /// 已弃用：仅为旧配置保留类型校验，不再控制 TUI 显示。
    #[allow(dead_code)]
    #[serde(default = "default_notification_show_countdown")]
    pub notification_show_countdown: bool,
    /// 已弃用：沿用旧值校验，错误卡片不再自动消失。
    #[serde(default = "default_notification_timeout_seconds")]
    pub notification_timeout_seconds: u64,
    #[serde(default = "default_skill_dirs")]
    pub skill_dirs: Vec<String>,
    #[serde(default)]
    pub skill_sources: Vec<crate::skills::SkillSource>,
    #[serde(default)]
    pub disabled_skills: Vec<String>,
    #[serde(default)]
    pub edit: EditMode,
    // 模型建连及连续无数据的闲置超时，不限制整轮生成的总耗时。
    #[serde(default = "default_request_timeout_seconds")]
    pub request_timeout_seconds: u64,
    // Zero disables opening-phase 429 retries entirely.
    #[serde(default = "default_rate_limit_retry_budget_seconds")]
    pub rate_limit_retry_budget_seconds: u64,
    #[serde(default = "default_read_max_bytes")]
    pub read_max_bytes: usize,
    #[serde(default = "default_read_max_lines")]
    pub read_max_lines: usize,
    #[serde(default = "default_bash_timeout_seconds")]
    pub bash_timeout_seconds: u64,
    #[serde(default = "default_auto_compact")]
    pub auto_compact: bool,
    #[serde(default = "default_auto_compact_threshold_percent")]
    pub auto_compact_threshold_percent: u8,
}

impl Settings {
    pub fn tool_limits(&self) -> ToolLimits {
        ToolLimits {
            read_max_bytes: self.read_max_bytes,
            read_max_lines: self.read_max_lines,
            bash_timeout_seconds: self.bash_timeout_seconds,
            grep: self.grep,
            output: self.tools.output,
        }
    }

    pub(crate) fn validate(&self) -> Result<()> {
        self.lsp.validate()?;
        ensure!(
            self.cost.icon().is_some(),
            "cost.currency must be one of {}",
            CostDisplay::currency_codes().collect::<Vec<_>>().join(", ")
        );
        ensure!(
            self.cost.exchange_rate.is_finite() && self.cost.exchange_rate > 0.0,
            "cost.exchange_rate must be positive and finite"
        );
        ensure!(
            (1..=120).contains(&self.tui.max_fps),
            "tui.max_fps must be between 1 and 120"
        );
        ensure!(
            self.notification_timeout_seconds > 0,
            "notification_timeout_seconds must be greater than zero"
        );
        ensure!(
            std::time::Instant::now()
                .checked_add(std::time::Duration::from_secs(
                    self.notification_timeout_seconds
                ))
                .is_some(),
            "notification_timeout_seconds is too large"
        );
        ensure!(
            self.skill_dirs.iter().all(|path| !path.trim().is_empty()),
            "skill_dirs must contain non-blank paths"
        );
        ensure!(
            self.disabled_skills
                .iter()
                .all(|name| !name.trim().is_empty()),
            "disabled_skills must contain non-blank names"
        );
        ensure!(
            self.request_timeout_seconds > 0,
            "request_timeout_seconds must be greater than zero"
        );
        ensure!(
            (1..=99).contains(&self.auto_compact_threshold_percent),
            "auto_compact_threshold_percent must be between 1 and 99"
        );
        ensure!(
            (1..=32).contains(&self.task.max_concurrency),
            "task.max_concurrency must be between 1 and 32"
        );
        ensure!(
            self.task.max_recursion_depth <= 8,
            "task.max_recursion_depth must not exceed 8"
        );
        self.tool_limits().validate()?;
        ensure!(
            self.tools.approval.iter().all(|(name, _)| {
                !name.trim().is_empty()
                    && (APPROVAL_TOOL_NAMES.contains(&name.as_str()) || name.starts_with("mcp__"))
            }),
            "tools.approval keys must be non-blank built-in tool names or mcp__ prefixes"
        );
        Ok(())
    }
}

/// 审批 policy 允许的内置工具名；MCP 工具只接受 `mcp__` 前缀防拼写错误。
const APPROVAL_TOOL_NAMES: &[&str] = &[
    "read",
    "grep",
    "glob",
    "lsp",
    "todo",
    "ask",
    "edit",
    "write",
    "bash",
    "bash_task",
    "task",
    "wait",
];

fn default_skill_dirs() -> Vec<String> {
    vec!["~/.pp/agent/skills".into()]
}

pub fn global_dir() -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .context("HOME is required to locate ~/.pp/agent")?;
    let home = PathBuf::from(home);
    ensure!(home.is_absolute(), "HOME must be an absolute path");
    Ok(home.join(".pp/agent"))
}

fn default_settings() -> Settings {
    Settings {
        task: Default::default(),
        grep: GrepSettings::default(),
        read: ReadDisplaySettings::default(),
        tui: TuiSettings::default(),
        cost: CostDisplay::default(),
        tools: ToolsSettings::default(),
        lsp: Default::default(),
        debug: false,
        notification_show_countdown: default_notification_show_countdown(),
        notification_timeout_seconds: default_notification_timeout_seconds(),
        skill_dirs: default_skill_dirs(),
        skill_sources: Vec::new(),
        disabled_skills: Vec::new(),
        edit: EditMode::Replace,
        request_timeout_seconds: default_request_timeout_seconds(),
        rate_limit_retry_budget_seconds: default_rate_limit_retry_budget_seconds(),
        read_max_bytes: default_read_max_bytes(),
        read_max_lines: default_read_max_lines(),
        bash_timeout_seconds: default_bash_timeout_seconds(),
        auto_compact: default_auto_compact(),
        auto_compact_threshold_percent: default_auto_compact_threshold_percent(),
    }
}

fn default_request_timeout_seconds() -> u64 {
    300
}

fn default_rate_limit_retry_budget_seconds() -> u64 {
    300
}

fn default_auto_compact() -> bool {
    true
}

fn default_auto_compact_threshold_percent() -> u8 {
    80
}

impl ModelCatalog {
    pub fn cost(&self, reference: &str) -> Option<crate::models::ModelCost> {
        let (provider, model) = reference.split_once('/')?;
        let model = self
            .providers
            .iter()
            .filter(|p| p.id == provider)
            .find_map(|p| p.models.iter().find(|m| m.id == model))?;
        self.pricing.rate(reference, &model.model)
    }

    pub fn pricing(&self) -> &models::ModelPricing {
        &self.pricing
    }

    pub fn mcp_path(&self) -> PathBuf {
        self.settings_path.with_file_name("mcp.json")
    }

    /// 用户级 LSP 服务器定义与 settings.yml 放在同一配置目录。
    ///
    /// Agent 安装 Registry 时显式传入此路径，Registry 本身不再读取 HOME，
    /// 因而测试可用临时目录完整隔离。
    pub fn lsp_path(&self) -> PathBuf {
        self.settings_path.with_file_name("lsp.json")
    }

    pub fn lsp_settings(&self) -> crate::config::LspSettings {
        self.settings.lsp
    }

    pub fn read_tool_result_preview(&self) -> bool {
        self.settings.read.tool_result_preview
    }
    pub fn from_files(settings_path: &Path, models_path: &Path) -> Result<Self> {
        Self::load(settings_path, models_path, false)
    }

    fn load(settings_path: &Path, models_path: &Path, allow_empty: bool) -> Result<Self> {
        // 一次性迁移先于任何 settings.yml 解析：旧四字段由迁移导入 agent.db 并从文件剥离。
        let dir = settings_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        crate::agent_store::initialize(dir, &pp_core::cancellation::CancellationToken::new())?;
        let settings: Settings = read_settings(settings_path)?;
        settings
            .validate()
            .with_context(|| format!("loading YAML settings config {}", settings_path.display()))?;
        Self::with_settings(settings, settings_path, models_path, allow_empty)
    }

    fn with_settings(
        settings: Settings,
        settings_path: &Path,
        models_path: &Path,
        allow_empty: bool,
    ) -> Result<Self> {
        let agent_db_path = settings_path.with_file_name("agent.db");
        let agent_store = crate::agent_store::AgentStore::open(&agent_db_path)?;
        let preferences = agent_store.preferences()?;
        preferences.validate()?;
        let store = agent_store.load_auth()?;
        let mut providers = if models_path.try_exists()? {
            models::load_models(models_path)?
        } else {
            Vec::new()
        };
        let pricing = models::ModelPricing::from_providers(&providers);
        providers.retain(|provider| {
            provider
                .auth
                .is_none_or(|auth| store.credentials.contains_key(&auth))
        });
        let subscription =
            crate::auth::models::Catalog::load(&settings_path.with_file_name("models-sub.yml"))?;
        let manual_references: std::collections::HashSet<_> = providers
            .iter()
            .flat_map(|provider| {
                provider
                    .models
                    .iter()
                    .map(|model| format!("{}/{}", provider.id, model.id))
            })
            .collect();
        for mut provider in subscription.validated(&store) {
            provider.models.retain(|model| {
                !manual_references.contains(&format!("{}/{}", provider.id, model.id))
            });
            if !provider.models.is_empty() {
                providers.push(provider);
            }
        }
        ensure!(
            allow_empty || !providers.is_empty(),
            "No models configured; use /login in the TUI or configure models.yml."
        );
        let default_reference = default_reference(&providers, &preferences);
        Ok(Self {
            settings,
            settings_path: settings_path.to_path_buf(),
            agent_db_path,
            providers,
            pricing,
            preferences,
            default_reference,
            auth_generations: store
                .credentials
                .into_iter()
                .map(|(provider, credential)| (provider, credential.generation))
                .collect(),
        })
    }

    pub fn edit_mode(&self) -> EditMode {
        self.settings.edit
    }

    pub fn default_think_level(&self) -> ThinkLevel {
        self.preferences
            .model_roles
            .get(&ModelRole::Default)
            .filter(|assignment| assignment.enabled)
            .map_or(self.preferences.think_level, |assignment| assignment.effort)
    }

    pub fn save_default_think_level(&mut self, level: ThinkLevel) -> Result<()> {
        let mut store = crate::agent_store::AgentStore::open(&self.agent_db_path)?;
        let preferences = store.set_default_effort(level, |model, level| {
            let config = self.resolve(model)?;
            ensure!(
                config.thinking.supports(level),
                "DEFAULT role effort is not supported by the model"
            );
            Ok(())
        })?;
        self.install(preferences);
        Ok(())
    }

    pub fn save_default_model(&mut self, reference: &str) -> Result<()> {
        let thinking = self.resolve(reference)?.thinking;
        let mut store = crate::agent_store::AgentStore::open(&self.agent_db_path)?;
        let preferences = store.set_default_model(reference, thinking)?;
        self.install(preferences);
        Ok(())
    }

    pub fn favorite_models(&self) -> &[String] {
        &self.preferences.favorite_models
    }

    pub fn toggle_favorite_model(&mut self, reference: &str) -> Result<Vec<String>> {
        ensure!(
            self.choices()
                .iter()
                .any(|choice| choice.reference == reference),
            "Cannot favorite an unavailable model"
        );
        let mut store = crate::agent_store::AgentStore::open(&self.agent_db_path)?;
        let preferences = store.toggle_favorite(reference)?;
        self.preferences = preferences;
        Ok(self.preferences.favorite_models.clone())
    }

    pub fn default_reference(&self) -> &str {
        &self.default_reference
    }

    pub fn choices(&self) -> Vec<ModelChoice> {
        self.providers
            .iter()
            .flat_map(|provider| {
                provider.models.iter().map(|model| ModelChoice {
                    reference: format!("{}/{}", provider.id, model.id),
                    model: model.model.clone(),
                    protocol: provider.protocol,
                    thinking: model.thinking,
                    context_window: model.context_window,
                })
            })
            .collect()
    }

    /// 模糊选择：`[provider/]model[:effort]`；provider 可省略，effort 省略时用模型默认档。
    ///
    /// 文本匹配按“完全相等 → 前缀 → 子串 → 子序列”分档（不区分大小写），同档内收藏优先，
    /// 其余保持 catalog 顺序。这里只读取现有目录与偏好，不写入 agent.db。
    pub fn select(&self, query: &str) -> Result<ModelSelection> {
        // 错误文本回显用户原输入，因此先保留未 trim 的串。
        let raw = query;
        let query = query.trim();
        ensure!(!query.is_empty(), "model must not be empty");
        // 显式档位后缀；`default` 不作为档位，避免与模型 id 混淆。
        let (query, effort) = match query.rsplit_once(':') {
            Some((head, tail)) if !head.is_empty() => match explicit_effort(tail.trim()) {
                Some(level) => (head, Some(level)),
                None => (query, None),
            },
            _ => (query, None),
        };
        // provider 前缀只在左侧与已配置 provider 同名时生效；模型 id 本身允许含 `/`。
        let (provider, query) = match query.split_once('/') {
            Some((provider, model))
                if self
                    .providers
                    .iter()
                    .any(|candidate| candidate.id.eq_ignore_ascii_case(provider)) =>
            {
                ensure!(
                    !provider.is_empty() && !model.is_empty(),
                    "model must not be empty"
                );
                (Some(provider), model)
            }
            _ => (None, query),
        };
        let needle = query.to_lowercase();
        let favorites = self.favorite_models();
        let choices = self.choices();
        let mut best: Option<((u8, bool, usize), &ModelChoice)> = None;
        for (index, choice) in choices.iter().enumerate() {
            if let Some(provider) = provider {
                let in_provider = choice
                    .reference
                    .split_once('/')
                    .is_some_and(|(id, _)| id.eq_ignore_ascii_case(provider));
                if !in_provider {
                    continue;
                }
            }
            // reference 形如 `provider/model_id`，首个 `/` 之后的全部字符都属于模型 id。
            let model_id = choice
                .reference
                .split_once('/')
                .map_or(choice.reference.as_str(), |(_, model)| model);
            let tier = std::iter::once(model_id)
                .chain([choice.model.as_str()])
                .chain(provider.is_none().then_some(choice.reference.as_str()))
                .filter_map(|haystack| match_tier(&needle, haystack))
                .min();
            let Some(tier) = tier else { continue };
            let favorite = favorites
                .iter()
                .any(|favorite| favorite == &choice.reference);
            let key = (tier, !favorite, index);
            if best.is_none_or(|(best_key, _)| key < best_key) {
                best = Some((key, choice));
            }
        }
        let Some((_, selected)) = best else {
            bail!("no configured model matches \"{raw}\"; use /model to list configured models");
        };
        let reference = selected.reference.clone();
        let thinking = self.resolve(&reference)?.thinking;
        let effort = match effort {
            Some(level) => {
                ensure!(
                    thinking.supports(level),
                    "model {} does not support effort {}; options: {}",
                    reference,
                    level.as_str(),
                    thinking
                        .levels()
                        .map(ThinkLevel::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                level
            }
            None => thinking.effective_default(self.default_think_level()),
        };
        Ok(ModelSelection { reference, effort })
    }

    pub fn reload_models(&mut self) -> Result<()> {
        // 模型目录刷新不改变运行参数；偏好一律从 agent.db 读取，全部验证成功后才替换内存状态。
        let next = Self::with_settings(
            self.settings.clone(),
            &self.settings_path,
            &self.settings_path.with_file_name("models.yml"),
            true,
        )?;
        self.providers = next.providers;
        self.pricing = next.pricing;
        self.auth_generations = next.auth_generations;
        self.default_reference = next.default_reference;
        self.preferences = next.preferences;
        Ok(())
    }

    /// 安装事务提交后返回的完整偏好快照，并按精确优先级重算默认引用。
    fn install(&mut self, preferences: ModelPreferences) {
        self.default_reference = default_reference(&self.providers, &preferences);
        self.preferences = preferences;
    }

    pub fn resolve(&self, reference: &str) -> Result<Config> {
        ensure!(
            !reference.is_empty(),
            "No models configured; use /login in the TUI or configure models.yml."
        );
        let (provider_id, model_id) = reference
            .split_once('/')
            .filter(|(provider, model)| !provider.trim().is_empty() && !model.trim().is_empty())
            .context("model reference must use provider/model")?;
        let (provider, model) = self
            .providers
            .iter()
            .find_map(|provider| {
                (provider.id == provider_id)
                    .then(|| {
                        provider
                            .models
                            .iter()
                            .find(|model| model.id == model_id)
                            .map(|model| (provider, model))
                    })
                    .flatten()
            })
            .context("model not found; use /model to list configured models")?;
        Ok(Config {
            auth: provider.auth.map(|provider| crate::auth::Config {
                provider,
                path: self.agent_db_path.clone(),
                generation: self
                    .auth_generations
                    .get(&provider)
                    .cloned()
                    .unwrap_or_default(),
            }),
            protocol: provider.protocol,
            thinking: model.thinking,
            api_key: provider.api_key.clone(),
            model: model.model.clone(),
            model_reference: reference.to_owned(),
            context_window: model.context_window,
            base_url: match provider.auth {
                Some(auth) => Url::parse(auth.base_url()).expect("内置供应商 URL 有效"),
                None => models::openai_base_url(provider.base_url.clone()),
            },
            agent_db_path: self.agent_db_path.clone(),
            settings: self.settings.clone(),
        })
    }

    /// 读取按逻辑请求累计的使用次数；打开或读取失败都是固定脱敏错误。
    pub fn model_usage(&self) -> Result<std::collections::BTreeMap<String, u64>> {
        crate::agent_store::AgentStore::open(&self.agent_db_path)
            .and_then(|store| store.model_usage())
    }
}

/// 显式档位后缀：只接受六个档位名（不区分大小写），`default` 留给模型 id。
fn explicit_effort(value: &str) -> Option<ThinkLevel> {
    ThinkLevel::ALL
        .into_iter()
        .find(|level| level.as_str().eq_ignore_ascii_case(value))
}

/// 文本匹配档位：`0` 完全相等、`1` 前缀、`2` 子串、`3` 子序列；不匹配返回 `None`。
fn match_tier(needle: &str, haystack: &str) -> Option<u8> {
    let haystack = haystack.to_lowercase();
    if haystack == needle {
        Some(0)
    } else if haystack.starts_with(needle) {
        Some(1)
    } else if haystack.contains(needle) {
        Some(2)
    } else if is_subsequence(needle, &haystack) {
        Some(3)
    } else {
        None
    }
}

/// `needle` 的每个字符按顺序出现在 `haystack` 中；两者均已小写。
fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut haystack = haystack.chars();
    needle
        .chars()
        .all(|needle| haystack.any(|candidate| candidate == needle))
}

/// 默认引用优先级：启用的 Default Role → default_model → 第一个可用模型；
/// 不可用的普通默认引用回退到第一个可用模型。
fn default_reference(
    providers: &[models::ValidatedProvider],
    preferences: &ModelPreferences,
) -> String {
    let first_reference = providers
        .first()
        .map(|provider| format!("{}/{}", provider.id, provider.models[0].id))
        .unwrap_or_default();
    let reference = preferences
        .model_roles
        .get(&ModelRole::Default)
        .filter(|assignment| assignment.enabled)
        .map(|assignment| assignment.model.as_str())
        .or(preferences.default_model.as_deref());
    let Some(reference) = reference else {
        return first_reference;
    };
    let (provider_id, model_id) = reference
        .split_once('/')
        .expect("validated default preference is a composite reference");
    let matched = providers.iter().any(|provider| {
        provider.id == provider_id && provider.models.iter().any(|model| model.id == model_id)
    });
    if matched {
        reference.to_owned()
    } else {
        first_reference
    }
}

pub(crate) fn update_settings(
    path: &Path,
    update: impl FnOnce(Settings) -> Result<(&'static str, serde_yaml::Value)>,
) -> Result<()> {
    update_settings_fields(path, |settings| Ok(vec![update(settings)?]))
}

pub(crate) fn update_settings_fields(
    path: &Path,
    update: impl FnOnce(Settings) -> Result<Vec<(&'static str, serde_yaml::Value)>>,
) -> Result<()> {
    let read = || match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).context("Failed to read settings.yml"),
    };
    let original = read()?;
    let settings: Settings = yaml::parse(original.as_deref().unwrap_or("{}"), "settings")?;
    settings.validate()?;
    let mut updated = original.as_deref().unwrap_or("{}\n").to_owned();
    for (name, value) in update(settings)? {
        updated = yaml::update_field(&updated, name, value)?;
    }
    let settings: Settings = yaml::parse(&updated, "settings")?;
    settings.validate()?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent).context("Failed to create global config directory")?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .context("Failed to create temporary config file")?;
    if let Ok(meta) = fs::metadata(path) {
        temporary.as_file().set_permissions(meta.permissions())?;
    }
    temporary
        .write_all(updated.as_bytes())
        .context("Failed to write configuration")?;
    temporary
        .as_file()
        .sync_all()
        .context("Failed to sync configuration")?;
    ensure!(
        read()? == original,
        "Configuration was modified by another process; please try again."
    );
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .context("Failed to commit configuration")?;
    Ok(())
}

pub fn read_settings(path: &Path) -> Result<Settings> {
    match fs::read_to_string(path) {
        // 缺少 settings.yml 时使用默认值；目录或无效 UTF-8 等其他读取错误仍会阻止加载。
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(default_settings()),
        result => {
            let text = result
                .with_context(|| format!("reading YAML settings config {}", path.display()))?;
            yaml::parse::<Settings>(&text, "settings")
                .and_then(|settings| {
                    settings.validate()?;
                    Ok(settings)
                })
                .with_context(|| format!("loading YAML settings config {}", path.display()))
        }
    }
}

#[cfg(test)]
mod tests;

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
