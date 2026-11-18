use super::{
    ProviderError, RawProviderResponse, SearchContext, browser_headers, clamp_num_results,
    classify_http_error, date_to_age_seconds, decode_html_entities, extract_attr,
    extract_tag_blocks_by_class, find_first_tag_with_class, map_reqwest_error,
    normalize_search_text, read_search_body, strip_html_tags,
};
use crate::web_search::query::{QuerySyntax, StructuredQuery, format_scraper_query};
use crate::web_search::{Recency, SearchSource};
use reqwest::Url;
use std::collections::HashSet;

const DEFAULT_NUM_RESULTS: usize = 10;
const MAX_NUM_RESULTS: usize = 20;

const DDG_QUERY_SYNTAX: QuerySyntax = QuerySyntax {
    phrases: true,
    negation: true,
    or: true,
    site: true,
    in_url: true,
    in_title: true,
    in_text: true,
    filetype: true,
    date_range: false,
};

fn recency_df(recency: Recency) -> &'static str {
    match recency {
        Recency::Day => "d",
        Recency::Week => "w",
        Recency::Month => "m",
        Recency::Year => "y",
    }
}

pub(crate) fn locale_to_kl(lang: Option<&str>) -> Option<&'static str> {
    let locale = lang?.trim().to_ascii_lowercase().replace('_', "-");
    let mapped = match locale.as_str() {
        "ca-es" => "ct-ca",
        "en-gb" => "uk-en",
        "es-419" => "xl-es",
        "es-us" => "ue-es",
        "ja-jp" => "jp-jp",
        "ko-kr" => "kr-kr",
        "zh-hk" => "hk-tzh",
        "zh-tw" => "tw-tzh",
        "zh-cn" => "cn-zh",
        "en-us" => "us-en",
        _ => return swap_kl(&locale),
    };
    Some(mapped)
}

fn swap_kl(locale: &str) -> Option<&'static str> {
    const ALLOWED: &[&str] = &[
        "xa-ar", "xa-en", "ar-es", "au-en", "at-de", "be-fr", "be-nl", "br-pt", "bg-bg", "ca-en",
        "ca-fr", "ct-ca", "cl-es", "cn-zh", "co-es", "hr-hr", "cz-cs", "dk-da", "ee-et", "fi-fi",
        "fr-fr", "de-de", "gr-el", "hk-tzh", "hu-hu", "in-en", "id-id", "id-en", "ie-en", "il-he",
        "it-it", "jp-jp", "kr-kr", "lv-lv", "lt-lt", "xl-es", "my-ms", "my-en", "mx-es", "nl-nl",
        "nz-en", "no-no", "pe-es", "ph-en", "ph-tl", "pl-pl", "pt-pt", "ro-ro", "ru-ru", "sg-en",
        "sk-sk", "sl-sl", "za-en", "es-es", "se-sv", "ch-de", "ch-fr", "ch-it", "tw-tzh", "th-th",
        "tr-tr", "ua-uk", "uk-en", "us-en", "ue-es", "ve-es", "vn-vi", "wt-wt",
    ];
    let (lang, region) = locale.split_once('-')?;
    if lang.len() != 2 || region.len() != 2 {
        return None;
    }
    let candidate = format!("{region}-{lang}");
    ALLOWED.iter().copied().find(|code| *code == candidate)
}

fn unwrap_result_url(href: &str) -> Option<String> {
    if href.is_empty() {
        return None;
    }
    let decoded = decode_html_entities(href);
    let lower = decoded.to_ascii_lowercase();
    if lower.starts_with("https://duckduckgo.com/y.js")
        || lower.starts_with("http://duckduckgo.com/y.js")
        || lower.starts_with("//duckduckgo.com/y.js")
        || lower.starts_with("https://www.duckduckgo.com/y.js")
        || lower.starts_with("//www.duckduckgo.com/y.js")
    {
        return None;
    }
    let abs = if decoded.starts_with("//") {
        format!("https:{decoded}")
    } else {
        decoded
    };
    if let Ok(url) = Url::parse(&abs) {
        if let Some((_, uddg)) = url.query_pairs().find(|(k, _)| k == "uddg")
            && matches!(
                Url::parse(uddg.as_ref())
                    .map(|u| u.scheme().to_owned())
                    .ok()
                    .as_deref(),
                Some("http" | "https")
            )
        {
            return Some(uddg.into_owned());
        }
        if matches!(url.scheme(), "http" | "https") {
            return Some(url.to_string());
        }
    }
    None
}

fn extract_published_date(block: &str) -> Option<String> {
    let (_, extras_inner) =
        find_first_tag_with_class(block, &["div"], Some("result__extras__url"))?;
    let lower = extras_inner.to_ascii_lowercase();
    let mut scan = 0usize;
    while let Some(rel) = lower[scan..].find("<span") {
        let start = scan + rel;
        let Some(tag_end_rel) = lower[start..].find('>') else {
            break;
        };
        let inner_start = start + tag_end_rel + 1;
        let close = lower[inner_start..]
            .find("</span")
            .map_or(extras_inner.len(), |p| inner_start + p);
        let text = strip_html_tags(&extras_inner[inner_start..close]);
        if is_iso_date_prefix(&text) {
            return Some(text);
        }
        scan = close;
    }
    None
}

fn is_iso_date_prefix(text: &str) -> bool {
    let b = text.as_bytes();
    b.len() >= 10
        && b[0..4].iter().all(u8::is_ascii_digit)
        && b[4] == b'-'
        && b[5..7].iter().all(u8::is_ascii_digit)
        && b[7] == b'-'
        && b[8..10].iter().all(u8::is_ascii_digit)
}

pub(crate) fn parse_html_results(html: &str) -> Vec<SearchSource> {
    let mut results = Vec::new();
    for block in extract_tag_blocks_by_class(html, "div", "result") {
        let Some((anchor_tag, anchor_inner)) =
            find_first_tag_with_class(block, &["a"], Some("result__a"))
        else {
            continue;
        };
        let Some(href) = extract_attr(anchor_tag, "href") else {
            continue;
        };
        let Some(url) = unwrap_result_url(&href) else {
            continue;
        };
        let Some(title) = normalize_search_text(Some(&strip_html_tags(anchor_inner))) else {
            continue;
        };
        let snippet =
            find_first_tag_with_class(block, &["a", "div", "span"], Some("result__snippet"))
                .and_then(|(_, inner)| normalize_search_text(Some(&strip_html_tags(inner))));
        let published_date = extract_published_date(block);
        let age_seconds = date_to_age_seconds(published_date.as_deref());
        results.push(SearchSource {
            title,
            url,
            snippet,
            published_date,
            age_seconds,
            author: None,
        });
    }
    results
}

fn parse_continuation_form(html: &str) -> Option<Vec<(String, String)>> {
    let lower = html.to_ascii_lowercase();
    let mut scan = 0usize;
    while let Some(rel) = lower[scan..].find("<form") {
        let start = scan + rel;
        let tag_end = lower[start..].find('>')? + start + 1;
        let form_end = lower[tag_end..]
            .find("</form")
            .map_or(html.len(), |p| tag_end + p);
        let body = &html[tag_end..form_end];
        let body_lower = body.to_ascii_lowercase();
        let mut fields = Vec::new();
        let mut has_s = false;
        let mut has_vqd = false;
        let mut i = 0usize;
        while let Some(irel) = body_lower[i..].find("<input") {
            let istart = i + irel;
            let Some(iend_rel) = body_lower[istart..].find('>') else {
                break;
            };
            let itag = &body[istart..istart + iend_rel + 1];
            if let Some(name) = extract_attr(itag, "name")
                && let Some(value) = extract_attr(itag, "value")
            {
                if name == "s" {
                    has_s = true;
                } else if name == "vqd" {
                    has_vqd = true;
                }
                fields.push((name, value));
            }
            i = istart + iend_rel + 1;
        }
        if has_s && has_vqd {
            return Some(fields);
        }
        scan = form_end;
    }
    None
}

fn is_anomaly_response(html: &str) -> bool {
    html.contains("anomaly-modal") || html.contains("anomaly.js")
}

async fn post_ddg_form(
    ctx: &SearchContext<'_>,
    form: &[(String, String)],
) -> Result<String, ProviderError> {
    let response = ctx
        .client
        .post(&ctx.endpoints.duckduckgo_html)
        .headers(browser_headers(Some("https://html.duckduckgo.com/")))
        .timeout(ctx.timeout)
        .form(form)
        .send()
        .await
        .map_err(|e| map_reqwest_error("duckduckgo", "DuckDuckGo", e))?;
    let (status, _, html) = read_search_body(response, "duckduckgo", "DuckDuckGo").await?;
    if !(200..300).contains(&status) {
        return Err(classify_http_error(
            "duckduckgo",
            "DuckDuckGo",
            status,
            &html,
        ));
    }
    if is_anomaly_response(&html) {
        return Err(ProviderError::new(
            "duckduckgo",
            "DuckDuckGo",
            "DuckDuckGo blocked the request with a bot-detection challenge.",
            Some(429),
        ));
    }
    Ok(html)
}

pub(crate) async fn search(
    ctx: &SearchContext<'_>,
    query: &str,
    parsed: &StructuredQuery,
    recency: Option<Recency>,
    limit: Option<usize>,
) -> Result<RawProviderResponse, ProviderError> {
    let num_results = clamp_num_results(limit, DEFAULT_NUM_RESULTS, MAX_NUM_RESULTS);
    let formatted = format_scraper_query(query, parsed, DDG_QUERY_SYNTAX);
    let kl = locale_to_kl(parsed.lang.as_deref()).unwrap_or("us-en");

    let mut initial_form = vec![
        ("q".to_owned(), formatted),
        ("kl".to_owned(), kl.to_owned()),
        ("b".to_owned(), String::new()),
    ];
    if let Some(df) = recency.map(recency_df) {
        initial_form.push(("df".to_owned(), df.to_owned()));
    }

    let mut sources = Vec::new();
    let mut seen = HashSet::new();
    let mut current_form = Some(initial_form);

    while let Some(form) = current_form {
        if sources.len() >= num_results {
            break;
        }
        let html = post_ddg_form(ctx, &form).await?;
        let before = sources.len();
        for src in parse_html_results(&html) {
            if seen.insert(src.url.clone()) {
                sources.push(src);
                if sources.len() >= num_results {
                    break;
                }
            }
        }
        if sources.len() == before {
            break;
        }
        current_form = parse_continuation_form(&html);
    }

    Ok(RawProviderResponse {
        provider: "duckduckgo",
        label: "DuckDuckGo",
        answer: None,
        sources,
    })
}
