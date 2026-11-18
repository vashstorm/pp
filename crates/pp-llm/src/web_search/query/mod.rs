use super::SearchSource;
use std::time::{SystemTime, UNIX_EPOCH};

/// 查询中的单个自由文本词条（非指令部分）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryTerm {
    pub text: String,
    pub phrase: bool,
    pub negated: bool,
    pub group: Option<usize>,
}

/// 从原始查询解析出的结构化查询。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StructuredQuery {
    pub raw: String,
    pub text: String,
    pub terms: Vec<QueryTerm>,
    pub sites: Vec<String>,
    pub excluded_sites: Vec<String>,
    pub in_url: Vec<String>,
    pub excluded_in_url: Vec<String>,
    pub in_title: Vec<String>,
    pub excluded_in_title: Vec<String>,
    pub in_text: Vec<String>,
    pub excluded_in_text: Vec<String>,
    pub filetypes: Vec<String>,
    pub excluded_filetypes: Vec<String>,
    pub after: Option<String>,
    pub before: Option<String>,
    pub lang: Option<String>,
    pub has_directives: bool,
    pub has_constraints: bool,
}

/// 搜索引擎原生支持的查询语法集合。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QuerySyntax {
    pub phrases: bool,
    pub negation: bool,
    pub or: bool,
    pub site: bool,
    pub in_url: bool,
    pub in_title: bool,
    pub in_text: bool,
    pub filetype: bool,
    pub date_range: bool,
}

pub const GOOGLE_QUERY_SYNTAX: QuerySyntax = QuerySyntax {
    phrases: true,
    negation: true,
    or: true,
    site: true,
    in_url: true,
    in_title: true,
    in_text: true,
    filetype: true,
    date_range: true,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConstraintFilterResult {
    pub sources: Vec<SearchSource>,
    pub dropped: Vec<String>,
}

#[derive(Clone, Copy)]
enum AllMode {
    Title,
    Url,
    Text,
}

#[derive(Clone, Copy)]
enum DirectiveField {
    Site,
    InUrl,
    InTitle,
    InText,
    Filetype,
    Before,
    After,
    Lang,
}

struct RawToken {
    text: String,
    quoted: bool,
    quoted_value: bool,
}

fn is_quote(ch: char) -> bool {
    matches!(ch, '"' | '\u{201c}' | '\u{201d}')
}

fn tokenize(raw: &str) -> Vec<RawToken> {
    let chars: Vec<char> = raw.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_whitespace() {
            i += 1;
            continue;
        }
        if is_quote(chars[i]) {
            let mut j = i + 1;
            let mut buf = String::new();
            while j < chars.len() && !is_quote(chars[j]) {
                buf.push(chars[j]);
                j += 1;
            }
            let trimmed = buf.trim();
            if !trimmed.is_empty() {
                tokens.push(RawToken {
                    text: trimmed.to_owned(),
                    quoted: true,
                    quoted_value: false,
                });
            }
            i = j + 1;
            continue;
        }
        let mut buf = String::new();
        let mut quoted_value = false;
        while i < chars.len() && !chars[i].is_whitespace() {
            let ch = chars[i];
            if is_quote(ch) && buf.ends_with(':') {
                let mut j = i + 1;
                while j < chars.len() && !is_quote(chars[j]) {
                    buf.push(chars[j]);
                    j += 1;
                }
                quoted_value = true;
                i = j + 1;
                continue;
            }
            if is_quote(ch) {
                break;
            }
            buf.push(ch);
            i += 1;
        }
        if !buf.is_empty() {
            tokens.push(RawToken {
                text: buf,
                quoted: false,
                quoted_value,
            });
        }
    }
    split_parens(tokens)
}

fn split_parens(tokens: Vec<RawToken>) -> Vec<RawToken> {
    let mut out = Vec::with_capacity(tokens.len());
    for tok in tokens {
        if tok.quoted || tok.quoted_value {
            out.push(tok);
            continue;
        }
        let mut text = tok.text;
        while let Some(rest) = text.strip_prefix('(') {
            out.push(RawToken {
                text: "(".to_owned(),
                quoted: false,
                quoted_value: false,
            });
            text = rest.to_owned();
        }
        let mut trailing = 0usize;
        while let Some(body) = text.strip_suffix(')') {
            let mut depth = 0isize;
            for ch in body.chars() {
                if ch == '(' {
                    depth += 1;
                } else if ch == ')' {
                    depth -= 1;
                }
            }
            if depth > 0 {
                break;
            }
            text = body.to_owned();
            trailing += 1;
        }
        if !text.is_empty() {
            out.push(RawToken {
                text,
                quoted: false,
                quoted_value: false,
            });
        }
        for _ in 0..trailing {
            out.push(RawToken {
                text: ")".to_owned(),
                quoted: false,
                quoted_value: false,
            });
        }
    }
    out
}

fn parse_directive(token: &str) -> Option<(bool, String, &str)> {
    let (negated, rest) = if let Some(s) = token.strip_prefix('-') {
        (true, s)
    } else if let Some(s) = token.strip_prefix('+') {
        (false, s)
    } else {
        (false, token)
    };
    let (name, value) = rest.split_once(':')?;
    let mut chars = name.chars();
    let first = chars.next()?;
    if !first.is_ascii_alphabetic() || !chars.all(|c| c.is_ascii_alphabetic() || c == '-') {
        return None;
    }
    Some((negated, name.to_ascii_lowercase(), value))
}

fn directive_field(name: &str) -> Option<DirectiveField> {
    match name {
        "site" | "domain" | "host" => Some(DirectiveField::Site),
        "inurl" | "url" => Some(DirectiveField::InUrl),
        "intitle" | "title" => Some(DirectiveField::InTitle),
        "intext" | "inbody" | "inanchor" => Some(DirectiveField::InText),
        "filetype" | "ext" => Some(DirectiveField::Filetype),
        "before" | "until" => Some(DirectiveField::Before),
        "after" | "since" => Some(DirectiveField::After),
        "lang" | "language" => Some(DirectiveField::Lang),
        _ => None,
    }
}

fn all_mode(name: &str) -> Option<AllMode> {
    match name {
        "allintitle" => Some(AllMode::Title),
        "allinurl" => Some(AllMode::Url),
        "allintext" => Some(AllMode::Text),
        _ => None,
    }
}

fn is_reserved_token(text: &str) -> bool {
    if matches!(
        text,
        "(" | ")" | "OR" | "AND" | "NOT" | "|" | "||" | "&&" | "!"
    ) {
        return true;
    }
    parse_directive(text)
        .is_some_and(|(_, name, _)| directive_field(&name).is_some() || all_mode(&name).is_some())
}

fn format_iso_date(year: i32, month: u32, day: u32) -> Option<String> {
    if !(1000..=9999).contains(&year) || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    Some(format!("{year:04}-{month:02}-{day:02}"))
}

pub fn parse_date_value(value: &str) -> Option<String> {
    let trimmed = value.trim();
    let sep = if trimmed.contains('-') {
        '-'
    } else if trimmed.contains('/') {
        '/'
    } else if trimmed.contains('.') {
        '.'
    } else {
        return (trimmed.len() == 4 && trimmed.bytes().all(|b| b.is_ascii_digit()))
            .then(|| trimmed.parse::<i32>().ok())
            .flatten()
            .and_then(|y| format_iso_date(y, 1, 1));
    };
    let parts: Vec<&str> = trimmed.split(sep).collect();
    if parts
        .iter()
        .any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    match parts.as_slice() {
        [y, m] if y.len() == 4 && (1..=2).contains(&m.len()) => {
            format_iso_date(y.parse().ok()?, m.parse().ok()?, 1)
        }
        [y, m, d] if y.len() == 4 && (1..=2).contains(&m.len()) && (1..=2).contains(&d.len()) => {
            format_iso_date(y.parse().ok()?, m.parse().ok()?, d.parse().ok()?)
        }
        [a, b, y] if y.len() == 4 && (1..=2).contains(&a.len()) && (1..=2).contains(&b.len()) => {
            let mut month: u32 = a.parse().ok()?;
            let mut day: u32 = b.parse().ok()?;
            if month > 12 && day <= 12 {
                std::mem::swap(&mut month, &mut day);
            }
            format_iso_date(y.parse().ok()?, month, day)
        }
        _ => None,
    }
}

/// Howard Hinnant 算法：将公历年月日转换为自 1970-01-01 起的天数。
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = year - i64::from(month <= 2);
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = i64::from(month);
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// 将公历天数转回 `(year, month, day)`。
fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    let year = y + i64::from(m <= 2);
    (year as i32, m as u32, d as u32)
}

pub fn iso_date_to_epoch_secs(iso: &str) -> Option<i64> {
    let prefix = iso.get(..10)?;
    let mut parts = prefix.split('-');
    let y: i64 = parts.next()?.parse().ok()?;
    let m: u32 = parts.next()?.parse().ok()?;
    let d: u32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some(days_from_civil(y, m, d) * 86_400)
}

pub fn now_epoch_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

pub fn recency_after_date(days_back: i64) -> String {
    let secs = now_epoch_secs() - days_back * 86_400;
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

fn normalize_site(value: &str) -> String {
    let mut site = value.trim().to_ascii_lowercase();
    if let Some((_, rest)) = site.split_once("://") {
        site = rest.to_owned();
    }
    if let Some(rest) = site.strip_prefix("*.") {
        site = rest.to_owned();
    }
    site.trim_end_matches(['/', '.']).to_owned()
}

pub fn parse_search_query(raw: &str) -> StructuredQuery {
    let mut q = StructuredQuery {
        raw: raw.to_owned(),
        text: String::new(),
        terms: Vec::new(),
        sites: Vec::new(),
        excluded_sites: Vec::new(),
        in_url: Vec::new(),
        excluded_in_url: Vec::new(),
        in_title: Vec::new(),
        excluded_in_title: Vec::new(),
        in_text: Vec::new(),
        excluded_in_text: Vec::new(),
        filetypes: Vec::new(),
        excluded_filetypes: Vec::new(),
        after: None,
        before: None,
        lang: None,
        has_directives: false,
        has_constraints: false,
    };

    let tokens = tokenize(raw);
    let mut negate_next = false;
    let mut or_pending = false;
    let mut last_was_term = false;
    let mut group_seq = 0usize;
    let mut current_all: Option<AllMode> = None;

    let push_constraint = |q: &mut StructuredQuery,
                           field: DirectiveField,
                           value: &str,
                           negated: bool,
                           or_pending: &mut bool,
                           last_was_term: &mut bool| {
        q.has_directives = true;
        *or_pending = false;
        *last_was_term = false;
        let v = value.trim();
        if v.is_empty() {
            return;
        }
        match field {
            DirectiveField::Site => {
                let site = normalize_site(v);
                if !site.is_empty() {
                    if negated {
                        q.excluded_sites.push(site);
                    } else {
                        q.sites.push(site);
                    }
                }
            }
            DirectiveField::InUrl => {
                if negated {
                    q.excluded_in_url.push(v.to_owned());
                } else {
                    q.in_url.push(v.to_owned());
                }
            }
            DirectiveField::InTitle => {
                if negated {
                    q.excluded_in_title.push(v.to_owned());
                } else {
                    q.in_title.push(v.to_owned());
                }
            }
            DirectiveField::InText => {
                if negated {
                    q.excluded_in_text.push(v.to_owned());
                } else {
                    q.in_text.push(v.to_owned());
                }
            }
            DirectiveField::Filetype => {
                let ext = v.to_ascii_lowercase().trim_start_matches('.').to_owned();
                if !ext.is_empty() {
                    if negated {
                        q.excluded_filetypes.push(ext);
                    } else {
                        q.filetypes.push(ext);
                    }
                }
            }
            DirectiveField::Before | DirectiveField::After | DirectiveField::Lang => {}
        }
    };

    let mut push_term = |q: &mut StructuredQuery,
                         text: String,
                         phrase: bool,
                         current_all: Option<AllMode>,
                         negate_next: &mut bool,
                         or_pending: &mut bool,
                         last_was_term: &mut bool| {
        let negated = *negate_next;
        *negate_next = false;
        if let Some(mode) = current_all {
            let field = match mode {
                AllMode::Title => DirectiveField::InTitle,
                AllMode::Url => DirectiveField::InUrl,
                AllMode::Text => DirectiveField::InText,
            };
            push_constraint(q, field, &text, negated, or_pending, last_was_term);
            return;
        }
        let mut group = None;
        if *or_pending
            && *last_was_term
            && let Some(prev) = q.terms.last_mut()
        {
            if prev.group.is_none() {
                group_seq += 1;
                prev.group = Some(group_seq);
            }
            group = prev.group;
        }
        *or_pending = false;
        *last_was_term = true;
        q.terms.push(QueryTerm {
            text,
            phrase,
            negated,
            group,
        });
    };

    let mut idx = 0;
    while idx < tokens.len() {
        let tok = &tokens[idx];
        if tok.quoted {
            push_term(
                &mut q,
                tok.text.clone(),
                true,
                current_all,
                &mut negate_next,
                &mut or_pending,
                &mut last_was_term,
            );
            idx += 1;
            continue;
        }
        if tok.text == "(" || tok.text == ")" {
            idx += 1;
            continue;
        }
        if matches!(tok.text.as_str(), "OR" | "|" | "||") {
            or_pending = true;
            q.has_directives = true;
            idx += 1;
            continue;
        }
        if matches!(tok.text.as_str(), "AND" | "&&") {
            q.has_directives = true;
            idx += 1;
            continue;
        }
        if matches!(tok.text.as_str(), "NOT" | "!") {
            negate_next = true;
            q.has_directives = true;
            idx += 1;
            continue;
        }
        if tok.text == "-" || tok.text == "+" {
            if tok.text == "-" && tokens.get(idx + 1).is_some_and(|n| n.quoted) {
                negate_next = true;
            }
            idx += 1;
            continue;
        }

        if let Some((prefix_neg, name, inline_val)) = parse_directive(&tok.text) {
            if let Some(mode) = all_mode(&name) {
                current_all = Some(mode);
                q.has_directives = true;
                let inline = inline_val.trim();
                if !inline.is_empty() {
                    let field = match mode {
                        AllMode::Title => DirectiveField::InTitle,
                        AllMode::Url => DirectiveField::InUrl,
                        AllMode::Text => DirectiveField::InText,
                    };
                    push_constraint(
                        &mut q,
                        field,
                        inline,
                        prefix_neg,
                        &mut or_pending,
                        &mut last_was_term,
                    );
                } else {
                    or_pending = false;
                    last_was_term = false;
                }
                idx += 1;
                continue;
            }
            if let Some(field) = directive_field(&name) {
                let mut value = inline_val.trim().to_owned();
                if value.is_empty()
                    && let Some(next) = tokens.get(idx + 1)
                    && (next.quoted || !is_reserved_token(&next.text))
                {
                    value = next.text.trim().to_owned();
                    idx += 1;
                }
                if value.is_empty() {
                    q.has_directives = true;
                    idx += 1;
                    continue;
                }
                let negated = prefix_neg || negate_next;
                negate_next = false;
                match field {
                    DirectiveField::Before | DirectiveField::After => {
                        if let Some(iso) = parse_date_value(&value) {
                            if matches!(field, DirectiveField::Before) {
                                q.before = Some(iso);
                            } else {
                                q.after = Some(iso);
                            }
                            q.has_directives = true;
                            or_pending = false;
                            last_was_term = false;
                        } else {
                            push_term(
                                &mut q,
                                tok.text.clone(),
                                false,
                                current_all,
                                &mut negate_next,
                                &mut or_pending,
                                &mut last_was_term,
                            );
                        }
                    }
                    DirectiveField::Lang => {
                        q.lang = Some(value.to_ascii_lowercase());
                        q.has_directives = true;
                        or_pending = false;
                        last_was_term = false;
                    }
                    _ => {
                        push_constraint(
                            &mut q,
                            field,
                            &value,
                            negated,
                            &mut or_pending,
                            &mut last_was_term,
                        );
                    }
                }
                idx += 1;
                continue;
            }
        }

        if let Some(stripped) = tok.text.strip_prefix('-')
            && !stripped.is_empty()
        {
            negate_next = true;
            q.has_directives = true;
            let trimmed = stripped.trim_start_matches('-');
            if !trimmed.is_empty() {
                if let Some((_, name, val)) = parse_directive(trimmed)
                    && let Some(field) = directive_field(&name)
                    && !matches!(
                        field,
                        DirectiveField::Before | DirectiveField::After | DirectiveField::Lang
                    )
                {
                    negate_next = false;
                    push_constraint(
                        &mut q,
                        field,
                        val,
                        true,
                        &mut or_pending,
                        &mut last_was_term,
                    );
                    idx += 1;
                    continue;
                }
                push_term(
                    &mut q,
                    trimmed.to_owned(),
                    false,
                    current_all,
                    &mut negate_next,
                    &mut or_pending,
                    &mut last_was_term,
                );
            }
            idx += 1;
            continue;
        }

        if let Some(stripped) = tok.text.strip_prefix('+')
            && !stripped.is_empty()
        {
            q.has_directives = true;
            push_term(
                &mut q,
                stripped.to_owned(),
                true,
                current_all,
                &mut negate_next,
                &mut or_pending,
                &mut last_was_term,
            );
            idx += 1;
            continue;
        }

        push_term(
            &mut q,
            tok.text.clone(),
            false,
            current_all,
            &mut negate_next,
            &mut or_pending,
            &mut last_was_term,
        );
        idx += 1;
    }

    q.text = render_terms(
        &q.terms,
        QuerySyntax {
            phrases: true,
            negation: true,
            or: true,
            ..Default::default()
        },
    );
    q.has_constraints = !q.sites.is_empty()
        || !q.excluded_sites.is_empty()
        || !q.in_url.is_empty()
        || !q.excluded_in_url.is_empty()
        || !q.in_title.is_empty()
        || !q.excluded_in_title.is_empty()
        || !q.filetypes.is_empty()
        || !q.excluded_filetypes.is_empty()
        || q.before.is_some()
        || q.after.is_some();
    q
}

mod filter;
use filter::render_terms;
pub use filter::{apply_query_constraints, format_query, format_scraper_query, matches_site};
