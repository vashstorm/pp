use anyhow::{Context, Result, ensure};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use crate::{models, yaml};

// Deliberately not Debug: this owns the API credential.
pub(crate) struct Config {
    pub(crate) thinking: models::ModelThinking,
    pub(crate) protocol: models::Protocol,
    pub(crate) api_key: String,
    pub(crate) model: String,
    pub(crate) context_window: Option<u64>,
    pub(crate) base_url: Url,
    pub(crate) settings: Settings,
}

// Deliberately not Debug: the catalog owns all startup credentials.
pub(crate) struct ModelCatalog {
    settings: Settings,
    settings_path: PathBuf,
    providers: Vec<models::ValidatedProvider>,
    default_reference: String,
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

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Settings {
    #[serde(default)]
    pub(crate) debug: bool,
    #[serde(default = "default_notification_show_countdown")]
    pub(crate) notification_show_countdown: bool,
    #[serde(default = "default_notification_timeout_seconds")]
    pub(crate) notification_timeout_seconds: u64,
    #[serde(default)]
    pub(crate) mcp_servers: std::collections::BTreeMap<String, crate::mcp::ServerConfig>,
    #[serde(default = "default_skill_dirs")]
    pub(crate) skill_dirs: Vec<String>,
    #[serde(default)]
    pub(crate) disabled_skills: Vec<String>,
    #[serde(default)]
    pub(crate) think_level: ThinkLevel,
    #[serde(default)]
    pub(crate) edit: EditMode,
    // None means "use the first provider's first model".
    #[serde(default)]
    pub(crate) default_model: Option<String>,
    #[serde(default = "default_max_model_requests")]
    pub(crate) max_model_requests: usize,
    #[serde(default = "default_request_timeout_seconds")]
    pub(crate) request_timeout_seconds: u64,
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
        crate::mcp::validate_config(&self.mcp_servers)?;
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
        ensure!(
            self.max_model_requests > 0,
            "max_model_requests must be greater than zero"
        );
        ensure!(
            self.request_timeout_seconds > 0,
            "request_timeout_seconds must be greater than zero"
        );
        ensure!(
            (1..=99).contains(&self.auto_compact_threshold_percent),
            "auto_compact_threshold_percent must be between 1 and 99"
        );
        self.tool_limits().validate()?;
        Ok(())
    }
}

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
        debug: false,
        notification_show_countdown: default_notification_show_countdown(),
        notification_timeout_seconds: default_notification_timeout_seconds(),
        mcp_servers: Default::default(),
        skill_dirs: default_skill_dirs(),
        disabled_skills: Vec::new(),
        think_level: ThinkLevel::None,
        edit: EditMode::Replace,
        default_model: None,
        max_model_requests: default_max_model_requests(),
        request_timeout_seconds: default_request_timeout_seconds(),
        read_max_bytes: default_read_max_bytes(),
        read_max_lines: default_read_max_lines(),
        bash_timeout_seconds: default_bash_timeout_seconds(),
        auto_compact: default_auto_compact(),
        auto_compact_threshold_percent: default_auto_compact_threshold_percent(),
    }
}

fn default_max_model_requests() -> usize {
    20
}

fn default_request_timeout_seconds() -> u64 {
    120
}

fn default_auto_compact() -> bool {
    true
}

fn default_auto_compact_threshold_percent() -> u8 {
    80
}

impl ModelCatalog {
    pub(crate) fn from_files(settings_path: &Path, models_path: &Path) -> Result<Self> {
        let settings: Settings = read_settings(settings_path)?;
        settings
            .validate()
            .with_context(|| format!("loading YAML settings config {}", settings_path.display()))?;
        let providers = models::load_models(models_path)?;
        let first_reference = format!("{}/{}", providers[0].id, providers[0].models[0].id);
        let default_reference = match settings.default_model.as_deref() {
            Some(reference) => {
                let (provider_id, model_id) = reference
                    .split_once('/')
                    .expect("validated default_model is a composite reference");
                let matched = providers.iter().any(|provider| {
                    provider.id == provider_id
                        && provider.models.iter().any(|model| model.id == model_id)
                });
                if matched {
                    reference.to_owned()
                } else {
                    first_reference
                }
            }
            None => first_reference,
        };
        Ok(Self {
            settings,
            settings_path: settings_path.to_path_buf(),
            providers,
            default_reference,
        })
    }

    pub(crate) fn edit_mode(&self) -> EditMode {
        self.settings.edit
    }

    pub(crate) fn default_think_level(&self) -> ThinkLevel {
        self.settings.think_level
    }

    pub(crate) fn save_default_think_level(&self, level: ThinkLevel) -> Result<()> {
        update_settings(&self.settings_path, |_| {
            Ok((
                "think_level",
                serde_yaml::Value::String(level.as_str().into()),
            ))
        })
    }

    pub(crate) fn save_default_model(&self, reference: &str) -> Result<()> {
        update_settings(&self.settings_path, |_| {
            Ok(("default_model", serde_yaml::Value::String(reference.into())))
        })
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

    pub(crate) fn resolve(&self, reference: &str) -> Result<Config> {
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
            protocol: provider.protocol,
            thinking: model.thinking,
            api_key: provider.api_key.clone(),
            model: model.model.clone(),
            context_window: model.context_window,
            base_url: models::openai_base_url(provider.base_url.clone()),
            settings: self.settings.clone(),
        })
    }
}

pub(crate) fn update_settings(
    path: &Path,
    update: impl FnOnce(Settings) -> Result<(&'static str, serde_yaml::Value)>,
) -> Result<()> {
    let read = || match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).context("Failed to read settings.yml"),
    };
    let original = read()?;
    let settings: Settings = yaml::parse(original.as_deref().unwrap_or("{}"), "settings")?;
    settings.validate()?;
    let (name, value) = update(settings)?;
    let updated = yaml::update_field(original.as_deref().unwrap_or("{}\n"), name, value)?;
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
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn notification_options_have_defaults_and_strict_overrides() {
        for source in [None, Some("{}")] {
            let options = load(source, Some(VALID_MODELS))
                .unwrap()
                .settings
                .notification_options();
            assert!(options.show_countdown);
            assert_eq!(options.timeout_seconds, 10);
        }
        let settings = load(
            Some(r#"{"notification_show_countdown":false,"notification_timeout_seconds":3}"#),
            Some(VALID_MODELS),
        )
        .unwrap()
        .settings;
        assert!(!settings.notification_options().show_countdown);
        assert_eq!(settings.notification_options().timeout_seconds, 3);
        for value in [
            json!(0),
            json!(-1),
            json!(1.5),
            json!(null),
            json!(true),
            json!("secret-marker"),
            json!(u64::MAX),
        ] {
            rejection(
                Some(&json!({"notification_timeout_seconds":value}).to_string()),
                Some(VALID_MODELS),
                &["settings.yml"],
            );
        }
        rejection(
            Some(r#"{"notification_timeout_seconds":18446744073709551616}"#),
            Some(VALID_MODELS),
            &["settings.yml"],
        );
        for value in [json!(0), json!(null), json!("secret-marker")] {
            rejection(
                Some(&json!({"notification_show_countdown":value}).to_string()),
                Some(VALID_MODELS),
                &["settings.yml"],
            );
        }
    }

    #[test]
    fn tool_limits_have_defaults_and_strict_positive_overrides() {
        let defaults = load(None, Some(VALID_MODELS))
            .unwrap()
            .settings
            .tool_limits();
        assert_eq!(defaults.read_max_bytes, 65536);
        assert_eq!(defaults.read_max_lines, 2000);
        assert_eq!(defaults.bash_timeout_seconds, 120);
        let settings = load(
            Some(r#"{"read_max_bytes":17,"read_max_lines":3,"bash_timeout_seconds":2}"#),
            Some(VALID_MODELS),
        )
        .unwrap()
        .settings;
        assert_eq!(settings.read_max_bytes, 17);
        assert_eq!(settings.read_max_lines, 3);
        assert_eq!(settings.bash_timeout_seconds, 2);
        for field in ["read_max_bytes", "read_max_lines", "bash_timeout_seconds"] {
            for value in [
                json!(0),
                json!(-1),
                json!(null),
                json!(true),
                json!(1.5),
                json!("secret-marker-invalid"),
            ] {
                let settings = json!({field:value}).to_string();
                rejection(Some(&settings), Some(VALID_MODELS), &["settings.yml"]);
            }
        }
    }

    const VALID_SETTINGS: &str = r#"{"default_model":"fixture/primary"}"#;
    const VALID_MODELS: &str = r#"{"providers":[{
        "id":"fixture","api_key":"secret-marker-fixture-key",
        "models":[{"id":"primary","model":"fixture-model"}]
    }]}"#;

    #[test]
    fn model_cost_accepts_existing_rates_and_rejects_invalid_unselected_models() {
        let mut models: serde_json::Value = serde_json::from_str(VALID_MODELS).unwrap();
        models["providers"][0]["models"][0]["cost"] =
            json!({"input":0.8,"output":2.8,"cacheRead":0.23});
        let configured = catalog(None, Some(&models.to_string())).unwrap();
        let cost = configured.providers[0].models[0].cost.unwrap();
        assert_eq!(cost.input, 0.8);
        assert_eq!(cost.output, 2.8);
        assert_eq!(cost.cache_read, 0.23);
        assert_eq!(cost.cache_write, None);
        models["providers"][0]["models"][0]["cost"]["cacheWrite"] = json!(1.25);
        assert_eq!(
            catalog(None, Some(&models.to_string())).unwrap().providers[0].models[0]
                .cost
                .unwrap()
                .cache_write,
            Some(1.25)
        );
        for invalid in [
            json!(null),
            json!({"input":-1,"output":1,"cacheRead":0}),
            json!({"input":1,"output":1}),
            json!({"input":"secret-marker","output":1,"cacheRead":0}),
            json!({"input":1,"output":1,"cacheRead":0,"typo":0}),
            json!({"input":1,"output":1,"cacheRead":0,"cacheWrite":-1}),
            json!({"input":1,"output":1,"cacheRead":0,"cacheWrite":null}),
            json!({"input":1,"output":1,"cacheRead":0,"cacheWrite":"secret-marker"}),
        ] {
            models["providers"][0]["models"]
                .as_array_mut()
                .unwrap()
                .truncate(1);
            models["providers"][0]["models"]
                .as_array_mut()
                .unwrap()
                .push(json!({"id":"unselected","model":"fixture-model","cost":invalid}));
            rejection(None, Some(&models.to_string()), &["models.yml"]);
        }
    }

    fn catalog(settings: Option<&str>, models: Option<&str>) -> Result<ModelCatalog> {
        let dir = tempfile::tempdir().unwrap();
        let settings_path = dir.path().join("settings.yml");
        let models_path = dir.path().join("models.yml");
        if let Some(settings) = settings {
            fs::write(&settings_path, settings).unwrap();
        }
        if let Some(models) = models {
            fs::write(&models_path, models).unwrap();
        }
        ModelCatalog::from_files(&settings_path, &models_path)
    }

    fn load(settings: Option<&str>, models: Option<&str>) -> Result<Config> {
        let catalog = catalog(settings, models)?;
        catalog.resolve(catalog.default_reference())
    }

    fn report(error: &anyhow::Error) -> String {
        format!("{error:#}")
    }

    fn rejection(settings: Option<&str>, models: Option<&str>, files: &[&str]) -> String {
        let error = load(settings, models)
            .err()
            .expect("invalid config was accepted");
        let chain = report(&error);
        assert!(!chain.contains("secret-marker"), "{chain}");
        for file in files {
            assert!(chain.contains(file), "{chain}");
        }
        chain
    }

    #[test]
    fn omitted_base_url_uses_default_openai_api_root() {
        let config = load(Some(VALID_SETTINGS), Some(VALID_MODELS)).unwrap();
        assert_eq!(config.base_url.as_str(), "https://api.openai.com/v1");
        assert_eq!(config.protocol, models::Protocol::Response);
    }

    #[test]
    fn think_level_config_accepts_exact_levels_and_rejects_invalid_values() {
        for level in ["none", "default", "low", "medium", "high", "xhigh", "max"] {
            let settings = json!({"think_level": level}).to_string();
            let config = load(Some(&settings), Some(VALID_MODELS)).unwrap();
            assert_eq!(
                config.settings.think_level.as_str(),
                if level == "default" { "none" } else { level }
            );
        }
        for value in [
            json!(null),
            json!(0),
            json!(true),
            json!("HIGH"),
            json!(" high"),
            json!("ultra"),
            json!("no think"),
            json!("NONE"),
        ] {
            rejection(
                Some(&json!({"think_level":value}).to_string()),
                Some(VALID_MODELS),
                &["settings.yml"],
            );
        }
        assert_eq!(
            load(None, Some(VALID_MODELS)).unwrap().settings.think_level,
            ThinkLevel::None
        );
        assert_eq!(
            load(Some("{}"), Some(VALID_MODELS))
                .unwrap()
                .settings
                .think_level,
            ThinkLevel::None
        );
    }

    #[test]
    fn model_thinking_validates_sets_defaults_and_unselected_models() {
        let make = |thinking: serde_json::Value| {
            let mut models: serde_json::Value = serde_json::from_str(VALID_MODELS).unwrap();
            models["providers"][0]["models"][0]["thinking"] = thinking;
            models
        };
        for thinking in [
            json!({}),
            json!({"efforts":[]}),
            json!({"defaultLevel":"default"}),
        ] {
            let config = load(None, Some(&make(thinking).to_string())).unwrap();
            assert!(config.thinking.supports(ThinkLevel::None));
            assert_eq!(
                config.thinking.levels().collect::<Vec<_>>(),
                ThinkLevel::ALL
            );
        }
        let config = load(
            None,
            Some(&make(json!({"efforts":["high","low","high"],"defaultLevel":"high"})).to_string()),
        )
        .unwrap();
        let policy = config.thinking;
        assert_eq!(
            policy.levels().collect::<Vec<_>>(),
            [ThinkLevel::Low, ThinkLevel::High]
        );
        assert_eq!(policy.effective_default(ThinkLevel::Max), ThinkLevel::High);
        assert_eq!(policy.explicit_default(), Some(ThinkLevel::High));
        assert_eq!(policy.next(ThinkLevel::High), ThinkLevel::Low);
        assert_eq!(policy.next(ThinkLevel::Low), ThinkLevel::High);
        assert_eq!(policy.next(ThinkLevel::Max), ThinkLevel::Low);
        let config = load(
            None,
            Some(&make(json!({"efforts":["xhigh","medium"]})).to_string()),
        )
        .unwrap();
        assert_eq!(
            config.thinking.effective_default(ThinkLevel::Max),
            ThinkLevel::Medium
        );
        assert_eq!(
            config.thinking.effective_default(ThinkLevel::Xhigh),
            ThinkLevel::Xhigh
        );
        let config = load(None, Some(&make(json!({"efforts":["low"]})).to_string())).unwrap();
        assert_eq!(config.thinking.next(ThinkLevel::Low), ThinkLevel::Low);
        let mut invalid = vec![
            json!(null),
            json!(true),
            json!([]),
            json!("secret-marker"),
            json!({"unknown":"secret-marker"}),
            json!({"efforts":["low"],"defaultLevel":"high"}),
        ];
        for value in [
            json!(null),
            json!(true),
            json!(1),
            json!(1.5),
            json!("secret-marker"),
            json!({}),
        ] {
            invalid.push(json!({"efforts":value}));
            invalid.push(json!({"defaultLevel":value}));
        }
        for value in [
            json!(null),
            json!(true),
            json!(1),
            json!("none"),
            json!("default"),
            json!("HIGH"),
            json!(" high"),
            json!("high "),
            json!("secret-marker"),
            json!([]),
        ] {
            invalid.push(json!({"efforts":[value]}));
        }
        for value in ["HIGH", " high", "high ", "secret-marker"] {
            invalid.push(json!({"defaultLevel":value}));
        }
        for thinking in invalid {
            let models = make(thinking);
            rejection(None, Some(&models.to_string()), &["models.yml"]);
            let mut unselected: serde_json::Value = serde_json::from_str(VALID_MODELS).unwrap();
            let mut model = models["providers"][0]["models"][0].clone();
            model["id"] = json!("unselected");
            unselected["providers"][0]["models"]
                .as_array_mut()
                .unwrap()
                .push(model);
            rejection(None, Some(&unselected.to_string()), &["models.yml"]);
        }
        let error = rejection(
            None,
            Some(&make(json!({"efforts":["none"]})).to_string()),
            &["models.yml"],
        );
        assert!(error.contains("thinking.efforts must contain only low, medium, high, xhigh, max"));
        let error = rejection(
            None,
            Some(&make(json!({"efforts":["low"],"defaultLevel":"high"})).to_string()),
            &["models.yml"],
        );
        assert!(error.contains("thinking.defaultLevel must be included in thinking.efforts"));
    }

    #[test]
    fn missing_or_empty_settings_default_to_first_provider_first_model() {
        let providers = r#"{"providers":[
            {"id":"z-first","protocol":"completion","api_key":"first-key","base_url":"https://a.example/v1","models":[
                {"id":"z-first","model":"first-model"},
                {"id":"a-second","model":"not-selected"}
            ]},
            {"id":"a-second","api_key":"second-key","models":[
                {"id":"primary","model":"also-not-selected"}
            ]}
        ]}"#;
        for settings in [None, Some("{}"), Some(r#"{"max_model_requests":5}"#)] {
            let config = load(settings, Some(providers)).unwrap();
            assert_eq!(config.settings.edit, EditMode::Replace);
            assert_eq!(config.api_key, "first-key");
            assert_eq!(config.model, "first-model");
            assert_eq!(config.base_url.as_str(), "https://a.example/v1");
            assert_eq!(config.protocol, models::Protocol::Completion);
        }
        rejection(None, None, &["models.yml"]);
    }

    #[test]
    fn explicit_values_select_named_provider_model() {
        let config = load(
            Some(r#"{"default_model":"second/secondary","max_model_requests":3,"request_timeout_seconds":7}"#),
            Some(
                r#"{"providers":[
                    {"id":"first","protocol":"response","api_key":"first-key","base_url":"https://a.example/v1","models":[
                        {"id":"primary","model":"first-primary"},
                        {"id":"secondary","model":"first-secondary"}
                    ]},
                    {"id":"second","protocol":"completion","api_key":"second-key","base_url":"http://localhost:8080/custom/v1/","models":[
                        {"id":"primary","model":"second-primary"},
                        {"id":"secondary","model":"second-secondary"}
                    ]}
                ]}"#,
            ),
        )
        .unwrap();
        assert_eq!(config.api_key, "second-key");
        assert_eq!(config.model, "second-secondary");
        assert_eq!(config.base_url.as_str(), "http://localhost:8080/custom/v1");
        assert_eq!(config.protocol, models::Protocol::Completion);
    }

    #[test]
    fn edit_modes_are_strict_and_redacted() {
        for (mode, expected) in [
            ("replace", EditMode::Replace),
            ("hashline", EditMode::Hashline),
        ] {
            let settings = json!({"edit": mode}).to_string();
            assert_eq!(
                load(Some(&settings), Some(VALID_MODELS))
                    .unwrap()
                    .settings
                    .edit,
                expected
            );
        }
        for value in [
            json!(""),
            json!("Replace"),
            json!("HASHLINE"),
            json!(" replace"),
            json!("hashline "),
            json!("secret-marker"),
            json!(null),
            json!(1),
            json!(true),
            json!(["secret-marker"]),
            json!({"secret-marker": true}),
        ] {
            let settings = json!({"edit": value}).to_string();
            rejection(Some(&settings), Some(VALID_MODELS), &["settings.yml"]);
        }
    }

    #[test]
    fn protocol_defaults_and_explicit_values_follow_selected_provider() {
        assert_eq!(models::Protocol::default(), models::Protocol::Response);
        let providers = r#"{"providers":[
            {"id":"completion","protocol":"completion","api_key":"completion-key","models":[
                {"id":"primary","model":"shared-model"}
            ]},
            {"id":"default","api_key":"default-key","models":[
                {"id":"primary","model":"shared-model"}
            ]},
            {"id":"response","protocol":"response","api_key":"response-key","models":[
                {"id":"primary","model":"shared-model"}
            ]}
        ]}"#;
        for (provider, expected) in [
            ("completion", models::Protocol::Completion),
            ("default", models::Protocol::Response),
            ("response", models::Protocol::Response),
        ] {
            let settings = json!({"default_model": format!("{provider}/primary")}).to_string();
            let config = load(Some(&settings), Some(providers)).unwrap();
            assert_eq!(config.protocol, expected);
            assert_eq!(config.model, "shared-model");
            assert_eq!(config.api_key, format!("{provider}-key"));
            assert_eq!(config.base_url.as_str(), "https://api.openai.com/v1");
        }
    }

    #[test]
    fn yaml_comments_and_multiline_strings_preserve_string_contents() {
        let config = load(
            Some("# 应用设置\ndefault_model: fixture/primary\n"),
            Some(
                r#"# 同一供应商的模型共享凭证
providers:
  - id: fixture
    api_key: |-
      key//with/*literal*/"quote # literal
    base_url: http://localhost:8080/custom/v1/ # 行尾注释
    models:
      - id: primary
        model: tool-model
"#,
            ),
        )
        .unwrap();
        assert_eq!(config.api_key, "key//with/*literal*/\"quote # literal");
        assert_eq!(config.model, "tool-model");
        assert_eq!(config.base_url.as_str(), "http://localhost:8080/custom/v1");
    }

    #[test]
    fn malformed_composite_references_are_rejected_without_values() {
        for reference in [
            "",
            "  ",
            "primary",
            "fixture",
            "secret-marker",
            "/",
            "/secret-marker",
            "secret-marker/",
            " /secret-marker",
            "secret-marker/ \t",
        ] {
            let settings = json!({"default_model": reference}).to_string();
            rejection(Some(&settings), Some(VALID_MODELS), &["settings.yml"]);
        }
    }

    #[test]
    fn unmatched_composite_references_fall_back_to_first_provider_first_model() {
        let models = r#"{"providers":[
            {"id":"first","api_key":"first-key","models":[
                {"id":"primary","model":"first-model"}
            ]},
            {"id":"second","api_key":"second-key","models":[
                {"id":"secondary","model":"second-model"}
            ]}
        ]}"#;
        for reference in [
            "secret-marker/primary",
            "first/secret-marker",
            "second/primary",
        ] {
            let settings = json!({"default_model": reference}).to_string();
            let config = load(Some(&settings), Some(models)).unwrap();
            assert_eq!(config.api_key, "first-key");
            assert_eq!(config.model, "first-model");
        }
    }

    #[test]
    fn provider_and_model_ids_match_exactly_and_split_only_at_first_slash() {
        let models = r#"{"providers":[
            {"id":" Fixture","api_key":"secret-marker","models":[
                {"id":" Primary","model":"spaced"},
                {"id":"team/model","model":"slashed"}
            ]},
            {"id":"fixture","api_key":"secret-marker","models":[
                {"id":"primary","model":"lowercase"}
            ]},
            {"id":"Fixture","api_key":"secret-marker","models":[
                {"id":"primary","model":"mixed-case-provider"},
                {"id":" primary","model":"spaced-model"},
                {"id":"PRIMARY","model":"uppercase-model"}
            ]}
        ]}"#;
        for (reference, expected) in [
            (" Fixture/ Primary", "spaced"),
            (" Fixture/team/model", "slashed"),
            ("fixture/primary", "lowercase"),
            ("Fixture/primary", "mixed-case-provider"),
            ("Fixture/ primary", "spaced-model"),
            ("Fixture/PRIMARY", "uppercase-model"),
        ] {
            let settings = json!({"default_model": reference}).to_string();
            assert_eq!(load(Some(&settings), Some(models)).unwrap().model, expected);
        }
        for reference in [
            "Fixture/ Primary",
            " fixture/ Primary",
            " Fixture/Primary",
            " Fixture/ primary",
            "FIXTURE/primary",
            "fixture/PRIMARY",
            "fixture/primary ",
            "fixture/team/model",
        ] {
            let settings = json!({"default_model": reference}).to_string();
            assert_eq!(load(Some(&settings), Some(models)).unwrap().model, "spaced");
        }
    }

    #[test]
    fn provider_and_model_scope_constraints_reject_whole_configuration() {
        let provider = json!({
            "id": "fixture", "api_key": "secret-marker",
            "models": [{"id": "primary", "model": "m"}]
        });
        let mut cases = vec![
            json!({"providers": []}),
            json!({"providers": [provider.clone(), provider.clone()]}),
        ];
        for id in ["", " \t", "secret-marker/provider"] {
            let mut invalid = provider.clone();
            invalid["id"] = json!(id);
            cases.push(json!({"providers": [invalid]}));
        }
        for models in [
            json!([]),
            json!([{"id": "", "model": "secret-marker"}]),
            json!([{"id": " \t", "model": "secret-marker"}]),
            json!([
                {"id": "primary", "model": "secret-marker-first"},
                {"id": "primary", "model": "secret-marker-second"}
            ]),
        ] {
            let mut invalid = provider.clone();
            invalid["models"] = models;
            cases.push(json!({"providers": [invalid]}));
        }
        for models in cases {
            rejection(
                Some(VALID_SETTINGS),
                Some(&models.to_string()),
                &["models.yml"],
            );
        }
    }

    #[test]
    fn invalid_settings_limits_and_numeric_types_are_rejected() {
        for value in [
            "0",
            "-1",
            "1.5",
            "20.0",
            "\"secret-marker\"",
            "null",
            "1e30",
            "18446744073709551616",
        ] {
            for field in ["max_model_requests", "request_timeout_seconds"] {
                let settings =
                    format!(r#"{{"default_model":"fixture/primary","{field}":{value}}}"#);
                rejection(Some(&settings), Some(VALID_MODELS), &["settings.yml"]);
            }
        }
    }

    #[test]
    fn unknown_and_misplaced_fields_and_old_format_are_rejected() {
        for settings in [
            r#"{"default_model":"fixture/primary","secret-marker":true}"#,
            r#"{"default_model":"fixture/primary","api_key":"secret-marker"}"#,
            r#"{"default_model":"fixture/primary","models":["secret-marker"]}"#,
        ] {
            rejection(Some(settings), Some(VALID_MODELS), &["settings.yml"]);
        }
        let valid: serde_json::Value = serde_json::from_str(VALID_MODELS).unwrap();
        let mut cases = vec![
            // The removed flat format is intentionally accepted nowhere.
            json!({"models": [{"id": "primary", "api_key": "secret-marker", "model": "m"}]}),
        ];
        for level in 0..3 {
            let mut invalid = valid.clone();
            let object = match level {
                0 => &mut invalid,
                1 => &mut invalid["providers"][0],
                _ => &mut invalid["providers"][0]["models"][0],
            };
            object["secret-marker"] = json!(true);
            cases.push(invalid);
        }
        for (field, value) in [
            ("api_key", json!("secret-marker")),
            ("base_url", json!("https://secret-marker.example/v1")),
            ("protocol", json!("completion")),
            ("max_model_requests", json!(2)),
        ] {
            let mut invalid = valid.clone();
            invalid["providers"][0]["models"][0][field] = value;
            cases.push(invalid);
        }
        for models in cases {
            rejection(
                Some(VALID_SETTINGS),
                Some(&models.to_string()),
                &["models.yml"],
            );
        }
    }

    #[test]
    fn protocol_and_base_url_reject_null_wrong_types_and_unknown_protocols() {
        let valid: serde_json::Value = serde_json::from_str(VALID_MODELS).unwrap();
        for field in ["protocol", "base_url"] {
            for value in [
                json!(null),
                json!(true),
                json!(12),
                json!(["secret-marker"]),
                json!({"secret-marker": true}),
                json!({"completion": null}),
                json!({"response": null}),
            ] {
                let mut invalid = valid.clone();
                invalid["providers"][0][field] = value;
                rejection(
                    Some(VALID_SETTINGS),
                    Some(&invalid.to_string()),
                    &["models.yml"],
                );
            }
        }
        for protocol in [
            "",
            "openai",
            "OpenAI",
            "OPENAI",
            "anthropic",
            "Anthropic",
            "Completion",
            "Response",
            "completion ",
            " response",
            "secret-marker",
        ] {
            let mut invalid = valid.clone();
            invalid["providers"][0]["protocol"] = json!(protocol);
            rejection(
                Some(VALID_SETTINGS),
                Some(&invalid.to_string()),
                &["models.yml"],
            );
        }
    }

    #[test]
    fn syntax_type_and_semantic_errors_do_not_disclose_values() {
        for models in [
            r#"{"providers":[{"id":"fixture","api_key":["secret-marker"],"models":[{"id":"primary","model":"m"}]}]}"#,
            r#"{"providers":[{"id":"fixture","api_key":"secret-marker","models":[{"id":"primary","model":""}]}]}"#,
            r#"{"providers":[{"id":"fixture","api_key":" ","models":[{"id":"primary","model":"secret-marker"}]}]}"#,
            r#"{"providers":[{"id":"fixture","api_key":"secret-marker","base_url":"","models":[{"id":"primary","model":"m"}]}]}"#,
            r#"{"providers":[{"id":"fixture","api_key":"secret-marker","base_url":"ftp://secret-marker","models":[{"id":"primary","model":"m"}]}]}"#,
            r#"{"providers":[{"id":"fixture","api_key":"secret-marker" "models":[{"id":"primary","model":"m"}]}]}"#,
            r#"{"providers":[{"id":"fixture","api_key":"secret-marker", /* unterminated"#,
        ] {
            rejection(Some(VALID_SETTINGS), Some(models), &["models.yml"]);
        }
        for settings in [
            r#"{"default_model":["secret-marker"]}"#,
            r#"{"default_model":true,"secret-marker":1}"#,
            r#"{"default_model" "secret-marker"}"#,
        ] {
            rejection(Some(settings), Some(VALID_MODELS), &["settings.yml"]);
        }
    }

    #[test]
    fn unselected_providers_and_models_are_validated_before_selection() {
        let valid: serde_json::Value = serde_json::from_str(VALID_MODELS).unwrap();
        let other = json!({
            "id": "other", "api_key": "secret-marker",
            "models": [{"id": "primary", "model": "other-model"}]
        });
        let mut invalid_providers = Vec::new();
        for (field, value) in [
            ("api_key", json!(" \t")),
            ("protocol", json!("openai")),
            ("protocol", json!("anthropic")),
            ("protocol", json!("secret-marker")),
            ("protocol", json!(null)),
            ("protocol", json!({"completion": null})),
            ("protocol", json!({"response": null})),
            ("base_url", json!("not-a-url-secret-marker")),
            (
                "base_url",
                json!("https://secret-marker.example/v1?token=secret-marker"),
            ),
            ("id", json!("")),
            ("models", json!([])),
            ("models", json!([{"id": "primary", "model": " \t"}])),
            (
                "models",
                json!([
                    {"id": "primary", "model": "secret-marker"},
                    {"id": "primary", "model": "duplicate"}
                ]),
            ),
        ] {
            let mut invalid = other.clone();
            invalid[field] = value;
            invalid_providers.push(invalid);
        }
        for provider in invalid_providers {
            let mut invalid = valid.clone();
            invalid["providers"].as_array_mut().unwrap().push(provider);
            for settings in [
                VALID_SETTINGS,
                r#"{"default_model":"secret-marker/missing"}"#,
            ] {
                rejection(Some(settings), Some(&invalid.to_string()), &["models.yml"]);
            }
        }
        for model in [
            json!({"id": "other", "model": " \t"}),
            json!({"id": "", "model": "secret-marker"}),
            json!({"id": "primary", "model": "secret-marker"}),
        ] {
            let mut invalid = valid.clone();
            invalid["providers"][0]["models"]
                .as_array_mut()
                .unwrap()
                .push(model);
            rejection(
                Some(VALID_SETTINGS),
                Some(&invalid.to_string()),
                &["models.yml"],
            );
        }
    }

    #[test]
    fn anthropic_is_rejected_whether_selected_or_not() {
        for base_url in [None, Some("https://secret-marker.example/custom")] {
            let mut anthropic = json!({
                "id": "anthropic", "protocol": "anthropic", "api_key": "secret-marker",
                "models": [{"id": "primary", "model": "secret-marker-model"}]
            });
            if let Some(base_url) = base_url {
                anthropic["base_url"] = json!(base_url);
            }
            let valid: serde_json::Value = serde_json::from_str(VALID_MODELS).unwrap();
            let models = json!({"providers": [anthropic, valid["providers"][0]]}).to_string();
            for settings in [
                None,
                Some("{}"),
                Some(VALID_SETTINGS),
                Some(r#"{"default_model":"anthropic/primary"}"#),
                Some(r#"{"default_model":"anthropic/secret-marker"}"#),
            ] {
                rejection(settings, Some(&models), &["models.yml"]);
            }
        }
    }

    #[test]
    fn missing_and_unparseable_files_are_reported_per_file() {
        rejection(Some(VALID_SETTINGS), None, &["models.yml"]);
        for settings in ["{", "{\"default_model\" \"secret-marker\"}", ""] {
            let chain = rejection(Some(settings), Some(VALID_MODELS), &["settings.yml"]);
            assert!(chain.contains("invalid YAML settings config"), "{chain}");
        }
        for models in ["{", "{\"providers\": \"secret-marker\"", ""] {
            let chain = rejection(Some(VALID_SETTINGS), Some(models), &["models.yml"]);
            assert!(chain.contains("invalid YAML models config"), "{chain}");
        }
    }
    #[test]
    fn skill_settings_default_empty_lists_and_strict_field_types() {
        let defaults = load(None, Some(VALID_MODELS)).unwrap().settings;
        assert_eq!(defaults.skill_dirs, ["~/.pp/agent/skills"]);
        assert!(defaults.disabled_skills.is_empty());
        let settings = load(
            Some(r#"{"skill_dirs":[],"disabled_skills":[]}"#),
            Some(VALID_MODELS),
        )
        .unwrap()
        .settings;
        assert!(settings.skill_dirs.is_empty());
        for field in ["skill_dirs", "disabled_skills"] {
            for value in [
                json!(null),
                json!("secret-marker"),
                json!([null]),
                json!([5]),
                json!([""]),
                json!(["  "]),
            ] {
                rejection(
                    Some(&json!({field:value}).to_string()),
                    Some(VALID_MODELS),
                    &["settings.yml"],
                );
            }
        }
    }

    #[test]
    fn context_window_omission_means_unknown_and_explicit_values_propagate() {
        let default_catalog = catalog(None, Some(VALID_MODELS)).unwrap();
        assert!(
            default_catalog
                .resolve(default_catalog.default_reference())
                .unwrap()
                .context_window
                .is_none()
        );
        assert!(
            default_catalog
                .choices()
                .iter()
                .all(|choice| choice.context_window.is_none())
        );
        let models = r#"{"providers":[{
            "id":"fixture","api_key":"fixture-key",
            "models":[
                {"id":"primary","model":"primary-model","context_window":32768},
                {"id":"secondary","model":"secondary-model"}
            ]
        }]}"#;
        let catalog = catalog(
            Some(r#"{"default_model":"fixture/secondary"}"#),
            Some(models),
        )
        .unwrap();
        // The unselected model keeps its explicit capacity; the selected one
        // stays unknown, so capacities are per model and never inferred.
        assert_eq!(
            catalog
                .resolve(catalog.default_reference())
                .unwrap()
                .context_window,
            None
        );
        assert_eq!(
            catalog.resolve("fixture/primary").unwrap().context_window,
            Some(32768)
        );
        let choices = catalog.choices();
        let window = |reference: &str| {
            choices
                .iter()
                .find(|choice| choice.reference == reference)
                .unwrap()
                .context_window
        };
        assert_eq!(window("fixture/primary"), Some(32768));
        assert_eq!(window("fixture/secondary"), None);
        let models = r#"{"providers":[{"id":"fixture","api_key":"fixture-key",
            "models":[{"id":"primary","model":"m","context_window":18446744073709551615}]}]}"#;
        assert_eq!(
            load(Some(VALID_SETTINGS), Some(models))
                .unwrap()
                .context_window,
            Some(u64::MAX)
        );
    }

    #[test]
    fn context_window_rejects_invalid_values_including_unselected_models() {
        for value in [
            "null",
            "0",
            "-1",
            "1.5",
            "20.0",
            "true",
            "\"secret-marker\"",
            "[]",
            "{}",
            "1e30",
            "18446744073709551616",
        ] {
            let selected = format!(
                r#"{{"providers":[{{"id":"fixture","api_key":"fixture-key","models":[{{"id":"primary","model":"m","context_window":{value}}}]}}]}}"#
            );
            rejection(Some(VALID_SETTINGS), Some(&selected), &["models.yml"]);
            let unselected_model = format!(
                r#"{{"providers":[{{"id":"fixture","api_key":"fixture-key","models":[{{"id":"primary","model":"m"}},{{"id":"other","model":"m","context_window":{value}}}]}}]}}"#
            );
            rejection(
                Some(VALID_SETTINGS),
                Some(&unselected_model),
                &["models.yml"],
            );
            let unselected_provider = format!(
                r#"{{"providers":[{{"id":"fixture","api_key":"fixture-key","models":[{{"id":"primary","model":"m"}}]}},{{"id":"other","api_key":"fixture-key","models":[{{"id":"primary","model":"m","context_window":{value}}}]}}]}}"#
            );
            rejection(
                Some(VALID_SETTINGS),
                Some(&unselected_provider),
                &["models.yml"],
            );
        }
        // Zero reaches semantic validation, so the fixed diagnostic names it.
        let models = r#"{"providers":[{"id":"fixture","api_key":"fixture-key",
            "models":[{"id":"primary","model":"m","context_window":0}]}]}"#;
        let chain = rejection(Some(VALID_SETTINGS), Some(models), &["models.yml"]);
        assert!(
            chain.contains("context_window must be a positive integer"),
            "{chain}"
        );
    }

    #[test]
    fn context_auto_compact_defaults_on_at_80_with_strict_bounds() {
        for settings in [None, Some("{}")] {
            let settings = load(settings, Some(VALID_MODELS)).unwrap().settings;
            assert!(settings.auto_compact);
            assert_eq!(settings.auto_compact_threshold_percent, 80);
        }
        let settings = load(
            Some(r#"{"auto_compact":false,"auto_compact_threshold_percent":42}"#),
            Some(VALID_MODELS),
        )
        .unwrap()
        .settings;
        assert!(!settings.auto_compact);
        assert_eq!(settings.auto_compact_threshold_percent, 42);
        for threshold in [1u8, 99] {
            let settings = json!({"auto_compact_threshold_percent": threshold}).to_string();
            assert_eq!(
                load(Some(&settings), Some(VALID_MODELS))
                    .unwrap()
                    .settings
                    .auto_compact_threshold_percent,
                threshold
            );
        }
        for value in [
            "0",
            "100",
            "255",
            "256",
            "-1",
            "1.5",
            "\"secret-marker\"",
            "null",
            "true",
            "1e2",
            "18446744073709551616",
        ] {
            let settings = format!(r#"{{"auto_compact_threshold_percent":{value}}}"#);
            rejection(Some(&settings), Some(VALID_MODELS), &["settings.yml"]);
        }
        for value in ["\"secret-marker\"", "null", "0", "1"] {
            let settings = format!(r#"{{"auto_compact":{value}}}"#);
            rejection(Some(&settings), Some(VALID_MODELS), &["settings.yml"]);
        }
        // u8 范围内的无效值返回固定语义校验提示，类型错误由 YAML 解析器统一脱敏。
        let settings = r#"{"auto_compact_threshold_percent":100}"#;
        let chain = rejection(Some(settings), Some(VALID_MODELS), &["settings.yml"]);
        assert!(
            chain.contains("auto_compact_threshold_percent must be between 1 and 99"),
            "{chain}"
        );
    }
}
