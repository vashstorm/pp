//! 位置与符号：行文本、符号匹配与 UTF-16 列解析。

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

/// 在单行文本上找出全部匹配的字节下标；裸标识符要求词边界。
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

/// `name` 或 `name#N`（N 为 1-based 出现次数，贪婪解析 `#` 后缀）。
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

/// 解析 `line` 上的 UTF-16 列；无 `symbol` 时取首个非空白字符。
pub(super) fn resolve_column(text: &str, line: u32, spec: Option<&str>) -> Result<u32> {
    let line = line.max(1);
    let text = line_text(text, line);
    let byte = match spec {
        None => text
            .char_indices()
            .find(|(_, ch)| !ch.is_whitespace())
            .map_or(0, |(index, _)| index),
        Some(spec) => {
            let (symbol, occurrence) = parse_symbol_spec(spec);
            ensure!(!symbol.is_empty(), "symbol must not be empty");
            let mut matches = find_matches(text, &symbol, false);
            if matches.is_empty() {
                matches = find_matches(text, &symbol, true);
            }
            ensure!(
                !matches.is_empty(),
                "Symbol \"{symbol}\" was not found on line {line}"
            );
            ensure!(
                occurrence <= matches.len(),
                "Symbol \"{symbol}\" occurrence {occurrence} is out of bounds on line {line} (found {})",
                matches.len()
            );
            matches[occurrence - 1]
        }
    };
    let prefix = text.get(..byte).unwrap_or("");
    Ok(prefix.encode_utf16().count() as u32)
}

pub(super) fn position_json(text: &str, line: u32, symbol: Option<&str>) -> Result<Value> {
    let line = line.max(1);
    let character = resolve_column(text, line, symbol)?;
    Ok(json!({"line": line - 1, "character": character}))
}
