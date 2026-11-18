pub mod format;
pub mod providers;
pub mod query;

use anyhow::Result;
use pp_config::config::{WebSearchProvider, WebSearchSettings};
use pp_core::cancellation::CancellationToken;
use providers::{
    Endpoints, ProviderError, RawProviderResponse, SearchContext, duckduckgo, hackernews, public,
    searxng, startpage, targets_hackernews,
};
use query::{apply_query_constraints, parse_search_query};
use serde::{Deserialize, Serialize};
use std::time::Duration;

const MAX_FALLBACK_REASON_CHARS: usize = 240;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Recency {
    Day,
    Week,
    Month,
    Year,
}

#[derive(Clone, Debug)]
pub struct WebSearchRequest {
    pub query: String,
    pub recency: Option<Recency>,
    pub limit: Option<usize>,
    pub provider: Option<WebSearchProvider>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SearchSource {
    pub title: String,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub published_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub age_seconds: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SearchFailureDetail {
    pub provider: String,
    pub label: String,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
}

#[derive(Clone, Debug, Serialize)]
pub struct WebSearchResponse {
    pub provider: String,
    pub provider_label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
    pub sources: Vec<SearchSource>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failures: Vec<SearchFailureDetail>,
    pub formatted_text: String,
}

fn has_renderable_content(raw: &RawProviderResponse) -> bool {
    raw.answer.as_deref().is_some_and(|a| !a.trim().is_empty()) || !raw.sources.is_empty()
}

fn build_failure_detail(err: &ProviderError) -> SearchFailureDetail {
    let flat = err
        .message
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_end_matches('.')
        .to_owned();
    SearchFailureDetail {
        provider: err.provider.clone(),
        label: err.label.clone(),
        reason: format::truncate_chars(&flat, MAX_FALLBACK_REASON_CHARS),
        status: err.status,
    }
}

fn build_candidate_chain(
    request: &WebSearchRequest,
    settings: &WebSearchSettings,
    parsed: &query::StructuredQuery,
    allow_external_network: bool,
) -> Result<(Vec<WebSearchProvider>, bool)> {
    let selected = request.provider.unwrap_or(settings.provider);
    if selected != WebSearchProvider::Auto {
        if selected == WebSearchProvider::Searxng {
            let Some(searxng_cfg) = &settings.searxng else {
                anyhow::bail!(
                    "SearXNG web search is not configured in settings.yml (webSearch.searxng.endpoint)."
                );
            };
            if !allow_external_network && !searxng_cfg.is_loopback_endpoint() {
                anyhow::bail!(
                    "SearXNG web search is blocked because sandbox outbound network access is disabled."
                );
            }
        } else if !allow_external_network {
            anyhow::bail!(
                "Web search provider '{}' is blocked because sandbox outbound network access is disabled.",
                selected.as_str()
            );
        }
        return Ok((vec![selected], true));
    }

    let mut chain = Vec::new();
    let hn_only = targets_hackernews(parsed);
    if hn_only && settings.keyless && allow_external_network {
        chain.push(WebSearchProvider::Hackernews);
    }
    if let Some(searxng_cfg) = &settings.searxng
        && (allow_external_network || searxng_cfg.is_loopback_endpoint())
    {
        chain.push(WebSearchProvider::Searxng);
    }
    if settings.keyless && allow_external_network {
        chain.push(WebSearchProvider::Public);
    }

    if chain.is_empty() {
        if !allow_external_network {
            anyhow::bail!(
                "Web search is blocked because sandbox outbound network access is disabled."
            );
        }
        anyhow::bail!(
            "No web search provider is available: webSearch.keyless is false and no SearXNG endpoint is configured."
        );
    }
    Ok((chain, false))
}

async fn run_provider(
    provider: WebSearchProvider,
    ctx: &SearchContext<'_>,
    query: &str,
    parsed: &query::StructuredQuery,
    recency: Option<Recency>,
    limit: Option<usize>,
) -> Result<RawProviderResponse, ProviderError> {
    match provider {
        WebSearchProvider::Public => public::search(ctx, query, parsed, recency, limit).await,
        WebSearchProvider::Startpage => startpage::search(ctx, query, parsed, recency, limit).await,
        WebSearchProvider::Duckduckgo => {
            duckduckgo::search(ctx, query, parsed, recency, limit).await
        }
        WebSearchProvider::Hackernews => hackernews::search(ctx, parsed, recency, limit).await,
        WebSearchProvider::Searxng => searxng::search(ctx, query, parsed, recency, limit).await,
        WebSearchProvider::Auto => unreachable!("Auto is expanded before dispatch"),
    }
}

pub(crate) async fn execute_search(
    client: &reqwest::Client,
    request: &WebSearchRequest,
    settings: &WebSearchSettings,
    allow_external_network: bool,
    cancellation: &CancellationToken,
) -> Result<WebSearchResponse> {
    execute_search_with_endpoints(
        client,
        request,
        settings,
        allow_external_network,
        &Endpoints::default(),
        cancellation,
    )
    .await
}

pub(crate) async fn execute_search_with_endpoints(
    client: &reqwest::Client,
    request: &WebSearchRequest,
    settings: &WebSearchSettings,
    allow_external_network: bool,
    endpoints: &Endpoints,
    cancellation: &CancellationToken,
) -> Result<WebSearchResponse> {
    pp_core::cancellation::check(cancellation)?;
    let trimmed_query = request.query.trim();
    anyhow::ensure!(
        !trimmed_query.is_empty(),
        "web_search query must not be empty"
    );
    let parsed = parse_search_query(trimmed_query);
    let (chain, _explicit) =
        build_candidate_chain(request, settings, &parsed, allow_external_network)?;
    let timeout = Duration::from_secs(settings.timeout_seconds.clamp(1, 300));
    let ctx = SearchContext {
        client,
        timeout,
        searxng: settings.searxng.as_ref(),
        endpoints,
    };

    let mut failures: Vec<SearchFailureDetail> = Vec::new();
    for provider in chain {
        pp_core::cancellation::check(cancellation)?;
        match run_provider(
            provider,
            &ctx,
            trimmed_query,
            &parsed,
            request.recency,
            request.limit,
        )
        .await
        {
            Ok(mut raw) => {
                let mut constraint_notes = Vec::new();
                if parsed.has_constraints && !raw.sources.is_empty() {
                    let filtered = apply_query_constraints(&raw.sources, &parsed);
                    raw.sources = filtered.sources;
                    for dropped in filtered.dropped {
                        constraint_notes.push(format!(
                            "no results matched `{dropped}`; the constraint was relaxed"
                        ));
                    }
                }
                if !has_renderable_content(&raw) {
                    failures.push(SearchFailureDetail {
                        provider: raw.provider.to_owned(),
                        label: raw.label.to_owned(),
                        reason: format!("{} returned no renderable search content", raw.label),
                        status: Some(204),
                    });
                    continue;
                }
                let mut notes = Vec::new();
                if !failures.is_empty() {
                    let failed_summary = failures
                        .iter()
                        .map(|f| format!("{} failed ({})", f.label, f.reason))
                        .collect::<Vec<_>>()
                        .join(" and ");
                    notes.push(format!("answered by {} after {failed_summary}", raw.label));
                }
                notes.extend(constraint_notes);
                let formatted = format::format_for_llm(raw.answer.as_deref(), &raw.sources, &notes);
                return Ok(raw.into_response(notes, failures, formatted));
            }
            Err(err) => {
                pp_core::cancellation::check(cancellation)?;
                failures.push(build_failure_detail(&err));
            }
        }
    }

    if failures.len() == 1 {
        anyhow::bail!("{}", failures[0].reason);
    }
    let summary = failures
        .iter()
        .map(|f| format!("{}: {}", f.provider, f.reason))
        .collect::<Vec<_>>()
        .join("; ");
    anyhow::bail!("All web search providers failed: {summary}")
}
