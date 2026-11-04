//! MCP 折叠摘要优先展示实际字段值，不用容器层级消耗预览额度。

use super::{Value, clip};

const LINES: usize = 8;
const ITEMS: usize = 3;
const FIELDS: usize = 4;
const DEPTH: usize = 8;

pub(super) fn preview(value: &Value, width: u16) -> Vec<String> {
    let mut preview = Preview {
        lines: Vec::new(),
        width: usize::from(width),
        truncated: false,
    };
    preview.visit("", value, 0);
    preview.lines.push(if preview.truncated {
        "… (Ctrl + O to expand)".into()
    } else {
        "(Ctrl + O to expand)".into()
    });
    preview.lines
}

struct Preview {
    lines: Vec<String>,
    width: usize,
    truncated: bool,
}

impl Preview {
    fn push(&mut self, text: String) {
        if self.lines.len() < LINES {
            let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
            let clipped = clip(&text, self.width, true);
            self.truncated |= clipped != text;
            self.lines.push(clipped);
        } else {
            self.truncated = true;
        }
    }

    fn visit(&mut self, path: &str, value: &Value, depth: usize) {
        if self.lines.len() >= LINES {
            self.truncated = true;
            return;
        }
        match value {
            Value::Object(map) if !map.is_empty() => {
                if depth >= DEPTH {
                    self.push(format!("{path}: {{…}}"));
                    self.truncated = true;
                    return;
                }
                let mut fields: Vec<_> = map.iter().collect();
                // 错误优先，其余标量在容器之前；未知字段仍可见。
                fields.sort_by_key(|(key, value)| {
                    (
                        priority(key) != 0,
                        value.is_object() || value.is_array(),
                        priority(key),
                    )
                });
                let count = fields.len();
                for (index, (key, value)) in fields.into_iter().enumerate() {
                    let path = if path.is_empty() {
                        key.to_owned()
                    } else {
                        format!("{path}.{key}")
                    };
                    self.visit(&path, value, depth + 1);
                    if self.lines.len() >= LINES {
                        self.truncated |= index + 1 < count;
                        break;
                    }
                }
            }
            Value::Array(items) if !items.is_empty() => {
                self.push(format!(
                    "{}: {} {}",
                    if path.is_empty() { "results" } else { path },
                    items.len(),
                    if items.len() == 1 { "item" } else { "items" }
                ));
                if depth >= DEPTH {
                    self.truncated = true;
                    return;
                }
                for (index, item) in items.iter().take(ITEMS).enumerate() {
                    let path = format!("{path}[{index}]");
                    if let Some((summary, omitted)) = record_summary(item) {
                        self.push(format!("{path}: {summary}"));
                        self.truncated |= omitted;
                    } else {
                        self.visit(&path, item, depth + 1);
                    }
                }
                if items.len() > ITEMS {
                    self.push(format!("  … {} more items", items.len() - ITEMS));
                    self.truncated = true;
                }
            }
            _ => self.push(format!(
                "{}: {}",
                if path.is_empty() { "value" } else { path },
                scalar(value)
            )),
        }
    }
}

fn priority(key: &str) -> u8 {
    match key {
        "error" | "errors" => 0,
        "name" | "label" | "title" => 1,
        "summary" | "description" | "message" => 2,
        "signature" | "source" | "code" | "snippet" | "text" | "content" => 3,
        "path" | "file_path" | "url" => 3,
        "members" | "count" | "total" | "total_count" | "total_nodes" | "total_edges" => 4,
        "top_nodes" => 5,
        "id" => 9,
        _ => 6,
    }
}

fn scalar(value: &Value) -> String {
    match value {
        Value::String(text) => clip(
            &text.split_whitespace().collect::<Vec<_>>().join(" "),
            60,
            true,
        ),
        _ => value.to_string(),
    }
}

fn record_summary(value: &Value) -> Option<(String, bool)> {
    let map = value.as_object()?;
    let mut fields: Vec<_> = map
        .iter()
        .filter(|(_, value)| {
            !value.is_object()
                && value.as_array().is_none_or(|items| {
                    items
                        .iter()
                        .all(|item| !item.is_object() && !item.is_array())
                })
        })
        .collect();
    fields.sort_by_key(|(key, _)| priority(key));
    if fields.is_empty() {
        return None;
    }
    let omitted = fields.len() > FIELDS || fields.len() < map.len();
    let mut summary = fields
        .into_iter()
        .take(FIELDS)
        .map(|(key, value)| {
            let value = if let Some(items) = value.as_array() {
                let mut values = items.iter().take(ITEMS).map(scalar).collect::<Vec<_>>();
                if items.len() > ITEMS {
                    values.push("…".into());
                }
                format!("[{}]", values.join(", "))
            } else {
                scalar(value)
            };
            format!("{key}: {value}")
        })
        .collect::<Vec<_>>()
        .join(" · ");
    if omitted {
        summary.push_str(" · …");
    }
    Some((summary, omitted))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::text::Line;
    use serde_json::json;

    #[test]
    fn mcp_preview_architecture_shows_counts_and_cluster_values() {
        let value = json!({
            "total_nodes": 8148, "total_edges": 53772,
            "clusters": (0..12).map(|i| json!({
                "id": i, "label": format!("module-{i}"), "members": 584,
                "cohesion": 0.7172, "top_nodes": ["write", "read", "save", "open"],
                "packages": ["src", "tests"], "edge_types": ["CALLS"]
            })).collect::<Vec<_>>()
        });
        let rows = preview(&value, 120);
        let text = rows.join("\n");
        for expected in [
            "total_nodes: 8148",
            "total_edges: 53772",
            "clusters: 12 items",
            "label: module-0",
            "members: 584",
            "top_nodes: [write, read, save, …]",
            "label: module-2",
            "9 more items",
            "Ctrl + O to expand",
        ] {
            assert!(text.contains(expected), "{expected}: {text}");
        }
        assert!(!text.contains("module-3"));
        assert!(rows.len() <= LINES + 1);
    }

    #[test]
    fn mcp_preview_nested_objects_and_errors_keep_actual_values() {
        let value = json!({
            "data": {"results": [{"metadata": {"path": "src/agent.rs", "line": 42}}]},
            "error": {"message": "Index unavailable", "code": "NOT_READY"},
            "message": "Try again"
        });
        let text = preview(&value, 100).join("\n");
        assert!(
            text.starts_with("error.message: Index unavailable"),
            "{text}"
        );
        assert!(text.contains("data.results[0].metadata.path: src/agent.rs"));
        assert!(text.contains("data.results[0].metadata.line: 42"));
    }

    #[test]
    fn mcp_preview_code_and_locations_precede_incidental_metadata() {
        let value = json!({
            "name": "run", "file_path": "src/agent.rs", "start_line": 42,
            "signature": "fn run()", "source": "fn run() {\n    execute();\n}",
            "alloc_in_loop": 0, "bt": "metadata", "callers": 2, "callees": 3,
            "cognitive": 1, "complexity": 1, "fp": "fingerprint", "lines": 3
        });
        let rows = preview(&value, 100);
        let text = rows.join("\n");
        assert!(text.contains("source: fn run() { execute(); }"), "{text}");
        assert!(text.contains("file_path: src/agent.rs"));
        assert_eq!(rows.len(), LINES + 1);
        assert!(rows.last().unwrap().starts_with('…'));
    }

    #[test]
    fn mcp_preview_bounds_deep_wide_and_unicode_results() {
        let value = json!({"items": (0..500).map(|_| json!({
            "name": "中e\u{301}😀".repeat(200),
            "description": "first\nsecond\tthird\u{1b}[31m",
            "path": "src/agent.rs"
        })).collect::<Vec<_>>()});
        for width in [12, 40, 100] {
            let rows = preview(&value, width);
            assert!(rows.len() <= LINES + 1);
            for row in &rows[..rows.len() - 1] {
                assert!(!row.contains(['\n', '\t', '\u{1b}']));
                assert!(Line::raw(row.as_str()).width() <= usize::from(width));
            }
        }
        let mut deep = json!({"leaf": "hidden"});
        for _ in 0..100 {
            deep = json!({"nested": deep});
        }
        let text = preview(&deep, 120).join("\n");
        assert!(!text.contains("hidden"));
        assert!(text.contains("{…}"));
    }

    #[test]
    fn mcp_preview_empty_and_unknown_fields_remain_visible() {
        for value in [json!({}), json!([]), json!({"custom": {"answer": 42}})] {
            let text = preview(&value, 100).join("\n");
            let expected = if value.is_array() {
                "[]"
            } else if value.get("custom").is_some() {
                "custom.answer: 42"
            } else {
                "{}"
            };
            assert!(text.contains(expected), "{text}");
        }
    }
}
