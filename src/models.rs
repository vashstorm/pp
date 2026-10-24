use anyhow::{Context, Result, ensure};
use reqwest::Url;
use serde::Deserialize;
use std::{fs, path::Path};

use crate::{config::ThinkLevel, yaml};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ModelThinking {
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
    pub(crate) fn supports(self, level: ThinkLevel) -> bool {
        self.allowed & (1 << level as u8) != 0
    }

    pub(crate) fn levels(self) -> impl Iterator<Item = ThinkLevel> {
        ThinkLevel::ALL
            .into_iter()
            .filter(move |&level| self.supports(level))
    }

    pub(crate) fn explicit_default(self) -> Option<ThinkLevel> {
        self.default_level
    }

    pub(crate) fn effective_default(self, global: ThinkLevel) -> ThinkLevel {
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

    pub(crate) fn next(self, current: ThinkLevel) -> ThinkLevel {
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
struct ModelsFile {
    #[serde(deserialize_with = "deserialize_named_entries")]
    providers: Vec<(String, ProviderEntry)>,
}

// 映射按文件顺序收集，避免改变默认模型和菜单顺序。
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Protocol {
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

// Deliberately not Debug: this owns the API credential.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderEntry {
    #[serde(default)]
    protocol: Protocol,
    api_key: String,
    #[serde(default, deserialize_with = "deserialize_optional_base_url")]
    base_url: Option<String>,
    #[serde(deserialize_with = "deserialize_named_entries")]
    models: Vec<(String, ModelEntry)>,
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
#[serde(deny_unknown_fields)]
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
}

#[derive(Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ModelCost {
    pub(crate) input: f64,
    #[serde(rename = "cacheRead")]
    pub(crate) cache_read: f64,
    #[serde(
        rename = "cacheWrite",
        default,
        deserialize_with = "deserialize_optional_rate"
    )]
    pub(crate) cache_write: Option<f64>,
    pub(crate) output: f64,
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
    if [
        Some(cost.input),
        Some(cost.cache_read),
        cost.cache_write,
        Some(cost.output),
    ]
    .iter()
    .flatten()
    .any(|rate| !rate.is_finite() || *rate < 0.0)
    {
        return Err(serde::de::Error::custom(
            "cost must contain non-negative finite rates",
        ));
    }
    Ok(Some(cost))
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
pub(crate) struct ValidatedProvider {
    pub(crate) id: String,
    pub(crate) protocol: Protocol,
    pub(crate) api_key: String,
    pub(crate) base_url: Option<Url>,
    pub(crate) models: Vec<ValidatedModel>,
}

pub(crate) struct ValidatedModel {
    pub(crate) id: String,
    pub(crate) model: String,
    pub(crate) cost: Option<ModelCost>,
    pub(crate) thinking: ModelThinking,
    pub(crate) context_window: Option<u64>,
}

// Validate every provider and model before selecting anything.
pub(crate) fn load_models(path: &Path) -> Result<Vec<ValidatedProvider>> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("reading YAML models config {}", path.display()))?;
    let ModelsFile { providers } = yaml::parse(&text, "models")
        .with_context(|| format!("loading YAML models config {}", path.display()))?;
    validate_providers(providers)
        .with_context(|| format!("loading YAML models config {}", path.display()))
}

fn validate_providers(providers: Vec<(String, ProviderEntry)>) -> Result<Vec<ValidatedProvider>> {
    ensure!(!providers.is_empty(), "providers must not be empty");
    for (id, _) in &providers {
        ensure!(!id.trim().is_empty(), "provider id must not be blank");
        ensure!(!id.contains('/'), "provider id must not contain '/'");
    }
    providers
        .into_iter()
        .map(|(id, provider)| {
            ensure!(
                !provider.api_key.trim().is_empty(),
                "api_key must not be blank"
            );
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
            Ok(ValidatedProvider {
                id,
                protocol: provider.protocol,
                api_key: provider.api_key,
                base_url,
                models: provider
                    .models
                    .into_iter()
                    .map(|(id, model)| {
                        Ok(ValidatedModel {
                            id,
                            model: model.model,
                            cost: model.cost,
                            thinking: model.thinking.validate()?,
                            context_window: model.context_window,
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
}
