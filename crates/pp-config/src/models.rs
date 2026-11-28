use anyhow::{Context, Result, ensure};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::Path,
    sync::{Arc, LazyLock},
};

use crate::yaml;
use pp_core::model::ThinkLevel;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModelThinking {
    allowed: u8,
    default_level: Option<ThinkLevel>,
}

impl Default for ModelThinking {
    fn default() -> Self {
        Self {
            allowed: (1 << ThinkLevel::ALL.len()) - 1,
            default_level: None,
        }
    }
}

impl ModelThinking {
    pub fn without_reasoning() -> Self {
        Self {
            allowed: 1,
            default_level: Some(ThinkLevel::None),
        }
    }

    pub fn supports(self, level: ThinkLevel) -> bool {
        self.allowed & (1 << level as u8) != 0
    }

    pub fn levels(self) -> impl Iterator<Item = ThinkLevel> {
        ThinkLevel::ALL
            .into_iter()
            .filter(move |&level| self.supports(level))
    }

    pub fn explicit_default(self) -> Option<ThinkLevel> {
        self.default_level
    }

    pub fn effective_default(self, global: ThinkLevel) -> ThinkLevel {
        self.default_level.unwrap_or_else(|| {
            if self.supports(global) {
                global
            } else {
                self.first()
            }
        })
    }

    fn first(self) -> ThinkLevel {
        self.levels()
            .next()
            .expect("validated thinking set is nonempty")
    }

    pub fn next(self, current: ThinkLevel) -> ThinkLevel {
        if !self.supports(current) {
            return self.first();
        }
        self.levels()
            .skip_while(|&level| level != current)
            .nth(1)
            .unwrap_or_else(|| self.first())
    }
}

#[derive(Default, Deserialize)]
struct ThinkingEntry {
    #[serde(default)]
    efforts: Vec<ThinkLevel>,
    #[serde(
        rename = "defaultLevel",
        default,
        deserialize_with = "deserialize_optional_think_level"
    )]
    default_level: Option<ThinkLevel>,
}

fn deserialize_optional_think_level<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<ThinkLevel>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    ThinkLevel::deserialize(deserializer).map(Some)
}

impl ThinkingEntry {
    fn validate(self) -> Result<ModelThinking> {
        let mut policy = ModelThinking::default();
        if !self.efforts.is_empty() {
            policy.allowed = 0;
            for level in self.efforts {
                ensure!(
                    level != ThinkLevel::None,
                    "thinking.efforts must contain only low, medium, high, xhigh, max"
                );
                policy.allowed |= 1 << level as u8;
            }
        }
        if let Some(level) = self.default_level {
            ensure!(
                policy.supports(level),
                "thinking.defaultLevel must be included in thinking.efforts"
            );
        }
        policy.default_level = self.default_level;
        Ok(policy)
    }
}

#[derive(Deserialize)]
struct ModelsFile {
    #[serde(deserialize_with = "deserialize_named_entries")]
    providers: Vec<(String, ProviderEntry)>,
}

// Collect mappings in file order so the default model and menu order never change.
fn deserialize_named_entries<'de, D, T>(
    deserializer: D,
) -> std::result::Result<Vec<(String, T)>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct EntriesVisitor<T>(std::marker::PhantomData<T>);

    impl<'de, T: Deserialize<'de>> serde::de::Visitor<'de> for EntriesVisitor<T> {
        type Value = Vec<(String, T)>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a mapping of ids to entries")
        }

        fn visit_map<M: serde::de::MapAccess<'de>>(
            self,
            mut map: M,
        ) -> std::result::Result<Self::Value, M::Error> {
            let mut entries = Vec::with_capacity(map.size_hint().unwrap_or(0));
            while let Some(entry) = map.next_entry()? {
                entries.push(entry);
            }
            Ok(entries)
        }
    }

    deserializer.deserialize_map(EntriesVisitor(std::marker::PhantomData))
}

#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    Completion,
    #[default]
    Response,
}

impl std::fmt::Display for Protocol {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Completion => "completion",
            Self::Response => "response",
        })
    }
}

impl<'de> Deserialize<'de> for Protocol {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct ProtocolVisitor;

        impl serde::de::Visitor<'_> for ProtocolVisitor {
            type Value = Protocol;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("the string \"completion\" or \"response\"")
            }

            fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                match value {
                    "completion" => Ok(Protocol::Completion),
                    "response" => Ok(Protocol::Response),
                    _ => Err(E::custom("protocol must be completion or response")),
                }
            }
        }

        deserializer.deserialize_str(ProtocolVisitor)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PromptCacheRetention {
    #[serde(rename = "in_memory")]
    InMemory,
    #[serde(rename = "24h")]
    Hours24,
}

// Deliberately not Debug: this owns the API credential.
#[derive(Deserialize)]
struct ProviderEntry {
    #[serde(default, deserialize_with = "deserialize_present_option")]
    protocol: Option<Protocol>,
    #[serde(default, deserialize_with = "deserialize_present_option")]
    auth: Option<crate::auth::Provider>,
    #[serde(default)]
    api_key: String,
    #[serde(default, deserialize_with = "deserialize_optional_base_url")]
    base_url: Option<String>,
    #[serde(default, deserialize_with = "deserialize_present_option")]
    prompt_cache_key: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_present_option")]
    prompt_cache_retention: Option<PromptCacheRetention>,
    #[serde(deserialize_with = "deserialize_named_entries")]
    models: Vec<(String, ModelEntry)>,
}

fn deserialize_present_option<'de, D, T>(
    deserializer: D,
) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

fn deserialize_optional_base_url<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    String::deserialize(deserializer).map(Some)
}

#[derive(Deserialize)]
struct ModelEntry {
    model: String,
    #[serde(default, deserialize_with = "deserialize_optional_cost")]
    cost: Option<ModelCost>,
    #[serde(default)]
    thinking: ThinkingEntry,
    // Omitted means unknown; an explicit value must be a positive integer.
    // deserialize_with makes an explicit null a type error instead of None.
    #[serde(default, deserialize_with = "deserialize_optional_context_window")]
    context_window: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_present_option")]
    prompt_cache_key: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_present_option")]
    prompt_cache_retention: Option<PromptCacheRetention>,
}

#[derive(Clone, Copy, PartialEq, serde::Serialize, Deserialize)]
pub struct ModelCost {
    pub input: f64,
    #[serde(rename = "cacheRead")]
    pub cache_read: f64,
    #[serde(
        rename = "cacheWrite",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_rate"
    )]
    pub cache_write: Option<f64>,
    pub output: f64,
}

#[derive(Default, serde::Serialize)]
pub struct TokenCost {
    pub input: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub output: f64,
}

impl TokenCost {
    pub fn total(&self) -> f64 {
        self.input + self.cache_read + self.cache_write + self.output
    }
}

impl ModelCost {
    fn valid(self) -> bool {
        [
            Some(self.input),
            Some(self.cache_read),
            self.cache_write,
            Some(self.output),
        ]
        .into_iter()
        .flatten()
        .all(|rate| rate.is_finite() && rate >= 0.0)
    }

    /// Shares strict pricing with the statistics page: unknown cache read volume must not be billed as zero.
    ///
    /// When the cache write volume is unknown and the write rate is either unconfigured or equal to the input rate, that cost folds into regular input.
    /// When the write rate differs from the input rate, the write volume is required to price the request.
    pub fn calculate(self, usage: pp_core::context::TokenUsage) -> Option<TokenCost> {
        let cached = usage.cached_tokens?;
        let written = match (usage.cache_write_tokens, self.cache_write) {
            (Some(written), _) => written,
            (None, rate) if rate.is_none_or(|rate| rate == self.input) => 0,
            (None, _) => return None,
        };
        let uncached = usage
            .input_tokens
            .checked_sub(cached)?
            .checked_sub(written)?;
        let cost = TokenCost {
            input: uncached as f64 * (self.input / 1e6),
            cache_read: cached as f64 * (self.cache_read / 1e6),
            cache_write: if written == 0 {
                0.0
            } else {
                written as f64 * (self.cache_write? / 1e6)
            },
            output: usage.output_tokens as f64 * (self.output / 1e6),
        };
        cost.total().is_finite().then_some(cost)
    }
}

fn deserialize_optional_rate<'de, D>(deserializer: D) -> std::result::Result<Option<f64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    f64::deserialize(deserializer).map(Some)
}

fn deserialize_optional_cost<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<ModelCost>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let cost = ModelCost::deserialize(deserializer)?;
    if !cost.valid() {
        return Err(serde::de::Error::custom(
            "cost must contain non-negative finite rates",
        ));
    }
    Ok(Some(cost))
}

static BUILTIN_PRICES: LazyLock<BTreeMap<String, ModelCost>> = LazyLock::new(|| {
    let prices: BTreeMap<String, ModelCost> =
        yaml::parse(include_str!("model-prices.yml"), "model prices")
            .expect("built-in model price table is well formed");
    assert!(
        !prices.is_empty()
            && prices.iter().all(|(name, rate)| {
                !name.is_empty()
                    && name.bytes().all(|byte| {
                        byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"-._".contains(&byte)
                    })
                    && rate.valid()
            }),
        "built-in model price table contains an invalid name or rate"
    );
    prices
});

/// Explicit rates of configuration references plus built-in rates matched by the actual model name.
#[derive(Clone, Default)]
pub struct ModelPricing {
    pub(crate) overrides: Arc<BTreeMap<String, ModelCost>>,
}

impl ModelPricing {
    pub fn from_providers(providers: &[ValidatedProvider]) -> Self {
        Self {
            overrides: Arc::new(
                providers
                    .iter()
                    .flat_map(|provider| {
                        provider.models.iter().filter_map(|model| {
                            model
                                .cost
                                .map(|cost| (format!("{}/{}", provider.id, model.id), cost))
                        })
                    })
                    .collect(),
            ),
        }
    }

    pub fn rate(&self, reference: &str, model: &str) -> Option<ModelCost> {
        self.overrides.get(reference).copied().or_else(|| {
            let name = model.rsplit('/').next()?;
            BUILTIN_PRICES.get(&name.to_ascii_lowercase()).copied()
        })
    }
}

/// Official context window fallback table for mainstream OpenAI models: it applies only when neither `models.yml` nor subscription discovery
/// declares `context_window`, matching the exact model name (ignoring the prefix path and ASCII case), and never guessing capacity for unknown models.
pub fn builtin_context_window(model: &str) -> Option<u64> {
    let name = model.rsplit('/').next()?.to_ascii_lowercase();
    match name.as_str() {
        "gpt-6-astra" | "gpt-6.1-sol" | "gpt-6.1-terra" | "gpt-6-luna" | "gpt-6-sol"
        | "gpt-6-cyber" | "gpt-6-rosalind" | "gpt-5.5" | "gpt-5.5-2026-04-23" | "gpt-5.4"
        | "gpt-5.4-2026-03-05" | "gpt-5.4-mini" | "gpt-5.3-codex" | "gpt-5" | "gpt-5-mini"
        | "gpt-5-codex" => Some(400_000),
        "gpt-4.1" | "gpt-4.1-mini" | "gpt-4.1-nano" => Some(1_047_576),
        "o3" | "o4-mini" => Some(200_000),
        "gpt-4o" | "gpt-4o-mini" => Some(128_000),
        _ => None,
    }
}

fn deserialize_optional_context_window<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    u64::deserialize(deserializer).map(Some)
}

// Deliberately not Debug: this owns the API credential.
#[derive(Clone)]
pub struct ValidatedProvider {
    pub id: String,
    pub(crate) protocol: Protocol,
    pub(crate) api_key: String,
    pub(crate) auth: Option<crate::auth::Provider>,
    pub(crate) base_url: Option<Url>,
    pub models: Vec<ValidatedModel>,
}

#[derive(Clone)]
pub struct ValidatedModel {
    pub id: String,
    pub(crate) model: String,
    pub cost: Option<ModelCost>,
    pub(crate) thinking: ModelThinking,
    pub(crate) context_window: Option<u64>,
    pub(crate) prompt_cache_key: bool,
    pub(crate) prompt_cache_retention: Option<PromptCacheRetention>,
}

// Validate every provider and model before selecting anything.
pub fn load_models(path: &Path) -> Result<Vec<ValidatedProvider>> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("reading YAML models config {}", path.display()))?;
    let ModelsFile { providers } = yaml::parse(&text, "models")
        .with_context(|| format!("loading YAML models config {}", path.display()))?;
    validate_providers(providers)
        .with_context(|| format!("loading YAML models config {}", path.display()))
}

fn validate_providers(providers: Vec<(String, ProviderEntry)>) -> Result<Vec<ValidatedProvider>> {
    for (id, _) in &providers {
        ensure!(!id.trim().is_empty(), "provider id must not be blank");
        ensure!(!id.contains('/'), "provider id must not contain '/'");
    }
    providers
        .into_iter()
        .map(|(id, provider)| {
            let protocol = provider.protocol.unwrap_or_else(|| {
                provider
                    .auth
                    .map_or(Protocol::default(), crate::auth::Provider::protocol)
            });
            ensure!(
                provider.auth.is_some() || !provider.api_key.trim().is_empty(),
                "api_key must not be blank (or configure auth)"
            );
            if let Some(auth) = provider.auth {
                ensure!(
                    provider.api_key.is_empty(),
                    "auth and api_key cannot be used together"
                );
                ensure!(
                    provider.base_url.is_none(),
                    "auth uses a fixed provider endpoint; omit base_url"
                );
                ensure!(
                    matches!(
                        auth,
                        crate::auth::Provider::GithubCopilot
                            | crate::auth::Provider::CommandCode
                            | crate::auth::Provider::OpencodeZen
                            | crate::auth::Provider::OpencodeGo
                    ) || protocol == auth.protocol(),
                    "protocol is incompatible with auth provider"
                );
            }
            let base_url = provider
                .base_url
                .as_deref()
                .map(parse_base_url)
                .transpose()?;
            ensure!(!provider.models.is_empty(), "models must not be empty");
            for (id, model) in &provider.models {
                ensure!(!id.trim().is_empty(), "model id must not be blank");
                ensure!(!model.model.trim().is_empty(), "model must not be blank");
                if let Some(context_window) = model.context_window {
                    ensure!(
                        context_window > 0,
                        "context_window must be a positive integer"
                    );
                }
            }
            let provider_cache_key = provider.prompt_cache_key.unwrap_or(true);
            let provider_cache_retention = provider.prompt_cache_retention;
            Ok(ValidatedProvider {
                id,
                protocol,
                api_key: provider.api_key,
                auth: provider.auth,
                base_url,
                models: provider
                    .models
                    .into_iter()
                    .map(|(id, model)| {
                        Ok(ValidatedModel {
                            id,
                            model: model.model,
                            cost: model.cost,
                            thinking: if matches!(
                                provider.auth,
                                Some(
                                    crate::auth::Provider::KimiCode
                                        | crate::auth::Provider::Glm
                                        | crate::auth::Provider::Zai
                                )
                            ) {
                                ensure!(
                                    model.thinking.efforts.is_empty()
                                        && model
                                            .thinking
                                            .default_level
                                            .is_none_or(|level| level == ThinkLevel::None),
                                    "this auth provider currently requires thinking disabled"
                                );
                                ModelThinking::without_reasoning()
                            } else {
                                model.thinking.validate()?
                            },
                            context_window: model.context_window,
                            prompt_cache_key: model.prompt_cache_key.unwrap_or(provider_cache_key),
                            prompt_cache_retention: model
                                .prompt_cache_retention
                                .or(provider_cache_retention),
                        })
                    })
                    .collect::<Result<_>>()?,
            })
        })
        .collect()
}

fn parse_base_url(base: &str) -> Result<Url> {
    ensure!(!base.trim().is_empty(), "base_url must not be blank");
    let url = Url::parse(base)
        .map_err(|_| anyhow::anyhow!("base_url must be a valid absolute HTTP(S) URL"))?;
    ensure!(
        matches!(url.scheme(), "http" | "https") && url.host_str().is_some(),
        "base_url must use HTTP(S) with a host"
    );
    ensure!(
        url.username().is_empty()
            && url.password().is_none()
            && !base
                .split("://")
                .nth(1)
                .unwrap_or("")
                .split(['/', '\\', '?', '#'])
                .next()
                .unwrap_or("")
                .contains('@'),
        "base_url must not contain userinfo"
    );
    ensure!(
        url.query().is_none() && url.fragment().is_none(),
        "base_url must not contain a query or fragment"
    );
    Ok(url)
}

pub(crate) fn openai_base_url(base_url: Option<Url>) -> Url {
    let mut url = base_url.unwrap_or_else(|| {
        Url::parse("https://api.openai.com/v1").expect("valid default OpenAI URL")
    });
    // HTTP(S) URLs retain their mandatory root slash.
    while url.path().len() > 1 && url.path().ends_with('/') {
        url.path_segments_mut()
            .expect("validated HTTP(S) URL supports path segments")
            .pop_if_empty();
    }
    url
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_url_preserves_prefix_and_rejects_ambiguous_roots() {
        for (base, expected) in [
            (
                Some("http://localhost:1234/custom/v1///"),
                "http://localhost:1234/custom/v1",
            ),
            (Some("https://example.com"), "https://example.com/"),
            (Some("https://example.com///"), "https://example.com/"),
            (
                Some("https://example.com/custom%2Fprefix/v1/"),
                "https://example.com/custom%2Fprefix/v1",
            ),
            (None, "https://api.openai.com/v1"),
        ] {
            let base_url = base.map(|base| parse_base_url(base).unwrap());
            assert_eq!(openai_base_url(base_url).as_str(), expected);
        }
        for invalid in [
            "",
            "  ",
            "file:///tmp/x",
            "https://u:p@example.com/v1",
            "https://@example.com",
            "https://example.com?x=1",
            "https://example.com/#x",
        ] {
            assert!(parse_base_url(invalid).is_err(), "accepted {invalid}");
        }
    }

    #[test]
    fn unknown_cache_write_is_priced_when_total_does_not_depend_on_it() {
        let usage = pp_core::context::TokenUsage {
            input_tokens: 1000,
            output_tokens: 100,
            cached_tokens: Some(200),
            cache_write_tokens: None,
            reasoning_tokens: None,
        };
        let free_write = ModelCost {
            input: 2.0,
            cache_read: 0.2,
            cache_write: None,
            output: 8.0,
        };
        // Without a separate write rate, writes count toward the uncached part at the input rate, so an unknown write volume does not change the total.
        let cost = free_write
            .calculate(usage)
            .expect("pricing stays exact without a write rate");
        assert!((cost.input - 0.0016).abs() < 1e-12);
        assert!((cost.cache_read - 0.00004).abs() < 1e-12);
        assert_eq!(cost.cache_write, 0.0);
        assert!((cost.total() - 0.00244).abs() < 1e-12);
        let zero_write = free_write
            .calculate(pp_core::context::TokenUsage {
                cache_write_tokens: Some(0),
                ..usage
            })
            .expect("a zero write volume is priceable too");
        assert!((zero_write.total() - cost.total()).abs() < 1e-12);
        let priced_write = ModelCost {
            cache_write: Some(2.5),
            ..free_write
        };
        assert!(priced_write.calculate(usage).is_none());
        assert!(
            free_write
                .calculate(pp_core::context::TokenUsage {
                    cache_write_tokens: Some(100),
                    ..usage
                })
                .is_none()
        );
    }

    #[test]
    fn builtin_context_window_matches_known_openai_models_and_never_guesses_unknown() {
        assert_eq!(builtin_context_window("gpt-5.4"), Some(400_000));
        assert_eq!(
            builtin_context_window("openai/GPT-5.3-CODEX"),
            Some(400_000)
        );
        assert_eq!(builtin_context_window("gateway/gpt-4.1"), Some(1_047_576));
        assert_eq!(builtin_context_window("o3"), Some(200_000));
        assert_eq!(builtin_context_window("gpt-4o"), Some(128_000));
        assert_eq!(builtin_context_window("fixture-model"), None);
        assert_eq!(builtin_context_window("custom-private-llm"), None);
    }

    #[test]
    fn prompt_cache_settings_inherit_from_provider_and_allow_model_override() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models.yml");
        fs::write(
            &path,
            r#"providers:
  default_provider:
    api_key: secret-a
    models:
      plain:
        model: gpt-5.4
  custom_provider:
    api_key: secret-b
    prompt_cache_key: false
    prompt_cache_retention: "24h"
    models:
      inherited:
        model: gpt-5.4
      overridden:
        model: gpt-5.4-mini
        prompt_cache_key: true
        prompt_cache_retention: in_memory
"#,
        )
        .unwrap();
        let providers = load_models(&path).unwrap();
        let plain = &providers[0].models[0];
        assert!(plain.prompt_cache_key);
        assert_eq!(plain.prompt_cache_retention, None);

        let inherited = &providers[1].models[0];
        assert!(!inherited.prompt_cache_key);
        assert_eq!(
            inherited.prompt_cache_retention,
            Some(PromptCacheRetention::Hours24)
        );

        let overridden = &providers[1].models[1];
        assert!(overridden.prompt_cache_key);
        assert_eq!(
            overridden.prompt_cache_retention,
            Some(PromptCacheRetention::InMemory)
        );

        for invalid in [
            "providers:\n  p:\n    api_key: k\n    prompt_cache_retention: 1h\n    models:\n      m:\n        model: gpt-5.4\n",
            "providers:\n  p:\n    api_key: k\n    prompt_cache_key: null\n    models:\n      m:\n        model: gpt-5.4\n",
        ] {
            fs::write(&path, invalid).unwrap();
            assert!(load_models(&path).is_err(), "{invalid}");
        }
    }
}
