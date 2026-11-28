pub mod duckduckgo;
pub mod hackernews;
pub mod public;
pub mod searxng;
pub mod startpage;

use super::query::{StructuredQuery, iso_date_to_epoch_secs, now_epoch_secs};
use super::{SearchSource, WebSearchResponse};
use pp_config::config::SearxngSettings;
use reqwest::header::{ACCEPT, ACCEPT_LANGUAGE, HeaderMap, HeaderValue, REFERER, USER_AGENT};
use std::time::Duration;

const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const MAX_ERROR_BYTES: usize = 8 * 1024;

#[derive(Clone, Debug)]
pub struct ProviderError {
    pub provider: String,
    pub label: String,
    pub message: String,
    pub status: Option<u16>,
}

impl ProviderError {
    pub fn new(
        provider: impl Into<String>,
        label: impl Into<String>,
        message: impl Into<String>,
        status: Option<u16>,
    ) -> Self {
        Self {
            provider: provider.into(),
            label: label.into(),
            message: message.into(),
            status,
        }
    }
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ProviderError {}

#[derive(Clone, Debug)]
pub(crate) struct Endpoints {
    pub startpage_home: String,
    pub startpage_search: String,
    pub duckduckgo_html: String,
    pub hackernews_search: String,
    pub hackernews_item_prefix: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            startpage_home: "https://www.startpage.com/".to_owned(),
            startpage_search: "https://www.startpage.com/sp/search".to_owned(),
            duckduckgo_html: "https://html.duckduckgo.com/html/".to_owned(),
            hackernews_search: "https://hn.algolia.com/api/v1/search".to_owned(),
            hackernews_item_prefix: "https://news.ycombinator.com/item?id=".to_owned(),
        }
    }
}

pub(crate) struct SearchContext<'a> {
    pub client: &'a reqwest::Client,
    pub timeout: Duration,
    pub searxng: Option<&'a SearxngSettings>,
    pub endpoints: &'a Endpoints,
}

pub(crate) struct RawProviderResponse {
    pub provider: &'static str,
    pub label: &'static str,
    pub answer: Option<String>,
    pub sources: Vec<SearchSource>,
}

impl RawProviderResponse {
    pub fn into_response(
        self,
        notes: Vec<String>,
        failures: Vec<super::SearchFailureDetail>,
        formatted_text: String,
    ) -> WebSearchResponse {
        WebSearchResponse {
            provider: self.provider.to_owned(),
            provider_label: self.label.to_owned(),
            answer: self.answer,
            sources: self.sources,
            notes,
            failures,
            formatted_text,
        }
    }
}

pub(crate) fn clamp_num_results(requested: Option<usize>, default: usize, max: usize) -> usize {
    requested.unwrap_or(default).clamp(1, max)
}

pub(crate) fn browser_headers(referer: Option<&str>) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        ACCEPT,
        HeaderValue::from_static(
            "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8",
        ),
    );
    headers.insert(ACCEPT_LANGUAGE, HeaderValue::from_static("en-US,en;q=0.9"));
    headers.insert(
        USER_AGENT,
        HeaderValue::from_static(
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/149.0.0.0 Safari/537.36",
        ),
    );
    headers.insert(
        "sec-ch-ua",
        HeaderValue::from_static(
            r#""Google Chrome";v="149", "Chromium";v="149", ";Not A Brand";v="99""#,
        ),
    );
    headers.insert("sec-ch-ua-mobile", HeaderValue::from_static("?0"));
    headers.insert("sec-ch-ua-platform", HeaderValue::from_static(r#""macOS""#));
    headers.insert("sec-fetch-dest", HeaderValue::from_static("document"));
    headers.insert("sec-fetch-mode", HeaderValue::from_static("navigate"));
    headers.insert("sec-fetch-user", HeaderValue::from_static("?1"));
    headers.insert("upgrade-insecure-requests", HeaderValue::from_static("1"));
    if let Some(referer) = referer
        && let Ok(val) = HeaderValue::from_str(referer)
    {
        headers.insert(REFERER, val);
        headers.insert("sec-fetch-site", HeaderValue::from_static("same-origin"));
    } else {
        headers.insert("sec-fetch-site", HeaderValue::from_static("none"));
    }
    headers
}

pub(crate) async fn read_limited_body(
    mut response: reqwest::Response,
    max_bytes: usize,
    provider: &str,
    label: &str,
) -> Result<(u16, String, String), ProviderError> {
    let status = response.status().as_u16();
    let final_url = response.url().to_string();
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| {
        ProviderError::new(
            provider,
            label,
            format!("{label} response read failed"),
            Some(502),
        )
    })? {
        if bytes.len() + chunk.len() > max_bytes {
            let take = max_bytes - bytes.len();
            bytes.extend_from_slice(&chunk[..take]);
            break;
        }
        bytes.extend_from_slice(&chunk);
    }
    let text = String::from_utf8_lossy(&bytes).into_owned();
    Ok((status, final_url, text))
}

pub(crate) async fn read_search_body(
    response: reqwest::Response,
    provider: &str,
    label: &str,
) -> Result<(u16, String, String), ProviderError> {
    let max = if response.status().is_success() {
        MAX_RESPONSE_BYTES
    } else {
        MAX_ERROR_BYTES
    };
    read_limited_body(response, max, provider, label).await
}

pub(crate) fn classify_http_error(provider: &str, label: &str, status: u16) -> ProviderError {
    let message = match status {
        401 | 403 => {
            format!("{label} authorization failed ({status}). Check credentials or endpoint.")
        }
        429 => format!("{label} rate-limited the request (429)."),
        _ => format!("{label} HTTP error ({status})"),
    };
    ProviderError::new(provider, label, message, Some(status))
}

pub(crate) fn map_reqwest_error(provider: &str, label: &str, err: reqwest::Error) -> ProviderError {
    if err.is_timeout() {
        return ProviderError::new(
            provider,
            label,
            format!("{label} request timed out"),
            Some(504),
        );
    }
    ProviderError::new(
        provider,
        label,
        format!("{label} request failed"),
        err.status().map(|s| s.as_u16()),
    )
}

pub(crate) fn date_to_age_seconds(value: Option<&str>) -> Option<i64> {
    let raw = value?.trim();
    let ts = iso_date_to_epoch_secs(raw)?;
    let age = now_epoch_secs() - ts;
    (age >= 0).then_some(age)
}

pub(crate) fn normalize_search_text(value: Option<&str>) -> Option<String> {
    let raw = value?;
    let collapsed: String = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = collapsed.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

pub(crate) fn decode_html_entities(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let Some(semi) = rest.find(';') else {
            out.push_str(rest);
            return out;
        };
        if semi > 12 {
            out.push('&');
            rest = &rest[1..];
            continue;
        }
        let entity = &rest[1..semi];
        let replacement = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" | "#39" => Some('\''),
            "nbsp" => Some(' '),
            _ if entity.starts_with("#x") || entity.starts_with("#X") => {
                u32::from_str_radix(&entity[2..], 16)
                    .ok()
                    .and_then(char::from_u32)
            }
            _ if entity.starts_with('#') => {
                entity[1..].parse::<u32>().ok().and_then(char::from_u32)
            }
            _ => None,
        };
        if let Some(ch) = replacement {
            out.push(ch);
            rest = &rest[semi + 1..];
        } else {
            out.push('&');
            rest = &rest[1..];
        }
    }
    out.push_str(rest);
    out
}

pub(crate) fn strip_html_tags(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(lt) = rest.find('<') {
        out.push_str(&rest[..lt]);
        let after_lt = &rest[lt + 1..];
        let Some(gt) = after_lt.find('>') else {
            rest = "";
            break;
        };
        let tag_body = after_lt[..gt].trim().trim_start_matches('/');
        let tag_name = tag_body
            .split(|c: char| c.is_ascii_whitespace() || c == '/')
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        if matches!(
            tag_name.as_str(),
            "p" | "br"
                | "div"
                | "li"
                | "tr"
                | "td"
                | "th"
                | "h1"
                | "h2"
                | "h3"
                | "h4"
                | "h5"
                | "h6"
                | "blockquote"
                | "pre"
        ) && !out.ends_with(char::is_whitespace)
        {
            out.push(' ');
        }
        rest = &after_lt[gt + 1..];
    }
    out.push_str(rest);
    let decoded = decode_html_entities(&out);
    decoded.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Extract an attribute value by name from a single opening tag (`<tag ...>`), supporting single-quoted, double-quoted and unquoted values.
pub(crate) fn extract_attr(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let target = name.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if i > 0 && (bytes[i - 1].is_ascii_whitespace()) && bytes[i..].starts_with(target) {
            let mut j = i + target.len();
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'=' {
                j += 1;
                while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
                if j >= bytes.len() {
                    return Some(String::new());
                }
                let quote = bytes[j];
                if quote == b'"' || quote == b'\'' {
                    let start = j + 1;
                    if let Some(rel_end) = tag[start..].find(quote as char) {
                        return Some(decode_html_entities(&tag[start..start + rel_end]));
                    }
                } else {
                    let start = j;
                    let mut end = start;
                    while end < bytes.len()
                        && !bytes[end].is_ascii_whitespace()
                        && bytes[end] != b'>'
                    {
                        end += 1;
                    }
                    return Some(decode_html_entities(&tag[start..end]));
                }
            }
        }
        i += 1;
    }
    None
}

pub(crate) fn has_class(tag: &str, class_name: &str) -> bool {
    extract_attr(tag, "class")
        .is_some_and(|classes| classes.split_ascii_whitespace().any(|c| c == class_name))
}

/// Find every complete block matching `<tag ...>` that contains the given CSS class (pairing nested same-name tags correctly).
pub(crate) fn extract_tag_blocks_by_class<'a>(
    html: &'a str,
    tag_name: &str,
    class_name: &str,
) -> Vec<&'a str> {
    let lower = html.to_ascii_lowercase();
    let open_pat = format!("<{tag_name}");
    let close_pat = format!("</{tag_name}");
    let mut blocks = Vec::new();
    let mut cursor = 0usize;

    while let Some(rel_start) = lower[cursor..].find(&open_pat) {
        let start = cursor + rel_start;
        let after_name = start + open_pat.len();
        let next_byte = lower.as_bytes().get(after_name).copied();
        if !matches!(next_byte, Some(b' ' | b'\t' | b'\n' | b'\r' | b'>' | b'/')) {
            cursor = after_name;
            continue;
        }
        let Some(rel_tag_end) = lower[start..].find('>') else {
            break;
        };
        let tag_end = start + rel_tag_end + 1;
        let open_tag = &html[start..tag_end];
        if !has_class(open_tag, class_name) {
            cursor = tag_end;
            continue;
        }
        let mut depth = 1usize;
        let mut scan = tag_end;
        let mut block_end = html.len();
        while scan < lower.len() {
            let next_open = lower[scan..].find(&open_pat).map(|p| scan + p);
            let next_close = lower[scan..].find(&close_pat).map(|p| scan + p);
            match (next_open, next_close) {
                (_, None) => break,
                (Some(o), Some(c)) if o < c => {
                    let after = o + open_pat.len();
                    if matches!(
                        lower.as_bytes().get(after).copied(),
                        Some(b' ' | b'\t' | b'\n' | b'\r' | b'>' | b'/')
                    ) {
                        depth += 1;
                    }
                    scan = after;
                }
                (_, Some(c)) => {
                    let after = c + close_pat.len();
                    let is_close = matches!(
                        lower.as_bytes().get(after).copied(),
                        Some(b' ' | b'\t' | b'\n' | b'\r' | b'>')
                    );
                    let close_gt = lower[c..].find('>').map_or(html.len(), |p| c + p + 1);
                    if is_close {
                        depth -= 1;
                        if depth == 0 {
                            block_end = close_gt;
                            break;
                        }
                    }
                    scan = close_gt;
                }
            }
        }
        blocks.push(&html[start..block_end]);
        cursor = block_end;
    }

    blocks
}

/// Extract the first matching tag `(open_tag, inner_html)` within the given fragment.
pub(crate) fn find_first_tag_with_class<'a>(
    fragment: &'a str,
    tag_names: &[&str],
    class_name: Option<&str>,
) -> Option<(&'a str, &'a str)> {
    let lower = fragment.to_ascii_lowercase();
    let mut best: Option<(usize, &str)> = None;
    for tag in tag_names {
        let open_pat = format!("<{tag}");
        let mut scan = 0usize;
        while let Some(rel) = lower[scan..].find(&open_pat) {
            let start = scan + rel;
            let after = start + open_pat.len();
            if !matches!(
                lower.as_bytes().get(after).copied(),
                Some(b' ' | b'\t' | b'\n' | b'\r' | b'>' | b'/')
            ) {
                scan = after;
                continue;
            }
            let tag_end = lower[start..].find('>')? + start + 1;
            let open_tag = &fragment[start..tag_end];
            if class_name.is_none_or(|c| has_class(open_tag, c)) {
                if best.is_none_or(|(pos, _)| start < pos) {
                    best = Some((start, *tag));
                }
                break;
            }
            scan = tag_end;
        }
    }
    let (start, tag) = best?;
    let tag_end = lower[start..].find('>')? + start + 1;
    let open_tag = &fragment[start..tag_end];
    let close_pat = format!("</{tag}");
    let open_pat = format!("<{tag}");
    let mut depth = 1usize;
    let mut scan = tag_end;
    let mut inner_end = fragment.len();
    while scan < lower.len() {
        let next_open = lower[scan..].find(&open_pat).map(|p| scan + p);
        let next_close = lower[scan..].find(&close_pat).map(|p| scan + p);
        match (next_open, next_close) {
            (_, None) => break,
            (Some(o), Some(c)) if o < c => {
                let after = o + open_pat.len();
                if matches!(
                    lower.as_bytes().get(after).copied(),
                    Some(b' ' | b'\t' | b'\n' | b'\r' | b'>' | b'/')
                ) {
                    depth += 1;
                }
                scan = after;
            }
            (_, Some(c)) => {
                let after = c + close_pat.len();
                let is_close = matches!(
                    lower.as_bytes().get(after).copied(),
                    Some(b' ' | b'\t' | b'\n' | b'\r' | b'>')
                );
                let close_gt = lower[c..].find('>').map_or(fragment.len(), |p| c + p + 1);
                if is_close {
                    depth -= 1;
                    if depth == 0 {
                        inner_end = c;
                        break;
                    }
                }
                scan = close_gt;
            }
        }
    }
    Some((open_tag, &fragment[tag_end..inner_end]))
}

pub(crate) fn targets_hackernews(parsed: &StructuredQuery) -> bool {
    !parsed.sites.is_empty()
        && parsed
            .sites
            .iter()
            .all(|site| site.split('/').next() == Some("news.ycombinator.com"))
}
