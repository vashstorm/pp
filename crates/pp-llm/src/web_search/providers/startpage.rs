use super::{
    ProviderError, RawProviderResponse, SearchContext, browser_headers, clamp_num_results,
    classify_http_error, extract_attr, extract_tag_blocks_by_class, find_first_tag_with_class,
    map_reqwest_error, normalize_search_text, read_search_body, strip_html_tags,
};
use crate::web_search::query::{GOOGLE_QUERY_SYNTAX, StructuredQuery, format_scraper_query};
use crate::web_search::{Recency, SearchSource};
use reqwest::Url;
use std::collections::HashSet;

const DEFAULT_NUM_RESULTS: usize = 10;
const MAX_NUM_RESULTS: usize = 20;

fn recency_param(recency: Recency) -> &'static str {
    match recency {
        Recency::Day => "d",
        Recency::Week => "w",
        Recency::Month => "m",
        Recency::Year => "y",
    }
}

fn is_challenge_response(status: u16, final_url: &str, html: &str) -> bool {
    let _ = status;
    final_url.contains("/errors/")
        || final_url.contains("/captcha/")
        || final_url.contains("/sp/captcha")
        || html.contains("component---src-pages-captcha")
        || html.contains("/sp/captcha")
}

fn parse_search_form_inputs(html: &str) -> Option<Vec<(String, String)>> {
    let lower = html.to_ascii_lowercase();
    let mut scan = 0usize;
    while let Some(rel) = lower[scan..].find("<form") {
        let start = scan + rel;
        let tag_end = lower[start..].find('>')? + start + 1;
        let open_tag = &html[start..tag_end];
        let action = extract_attr(open_tag, "action").unwrap_or_default();
        if action == "/sp/search" || action.ends_with("/sp/search") {
            let form_end = lower[tag_end..]
                .find("</form")
                .map_or(html.len(), |p| tag_end + p);
            let body = &html[tag_end..form_end];
            let body_lower = body.to_ascii_lowercase();
            let mut inputs = Vec::new();
            let mut has_sc = false;
            let mut i = 0usize;
            while let Some(irel) = body_lower[i..].find("<input") {
                let istart = i + irel;
                let Some(iend_rel) = body_lower[istart..].find('>') else {
                    break;
                };
                let itag = &body[istart..istart + iend_rel + 1];
                if extract_attr(itag, "type").is_some_and(|t| t.eq_ignore_ascii_case("hidden"))
                    && let Some(name) = extract_attr(itag, "name")
                    && !name.is_empty()
                {
                    let value = extract_attr(itag, "value").unwrap_or_default();
                    if name == "sc" && !value.is_empty() {
                        has_sc = true;
                    }
                    inputs.push((name, value));
                }
                i = istart + iend_rel + 1;
            }
            if has_sc {
                return Some(inputs);
            }
        }
        scan = tag_end;
    }
    None
}

fn sanitize_result_url(href: &str, home_url: &str) -> Option<String> {
    let base = Url::parse(home_url).ok()?;
    let url = base.join(href).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if host == "startpage.com" || host.ends_with(".startpage.com") {
        return None;
    }
    Some(url.to_string())
}

pub(crate) fn parse_html_results(html: &str, home_url: &str) -> Vec<SearchSource> {
    let mut results = Vec::new();
    let mut seen = HashSet::new();
    for block in extract_tag_blocks_by_class(html, "div", "result") {
        let Some((anchor_tag, anchor_inner)) =
            find_first_tag_with_class(block, &["a"], Some("result-link"))
        else {
            continue;
        };
        let Some(href) = extract_attr(anchor_tag, "href") else {
            continue;
        };
        let Some(url) = sanitize_result_url(&href, home_url) else {
            continue;
        };
        let title_raw = find_first_tag_with_class(anchor_inner, &["h2", "h3"], None)
            .map_or(anchor_inner, |(_, inner)| inner);
        let Some(title) = normalize_search_text(Some(&strip_html_tags(title_raw))) else {
            continue;
        };
        let snippet = find_first_tag_with_class(block, &["p"], Some("description"))
            .and_then(|(_, inner)| normalize_search_text(Some(&strip_html_tags(inner))));
        if seen.insert(url.clone()) {
            results.push(SearchSource {
                title,
                url,
                snippet,
                published_date: None,
                age_seconds: None,
                author: None,
            });
        }
    }
    results
}

async fn fetch_form_inputs(ctx: &SearchContext<'_>) -> Option<Vec<(String, String)>> {
    let response = ctx
        .client
        .get(&ctx.endpoints.startpage_home)
        .headers(browser_headers(None))
        .timeout(ctx.timeout)
        .send()
        .await
        .ok()?;
    let (status, final_url, html) = read_search_body(response, "startpage", "Startpage")
        .await
        .ok()?;
    if !(200..300).contains(&status) || is_challenge_response(status, &final_url, &html) {
        return None;
    }
    parse_search_form_inputs(&html)
}

pub(crate) async fn search(
    ctx: &SearchContext<'_>,
    query: &str,
    parsed: &StructuredQuery,
    recency: Option<Recency>,
    limit: Option<usize>,
) -> Result<RawProviderResponse, ProviderError> {
    let num_results = clamp_num_results(limit, DEFAULT_NUM_RESULTS, MAX_NUM_RESULTS);
    let formatted = format_scraper_query(query, parsed, GOOGLE_QUERY_SYNTAX);
    let with_date = recency.map(recency_param);

    let response = if let Some(mut form_inputs) = fetch_form_inputs(ctx).await {
        form_inputs.retain(|(k, _)| k != "query" && k != "with_date");
        form_inputs.push(("query".to_owned(), formatted));
        if let Some(date) = with_date {
            form_inputs.push(("with_date".to_owned(), date.to_owned()));
        }
        ctx.client
            .post(&ctx.endpoints.startpage_search)
            .headers(browser_headers(Some(&ctx.endpoints.startpage_home)))
            .timeout(ctx.timeout)
            .form(&form_inputs)
            .send()
            .await
            .map_err(|e| map_reqwest_error("startpage", "Startpage", e))?
    } else {
        let mut url = Url::parse(&ctx.endpoints.startpage_search).map_err(|_| {
            ProviderError::new("startpage", "Startpage", "invalid Startpage URL", None)
        })?;
        {
            let mut pairs = url.query_pairs_mut();
            pairs.append_pair("query", &formatted);
            if let Some(date) = with_date {
                pairs.append_pair("with_date", date);
            }
        }
        ctx.client
            .get(url)
            .headers(browser_headers(Some(&ctx.endpoints.startpage_home)))
            .timeout(ctx.timeout)
            .send()
            .await
            .map_err(|e| map_reqwest_error("startpage", "Startpage", e))?
    };

    let (status, final_url, html) = read_search_body(response, "startpage", "Startpage").await?;
    if is_challenge_response(status, &final_url, &html) {
        return Err(ProviderError::new(
            "startpage",
            "Startpage",
            "Startpage blocked the request with a CAPTCHA challenge.",
            Some(429),
        ));
    }
    if !(200..300).contains(&status) {
        return Err(classify_http_error("startpage", "Startpage", status, &html));
    }

    let mut sources = parse_html_results(&html, &ctx.endpoints.startpage_home);
    sources.truncate(num_results);
    Ok(RawProviderResponse {
        provider: "startpage",
        label: "Startpage",
        answer: None,
        sources,
    })
}
