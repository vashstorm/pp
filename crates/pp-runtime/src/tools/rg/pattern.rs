//! `rg` 正则编译：Rust regex 优先、PCRE2 回退与括号／量词兼容。
use anyhow::{Context, Result};

pub(super) enum Compiled {
    Rust(grep_regex::RegexMatcher),
    Pcre(grep_pcre2::RegexMatcher),
}
pub(super) fn compile(
    pattern: &str,
    insensitive: bool,
    multiline: bool,
    fixed: bool,
    word: bool,
) -> Result<Compiled> {
    let rust = |p: &str| {
        let mut b = grep_regex::RegexMatcherBuilder::new();
        b.case_insensitive(insensitive)
            .multi_line(multiline)
            .fixed_strings(fixed)
            .word(word);
        b.build(p)
    };
    let pcre = |p: &str| {
        let mut b = grep_pcre2::RegexMatcherBuilder::new();
        b.caseless(insensitive)
            .multi_line(multiline)
            .utf(true)
            .ucp(true)
            .fixed_strings(fixed)
            .word(word)
            .jit_if_available(true);
        b.build(p)
    };
    if fixed {
        return Ok(Compiled::Rust(rust(pattern).context("invalid rg pattern")?));
    }
    let sanitized = sanitize_braces(pattern);
    match rust(&sanitized) {
        Ok(m) => return Ok(Compiled::Rust(m)),
        Err(e) => {
            if let Ok(m) = pcre(&sanitized) {
                return Ok(Compiled::Pcre(m));
            }
            if e.to_string().contains("unclosed group") || e.to_string().contains("unopened group")
            {
                let fixed = escape_parens(&sanitized);
                if let Ok(m) = rust(&fixed) {
                    return Ok(Compiled::Rust(m));
                }
                if let Ok(m) = pcre(&fixed) {
                    return Ok(Compiled::Pcre(m));
                }
            }
        }
    }
    Ok(Compiled::Rust(
        rust(&regex::escape(pattern)).context("invalid rg pattern")?,
    ))
}
fn sanitize_braces(p: &str) -> String {
    let mut out = String::new();
    let mut chars = p.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            out.push(c);
            if let Some(n) = chars.next() {
                out.push(n);
                if matches!(n, 'p' | 'P' | 'x' | 'u') && chars.peek() == Some(&'{') {
                    for x in chars.by_ref() {
                        out.push(x);
                        if x == '}' {
                            break;
                        }
                    }
                }
            }
            continue;
        }
        if c == '{' {
            let suffix: String = chars.clone().collect();
            let end = suffix.find('}');
            let valid = end.is_some_and(|i| {
                let q = &suffix[..i];
                let mut parts = q.split(',');
                let a = parts.next().unwrap_or("");
                !a.is_empty()
                    && a.bytes().all(|x| x.is_ascii_digit())
                    && parts
                        .next()
                        .is_none_or(|b| b.bytes().all(|x| x.is_ascii_digit()))
                    && parts.next().is_none()
            });
            if !valid {
                out.push('\\');
            }
        }
        if c == '}' {
            // 有效量词的右括号已经由左括号处复制。
            out.push_str("\\}");
            continue;
        }
        if c == '{' {
            let suffix: String = chars.clone().collect();
            if let Some(i) = suffix.find('}') {
                let q = &suffix[..i];
                let mut parts = q.split(',');
                let a = parts.next().unwrap_or("");
                if !a.is_empty()
                    && a.bytes().all(|x| x.is_ascii_digit())
                    && parts
                        .next()
                        .is_none_or(|b| b.bytes().all(|x| x.is_ascii_digit()))
                    && parts.next().is_none()
                {
                    out.push('{');
                    for _ in 0..=i {
                        if let Some(ch) = chars.next() {
                            out.push(ch);
                        }
                    }
                    continue;
                }
            }
        }
        out.push(c);
    }
    out
}
fn escape_parens(p: &str) -> String {
    let mut out = String::new();
    let mut chars = p.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            out.push(c);
            if let Some(n) = chars.next() {
                out.push(n);
            }
        } else {
            if matches!(c, '(' | ')') {
                out.push('\\');
            }
            out.push(c);
        }
    }
    out
}
