use anyhow::{Context, Result, ensure};
use reqwest::Url;
use serde::Deserialize;
use std::{collections::HashSet, fs, path::Path};

use crate::jsonc;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelsFile {
    providers: Vec<ProviderEntry>,
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
    id: String,
    #[serde(default)]
    protocol: Protocol,
    api_key: String,
    #[serde(default, deserialize_with = "deserialize_optional_base_url")]
    base_url: Option<String>,
    models: Vec<ModelEntry>,
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
    id: String,
    model: String,
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
}

// Validate every provider and model before selecting anything.
pub(crate) fn load_models(path: &Path) -> Result<Vec<ValidatedProvider>> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("reading JSONC models config {}", path.display()))?;
    let ModelsFile { providers } = jsonc::parse(&text, "models")
        .with_context(|| format!("loading JSONC models config {}", path.display()))?;
    validate_providers(providers)
        .with_context(|| format!("loading JSONC models config {}", path.display()))
}

fn validate_providers(providers: Vec<ProviderEntry>) -> Result<Vec<ValidatedProvider>> {
    ensure!(!providers.is_empty(), "providers must not be empty");
    let mut ids = HashSet::with_capacity(providers.len());
    for provider in &providers {
        ensure!(
            !provider.id.trim().is_empty(),
            "provider id must not be blank"
        );
        ensure!(
            !provider.id.contains('/'),
            "provider id must not contain '/'"
        );
        ensure!(ids.insert(provider.id.as_str()), "duplicate provider id");
    }
    drop(ids);
    providers
        .into_iter()
        .map(|provider| {
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
            let mut ids = HashSet::with_capacity(provider.models.len());
            for model in &provider.models {
                ensure!(!model.id.trim().is_empty(), "model id must not be blank");
                ensure!(ids.insert(model.id.as_str()), "duplicate model id");
                ensure!(!model.model.trim().is_empty(), "model must not be blank");
            }
            drop(ids);
            Ok(ValidatedProvider {
                id: provider.id,
                protocol: provider.protocol,
                api_key: provider.api_key,
                base_url,
                models: provider
                    .models
                    .into_iter()
                    .map(|model| ValidatedModel {
                        id: model.id,
                        model: model.model,
                    })
                    .collect(),
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
