use super::{
    ProviderError, RawProviderResponse, SearchContext, clamp_num_results, classify_http_error,
    date_to_age_seconds, map_reqwest_error, normalize_search_text, read_search_body,
    strip_html_tags,
};
use crate::web_search::query::{
    QuerySyntax, QueryTerm, StructuredQuery, format_query, iso_date_to_epoch_secs,
    recency_after_date,
};
use crate::web_search::{Recency, SearchSource};
use reqwest::Url;
use reqwest::header::ACCEPT;
use serde::Deserialize;

const DEFAULT_NUM_RESULTS: usize = 10;
const MAX_NUM_RESULTS: usize = 30;

const HN_QUERY_SYNTAX: QuerySyntax = QuerySyntax {
    phrases: true,
    negation: true,
    or: false,
    site: false,
    in_url: false,
    in_title: false,
    in_text: false,
    filetype: false,
    date_range: false,
};

#[derive(Deserialize)]
struct AlgoliaResponse {
    #[serde(default)]
    hits: Vec<HnHit>,
}

#[derive(Deserialize)]
struct HnHit {
    #[serde(rename = "objectID")]
    object_id: Option<String>,
    author: Option<String>,
    created_at: Option<String>,
    title: Option<String>,
    url: Option<String>,
    story_text: Option<String>,
    points: Option<i64>,
    num_comments: Option<i64>,
    comment_text: Option<String>,
    story_title: Option<String>,
}

#[derive(Clone, Copy)]
enum HnKind {
    Story,
    Comment,
}

impl HnKind {
    fn tag(self) -> &'static str {
        match self {
            Self::Story => "story",
            Self::Comment => "comment",
        }
    }
}

fn keyword_query(parsed: &StructuredQuery) -> String {
    let mut terms = parsed.terms.clone();
    for text in parsed
        .in_title
        .iter()
        .chain(&parsed.in_text)
        .chain(&parsed.in_url)
    {
        terms.push(QueryTerm {
            text: text.clone(),
            phrase: false,
            negated: false,
            group: None,
        });
    }
    if terms.is_empty() {
        return String::new();
    }
    let mut cloned = parsed.clone();
    cloned.terms = terms;
    format_query(&cloned, HN_QUERY_SYNTAX)
}

fn date_filters(parsed: &StructuredQuery, recency: Option<Recency>) -> Vec<String> {
    let recency_start = match (parsed.after.as_deref(), recency, parsed.before.as_deref()) {
        (Some(after), _, _) => Some(after.to_owned()),
        (None, Some(r), None) => {
            let days = match r {
                Recency::Day => 1,
                Recency::Week => 7,
                Recency::Month => 30,
                Recency::Year => 365,
            };
            Some(recency_after_date(days))
        }
        (None, _, _) => None,
    };
    let mut filters = Vec::new();
    if let Some(start) = recency_start
        && let Some(secs) = iso_date_to_epoch_secs(&start)
    {
        filters.push(format!("created_at_i>={secs}"));
    }
    if let Some(before) = parsed.before.as_deref()
        && let Some(secs) = iso_date_to_epoch_secs(before)
    {
        filters.push(format!("created_at_i<{secs}"));
    }
    filters
}

fn to_source(hit: HnHit, kind: HnKind, item_prefix: &str) -> Option<SearchSource> {
    let id = hit.object_id?;
    let url = format!("{item_prefix}{id}");
    let body_html = match kind {
        HnKind::Story => hit.story_text.as_deref(),
        HnKind::Comment => hit.comment_text.as_deref(),
    };
    let text = body_html.and_then(|html| normalize_search_text(Some(&strip_html_tags(html))));
    let title = match kind {
        HnKind::Story => hit.title.unwrap_or_else(|| url.clone()),
        HnKind::Comment => {
            let by = match hit.author.as_deref() {
                Some(a) => format!("Comment by {a}"),
                None => "Comment".to_owned(),
            };
            match hit.story_title.as_deref() {
                Some(st) => format!("{by} on: {st}"),
                None => by,
            }
        }
    };
    let snippet = match kind {
        HnKind::Story => {
            let mut stats = vec![
                format!("{} points", hit.points.unwrap_or(0)),
                format!("{} comments", hit.num_comments.unwrap_or(0)),
            ];
            if let Some(author) = &hit.author {
                stats.push(format!("by {author}"));
            }
            let mut parts = vec![stats.join(", ")];
            if let Some(u) = &hit.url {
                parts.push(u.clone());
            }
            if let Some(t) = text {
                parts.push(t);
            }
            Some(parts.join(" — "))
        }
        HnKind::Comment => text,
    };
    let age_seconds = date_to_age_seconds(hit.created_at.as_deref());
    Some(SearchSource {
        title,
        url,
        snippet,
        published_date: hit.created_at,
        age_seconds,
        author: hit.author,
    })
}

fn merge_sources(
    stories: Vec<SearchSource>,
    comments: Vec<SearchSource>,
    count: usize,
) -> Vec<SearchSource> {
    let half = count.div_ceil(2);
    let story_limit = half.max(count.saturating_sub(comments.len()));
    let mut out: Vec<SearchSource> = stories.into_iter().take(story_limit).collect();
    let remaining = count.saturating_sub(out.len());
    out.extend(comments.into_iter().take(remaining));
    out
}

async fn fetch_hits(
    ctx: &SearchContext<'_>,
    url: Url,
    kind: HnKind,
) -> Result<Vec<SearchSource>, ProviderError> {
    let response = ctx
        .client
        .get(url)
        .header(ACCEPT, "application/json")
        .timeout(ctx.timeout)
        .send()
        .await
        .map_err(|e| map_reqwest_error("hackernews", "Hacker News", e))?;
    let (status, _, body) = read_search_body(response, "hackernews", "Hacker News").await?;
    if !(200..300).contains(&status) {
        return Err(classify_http_error(
            "hackernews",
            "Hacker News",
            status,
            &body,
        ));
    }
    let parsed: AlgoliaResponse = serde_json::from_str(&body).map_err(|_| {
        ProviderError::new(
            "hackernews",
            "Hacker News",
            "Hacker News search returned invalid JSON",
            Some(502),
        )
    })?;
    Ok(parsed
        .hits
        .into_iter()
        .filter_map(|hit| to_source(hit, kind, &ctx.endpoints.hackernews_item_prefix))
        .collect())
}

pub(crate) async fn search(
    ctx: &SearchContext<'_>,
    parsed: &StructuredQuery,
    recency: Option<Recency>,
    limit: Option<usize>,
) -> Result<RawProviderResponse, ProviderError> {
    let count = clamp_num_results(limit, DEFAULT_NUM_RESULTS, MAX_NUM_RESULTS);
    let keywords = keyword_query(parsed);
    let optional: Vec<&str> = parsed
        .terms
        .iter()
        .filter(|t| t.group.is_some() && !t.negated)
        .map(|t| t.text.as_str())
        .collect();
    let filters = date_filters(parsed, recency);

    let build_url = |kind: HnKind| -> Result<Url, ProviderError> {
        let mut url = Url::parse(&ctx.endpoints.hackernews_search).map_err(|_| {
            ProviderError::new("hackernews", "Hacker News", "invalid Hacker News URL", None)
        })?;
        {
            let mut pairs = url.query_pairs_mut();
            pairs.append_pair("query", &keywords);
            pairs.append_pair("advancedSyntax", "true");
            pairs.append_pair("hitsPerPage", &count.to_string());
            pairs.append_pair("tags", kind.tag());
            if !optional.is_empty() {
                pairs.append_pair("optionalWords", &optional.join(" "));
            }
            if !filters.is_empty() {
                pairs.append_pair("numericFilters", &filters.join(","));
            }
        }
        Ok(url)
    };

    let story_url = build_url(HnKind::Story)?;
    let (stories, comments) = if keywords.is_empty() {
        (fetch_hits(ctx, story_url, HnKind::Story).await?, Vec::new())
    } else {
        let comment_url = build_url(HnKind::Comment)?;
        let (s_res, c_res) = tokio::join!(
            fetch_hits(ctx, story_url, HnKind::Story),
            fetch_hits(ctx, comment_url, HnKind::Comment),
        );
        (s_res?, c_res?)
    };

    Ok(RawProviderResponse {
        provider: "hackernews",
        label: "Hacker News",
        answer: None,
        sources: merge_sources(stories, comments, count),
    })
}
