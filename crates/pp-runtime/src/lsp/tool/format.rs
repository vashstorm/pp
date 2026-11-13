//! 服务器结果格式化：位置、hover 文本与符号列表。

use crate::lsp::client;
use crate::lsp::types::DocumentSymbol;
use crate::lsp::types::Location;
use crate::lsp::types::LocationLink;
use crate::lsp::types::SymbolInformation;
use crate::lsp::uri;
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::Path;

use super::servers::display_path;

/// 位置上下文行数（上下各一行）。
const CONTEXT_LINES: u32 = 1;

pub(super) fn parse_locations(value: &Value) -> Result<Vec<Location>> {
    let items: Vec<&Value> = match value {
        Value::Null => Vec::new(),
        Value::Array(items) => items.iter().collect(),
        Value::Object(_) => vec![value],
        _ => bail!("the language server returned an unsupported navigation result"),
    };
    let mut locations = Vec::with_capacity(items.len());
    for item in items {
        let object = item
            .as_object()
            .context("the language server returned an unsupported navigation result")?;
        if object.contains_key("uri") {
            let location: Location = serde_json::from_value(item.clone())
                .context("the language server returned an invalid Location")?;
            locations.push(location);
        } else if object.contains_key("targetUri") {
            let link: LocationLink = serde_json::from_value(item.clone())
                .context("the language server returned an invalid LocationLink")?;
            let range = link
                .target_selection_range
                .or(link.target_range)
                .context("the language server returned a LocationLink without a target range")?;
            locations.push(Location {
                uri: link.target_uri,
                range,
            });
        } else {
            bail!("the language server returned a navigation entry without uri or targetUri");
        }
    }
    Ok(locations)
}

pub(super) fn format_location(cwd: &Path, location: &Location) -> String {
    let path = uri::uri_to_path(&location.uri);
    let file = match &path {
        Some(path) => display_path(cwd, path),
        None => location.uri.clone(),
    };
    format!(
        "{file}:{}:{}",
        location.range.start.line.saturating_add(1),
        location.range.start.character.saturating_add(1)
    )
}

pub(super) fn format_location_with_context(cwd: &Path, location: &Location) -> String {
    let header = format!("  {}", format_location(cwd, location));
    let Some(path) = uri::uri_to_path(&location.uri) else {
        return header;
    };
    let Ok(text) = client::read_document_text(&path) else {
        return header;
    };
    let lines: Vec<&str> = text.split('\n').collect();
    if lines.is_empty() {
        return header;
    }
    let line = location.range.start.line.saturating_add(1);
    let total = lines.len() as u32;
    let start = line.saturating_sub(CONTEXT_LINES).max(1);
    let end = (line.saturating_add(CONTEXT_LINES)).min(total);
    let mut rendered = vec![header];
    for current in start..=end {
        let content = lines
            .get(current.saturating_sub(1) as usize)
            .copied()
            .unwrap_or("");
        let content = content.strip_suffix('\r').unwrap_or(content);
        rendered.push(format!("    {current}: {content}"));
    }
    rendered.join("\n")
}

pub(super) fn is_only_queried_declaration(
    locations: &[Location],
    uri: &str,
    position: &Value,
) -> bool {
    let (Some(line), Some(character)) = (
        position.get("line").and_then(Value::as_u64),
        position.get("character").and_then(Value::as_u64),
    ) else {
        return false;
    };
    let (line, character) = (line as u32, character as u32);
    let contains = |range: &crate::lsp::types::Range| {
        let start = (range.start.line, range.start.character);
        let end = (range.end.line, range.end.character);
        start <= (line, character) && (line, character) <= end
    };
    matches!(locations, [only] if only.uri == uri && contains(&only.range))
}

pub(super) fn extract_hover_text(contents: &Value) -> String {
    match contents {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .map(extract_hover_text)
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n"),
        Value::Object(map) => map
            .get("value")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| serde_json::to_string(contents).unwrap_or_default()),
        other => other.to_string(),
    }
}

fn symbol_kind_name(kind: Option<u8>) -> &'static str {
    match kind {
        Some(1) => "File",
        Some(2) => "Module",
        Some(3) => "Namespace",
        Some(4) => "Package",
        Some(5) => "Class",
        Some(6) => "Method",
        Some(7) => "Property",
        Some(8) => "Field",
        Some(9) => "Constructor",
        Some(10) => "Enum",
        Some(11) => "Interface",
        Some(12) => "Function",
        Some(13) => "Variable",
        Some(14) => "Constant",
        Some(15) => "String",
        Some(16) => "Number",
        Some(17) => "Boolean",
        Some(18) => "Array",
        Some(19) => "Object",
        Some(20) => "Key",
        Some(21) => "Null",
        Some(22) => "EnumMember",
        Some(23) => "Struct",
        Some(24) => "Event",
        Some(25) => "Operator",
        Some(26) => "TypeParameter",
        _ => "Unknown",
    }
}

pub(super) enum SymbolEntry {
    Document(DocumentSymbol),
    Flat(SymbolInformation),
}

pub(super) fn parse_symbols(value: &Value) -> Result<Vec<SymbolEntry>> {
    let items: Vec<&Value> = match value {
        Value::Null => Vec::new(),
        Value::Array(items) => items.iter().collect(),
        _ => bail!("the language server returned an invalid symbol list"),
    };
    let mut symbols = Vec::with_capacity(items.len());
    for item in items {
        let object = item
            .as_object()
            .context("the language server returned an invalid symbol")?;
        if object.contains_key("selectionRange") {
            let symbol: DocumentSymbol = serde_json::from_value(item.clone())
                .context("the language server returned an invalid document symbol")?;
            symbols.push(SymbolEntry::Document(symbol));
        } else {
            let symbol: SymbolInformation = serde_json::from_value(item.clone())
                .context("the language server returned an invalid symbol")?;
            symbols.push(SymbolEntry::Flat(symbol));
        }
    }
    Ok(symbols)
}

pub(super) fn format_document_symbol(symbol: &DocumentSymbol, indent: usize) -> Vec<String> {
    let prefix = "  ".repeat(indent);
    let detail = symbol
        .detail
        .as_deref()
        .map(|detail| format!(" {detail}"))
        .unwrap_or_default();
    let line = symbol
        .range
        .as_ref()
        .map(|range| range.start.line.saturating_add(1).to_string())
        .unwrap_or_else(|| "unknown".to_owned());
    let mut lines = vec![format!(
        "{prefix}{} {}{detail} @ line {line}",
        symbol_kind_name(symbol.kind),
        symbol.name
    )];
    for child in &symbol.children {
        lines.extend(format_document_symbol(child, indent + 1));
    }
    lines
}

pub(super) fn format_flat_symbol(cwd: &Path, symbol: &SymbolInformation) -> String {
    let container = symbol
        .container_name
        .as_deref()
        .map(|name| format!(" ({name})"))
        .unwrap_or_default();
    format!(
        "{} {}{container} @ {}",
        symbol_kind_name(symbol.kind),
        symbol.name,
        format_location(cwd, &symbol.location)
    )
}

pub(super) fn filter_workspace_symbols(
    symbols: Vec<SymbolInformation>,
    query: &str,
) -> Vec<SymbolInformation> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return symbols;
    }
    symbols
        .into_iter()
        .filter(|symbol| {
            symbol.name.to_lowercase().contains(&needle)
                || symbol
                    .container_name
                    .as_deref()
                    .is_some_and(|name| name.to_lowercase().contains(&needle))
                || symbol.location.uri.to_lowercase().contains(&needle)
        })
        .collect()
}

pub(super) fn dedupe_workspace_symbols(symbols: Vec<SymbolInformation>) -> Vec<SymbolInformation> {
    let mut seen = BTreeSet::new();
    let mut unique = Vec::with_capacity(symbols.len());
    for symbol in symbols {
        let key = (
            symbol.name.clone(),
            symbol.container_name.clone().unwrap_or_default(),
            symbol.kind.unwrap_or(0),
            symbol.location.uri.clone(),
            symbol.location.range.start.line,
            symbol.location.range.start.character,
        );
        if seen.insert(key) {
            unique.push(symbol);
        }
    }
    unique
}
