use anyhow::{Context, Result, bail, ensure};
use pp_core::model::ThinkLevel;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use crate::models;
#[cfg(test)]
use crate::yaml;
#[cfg(test)]
use std::fs;

mod io;
pub use io::{load_hooks, read_hooks, read_settings};
mod initialize;
pub use initialize::initialize_defaults;
pub(crate) use io::{update_settings, update_settings_fields, update_settings_fields_returning};

mod roles;
pub use roles::{ModelRole, ModelRoles, RoleAssignment, RoleModel, RoleRoute};

mod hooks;
pub use hooks::{
    CommandHook, DEFAULT_HOOK_TIMEOUT_SECONDS, HookEvent, HookFailurePolicy, HookHandlerType,
    HookMatcherGroup, HooksSettings, MAX_HOOK_TIMEOUT_SECONDS,
};

mod sections;
pub use sections::{
    CostDisplay, EditMode, EvalSettings, LspSettings, McpProxyMode, McpSettings, OutputSettings,
    PermissionMode, PermissionPolicy, ReadDisplaySettings, TaskSettings,
};

mod stats;
pub use stats::{StatsSettings, StatsStyle};

mod theme;
pub use theme::{DesktopNotifications, IconMode, Theme, TuiSettings};

mod tool_style;
pub use tool_style::ToolStyle;

mod general;
pub use general::{SettingPatch, update_general_setting};

mod limits;
pub use limits::{RgSettings, ToolLimits, ToolsSettings};

mod sandbox;
pub use sandbox::{SandboxMode, SandboxReadScope, SandboxSettings};

mod web_search;
pub use web_search::{
    DEFAULT_WEB_SEARCH_TIMEOUT_SECONDS, MAX_WEB_SEARCH_TIMEOUT_SECONDS, SearxngSettings,
    WebSearchProvider, WebSearchSettings,
};

// Deliberately not Debug: this owns the API credential.
pub struct Config {
    pub auth: Option<crate::auth::Config>,
    pub thinking: models::ModelThinking,
    pub protocol: models::Protocol,
    pub api_key: String,
    pub model: String,
    // Full provider/model configuration reference; differs from the provider's actual wire model name (the model field).
    pub model_reference: String,
    pub context_window: Option<u64>,
    pub prompt_cache_key: bool,
    pub prompt_cache_retention: Option<models::PromptCacheRetention>,
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
    /// The agent.db connection opened at construction: read-only access such as `model_usage` reuses it instead of reopening per call;
    /// writes still open their own connection and transaction, and the `Mutex` serializes reads across threads.
    agent_store: Arc<Mutex<crate::agent_store::AgentStore>>,
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

/// `--model` resolution result: the full reference plus the resolved concrete effort level.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelSelection {
    pub reference: String,
    pub effort: ThinkLevel,
}

pub(crate) fn default_read_max_bytes() -> usize {
    65_536
}
pub(crate) fn default_read_max_lines() -> usize {
    2_000
}
pub(crate) fn default_bash_timeout_seconds() -> u64 {
    60
}

pub(crate) fn default_bash_max_timeout_seconds() -> u64 {
    600
}

fn default_notification_show_countdown() -> bool {
    true
}

fn default_notification_timeout_seconds() -> u64 {
    10
}

/// Persistence authority for the default, favorites, roles and effort; migrated from the four settings.yml fields into agent.db.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Keeps the existing validation rules of the four settings.yml fields; availability is validated at the ModelCatalog entry points.
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

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub stats: StatsSettings,
    #[serde(default)]
    pub tui: TuiSettings,
    #[serde(default)]
    pub cost: CostDisplay,
    #[serde(default)]
    pub task: crate::config::TaskSettings,
    #[serde(default)]
    pub rg: RgSettings,
    #[serde(default)]
    pub read: ReadDisplaySettings,
    #[serde(default)]
    pub tools: ToolsSettings,
    #[serde(default)]
    pub eval: EvalSettings,
    #[serde(default)]
    pub lsp: crate::config::LspSettings,
    #[serde(default)]
    pub mcp: crate::config::McpSettings,
    #[serde(default, rename = "webSearch")]
    pub web_search: WebSearchSettings,
    #[serde(default)]
    pub debug: bool,
    /// Deprecated: kept only for type validation of old configurations; no longer controls TUI display.
    #[allow(dead_code)]
    #[serde(default = "default_notification_show_countdown")]
    pub notification_show_countdown: bool,
    /// Deprecated: keeps the old value validation; error cards no longer disappear automatically.
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
    // Idle timeout for connecting to the model and for consecutive runs without data; it does not limit the total time of a whole generation.
    #[serde(default = "default_request_timeout_seconds")]
    pub request_timeout_seconds: u64,
    // Cumulative retry wait budget per model request; 0 means no retry.
    #[serde(default = "default_rate_limit_retry_budget_seconds")]
    pub rate_limit_retry_budget_seconds: u64,
    #[serde(default = "default_read_max_bytes")]
    pub read_max_bytes: usize,
    #[serde(default = "default_read_max_lines")]
    pub read_max_lines: usize,
    #[serde(default = "default_bash_timeout_seconds")]
    pub bash_timeout_seconds: u64,
    #[serde(default = "default_bash_max_timeout_seconds")]
    pub bash_max_timeout_seconds: u64,
    #[serde(default = "default_auto_compact")]
    pub auto_compact: bool,
    #[serde(default = "default_auto_compact_threshold_percent")]
    pub auto_compact_threshold_percent: u8,
    #[serde(default = "default_auto_prune")]
    pub auto_prune: bool,
    #[serde(default = "default_auto_prune_threshold_percent")]
    pub auto_prune_threshold_percent: u8,
}

impl Settings {
    pub fn tool_limits(&self) -> ToolLimits {
        ToolLimits {
            read_max_bytes: self.read_max_bytes,
            read_max_lines: self.read_max_lines,
            bash_timeout_seconds: self.bash_timeout_seconds,
            bash_max_timeout_seconds: self.bash_max_timeout_seconds,
            eval_timeout_seconds: self.eval.timeout_seconds,
            eval_max_timeout_seconds: self.eval.max_timeout_seconds,
            rg: self.rg,
            output: self.tools.output,
        }
    }

    pub(crate) fn validate(&self) -> Result<()> {
        self.lsp.validate()?;
        self.mcp.validate()?;
        self.eval.validate()?;
        self.web_search.validate()?;
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
            (1..=99).contains(&self.auto_prune_threshold_percent),
            "auto_prune_threshold_percent must be between 1 and 99"
        );
        ensure!(
            (1..=32).contains(&self.task.max_concurrency),
            "task.max_concurrency must be between 1 and 32"
        );
        ensure!(
            self.task.max_recursion_depth <= 8,
            "task.max_recursion_depth must not exceed 8"
        );
        if let Some(tokens) = self.task.token_budget {
            ensure!(tokens > 0, "task.token_budget must be greater than zero");
        }
        if let Some(usd) = self.task.cost_budget_usd {
            ensure!(
                usd.is_finite() && usd > 0.0,
                "task.cost_budget_usd must be positive and finite"
            );
        }
        self.tool_limits().validate()?;
        self.tools.sandbox.validate()?;
        ensure!(
            self.tools.permission.iter().all(|(name, _)| {
                !name.trim().is_empty()
                    && (PERMISSION_TOOL_NAMES.contains(&name.as_str()) || name.starts_with("mcp__"))
            }),
            "tools.permission keys must be non-blank built-in tool names or mcp__ prefixes"
        );
        Ok(())
    }
}

/// Built-in tool names allowed by the permission policy; MCP tools are accepted only through the `mcp__` prefix to catch typos.
const PERMISSION_TOOL_NAMES: &[&str] = &[
    "read",
    "rg",
    "glob",
    "web_search",
    "lsp",
    "todo",
    "ask",
    "edit",
    "write",
    "bash",
    "bash_task",
    "eval",
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

impl Default for Settings {
    fn default() -> Self {
        default_settings()
    }
}

fn default_settings() -> Settings {
    Settings {
        stats: StatsSettings::default(),
        task: Default::default(),
        rg: RgSettings::default(),
        read: ReadDisplaySettings::default(),
        tui: TuiSettings::default(),
        cost: CostDisplay::default(),
        tools: ToolsSettings::default(),
        eval: EvalSettings::default(),
        lsp: Default::default(),
        mcp: McpSettings::default(),
        web_search: WebSearchSettings::default(),
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
        bash_max_timeout_seconds: default_bash_max_timeout_seconds(),
        auto_compact: default_auto_compact(),
        auto_compact_threshold_percent: default_auto_compact_threshold_percent(),
        auto_prune: default_auto_prune(),
        auto_prune_threshold_percent: default_auto_prune_threshold_percent(),
    }
}

fn default_request_timeout_seconds() -> u64 {
    300
}

fn default_rate_limit_retry_budget_seconds() -> u64 {
    1022
}

fn default_auto_compact() -> bool {
    true
}

fn default_auto_compact_threshold_percent() -> u8 {
    80
}

fn default_auto_prune() -> bool {
    true
}

fn default_auto_prune_threshold_percent() -> u8 {
    60
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

    /// User-level LSP server definitions live in the same configuration directory as settings.yml.
    ///
    /// The Agent passes this path explicitly when installing the Registry, and the Registry itself no longer reads HOME,
    /// so tests can isolate it completely with a temporary directory.
    pub fn lsp_path(&self) -> PathBuf {
        self.settings_path.with_file_name("lsp.json")
    }

    pub fn lsp_settings(&self) -> crate::config::LspSettings {
        self.settings.lsp
    }

    pub fn mcp_settings(&self) -> crate::config::McpSettings {
        self.settings.mcp
    }

    pub fn eval_settings(&self) -> &crate::config::EvalSettings {
        &self.settings.eval
    }

    pub fn web_search_settings(&self) -> &WebSearchSettings {
        &self.settings.web_search
    }

    /// User-level hooks configuration lives in the same configuration directory as settings.yml.
    pub fn hooks_path(&self) -> PathBuf {
        self.settings_path.with_file_name("hooks.yml")
    }

    pub fn read_tool_result_preview(&self) -> bool {
        self.settings.read.tool_result_preview
    }
    pub fn from_files(settings_path: &Path, models_path: &Path) -> Result<Self> {
        Self::load(settings_path, models_path, false)
    }

    /// Startup entry point: the caller has already completed the one-time migration and parsed and validated `settings.yml`.
    ///
    /// It differs from [`Self::from_files`] only in the two steps the caller already did: it neither re-reads
    /// `settings.yml` nor opens agent.db for a second migration check. agent.db is opened exactly once here,
    /// and [`Self::model_usage`] then reuses the same connection.
    pub fn from_migrated_settings(
        settings: Settings,
        settings_path: &Path,
        models_path: &Path,
    ) -> Result<Self> {
        Self::with_settings(settings, settings_path, models_path, false, None)
    }

    /// Reuses the `AgentStore` connection already opened by the startup migration check, avoiding repeated WAL and `-shm` churn.
    pub fn from_migrated_settings_with_store(
        settings: Settings,
        settings_path: &Path,
        models_path: &Path,
        agent_store: crate::agent_store::AgentStore,
    ) -> Result<Self> {
        Self::with_settings(
            settings,
            settings_path,
            models_path,
            false,
            Some(agent_store),
        )
    }

    fn load(settings_path: &Path, models_path: &Path, allow_empty: bool) -> Result<Self> {
        // The one-time migration runs before any settings.yml parsing: the four legacy fields are imported into agent.db and stripped from the file.
        // Paths where `main::run` already migrated at startup use [`Self::from_migrated_settings_with_store`] instead.
        let dir = settings_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let store = crate::agent_store::initialize_store(
            dir,
            &pp_core::cancellation::CancellationToken::new(),
        )?;
        let settings: Settings = read_settings(settings_path)?;
        settings
            .validate()
            .with_context(|| format!("loading YAML settings config {}", settings_path.display()))?;
        Self::with_settings(
            settings,
            settings_path,
            models_path,
            allow_empty,
            Some(store),
        )
    }

    fn with_settings(
        settings: Settings,
        settings_path: &Path,
        models_path: &Path,
        allow_empty: bool,
        agent_store: Option<crate::agent_store::AgentStore>,
    ) -> Result<Self> {
        let agent_db_path = settings_path.with_file_name("agent.db");
        let agent_store = match agent_store {
            Some(store) => store,
            None => crate::agent_store::AgentStore::open(&agent_db_path)?,
        };
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
            agent_store: Arc::new(Mutex::new(agent_store)),
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

    pub fn sandbox_settings(&self) -> &crate::config::SandboxSettings {
        &self.settings.tools.sandbox
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
                    context_window: model
                        .context_window
                        .or_else(|| models::builtin_context_window(&model.model)),
                })
            })
            .collect()
    }

    /// Fuzzy selection: `[provider/]model[:effort]`; the provider may be omitted, and an omitted effort uses the model's default level.
    ///
    /// Text matching tiers as exact → prefix → substring → subsequence (case-insensitive); within a tier favorites win,
    /// the rest keep catalog order. This only reads the existing catalog and preferences and never writes agent.db.
    pub fn select(&self, query: &str) -> Result<ModelSelection> {
        // Error text echoes the user's original input, so keep the untrimmed string first.
        let raw = query;
        let query = query.trim();
        ensure!(!query.is_empty(), "model must not be empty");
        // Explicit effort suffix; `default` is not an effort level, so it never collides with a model id.
        let (query, effort) = match query.rsplit_once(':') {
            Some((head, tail)) if !head.is_empty() => match explicit_effort(tail.trim()) {
                Some(level) => (head, Some(level)),
                None => (query, None),
            },
            _ => (query, None),
        };
        // The provider prefix applies only when its left part names a configured provider; model ids themselves may contain `/`.
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
            // The reference looks like `provider/model_id`; everything after the first `/` belongs to the model id.
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
        // Catalog refresh leaves runtime parameters untouched; preferences always come from agent.db, and the in-memory state is replaced only after everything validates.
        let next = Self::with_settings(
            self.settings.clone(),
            &self.settings_path,
            &self.settings_path.with_file_name("models.yml"),
            true,
            None,
        )?;
        self.providers = next.providers;
        self.pricing = next.pricing;
        self.auth_generations = next.auth_generations;
        self.default_reference = next.default_reference;
        self.preferences = next.preferences;
        Ok(())
    }

    /// Installs the full preference snapshot returned once the transaction commits and recomputes the default reference by the exact priority.
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
            context_window: model
                .context_window
                .or_else(|| models::builtin_context_window(&model.model)),
            prompt_cache_key: model.prompt_cache_key,
            prompt_cache_retention: model.prompt_cache_retention,
            base_url: match provider.auth {
                Some(auth) => Url::parse(auth.base_url()).expect("built-in provider URL is valid"),
                None => models::openai_base_url(provider.base_url.clone()),
            },
            agent_db_path: self.agent_db_path.clone(),
            settings: self.settings.clone(),
        })
    }

    /// Reads the accumulated usage count per logical request; a read failure still yields the fixed redacted error.
    ///
    /// Reuses the connection opened at construction, so refreshing the model panel never opens agent.db again.
    pub fn model_usage(&self) -> Result<std::collections::BTreeMap<String, u64>> {
        self.agent_store
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .model_usage()
    }
}

/// Explicit effort suffix: only the six level names are accepted (case-insensitive); `default` is reserved for the model id.
fn explicit_effort(value: &str) -> Option<ThinkLevel> {
    ThinkLevel::ALL
        .into_iter()
        .find(|level| level.as_str().eq_ignore_ascii_case(value))
}

/// Text match tier: `0` exact, `1` prefix, `2` substring, `3` subsequence; returns `None` when nothing matches.
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

/// Every character of `needle` appears in order in `haystack`; both are already lowercase.
fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut haystack = haystack.chars();
    needle
        .chars()
        .all(|needle| haystack.any(|candidate| candidate == needle))
}

/// Default reference priority: the enabled Default Role → default_model → the first available model;
/// an unavailable plain default reference falls back to the first available model.
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

#[cfg(test)]
mod tests;
