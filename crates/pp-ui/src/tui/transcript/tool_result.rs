//! 工具结果到卡片正文的投影，以及后台任务状态行。

use super::{EntryKind, Value};

pub(super) fn background_status(result: &Value) -> Option<String> {
    Some(format!(
        "Task {}: {}",
        result["task_id"].as_str()?,
        result["status"].as_str()?
    ))
}

pub(super) fn tool_result(kind: EntryKind, name: &str, result: &Value) -> String {
    if name == "lsp" {
        if let Some(content) = result["content"].as_str() {
            return content.to_owned();
        }
        if let Some(error) = result.get("error") {
            return error
                .as_str()
                .map_or_else(|| error.to_string(), str::to_owned);
        }
        return String::new();
    }
    if matches!(name, "rg" | "glob")
        && let Some(content) = result["content"].as_str()
    {
        return content.to_owned();
    }
    if name == "todo" {
        // 失败正文已按“诊断 → 可信调用前列表”排列；缺 content 时退回诊断。
        if let Some(content) = result["content"].as_str() {
            return content.to_owned();
        }
        if let Some(error) = result.get("error") {
            return error
                .as_str()
                .map_or_else(|| error.to_string(), str::to_owned);
        }
        return String::new();
    }
    if kind == EntryKind::Bash || matches!(name, "rg" | "find") {
        let mut lines = Vec::new();
        if let Some(output) = result["output"].as_str() {
            lines.push(output.trim_end_matches('\n').to_owned());
        }
        let notice = pp_runtime::tools::output::notice(result);
        if !notice.is_empty() {
            lines.push(notice);
        }
        // 后台状态已在 Bash footer 展示，不占用输出正文和尾窗行数。
        for (field, label) in [("stdout", "stdout"), ("stderr", "stderr")] {
            if let Some(text) = result
                .get(field)
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
            {
                lines.push(if field == "stdout" {
                    text.strip_suffix('\n').unwrap_or(text).to_owned()
                } else {
                    format!("[{label}]\n{}", text.strip_suffix('\n').unwrap_or(text))
                });
            }
            if result
                .get(format!("{field}_truncated"))
                .and_then(Value::as_bool)
                == Some(true)
            {
                lines.push(format!("[{field} truncated by output limits]"));
            }
        }
        if let Some(error) = result.get("error") {
            lines.push(
                error
                    .as_str()
                    .map_or_else(|| error.to_string(), str::to_owned),
            );
        }
        if let Some(warning) = result.get("warning").and_then(Value::as_str) {
            lines.push(format!("Warning: {warning}"));
        }
        return lines.join("\n");
    }
    if name.starts_with("mcp__")
        && let Some(blocks) = result["result"]["content"].as_array()
    {
        let text = blocks
            .iter()
            .filter(|block| block["type"] == "text")
            .filter_map(|block| block["text"].as_str())
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        if !text.is_empty() {
            return text;
        }
    }
    if result.get("ok").and_then(Value::as_bool) == Some(false) {
        if let Some(error) = result.get("error") {
            return error
                .as_str()
                .map_or_else(|| error.to_string(), str::to_owned);
        }
    } else if name == "read" {
        if let Some(content) = result.get("content").and_then(Value::as_str) {
            return content.into();
        }
    } else if name == "write"
        && let Some(bytes) = result.get("bytes_written").and_then(Value::as_u64)
    {
        return format!("Wrote {bytes} bytes");
    }
    serde_json::to_string_pretty(result).unwrap_or_else(|_| result.to_string())
}
