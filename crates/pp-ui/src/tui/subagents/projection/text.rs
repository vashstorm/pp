//! 摘要文本辅助：大小写不敏感子序列、单行截断与参数摘要。

use super::*;

/// 大小写不敏感的子序列匹配（两侧均已小写）。
pub(super) fn fuzzy(needle: &str, haystack: &str) -> bool {
    let mut characters = haystack.chars();
    needle
        .chars()
        .all(|wanted| characters.any(|found| found == wanted))
}

/// 单行摘要：折叠换行/Tab，并限制 Unicode 标量数量。
pub(in crate::tui::subagents) fn truncate(text: &str) -> String {
    let mut out = String::new();
    for (count, character) in text.chars().enumerate() {
        if count >= SUMMARY_SCALARS {
            out.push('…');
            break;
        }
        out.push(match character {
            '\n' | '\r' | '\t' => ' ',
            other => other,
        });
    }
    out.trim_end().to_owned()
}

pub(super) fn summarize_arguments(arguments: &str) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(arguments) else {
        return truncate(arguments);
    };
    ["description", "path", "pattern", "command", "url", "input"]
        .iter()
        .filter_map(|key| value.get(key).and_then(serde_json::Value::as_str))
        .find(|text| !text.trim().is_empty())
        .map_or_else(|| truncate(arguments), truncate)
}

pub(in crate::tui) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
