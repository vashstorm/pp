//! Positions and symbols: line text, symbol matching and UTF-16 column resolution.

use anyhow::{Result, ensure};
use serde_json::{Value, json};

pub(super) fn line_text(text: &str, line: u32) -> &str {
    let raw = text
        .split('\n')
        .nth(line.saturating_sub(1) as usize)
        .unwrap_or("");
    raw.strip_suffix('\r').unwrap_or(raw)
}

fn is_bare_identifier(symbol: &str) -> bool {
    let mut chars = symbol.chars();
    match chars.next() {
        Some(first) if first == '$' || first == '_' || first.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|ch| ch == '$' || ch == '_' || ch.is_ascii_alphanumeric())
}

fn is_identifier_byte(byte: u8) -> bool {
    byte == b'$' || byte == b'_' || byte.is_ascii_alphanumeric()
}

/// Find the byte index of every match on a single line of text; a bare identifier requires word boundaries.
fn find_matches(line: &str, symbol: &str, case_insensitive: bool) -> Vec<usize> {
    if symbol.is_empty() {
        return Vec::new();
    }
    let (haystack, needle) = if case_insensitive {
        (line.to_ascii_lowercase(), symbol.to_ascii_lowercase())
    } else {
        (line.to_owned(), symbol.to_owned())
    };
    let boundary = is_bare_identifier(symbol);
    let bytes = haystack.as_bytes();
    let mut matches = Vec::new();
    let mut from = 0usize;
    while from + needle.len() <= bytes.len() {
        let Some(offset) = haystack[from..].find(needle.as_str()) else {
            break;
        };
        let index = from + offset;
        let before_ok = index == 0 || !is_identifier_byte(bytes[index - 1]);
        let after = index + needle.len();
        let after_ok = after >= bytes.len() || !is_identifier_byte(bytes[after]);
        if !boundary || (before_ok && after_ok) {
            matches.push(index);
        }
        from = index + needle.len().max(1);
    }
    matches
}

/// `name` or `name#N` (N is a 1-based occurrence count, the `#` suffix is parsed greedily).
fn parse_symbol_spec(spec: &str) -> (String, usize) {
    if let Some((name, digits)) = spec.rsplit_once('#')
        && !name.is_empty()
        && !digits.is_empty()
        && digits.bytes().all(|byte| byte.is_ascii_digit())
    {
        let occurrence = digits.parse::<usize>().unwrap_or(1).max(1);
        return (name.to_owned(), occurrence);
    }
    (spec.to_owned(), 1)
}

/// Scan ±5 neighbouring lines when the symbol is not found on the target line, returning at most 3 candidate line numbers (1-based) sorted by distance.
fn nearby_symbol_lines(text: &str, line: u32, symbol: &str) -> Vec<u32> {
    const WINDOW: u32 = 5;
    let start = line.saturating_sub(WINDOW).max(1);
    let end = line.saturating_add(WINDOW);
    let mut hits = Vec::new();
    for (idx, raw) in text
        .split('\n')
        .enumerate()
        .skip((start - 1) as usize)
        .take((end - start + 1) as usize)
    {
        let current = idx as u32 + 1;
        if current == line {
            continue;
        }
        let body = raw.strip_suffix('\r').unwrap_or(raw);
        if !find_matches(body, symbol, false).is_empty()
            || !find_matches(body, symbol, true).is_empty()
        {
            hits.push(current);
        }
    }
    hits.sort_by_key(|&candidate| (candidate.abs_diff(line), candidate));
    hits.truncate(3);
    hits
}

/// Resolve the UTF-16 column on `line`; without `symbol` use the first non-whitespace character.
pub(super) fn resolve_column(text: &str, line: u32, spec: Option<&str>) -> Result<u32> {
    let line = line.max(1);
    let current = line_text(text, line);
    let byte = match spec {
        None => current
            .char_indices()
            .find(|(_, ch)| !ch.is_whitespace())
            .map_or(0, |(index, _)| index),
        Some(spec) => {
            let (symbol, occurrence) = parse_symbol_spec(spec);
            ensure!(!symbol.is_empty(), "symbol must not be empty");
            let mut matches = find_matches(current, &symbol, false);
            if matches.is_empty() {
                matches = find_matches(current, &symbol, true);
            }
            if matches.is_empty() {
                let nearby = nearby_symbol_lines(text, line, &symbol);
                if nearby.is_empty() {
                    anyhow::bail!("Symbol \"{symbol}\" was not found on line {line}");
                }
                let list = nearby
                    .iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(", ");
                let label = if nearby.len() == 1 { "line" } else { "lines" };
                anyhow::bail!(
                    "Symbol \"{symbol}\" was not found on line {line} (found on nearby {label} {list})"
                );
            }
            ensure!(
                occurrence <= matches.len(),
                "Symbol \"{symbol}\" occurrence {occurrence} is out of bounds on line {line} (found {})",
                matches.len()
            );
            matches[occurrence - 1]
        }
    };
    let prefix = current.get(..byte).unwrap_or("");
    Ok(prefix.encode_utf16().count() as u32)
}

pub(super) fn position_json(text: &str, line: u32, symbol: Option<&str>) -> Result<Value> {
    let line = line.max(1);
    let character = resolve_column(text, line, symbol)?;
    Ok(json!({"line": line - 1, "character": character}))
}
