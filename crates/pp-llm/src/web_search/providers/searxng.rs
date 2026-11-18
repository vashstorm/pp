use super::{
    ProviderError, RawProviderResponse, SearchContext, clamp_num_results, classify_http_error,
    date_to_age_seconds, map_reqwest_error, normalize_search_text, read_search_body,
};
use crate::web_search::query::{GOOGLE_QUERY_SYNTAX, StructuredQuery, format_scraper_query};
use crate::web_search::{Recency, SearchSource};
use pp_config::config::SearxngSettings;
use reqwest::Url;
use reqwest::header::ACCEPT;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

const DEFAULT_NUM_RESULTS: usize = 10;
const MAX_NUM_RESULTS: usize = 20;

static ENGINE_MAP_CACHE: OnceLock<Mutex<HashMap<String, HashMap<String, String>>>> =
    OnceLock::new();

#[derive(Deserialize)]
struct SearxngConfigPayload {
    #[serde(default)]
    engines: Vec<SearxngEngineEntry>,
}

#[derive(Deserialize)]
struct SearxngEngineEntry {
    name: Option<String>,
    shortcut: Option<String>,
}

#[derive(Deserialize)]
struct SearxngResponsePayload {
    #[serde(default)]
    results: Vec<SearxngResultItem>,
    #[serde(default)]
    answers: Vec<Value>,
}

#[derive(Deserialize)]
struct SearxngResultItem {
    title: Option<String>,
    url: Option<String>,
    content: Option<String>,
    snippet: Option<String>,
    #[serde(rename = "publishedDate")]
    published_date: Option<String>,
    published_date_snake: Option<String>,
}

fn strip_external_bangs(query: &str) -> String {
    query
        .split_whitespace()
        .filter(|part| !part.starts_with("!!"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn recency_param(recency: Recency) -> &'static str {
    match recency {
        Recency::Day => "day",
        Recency::Week | Recency::Month => "month",
        Recency::Year => "year",
    }
}

fn apply_auth(
    builder: reqwest::RequestBuilder,
    settings: &SearxngSettings,
) -> reqwest::RequestBuilder {
    if let (Some(user), Some(pass)) = (&settings.basic_username, &settings.basic_password) {
        return builder.basic_auth(user, Some(pass));
    }
    if let Some(token) = &settings.token {
        return builder.bearer_auth(token);
    }
    builder
}

async fn resolve_engine_names(
    ctx: &SearchContext<'_>,
    settings: &SearxngSettings,
    raw: &str,
) -> String {
    let entries: Vec<&str> = raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if entries.is_empty() {
        return String::new();
    }
    let base = settings.endpoint.trim().trim_end_matches('/');
    let cache = ENGINE_MAP_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(cached) = cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(base)
        .cloned()
    {
        return entries
            .iter()
            .map(|e| {
                cached
                    .get(&e.to_ascii_lowercase())
                    .map_or(*e, String::as_str)
            })
            .collect::<Vec<_>>()
            .join(",");
    }

    let config_url = format!("{base}/config");
    let fetched = apply_auth(
        ctx.client
            .get(&config_url)
            .header(ACCEPT, "application/json")
            .timeout(ctx.timeout),
        settings,
    )
    .send()
    .await
    .ok();

    if let Some(resp) = fetched
        && resp.status().is_success()
        && let Ok((_, _, body)) = read_search_body(resp, "searxng", "SearXNG").await
        && let Ok(payload) = serde_json::from_str::<SearxngConfigPayload>(&body)
    {
        let mut map = HashMap::new();
        for engine in payload.engines {
            let Some(name) = engine.name else { continue };
            map.insert(name.to_ascii_lowercase(), name.clone());
            if let Some(shortcut) = engine.shortcut {
                map.insert(shortcut.to_ascii_lowercase(), name);
            }
        }
        if !map.is_empty() {
            let mapped = entries
                .iter()
                .map(|e| map.get(&e.to_ascii_lowercase()).map_or(*e, String::as_str))
                .collect::<Vec<_>>()
                .join(",");
            cache
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(base.to_owned(), map);
            return mapped;
        }
    }
    entries.join(",")
}

fn extract_answer_text(value: &Value) -> Option<String> {
    if let Some(s) = value.as_str() {
        return normalize_search_text(Some(s));
    }
    let obj = value.as_object()?;
    if let Some(ans) = obj.get("answer").and_then(Value::as_str) {
        return normalize_search_text(Some(ans));
    }
    if let Some(translations) = obj.get("translations").and_then(Value::as_array) {
        let items: Vec<String> = translations
            .iter()
            .filter_map(|item| item.get("text").and_then(Value::as_str))
            .filter_map(|s| normalize_search_text(Some(s)))
            .take(3)
            .collect();
        if !items.is_empty() {
            return Some(items.join("\n"));
        }
    }
    None
}

fn format_answers(answers: &[Value]) -> Option<String> {
    let items: Vec<String> = answers
        .iter()
        .filter_map(extract_answer_text)
        .take(3)
        .collect();
    (!items.is_empty()).then(|| items.join("\n\n"))
}

pub(crate) async fn search(
    ctx: &SearchContext<'_>,
    query: &str,
    parsed: &StructuredQuery,
    recency: Option<Recency>,
    limit: Option<usize>,
) -> Result<RawProviderResponse, ProviderError> {
    let settings = ctx.searxng.ok_or_else(|| {
        ProviderError::new(
            "searxng",
            "SearXNG",
            "SearXNG is not configured in settings.yml (webSearch.searxng.endpoint).",
            None,
        )
    })?;
    let num_results = clamp_num_results(limit, DEFAULT_NUM_RESULTS, MAX_NUM_RESULTS);
    let formatted = format_scraper_query(query, parsed, GOOGLE_QUERY_SYNTAX);
    let cleaned = strip_external_bangs(&formatted);
    let effective_query = if cleaned.is_empty() {
        formatted
    } else {
        cleaned
    };

    let base = settings.endpoint.trim().trim_end_matches('/');
    let mut url = Url::parse(&format!("{base}/search")).map_err(|_| {
        ProviderError::new("searxng", "SearXNG", "invalid SearXNG endpoint URL", None)
    })?;

    let resolved_engines = match &settings.engines {
        Some(raw) => {
            let resolved = resolve_engine_names(ctx, settings, raw).await;
            (!resolved.is_empty()).then_some(resolved)
        }
        None => None,
    };

    {
        let mut pairs = url.query_pairs_mut();
        pairs.append_pair("q", &effective_query);
        pairs.append_pair("format", "json");
        pairs.append_pair("pageno", "1");
        if let Some(r) = recency {
            pairs.append_pair("time_range", recency_param(r));
        }
        if let Some(categories) = &settings.categories {
            pairs.append_pair("categories", categories);
        }
        if let Some(engines) = &resolved_engines {
            pairs.append_pair("engines", engines);
        }
        if let Some(safesearch) = settings.safesearch {
            pairs.append_pair("safesearch", &safesearch.to_string());
        }
        if let Some(language) = settings.language.as_deref().or(parsed.lang.as_deref()) {
            pairs.append_pair("language", language);
        }
    }

    let response = apply_auth(
        ctx.client
            .get(url)
            .header(ACCEPT, "application/json")
            .timeout(ctx.timeout),
        settings,
    )
    .send()
    .await
    .map_err(|e| map_reqwest_error("searxng", "SearXNG", e))?;

    let (status, _, body) = read_search_body(response, "searxng", "SearXNG").await?;
    if !(200..300).contains(&status) {
        return Err(classify_http_error("searxng", "SearXNG", status, &body));
    }

    let payload: SearxngResponsePayload = serde_json::from_str(&body).map_err(|_| {
        ProviderError::new(
            "searxng",
            "SearXNG",
            "SearXNG returned invalid JSON response",
            Some(502),
        )
    })?;

    let sources: Vec<SearchSource> = payload
        .results
        .into_iter()
        .filter_map(|item| {
            let url = item.url?.trim().to_owned();
            if url.is_empty() {
                return None;
            }
            let title = normalize_search_text(item.title.as_deref()).unwrap_or_else(|| url.clone());
            let snippet = item
                .content
                .as_deref()
                .or(item.snippet.as_deref())
                .and_then(|s| normalize_search_text(Some(s)));
            let published_date = item.published_date.or(item.published_date_snake);
            let age_seconds = date_to_age_seconds(published_date.as_deref());
            Some(SearchSource {
                title,
                url,
                snippet,
                published_date,
                age_seconds,
                author: None,
            })
        })
        .take(num_results)
        .collect();

    Ok(RawProviderResponse {
        provider: "searxng",
        label: "SearXNG",
        answer: format_answers(&payload.answers),
        sources,
    })
}
