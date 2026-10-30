use anyhow::{Context, Result, ensure};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use crate::{models, yaml};

mod roles;
pub(crate) use roles::{ModelRole, ModelRoles, RoleAssignment};

// Deliberately not Debug: this owns the API credential.
pub(crate) struct Config {
    pub(crate) auth: Option<crate::auth::Config>,
    pub(crate) thinking: models::ModelThinking,
    pub(crate) protocol: models::Protocol,
    pub(crate) api_key: String,
    pub(crate) model: String,
    // 完整 provider/model 配置引用；与供应商实际 wire model 名（model 字段）不同。
    pub(crate) model_reference: String,
    pub(crate) context_window: Option<u64>,
    pub(crate) base_url: Url,
    pub(crate) agent_db_path: PathBuf,
    pub(crate) settings: Settings,
}

// Deliberately not Debug: the catalog owns all startup credentials.
#[derive(Clone)]
pub(crate) struct ModelCatalog {
    settings: Settings,
    settings_path: PathBuf,
    agent_db_path: PathBuf,
    providers: Vec<models::ValidatedProvider>,
    preferences: ModelPreferences,
    default_reference: String,
    auth_generations: std::collections::BTreeMap<crate::auth::Provider, String>,
}

#[derive(Clone)]
pub(crate) struct ModelChoice {
    pub(crate) thinking: models::ModelThinking,
    pub(crate) reference: String,
    pub(crate) model: String,
    pub(crate) protocol: models::Protocol,
    pub(crate) context_window: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum EditMode {
    #[default]
    Replace,
    Hashline,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ThinkLevel {
    #[default]
    #[serde(alias = "default")]
    None,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl ThinkLevel {
    pub(crate) const ALL: [Self; 6] = [
        Self::None,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::Xhigh,
        Self::Max,
    ];
    pub(crate) const OPTIONS: &str = "none, low, medium, high, xhigh, max";

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }

    pub(crate) fn label(self) -> &'static str {
        if self == Self::None {
            "no think"
        } else {
            self.as_str()
        }
    }

    pub(crate) fn effort(self) -> async_openai::types::responses::ReasoningEffort {
        use async_openai::types::responses::ReasoningEffort;
        match self {
            Self::None => ReasoningEffort::None,
            Self::Low => ReasoningEffort::Low,
            Self::Medium => ReasoningEffort::Medium,
            Self::High => ReasoningEffort::High,
            Self::Xhigh => ReasoningEffort::Xhigh,
            Self::Max => ReasoningEffort::Max,
        }
    }
}

impl std::str::FromStr for ThinkLevel {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "none" | "default" => Ok(Self::None),
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "xhigh" => Ok(Self::Xhigh),
            "max" => Ok(Self::Max),
            _ => anyhow::bail!("thinking level must be one of: {}", Self::OPTIONS),
        }
    }
}

#[derive(Clone, Copy, Serialize, Deserialize)]
pub(crate) struct ToolLimits {
    pub(crate) read_max_bytes: usize,
    pub(crate) read_max_lines: usize,
    pub(crate) bash_timeout_seconds: u64,
    #[serde(default)]
    pub(crate) grep: GrepSettings,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct GrepSettings {
    #[serde(rename = "contextBefore")]
    pub(crate) context_before: usize,
    #[serde(rename = "contextAfter")]
    pub(crate) context_after: usize,
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
pub(crate) struct ToolsSettings {
    #[serde(rename = "approvalMode")]
    pub(crate) approval_mode: crate::tools::approval::ApprovalMode,
    pub(crate) approval: std::collections::BTreeMap<String, crate::tools::approval::ApprovalPolicy>,
}

impl ToolLimits {
    pub(crate) fn validate(&self) -> Result<()> {
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
    120
}

fn default_notification_show_countdown() -> bool {
    true
}

fn default_notification_timeout_seconds() -> u64 {
    10
}

#[derive(Clone, Copy)]
pub(crate) struct NotificationOptions {
    pub(crate) show_countdown: bool,
    pub(crate) timeout_seconds: u64,
}

impl Default for NotificationOptions {
    fn default() -> Self {
        Self {
            show_countdown: default_notification_show_countdown(),
            timeout_seconds: default_notification_timeout_seconds(),
        }
    }
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReadDisplaySettings {
    #[serde(default, rename = "toolResultPreview")]
    pub(crate) tool_result_preview: bool,
}

/// 默认/收藏/Role/effort 的持久化权威，本批自 settings.yml 四字段迁入 agent.db。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ModelPreferences {
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
            ensure!(
                assignment
                    .model
                    .split_once('/')
                    .is_some_and(
                        |(provider, model)| !provider.trim().is_empty() && !model.trim().is_empty()
                    ),
                "model_roles model must use provider/model"
            );
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
pub(crate) struct Settings {
    #[serde(default)]
    pub(crate) task: crate::subagent::Settings,
    #[serde(default)]
    pub(crate) grep: GrepSettings,
    #[serde(default)]
    pub(crate) read: ReadDisplaySettings,
    #[serde(default)]
    pub(crate) tools: ToolsSettings,
    #[serde(default)]
    pub(crate) debug: bool,
    #[serde(default = "default_notification_show_countdown")]
    pub(crate) notification_show_countdown: bool,
    #[serde(default = "default_notification_timeout_seconds")]
    pub(crate) notification_timeout_seconds: u64,
    #[serde(default = "default_skill_dirs")]
    pub(crate) skill_dirs: Vec<String>,
    #[serde(default)]
    pub(crate) skill_sources: Vec<crate::skills::SkillSource>,
    #[serde(default)]
    pub(crate) disabled_skills: Vec<String>,
    #[serde(default)]
    pub(crate) edit: EditMode,
    // 模型建连及连续无数据的闲置超时，不限制整轮生成的总耗时。
    #[serde(default = "default_request_timeout_seconds")]
    pub(crate) request_timeout_seconds: u64,
    // Zero disables opening-phase 429 retries entirely.
    #[serde(default = "default_rate_limit_retry_budget_seconds")]
    pub(crate) rate_limit_retry_budget_seconds: u64,
    #[serde(default = "default_read_max_bytes")]
    pub(crate) read_max_bytes: usize,
    #[serde(default = "default_read_max_lines")]
    pub(crate) read_max_lines: usize,
    #[serde(default = "default_bash_timeout_seconds")]
    pub(crate) bash_timeout_seconds: u64,
    #[serde(default = "default_auto_compact")]
    pub(crate) auto_compact: bool,
    #[serde(default = "default_auto_compact_threshold_percent")]
    pub(crate) auto_compact_threshold_percent: u8,
}

impl Settings {
    pub(crate) fn notification_options(&self) -> NotificationOptions {
        NotificationOptions {
            show_countdown: self.notification_show_countdown,
            timeout_seconds: self.notification_timeout_seconds,
        }
    }

    pub(crate) fn tool_limits(&self) -> ToolLimits {
        ToolLimits {
            read_max_bytes: self.read_max_bytes,
            read_max_lines: self.read_max_lines,
            bash_timeout_seconds: self.bash_timeout_seconds,
            grep: self.grep,
        }
    }

    pub(crate) fn validate(&self) -> Result<()> {
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

pub(crate) fn global_dir() -> Result<PathBuf> {
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
        tools: ToolsSettings::default(),
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
    pub(crate) fn cost(&self, reference: &str) -> Option<crate::models::ModelCost> {
        let (provider, model) = reference.split_once('/')?;
        self.providers
            .iter()
            .find(|p| p.id == provider)?
            .models
            .iter()
            .find(|m| m.id == model)?
            .cost
    }

    pub(crate) fn mcp_path(&self) -> PathBuf {
        self.settings_path.with_file_name("mcp.json")
    }

    pub(crate) fn read_tool_result_preview(&self) -> bool {
        self.settings.read.tool_result_preview
    }
    pub(crate) fn from_files(settings_path: &Path, models_path: &Path) -> Result<Self> {
        Self::load(settings_path, models_path, false)
    }

    fn load(settings_path: &Path, models_path: &Path, allow_empty: bool) -> Result<Self> {
        // 一次性迁移先于任何 settings.yml 解析：旧四字段由迁移导入 agent.db 并从文件剥离。
        let dir = settings_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        crate::agent_store::initialize(dir, &crate::cancellation::CancellationToken::new())?;
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
            preferences,
            default_reference,
            auth_generations: store
                .credentials
                .into_iter()
                .map(|(provider, credential)| (provider, credential.generation))
                .collect(),
        })
    }

    pub(crate) fn edit_mode(&self) -> EditMode {
        self.settings.edit
    }

    pub(crate) fn default_think_level(&self) -> ThinkLevel {
        self.preferences
            .model_roles
            .get(&ModelRole::Default)
            .filter(|assignment| assignment.enabled)
            .map_or(self.preferences.think_level, |assignment| assignment.effort)
    }

    pub(crate) fn save_default_think_level(&mut self, level: ThinkLevel) -> Result<()> {
        let mut store = crate::agent_store::AgentStore::open(&self.agent_db_path)?;
        let preferences = store.set_default_effort(level, |model, level| {
            let config = self.resolve(model)?;
            ensure!(
                config.thinking.supports(level),
                "DEFAULT role effort 不受模型支持"
            );
            Ok(())
        })?;
        self.install(preferences);
        Ok(())
    }

    pub(crate) fn save_default_model(&mut self, reference: &str) -> Result<()> {
        let thinking = self.resolve(reference)?.thinking;
        let mut store = crate::agent_store::AgentStore::open(&self.agent_db_path)?;
        let preferences = store.set_default_model(reference, thinking)?;
        self.install(preferences);
        Ok(())
    }

    pub(crate) fn favorite_models(&self) -> &[String] {
        &self.preferences.favorite_models
    }

    pub(crate) fn toggle_favorite_model(&mut self, reference: &str) -> Result<Vec<String>> {
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

    pub(crate) fn default_reference(&self) -> &str {
        &self.default_reference
    }

    pub(crate) fn choices(&self) -> Vec<ModelChoice> {
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

    pub(crate) fn reload_models(&mut self) -> Result<()> {
        // 模型目录刷新不改变运行参数；偏好一律从 agent.db 读取，全部验证成功后才替换内存状态。
        let next = Self::with_settings(
            self.settings.clone(),
            &self.settings_path,
            &self.settings_path.with_file_name("models.yml"),
            true,
        )?;
        self.providers = next.providers;
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

    pub(crate) fn resolve(&self, reference: &str) -> Result<Config> {
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
    pub(crate) fn model_usage(&self) -> Result<std::collections::BTreeMap<String, u64>> {
        crate::agent_store::AgentStore::open(&self.agent_db_path)
            .and_then(|store| store.model_usage())
    }
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

pub(crate) fn read_settings(path: &Path) -> Result<Settings> {
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
