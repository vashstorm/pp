use super::{
    ConstraintFilterResult, QuerySyntax, QueryTerm, SearchSource, StructuredQuery,
    iso_date_to_epoch_secs, now_epoch_secs,
};
use reqwest::Url;

fn quote_value(value: &str) -> String {
    if value.chars().any(char::is_whitespace) {
        format!("\"{value}\"")
    } else {
        value.to_owned()
    }
}

fn render_term(term: &QueryTerm, syntax: QuerySyntax) -> Option<String> {
    if term.negated && !syntax.negation {
        return None;
    }
    let body = if term.phrase && syntax.phrases {
        format!("\"{}\"", term.text)
    } else {
        term.text.clone()
    };
    Some(if term.negated {
        format!("-{body}")
    } else {
        body
    })
}

pub(super) fn render_terms(terms: &[QueryTerm], syntax: QuerySyntax) -> String {
    let mut parts = Vec::new();
    let mut i = 0;
    while i < terms.len() {
        let term = &terms[i];
        if let Some(group) = term.group
            && syntax.or
        {
            let mut members = Vec::new();
            let mut j = i;
            while j < terms.len() && terms[j].group == Some(group) {
                if let Some(rendered) = render_term(&terms[j], syntax) {
                    members.push(rendered);
                }
                j += 1;
            }
            i = j;
            if members.len() > 1 {
                parts.push(format!("({})", members.join(" OR ")));
            } else if let Some(single) = members.into_iter().next() {
                parts.push(single);
            }
            continue;
        }
        if let Some(rendered) = render_term(term, syntax) {
            parts.push(rendered);
        }
        i += 1;
    }
    parts.join(" ")
}

pub fn format_query(q: &StructuredQuery, syntax: QuerySyntax) -> String {
    let mut parts = Vec::new();
    let text = render_terms(&q.terms, syntax);
    if !text.is_empty() {
        parts.push(text);
    }
    if syntax.site {
        if q.sites.len() > 1 && syntax.or {
            let group: Vec<String> = q.sites.iter().map(|s| format!("site:{s}")).collect();
            parts.push(format!("({})", group.join(" OR ")));
        } else {
            parts.extend(q.sites.iter().map(|s| format!("site:{s}")));
        }
        parts.extend(q.excluded_sites.iter().map(|s| format!("-site:{s}")));
    }
    if syntax.in_url {
        parts.extend(q.in_url.iter().map(|v| format!("inurl:{}", quote_value(v))));
        parts.extend(
            q.excluded_in_url
                .iter()
                .map(|v| format!("-inurl:{}", quote_value(v))),
        );
    }
    if syntax.in_title {
        parts.extend(
            q.in_title
                .iter()
                .map(|v| format!("intitle:{}", quote_value(v))),
        );
        parts.extend(
            q.excluded_in_title
                .iter()
                .map(|v| format!("-intitle:{}", quote_value(v))),
        );
    }
    if syntax.in_text {
        parts.extend(
            q.in_text
                .iter()
                .map(|v| format!("intext:{}", quote_value(v))),
        );
        parts.extend(
            q.excluded_in_text
                .iter()
                .map(|v| format!("-intext:{}", quote_value(v))),
        );
    }
    if syntax.filetype {
        if q.filetypes.len() > 1 && syntax.or {
            let group: Vec<String> = q
                .filetypes
                .iter()
                .map(|f| format!("filetype:{f}"))
                .collect();
            parts.push(format!("({})", group.join(" OR ")));
        } else {
            parts.extend(q.filetypes.iter().map(|f| format!("filetype:{f}")));
        }
        parts.extend(
            q.excluded_filetypes
                .iter()
                .map(|f| format!("-filetype:{f}")),
        );
    }
    if syntax.date_range {
        if let Some(after) = &q.after {
            parts.push(format!("after:{after}"));
        }
        if let Some(before) = &q.before {
            parts.push(format!("before:{before}"));
        }
    }

    let result = parts.join(" ").trim().to_owned();
    if !result.is_empty() {
        return result;
    }
    let fallback: Vec<&str> = q
        .sites
        .iter()
        .chain(&q.in_title)
        .chain(&q.in_url)
        .chain(&q.in_text)
        .chain(&q.filetypes)
        .map(String::as_str)
        .collect();
    let joined = fallback.join(" ").trim().to_owned();
    if joined.is_empty() {
        q.raw.trim().to_owned()
    } else {
        joined
    }
}

pub fn format_scraper_query(query: &str, parsed: &StructuredQuery, syntax: QuerySyntax) -> String {
    if !parsed.has_directives {
        return query.to_owned();
    }
    let mut downgraded = parsed.clone();
    let mut bare_sites = Vec::new();
    for site in &parsed.sites {
        if site.contains('/') {
            downgraded.terms.push(QueryTerm {
                text: site.clone(),
                phrase: false,
                negated: false,
                group: None,
            });
        } else {
            bare_sites.push(site.clone());
        }
    }
    for val in &parsed.in_url {
        downgraded.terms.push(QueryTerm {
            text: val.clone(),
            phrase: false,
            negated: false,
            group: None,
        });
    }
    downgraded.sites = bare_sites;
    downgraded.in_url.clear();
    format_query(&downgraded, syntax)
}

fn host_and_path(raw_url: &str) -> Option<(String, String)> {
    let url = Url::parse(raw_url).ok()?;
    let host = url.host_str()?.to_ascii_lowercase();
    Some((host, url.path().to_owned()))
}

pub fn matches_site(url: &str, site: &str) -> bool {
    let Some((host, path)) = host_and_path(url) else {
        return false;
    };
    let (site_host, site_path) = match site.split_once('/') {
        Some((h, p)) => (h, Some(p)),
        None => (site, None),
    };
    if host != site_host && !host.ends_with(&format!(".{site_host}")) {
        return false;
    }
    if let Some(prefix) = site_path {
        let lower_path = path.to_ascii_lowercase();
        let lower_prefix = format!("/{}", prefix.trim_start_matches('/').to_ascii_lowercase());
        if !lower_path.starts_with(&lower_prefix) {
            return false;
        }
    }
    true
}

fn matches_filetype(url: &str, ext: &str) -> bool {
    host_and_path(url)
        .is_some_and(|(_, path)| path.to_ascii_lowercase().ends_with(&format!(".{ext}")))
}

fn parse_relative_age_seconds(text: &str) -> Option<i64> {
    let trimmed = text.trim().to_ascii_lowercase();
    let rest = trimmed.strip_suffix("ago")?.trim();
    let mut parts = rest.split_whitespace();
    let num: i64 = parts.next()?.parse().ok()?;
    let unit = parts.next()?;
    if parts.next().is_some() || num <= 0 {
        return None;
    }
    let mult = match unit {
        "m" | "min" | "minute" | "minutes" => 60_i64,
        "h" | "hr" | "hour" | "hours" => 3600,
        "d" | "day" | "days" => 86_400,
        "w" | "week" | "weeks" => 604_800,
        "mo" | "month" | "months" => 2_592_000,
        "y" | "yr" | "year" | "years" => 31_536_000,
        _ => return None,
    };
    num.checked_mul(mult)
}

fn source_time_epoch(source: &SearchSource) -> Option<i64> {
    if let Some(age) = source.age_seconds {
        return now_epoch_secs().checked_sub(age);
    }
    let published = source.published_date.as_deref()?;
    if let Some(secs) = parse_relative_age_seconds(published) {
        return now_epoch_secs().checked_sub(secs);
    }
    iso_date_to_epoch_secs(published.trim())
}

/// Run lenient post-filtering per dimension: when a constraint dimension would filter every result out, relax that dimension and record a note.
pub fn apply_query_constraints(
    sources: &[SearchSource],
    q: &StructuredQuery,
) -> ConstraintFilterResult {
    let mut current = sources.to_vec();
    let mut dropped = Vec::new();
    if current.is_empty() || !q.has_constraints {
        return ConstraintFilterResult {
            sources: current,
            dropped,
        };
    }

    let mut apply_dim = |label: String, pred: &dyn Fn(&SearchSource) -> bool| {
        let kept: Vec<SearchSource> = current.iter().filter(|s| pred(s)).cloned().collect();
        if !kept.is_empty() {
            current = kept;
        } else {
            dropped.push(label);
        }
    };

    if !q.sites.is_empty() {
        let label = q
            .sites
            .iter()
            .map(|s| format!("site:{s}"))
            .collect::<Vec<_>>()
            .join(" OR ");
        apply_dim(label, &|s| {
            q.sites.iter().any(|site| matches_site(&s.url, site))
        });
    }
    if !q.excluded_sites.is_empty() {
        let label = q
            .excluded_sites
            .iter()
            .map(|s| format!("-site:{s}"))
            .collect::<Vec<_>>()
            .join(" ");
        apply_dim(label, &|s| {
            !q.excluded_sites
                .iter()
                .any(|site| matches_site(&s.url, site))
        });
    }
    if !q.in_url.is_empty() {
        let label = q
            .in_url
            .iter()
            .map(|v| format!("inurl:{v}"))
            .collect::<Vec<_>>()
            .join(" ");
        apply_dim(label, &|s| {
            let url = s.url.to_ascii_lowercase();
            q.in_url
                .iter()
                .all(|v| url.contains(&v.to_ascii_lowercase()))
        });
    }
    if !q.excluded_in_url.is_empty() {
        let label = q
            .excluded_in_url
            .iter()
            .map(|v| format!("-inurl:{v}"))
            .collect::<Vec<_>>()
            .join(" ");
        apply_dim(label, &|s| {
            let url = s.url.to_ascii_lowercase();
            !q.excluded_in_url
                .iter()
                .any(|v| url.contains(&v.to_ascii_lowercase()))
        });
    }
    if !q.in_title.is_empty() {
        let label = q
            .in_title
            .iter()
            .map(|v| format!("intitle:{v}"))
            .collect::<Vec<_>>()
            .join(" ");
        apply_dim(label, &|s| {
            let title = s.title.to_ascii_lowercase();
            q.in_title
                .iter()
                .all(|v| title.contains(&v.to_ascii_lowercase()))
        });
    }
    if !q.excluded_in_title.is_empty() {
        let label = q
            .excluded_in_title
            .iter()
            .map(|v| format!("-intitle:{v}"))
            .collect::<Vec<_>>()
            .join(" ");
        apply_dim(label, &|s| {
            let title = s.title.to_ascii_lowercase();
            !q.excluded_in_title
                .iter()
                .any(|v| title.contains(&v.to_ascii_lowercase()))
        });
    }
    if !q.filetypes.is_empty() {
        let label = q
            .filetypes
            .iter()
            .map(|f| format!("filetype:{f}"))
            .collect::<Vec<_>>()
            .join(" OR ");
        apply_dim(label, &|s| {
            q.filetypes.iter().any(|ext| matches_filetype(&s.url, ext))
        });
    }
    if !q.excluded_filetypes.is_empty() {
        let label = q
            .excluded_filetypes
            .iter()
            .map(|f| format!("-filetype:{f}"))
            .collect::<Vec<_>>()
            .join(" ");
        apply_dim(label, &|s| {
            !q.excluded_filetypes
                .iter()
                .any(|ext| matches_filetype(&s.url, ext))
        });
    }
    if q.after.is_some() || q.before.is_some() {
        let after_secs = q.after.as_deref().and_then(iso_date_to_epoch_secs);
        let before_secs = q.before.as_deref().and_then(iso_date_to_epoch_secs);
        let label = [
            q.after.as_ref().map(|d| format!("after:{d}")),
            q.before.as_ref().map(|d| format!("before:{d}")),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ");
        apply_dim(label, &|s| {
            let Some(ts) = source_time_epoch(s) else {
                return true;
            };
            if let Some(after) = after_secs
                && ts < after
            {
                return false;
            }
            if let Some(before) = before_secs
                && ts >= before
            {
                return false;
            }
            true
        });
    }

    ConstraintFilterResult {
        sources: current,
        dropped,
    }
}
