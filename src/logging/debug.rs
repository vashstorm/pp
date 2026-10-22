use serde_json::{Map, Value};
use std::io::{self, Write};

const METADATA_ORDER: &[&str] = &[
    "model_reference",
    "protocol",
    "pid",
    "session_id",
    "status",
    "request_index",
    "max_model_requests",
    "history_items",
    "tool_count",
    "think_level",
    "edit_mode",
    "cwd",
    "tool_limits",
];
const REQUEST_SECTIONS: &[(&str, &str)] = &[
    ("instructions", "系统指令"),
    ("messages", "对话消息"),
    ("input", "输入历史"),
    ("tools", "工具定义"),
];

pub(super) fn format_record(record: &Value) -> io::Result<Vec<u8>> {
    let mut output = Vec::new();
    let event = record["event"].as_str().unwrap_or("debug");
    let timestamp = record["timestamp"].as_str().unwrap_or("");
    writeln!(output, "=== {event} | {timestamp} ===")?;
    if let Some(fields) = record.as_object() {
        for name in METADATA_ORDER {
            if let Some(value) = fields.get(*name) {
                write_value(&mut output, name, value, 0)?;
            }
        }
        write_fields(
            &mut output,
            fields,
            &[METADATA_ORDER, &["event", "timestamp", "request"]].concat(),
            0,
        )?;
        if let Some(request) = fields.get("request") {
            write_request(&mut output, request)?;
        }
    } else {
        write_value(&mut output, "value", record, 0)?;
    }
    writeln!(output)?;
    Ok(output)
}

fn write_request(output: &mut Vec<u8>, request: &Value) -> io::Result<()> {
    let Some(fields) = request.as_object() else {
        return write_value(output, "request", request, 0);
    };
    writeln!(output, "\n-- 请求参数 --")?;
    let section_keys: Vec<_> = REQUEST_SECTIONS.iter().map(|(name, _)| *name).collect();
    write_fields(output, fields, &section_keys, 0)?;
    for (name, title) in REQUEST_SECTIONS {
        let Some(value) = fields.get(*name) else {
            continue;
        };
        if let Some(items) = value.as_array() {
            writeln!(output, "\n-- {title} / {name} ({} 项) --", items.len())?;
            if items.is_empty() {
                writeln!(output, "{name}: []")?;
            }
            for (index, item) in items.iter().enumerate() {
                let role = item["role"]
                    .as_str()
                    .or_else(|| item["type"].as_str())
                    .unwrap_or("item");
                let tool_name = item["name"]
                    .as_str()
                    .or_else(|| item["function"]["name"].as_str());
                write!(output, "\n[{index}] {role}")?;
                if let Some(name) = tool_name {
                    write!(output, " / {name}")?;
                }
                writeln!(output)?;
                if let Some(fields) = item.as_object() {
                    write_fields(output, fields, &[], 1)?;
                } else {
                    write_value(output, "value", item, 1)?;
                }
            }
        } else {
            writeln!(output, "\n-- {title} / {name} --")?;
            write_value(output, name, value, 0)?;
        }
    }
    Ok(())
}

fn write_fields(
    output: &mut Vec<u8>,
    fields: &Map<String, Value>,
    excluded: &[&str],
    depth: usize,
) -> io::Result<()> {
    // 身份字段置前，正文和工具参数随后；未知字段仍完整保留。
    const ORDER: &[&str] = &["id", "call_id", "tool_call_id", "role", "type", "name"];
    for name in ORDER {
        if !excluded.contains(name)
            && let Some(value) = fields.get(*name)
        {
            write_value(output, name, value, depth)?;
        }
    }
    for (name, value) in fields {
        if !excluded.contains(&name.as_str()) && !ORDER.contains(&name.as_str()) {
            write_value(output, name, value, depth)?;
        }
    }
    Ok(())
}

fn write_value(output: &mut Vec<u8>, name: &str, value: &Value, depth: usize) -> io::Result<()> {
    write!(output, "{:width$}{name}:", "", width = depth * 2)?;
    match value {
        // 工具 schema 使用标准 JSON，保留数组与类型的可辨识结构。
        Value::Object(_) if name == "parameters" => {
            writeln!(output)?;
            let json = serde_json::to_string_pretty(value).map_err(io::Error::other)?;
            write_block(output, &json, depth + 1)?;
        }
        Value::Object(fields) if !fields.is_empty() => {
            writeln!(output)?;
            write_fields(output, fields, &[], depth + 1)?;
        }
        Value::Array(items) if !items.is_empty() => {
            writeln!(output)?;
            for (index, value) in items.iter().enumerate() {
                write_value(output, &format!("[{index}]"), value, depth + 1)?;
            }
        }
        Value::String(text)
            if text.contains('\n')
                || matches!(
                    name,
                    "content" | "text" | "instructions" | "description" | "output"
                ) =>
        {
            writeln!(output, " |")?;
            write_block(output, text, depth + 1)?;
        }
        Value::String(text) if !text.is_empty() => writeln!(output, " {text}")?,
        _ => writeln!(output, " {value}")?,
    }
    Ok(())
}

fn write_block(output: &mut Vec<u8>, text: &str, depth: usize) -> io::Result<()> {
    // 不折行、不转义、不截断，保留代码缩进、空行和末尾换行。
    for line in text.split('\n') {
        writeln!(output, "{:width$}{line}", "", width = depth * 2)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn responses_prompt_preserves_reasoning_calls_multimodal_and_unknown_fields() {
        let record = json!({
            "event": "llm_first_prompt", "protocol": "response", "request": {
                "instructions": "Follow the instructions.\n  Keep indentation.",
                "input": [
                    {"role": "user", "content": [
                        {"type": "input_text", "text": "Read this image."},
                        {"type": "input_image", "image_url": "https://example.test/image.png"}
                    ]},
                    {"type": "reasoning", "encrypted_content": "opaque-reasoning", "summary": []},
                    {"type": "function_call", "call_id": "call-1", "name": "read",
                        "arguments": "{\"path\":\"source.rs\"}"},
                    {"type": "function_call_output", "call_id": "call-1", "output": "a\n\nb"}
                ],
                "tools": [], "future_field": {"empty": "", "missing": null}
            }
        });
        let text = String::from_utf8(format_record(&record).unwrap()).unwrap();
        let sections: Vec<_> = text
            .lines()
            .filter(|line| line.starts_with("-- "))
            .collect();
        assert_eq!(
            sections,
            [
                "-- 请求参数 --",
                "-- 系统指令 / instructions --",
                "-- 输入历史 / input (4 项) --",
                "-- 工具定义 / tools (0 项) --",
            ]
        );
        for block in [
            "instructions: |\n  Follow the instructions.\n    Keep indentation.\n",
            "[0] user\n  role: user\n",
            "type: input_text\n      text: |\n        Read this image.",
            "image_url: https://example.test/image.png",
            "[1] reasoning\n",
            "encrypted_content: opaque-reasoning",
            "summary: []",
            "[2] function_call / read\n  call_id: call-1\n",
            "arguments: {\"path\":\"source.rs\"}",
            "[3] function_call_output\n  call_id: call-1\n",
            "output: |\n    a\n    \n    b\n",
            "tools: []",
            "future_field:\n  empty: \"\"\n  missing: null\n",
        ] {
            assert!(text.contains(block), "缺少内容 {block:?}:\n{text}");
        }
        // Responses 的简写文本输入也必须完整可读。
        let text =
            String::from_utf8(format_record(&json!({"request": {"input": "a\nb"}})).unwrap())
                .unwrap();
        assert!(text.contains("-- 输入历史 / input --\ninput: |\n  a\n  b\n"));
    }
}
