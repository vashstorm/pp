use super::{
    ProviderError, RawProviderResponse, SearchContext, clamp_num_results, duckduckgo, startpage,
};
use crate::web_search::Recency;
use crate::web_search::SearchSource;
use crate::web_search::query::StructuredQuery;
use reqwest::Url;
use std::collections::HashMap;
use std::time::Duration;

const DEFAULT_NUM_RESULTS: usize = 15;
const MAX_NUM_RESULTS: usize = 30;
const DEFAULT_SOFT_DEADLINE: Duration = Duration::from_secs(5);
const DEFAULT_HARD_DEADLINE: Duration = Duration::from_secs(30);

struct MergedSource {
    source: SearchSource,
    engines: usize,
    best_rank: usize,
    order: usize,
}

pub(crate) fn dedup_key(raw_url: &str) -> String {
    let Ok(url) = Url::parse(raw_url) else {
        return raw_url.to_owned();
    };
    let Some(host) = url.host_str() else {
        return raw_url.to_owned();
    };
    let host = host.to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    let mut path = url.path();
    if path.len() > 1 && path.ends_with('/') {
        path = &path[..path.len() - 1];
    }
    match url.query() {
        Some(q) => format!("{host}{path}?{q}"),
        None => format!("{host}{path}"),
    }
}

fn merge_sources(merged: &mut HashMap<String, MergedSource>, sources: Vec<SearchSource>) {
    for (rank, source) in sources.into_iter().enumerate() {
        let key = dedup_key(&source.url);
        if let Some(existing) = merged.get_mut(&key) {
            existing.engines += 1;
            if rank < existing.best_rank {
                existing.best_rank = rank;
                existing.source.title = source.title;
                existing.source.url = source.url;
            }
            let incoming_len = source.snippet.as_ref().map_or(0, String::len);
            let current_len = existing.source.snippet.as_ref().map_or(0, String::len);
            if incoming_len > current_len {
                existing.source.snippet = source.snippet;
            }
            if existing.source.published_date.is_none() {
                existing.source.published_date = source.published_date;
            }
            if existing.source.age_seconds.is_none() {
                existing.source.age_seconds = source.age_seconds;
            }
        } else {
            let order = merged.len();
            merged.insert(
                key,
                MergedSource {
                    source,
                    engines: 1,
                    best_rank: rank,
                    order,
                },
            );
        }
    }
}

pub(crate) async fn search(
    ctx: &SearchContext<'_>,
    query: &str,
    parsed: &StructuredQuery,
    recency: Option<Recency>,
    limit: Option<usize>,
) -> Result<RawProviderResponse, ProviderError> {
    search_with_deadlines(
        ctx,
        query,
        parsed,
        recency,
        limit,
        DEFAULT_SOFT_DEADLINE.min(ctx.timeout),
        DEFAULT_HARD_DEADLINE.min(ctx.timeout),
    )
    .await
}

pub(crate) async fn search_with_deadlines(
    ctx: &SearchContext<'_>,
    query: &str,
    parsed: &StructuredQuery,
    recency: Option<Recency>,
    limit: Option<usize>,
    soft_deadline: Duration,
    hard_deadline: Duration,
) -> Result<RawProviderResponse, ProviderError> {
    let num_results = clamp_num_results(limit, DEFAULT_NUM_RESULTS, MAX_NUM_RESULTS);
    // 优先级顺序：Startpage 在前，DuckDuckGo 在后（作为并列共识的决胜顺序）。
    let mut sp_fut = std::pin::pin!(startpage::search(ctx, query, parsed, recency, limit));
    let mut ddg_fut = std::pin::pin!(duckduckgo::search(ctx, query, parsed, recency, limit));

    let mut sp_res: Option<Result<RawProviderResponse, ProviderError>> = None;
    let mut ddg_res: Option<Result<RawProviderResponse, ProviderError>> = None;

    let soft_sleep = tokio::time::sleep(soft_deadline);
    tokio::pin!(soft_sleep);
    let hard_sleep = tokio::time::sleep(hard_deadline);
    tokio::pin!(hard_sleep);

    let mut soft_elapsed = false;

    loop {
        let both_done = sp_res.is_some() && ddg_res.is_some();
        let any_ok = sp_res.as_ref().is_some_and(Result::is_ok)
            || ddg_res.as_ref().is_some_and(Result::is_ok);
        if both_done || (soft_elapsed && any_ok) {
            break;
        }

        tokio::select! {
            res = &mut sp_fut, if sp_res.is_none() => {
                sp_res = Some(res);
            }
            res = &mut ddg_fut, if ddg_res.is_none() => {
                ddg_res = Some(res);
            }
            () = &mut soft_sleep, if !soft_elapsed => {
                soft_elapsed = true;
            }
            () = &mut hard_sleep => {
                break;
            }
        }
    }

    let mut merged = HashMap::new();
    let mut failures = Vec::new();
    let mut any_responded = false;

    for (name, res) in [("startpage", sp_res), ("duckduckgo", ddg_res)] {
        match res {
            Some(Ok(resp)) => {
                any_responded = true;
                merge_sources(&mut merged, resp.sources);
            }
            Some(Err(err)) => failures.push(format!("{name}: {}", err.message)),
            None => {}
        }
    }

    if !any_responded {
        let detail = if failures.is_empty() {
            "Public web search timed out before any engine responded.".to_owned()
        } else {
            format!("All public engines failed: {}", failures.join("; "))
        };
        return Err(ProviderError::new(
            "public",
            "Public Web",
            detail,
            Some(503),
        ));
    }

    let mut entries: Vec<MergedSource> = merged.into_values().collect();
    entries.sort_by(|a, b| {
        b.engines
            .cmp(&a.engines)
            .then_with(|| a.best_rank.cmp(&b.best_rank))
            .then_with(|| a.order.cmp(&b.order))
    });

    let sources = entries
        .into_iter()
        .take(num_results)
        .map(|entry| entry.source)
        .collect();

    Ok(RawProviderResponse {
        provider: "public",
        label: "Public Web",
        answer: None,
        sources,
    })
}
