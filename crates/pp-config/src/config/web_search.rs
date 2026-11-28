use anyhow::{Context, Result, ensure};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use std::fmt;

pub const DEFAULT_WEB_SEARCH_TIMEOUT_SECONDS: u64 = 60;
pub const MAX_WEB_SEARCH_TIMEOUT_SECONDS: u64 = 300;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WebSearchProvider {
    #[default]
    Auto,
    Public,
    Duckduckgo,
    Startpage,
    Hackernews,
    Searxng,
}

impl WebSearchProvider {
    pub const ALL: [Self; 6] = [
        Self::Auto,
        Self::Public,
        Self::Duckduckgo,
        Self::Startpage,
        Self::Hackernews,
        Self::Searxng,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Public => "public",
            Self::Duckduckgo => "duckduckgo",
            Self::Startpage => "startpage",
            Self::Hackernews => "hackernews",
            Self::Searxng => "searxng",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::Public => "Public Web",
            Self::Duckduckgo => "DuckDuckGo",
            Self::Startpage => "Startpage",
            Self::Hackernews => "Hacker News",
            Self::Searxng => "SearXNG",
        }
    }
}

fn default_enabled() -> bool {
    true
}

fn default_keyless() -> bool {
    true
}

fn default_timeout_seconds() -> u64 {
    DEFAULT_WEB_SEARCH_TIMEOUT_SECONDS
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearxngSettings {
    pub endpoint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub basic_username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub basic_password: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engines: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub categories: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub safesearch: Option<u8>,
}

// Redact credentials so tokens and passwords never reach debug or error logs.
impl fmt::Debug for SearxngSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SearxngSettings")
            .field("endpoint", &self.endpoint)
            .field("token", &self.token.as_ref().map(|_| "[REDACTED]"))
            .field("basic_username", &self.basic_username)
            .field(
                "basic_password",
                &self.basic_password.as_ref().map(|_| "[REDACTED]"),
            )
            .field("engines", &self.engines)
            .field("categories", &self.categories)
            .field("language", &self.language)
            .field("safesearch", &self.safesearch)
            .finish()
    }
}

impl SearxngSettings {
    pub fn validate(&self) -> Result<()> {
        let trimmed = self.endpoint.trim();
        let url = Url::parse(trimmed).context("webSearch.searxng.endpoint must be a valid URL")?;
        ensure!(
            matches!(url.scheme(), "http" | "https") && url.host_str().is_some(),
            "webSearch.searxng.endpoint must use http or https with a host"
        );
        ensure!(
            url.username().is_empty() && url.password().is_none(),
            "webSearch.searxng.endpoint must not embed credentials"
        );
        ensure!(
            url.query().is_none() && url.fragment().is_none(),
            "webSearch.searxng.endpoint must not contain query parameters or fragments"
        );

        if let Some(token) = &self.token {
            ensure!(
                !token.trim().is_empty() && !token.chars().any(char::is_control),
                "webSearch.searxng.token must be non-blank and free of control characters"
            );
            reqwest::header::HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| {
                anyhow::anyhow!("webSearch.searxng.token contains invalid header characters")
            })?;
        }

        match (&self.basic_username, &self.basic_password) {
            (Some(user), Some(pass)) => {
                ensure!(
                    !user.is_empty() && !user.contains(':'),
                    "webSearch.searxng.basicUsername must be non-empty and must not contain ':'"
                );
                ensure!(
                    !user.chars().any(char::is_control) && !pass.chars().any(char::is_control),
                    "webSearch.searxng Basic auth credentials must not contain control characters"
                );
            }
            (None, None) => {}
            _ => anyhow::bail!(
                "webSearch.searxng.basicUsername and basicPassword must be configured together"
            ),
        }

        for (label, value) in [
            ("engines", &self.engines),
            ("categories", &self.categories),
            ("language", &self.language),
        ] {
            if let Some(text) = value {
                ensure!(
                    !text.trim().is_empty() && !text.chars().any(char::is_control),
                    "webSearch.searxng.{label} must be non-blank and free of control characters"
                );
            }
        }

        if let Some(level) = self.safesearch {
            ensure!(
                level <= 2,
                "webSearch.searxng.safesearch must be 0, 1, or 2"
            );
        }

        Ok(())
    }

    pub fn is_loopback_endpoint(&self) -> bool {
        let Ok(url) = Url::parse(self.endpoint.trim()) else {
            return false;
        };
        let Some(host) = url.host_str() else {
            return false;
        };
        host.eq_ignore_ascii_case("localhost")
            || host
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    }
}

/// The `webSearch:` settings section of `settings.yml`; field names stay camelCase.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct WebSearchSettings {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub provider: WebSearchProvider,
    #[serde(default = "default_keyless")]
    pub keyless: bool,
    #[serde(default = "default_timeout_seconds")]
    pub timeout_seconds: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub searxng: Option<SearxngSettings>,
}

impl Default for WebSearchSettings {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            provider: WebSearchProvider::default(),
            keyless: default_keyless(),
            timeout_seconds: default_timeout_seconds(),
            searxng: None,
        }
    }
}

impl WebSearchSettings {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=MAX_WEB_SEARCH_TIMEOUT_SECONDS).contains(&self.timeout_seconds),
            "webSearch.timeoutSeconds must be between 1 and {MAX_WEB_SEARCH_TIMEOUT_SECONDS}"
        );
        if let Some(searxng) = &self.searxng {
            searxng.validate()?;
        }
        Ok(())
    }
}
