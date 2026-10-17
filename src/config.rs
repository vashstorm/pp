use anyhow::{Context, Result, ensure};
use reqwest::Url;
use serde::Deserialize;
use std::{
    fs,
    path::{Path, PathBuf},
};

use crate::{jsonc, models};

// Deliberately not Debug: this owns the API credential.
pub(crate) struct Config {
    pub(crate) protocol: models::Protocol,
    pub(crate) api_key: String,
    pub(crate) model: String,
    pub(crate) base_url: Url,
    pub(crate) settings: Settings,
}

// Deliberately not Debug: the catalog owns all startup credentials.
pub(crate) struct ModelCatalog {
    settings: Settings,
    providers: Vec<models::ValidatedProvider>,
    default_reference: String,
}

#[derive(Clone)]
pub(crate) struct ModelChoice {
    pub(crate) reference: String,
    pub(crate) model: String,
    pub(crate) protocol: models::Protocol,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum EditMode {
    #[default]
    Replace,
    Hashline,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
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

    pub(crate) fn next(self) -> Self {
        match self {
            Self::None => Self::Low,
            Self::Low => Self::Medium,
            Self::Medium => Self::High,
            Self::High => Self::Xhigh,
            Self::Xhigh => Self::Max,
            Self::Max => Self::None,
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

#[derive(Clone, Copy)]
pub(crate) struct ToolLimits {
    pub(crate) read_max_bytes: usize,
    pub(crate) read_max_lines: usize,
    pub(crate) bash_timeout_seconds: u64,
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

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Settings {
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
}

impl Settings {
    pub(crate) fn tool_limits(&self) -> ToolLimits {
        ToolLimits {
            read_max_bytes: self.read_max_bytes,
            read_max_lines: self.read_max_lines,
            bash_timeout_seconds: self.bash_timeout_seconds,
        }
    }

    pub(crate) fn validate(&self) -> Result<()> {
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
    }
}

fn default_max_model_requests() -> usize {
    20
}

fn default_request_timeout_seconds() -> u64 {
    120
}

impl ModelCatalog {
    pub(crate) fn from_files(settings_path: &Path, models_path: &Path) -> Result<Self> {
        let settings: Settings = read_settings(settings_path)?;
        settings.validate().with_context(|| {
            format!("loading JSONC settings config {}", settings_path.display())
        })?;
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
            providers,
            default_reference,
        })
    }

    pub(crate) fn edit_mode(&self) -> EditMode {
        self.settings.edit
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
            api_key: provider.api_key.clone(),
            model: model.model.clone(),
            base_url: models::openai_base_url(provider.base_url.clone()),
            settings: self.settings.clone(),
        })
    }
}

pub(crate) fn read_settings(path: &Path) -> Result<Settings> {
    match fs::read_to_string(path) {
        // A missing settings.jsonc is not fatal: defaults apply. Other read
        // errors (e.g. a directory or invalid UTF-8) still fail startup.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(default_settings()),
        result => {
            let text = result
                .with_context(|| format!("reading JSONC settings config {}", path.display()))?;
            jsonc::parse::<Settings>(&text, "settings")
                .and_then(|settings| {
                    settings.validate()?;
                    Ok(settings)
                })
                .with_context(|| format!("loading JSONC settings config {}", path.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
                rejection(Some(&settings), Some(VALID_MODELS), &["settings.jsonc"]);
            }
        }
    }

    const VALID_SETTINGS: &str = r#"{"default_model":"fixture/primary"}"#;
    const VALID_MODELS: &str = r#"{"providers":[{
        "id":"fixture","api_key":"secret-marker-fixture-key",
        "models":[{"id":"primary","model":"fixture-model"}]
    }]}"#;

    fn load(settings: Option<&str>, models: Option<&str>) -> Result<Config> {
        let dir = tempfile::tempdir().unwrap();
        let settings_path = dir.path().join("settings.jsonc");
        let models_path = dir.path().join("models.jsonc");
        if let Some(settings) = settings {
            fs::write(&settings_path, settings).unwrap();
        }
        if let Some(models) = models {
            fs::write(&models_path, models).unwrap();
        }
        let catalog = ModelCatalog::from_files(&settings_path, &models_path)?;
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
                &["settings.jsonc"],
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
        rejection(None, None, &["models.jsonc"]);
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
            rejection(Some(&settings), Some(VALID_MODELS), &["settings.jsonc"]);
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
    fn jsonc_comments_and_trailing_commas_preserve_string_contents() {
        let config = load(
            Some(
                r#"{
                    // App settings, not JSON5.
                    "default_model": "fixture/primary", /* trailing comma below */
                }"#,
            ),
            Some(
                r#"{
                    // Provider credentials are shared by their models.
                    "providers": [
                        {
                            "id": "fixture",
                            "api_key": "key//with/*literal*/\"quote",
                            /* comments may span lines */
                            "base_url": "http://localhost:8080/custom/v1/",
                            "models": [
                                {"id": "primary", "model": "tool-model",},
                            ],
                        },
                    ],
                }"#,
            ),
        )
        .unwrap();
        assert_eq!(config.api_key, "key//with/*literal*/\"quote");
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
            rejection(Some(&settings), Some(VALID_MODELS), &["settings.jsonc"]);
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
                &["models.jsonc"],
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
                rejection(Some(&settings), Some(VALID_MODELS), &["settings.jsonc"]);
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
            rejection(Some(settings), Some(VALID_MODELS), &["settings.jsonc"]);
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
                &["models.jsonc"],
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
                    &["models.jsonc"],
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
                &["models.jsonc"],
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
            r#"{"providers":[{"id":"fixture","api_key":'secret-marker',"models":[{"id":"primary","model":"m"}]}]}"#,
            r#"{"providers":[{"id":"fixture","api_key":"secret-marker", /* unterminated"#,
        ] {
            rejection(Some(VALID_SETTINGS), Some(models), &["models.jsonc"]);
        }
        for settings in [
            r#"{"default_model":["secret-marker"]}"#,
            r#"{"default_model":true,"secret-marker":1}"#,
            r#"{"default_model" "secret-marker"}"#,
        ] {
            rejection(Some(settings), Some(VALID_MODELS), &["settings.jsonc"]);
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
                rejection(
                    Some(settings),
                    Some(&invalid.to_string()),
                    &["models.jsonc"],
                );
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
                &["models.jsonc"],
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
                rejection(settings, Some(&models), &["models.jsonc"]);
            }
        }
    }

    #[test]
    fn missing_and_unparseable_files_are_reported_per_file() {
        rejection(Some(VALID_SETTINGS), None, &["models.jsonc"]);
        for settings in ["{", "{\"default_model\" \"secret-marker\"}", ""] {
            let chain = rejection(Some(settings), Some(VALID_MODELS), &["settings.jsonc"]);
            assert!(chain.contains("invalid JSONC settings config"), "{chain}");
        }
        for models in ["{", "{\"providers\": \"secret-marker\"", ""] {
            let chain = rejection(Some(VALID_SETTINGS), Some(models), &["models.jsonc"]);
            assert!(chain.contains("invalid JSONC models config"), "{chain}");
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
                    &["settings.jsonc"],
                );
            }
        }
    }
}
