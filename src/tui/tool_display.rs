//! 工具显示策略，基准为 Oh My Pi 0b5c27ab 的 ANSI 渲染路径。
//! 所有限制仅用于交互预览，完整回放始终使用保留的原始正文。

use super::transcript::{block_line, wrap_block};
use super::transcript_style as palette;
use ratatui::{style::Style, text::Line};
use serde_json::Value;

const READ_LINES: usize = 3;
const GREP_LINES: usize = 6;
const SEARCH_EXPANDED_LINES: usize = 24;
const FIND_ITEMS: usize = 8;
const BASH_LINES: usize = 10;
pub(super) const DIFF_HUNKS: usize = 8;
pub(super) const DIFF_LINES: usize = 40;
const WRITE_LINES: usize = 6;
const WRITE_PENDING_LINES: usize = 12;
const MCP_LINES: usize = 4;
const MCP_EXPANDED_LINES: usize = 12;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Context {
    pub terminal_rows: u16,
    pub read_preview: bool,
    pub full: bool,
}

impl Context {
    pub fn window(self) -> usize {
        usize::from(if self.terminal_rows == 0 {
            30
        } else {
            self.terminal_rows
        })
        .saturating_sub(20)
        .max(6)
    }
}

pub(super) struct View<'a> {
    pub name: &'a str,
    pub text: &'a str,
    pub arguments: &'a Value,
    pub result: Option<&'a Value>,
    pub running: bool,
    pub failed: bool,
    pub expanded: bool,
    pub context: Context,
}

pub(super) fn clip(text: &str, width: usize, ellipsis: bool) -> String {
    let safe = palette::display_text(text);
    // 列宽计算必须与实际绘制使用相同的 tab stop，避免截断后再次折成多行。
    let expanded_tabs;
    let safe = if safe.contains('\t') {
        let mut columns = 0;
        let mut expanded = String::new();
        for grapheme in Line::raw(safe.as_ref()).styled_graphemes(Style::default()) {
            if grapheme.symbol == "\t" {
                let spaces = 4 - columns % 4;
                expanded.push_str(&" ".repeat(spaces));
                columns += spaces;
            } else {
                expanded.push_str(grapheme.symbol);
                columns += Line::raw(grapheme.symbol).width();
            }
        }
        expanded_tabs = expanded;
        std::borrow::Cow::Borrowed(expanded_tabs.as_str())
    } else {
        safe
    };
    if Line::raw(safe.as_ref()).width() <= width {
        return safe.into_owned();
    }
    let budget = width.saturating_sub(usize::from(ellipsis));
    let mut output = String::new();
    let mut columns = 0;
    for grapheme in Line::raw(safe.as_ref()).styled_graphemes(Style::default()) {
        let size = Line::raw(grapheme.symbol).width();
        if columns + size > budget {
            break;
        }
        output.push_str(grapheme.symbol);
        columns += size;
    }
    if ellipsis && width > 0 {
        output.push('…');
    }
    output
}

fn logical(text: &str) -> Vec<&str> {
    if text.is_empty() {
        Vec::new()
    } else {
        text.strip_suffix('\n')
            .unwrap_or(text)
            .split('\n')
            .collect()
    }
}

enum MoreHint {
    Expand(&'static str),
    Plain(&'static str),
}

fn hint(hidden: usize, more: MoreHint) -> String {
    match more {
        MoreHint::Expand(unit) => format!("… {hidden} more {unit} (Ctrl + O to expand)"),
        MoreHint::Plain(unit) => format!("… {hidden} more {unit}"),
    }
}

pub(super) fn wrap(
    lines: impl IntoIterator<Item = String>,
    width: u16,
    style: Style,
) -> Vec<Line<'static>> {
    lines
        .into_iter()
        .flat_map(|line| wrap_block(block_line(line, style), width, ""))
        .collect()
}

pub(super) fn command(text: &str, expanded: bool, context: Context) -> Vec<String> {
    let mut lines: Vec<_> = logical(text).into_iter().map(str::to_owned).collect();
    let max = context.window();
    if !expanded && !context.full && lines.len() > max {
        let hidden = lines.len() - max + 1;
        lines.drain(..hidden);
        lines.insert(0, format!("… {hidden} earlier lines (Ctrl + O to expand)"));
    }
    lines
}

pub(super) fn is_skill_read(arguments: &Value, result: Option<&Value>) -> bool {
    arguments["path"]
        .as_str()
        .is_some_and(|p| p.starts_with("skill://"))
        || result.is_some_and(|r| {
            r["source"]
                .as_str()
                .is_some_and(|p| p.starts_with("skill://"))
        })
}

pub(super) fn body(view: View<'_>, width: u16, style: Style) -> Vec<Line<'static>> {
    let expanded = view.expanded || view.context.full;
    let text = view.text;
    if view.context.full {
        let text = if view.name == "write" && !view.failed {
            view.arguments
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or(text)
        } else {
            text
        };
        return wrap(logical(text).into_iter().map(str::to_owned), width, style);
    }
    if view.failed
        && !matches!(view.name, "bash" | "bash_task" | "grep" | "find")
        && !view.name.starts_with("mcp__")
    {
        return wrap(logical(text).into_iter().map(str::to_owned), width, style);
    }
    match view.name {
        "read" => {
            if !view.context.read_preview && !is_skill_read(view.arguments, view.result) {
                return Vec::new();
            }
            head(
                text,
                if expanded { usize::MAX } else { READ_LINES },
                false,
                false,
                width,
                style,
                MoreHint::Expand("lines"),
            )
        }
        "grep" => head(
            text,
            if expanded { usize::MAX } else { GREP_LINES },
            true,
            true,
            width,
            style,
            MoreHint::Expand("items"),
        ),
        "find" => head(
            text,
            if expanded { usize::MAX } else { FIND_ITEMS },
            false,
            true,
            width,
            style,
            MoreHint::Expand("files"),
        ),
        "plan_find" => {
            let files = view.result.and_then(|r| r["paths"].as_array());
            let text = files
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_else(|| text.into());
            let mut rows = head(
                &text,
                if expanded { usize::MAX } else { FIND_ITEMS },
                false,
                true,
                width,
                style,
                MoreHint::Expand("files"),
            );
            append_notices(&mut rows, view.result, width, style);
            rows
        }
        "plan_search" => plan_search(&view, width, style),
        "bash" | "bash_task" => {
            let mut rows = wrap(logical(text).into_iter().map(str::to_owned), width, style);
            let total = rows.len();
            let limit = BASH_LINES.min(view.context.window());
            if !expanded && total > limit {
                rows.drain(..total - limit);
                let marker = format!(
                    "… ({} earlier lines, showing {limit} of {total}) (Ctrl + O to expand)",
                    total - limit
                );
                let mut prefix = wrap([clip(&marker, usize::from(width), true)], width, style);
                prefix.append(&mut rows);
                rows = prefix;
            }
            rows
        }
        "write" => {
            let content = view.arguments["content"].as_str().unwrap_or(text);
            if view.running && !expanded {
                let lines = logical(content);
                let hidden = lines.len().saturating_sub(WRITE_PENDING_LINES);
                let mut shown = Vec::new();
                if hidden > 0 {
                    shown.push(format!("… {hidden} earlier lines (Ctrl + O to expand)"));
                }
                shown.extend(lines[hidden..].iter().map(|line| (*line).to_owned()));
                wrap(shown, width, style)
            } else {
                head(
                    content,
                    if expanded { usize::MAX } else { WRITE_LINES },
                    false,
                    false,
                    width,
                    style,
                    MoreHint::Expand("lines"),
                )
            }
        }
        name if name.starts_with("mcp__") => {
            let output = view
                .result
                .and_then(|r| r["result"]["content"].as_array())
                .map(|blocks| {
                    blocks
                        .iter()
                        .filter(|block| block["type"] == "text")
                        .filter_map(|block| block["text"].as_str())
                        .filter(|s| !s.is_empty())
                        .collect::<Vec<_>>()
                        .join("\n\n")
                });
            let output = output
                .filter(|text| !text.is_empty())
                .unwrap_or_else(|| text.into());
            with_arguments(
                generic(&output, true, expanded, width, style),
                &view,
                width,
                style,
            )
        }
        _ => with_arguments(
            generic(text, false, expanded, width, style),
            &view,
            width,
            style,
        ),
    }
}

fn head(
    text: &str,
    limit: usize,
    reserve: bool,
    clipped: bool,
    width: u16,
    style: Style,
    more: MoreHint,
) -> Vec<Line<'static>> {
    let lines: Vec<_> = logical(text)
        .into_iter()
        .filter(|line| !clipped || !line.trim().is_empty())
        .collect();
    let count = lines.len();
    let shown = if count > limit && reserve {
        limit.saturating_sub(1)
    } else {
        count.min(limit)
    };
    let mut output: Vec<_> = lines[..shown]
        .iter()
        .map(|line| {
            if clipped {
                clip(line, usize::from(width), false)
            } else {
                (*line).to_owned()
            }
        })
        .collect();
    if shown < count {
        output.push(hint(count - shown, more));
    }
    if clipped {
        for line in &mut output {
            *line = clip(line, usize::from(width), false);
        }
    }
    wrap(output, width, style)
}

fn with_arguments(
    mut rows: Vec<Line<'static>>,
    view: &View<'_>,
    width: u16,
    style: Style,
) -> Vec<Line<'static>> {
    if view.expanded
        && view
            .arguments
            .as_object()
            .is_some_and(|args| !args.is_empty())
    {
        let (args, truncated) = json_tree(view.arguments, true);
        let mut prefix = wrap(["Args".into()].into_iter().chain(args), width, style);
        if truncated {
            prefix.extend(wrap(["…".into()], width, style));
        }
        prefix.extend(wrap(["Output".into()], width, style));
        prefix.append(&mut rows);
        prefix
    } else {
        rows
    }
}

fn append_notices(rows: &mut Vec<Line<'static>>, result: Option<&Value>, width: u16, style: Style) {
    if let Some(result) = result {
        if result["truncated"] == true || !result["next_offset"].is_null() {
            rows.extend(wrap(
                ["[truncated by output limits; more results may be available]".into()],
                width,
                style,
            ));
        }
        if let Some(warnings) = result["warnings"].as_array() {
            for warning in warnings {
                rows.extend(wrap(
                    [format!(
                        "Warning: {}: {}",
                        warning["path"].as_str().unwrap_or_default(),
                        warning["reason"].as_str().unwrap_or_default()
                    )],
                    width,
                    style,
                ));
            }
        }
    }
}

fn plan_search(view: &View<'_>, width: u16, style: Style) -> Vec<Line<'static>> {
    let Some(matches) = view.result.and_then(|r| r["matches"].as_array()) else {
        return head(
            view.text,
            if view.expanded {
                SEARCH_EXPANDED_LINES
            } else {
                GREP_LINES
            },
            true,
            true,
            width,
            style,
            if view.expanded {
                MoreHint::Plain("lines")
            } else {
                MoreHint::Expand("lines")
            },
        );
    };
    let mut lines = Vec::new();
    let mut previous = "";
    for item in matches {
        let path = item["path"].as_str().unwrap_or_default();
        if path != previous {
            lines.push(path.to_owned());
            previous = path;
        }
        lines.push(format!(
            "  {}│{}",
            item["line"],
            item["text"].as_str().unwrap_or_default()
        ));
    }
    let mut rows = head(
        &lines.join("\n"),
        if view.expanded {
            SEARCH_EXPANDED_LINES
        } else {
            GREP_LINES
        },
        true,
        true,
        width,
        style,
        if view.expanded {
            MoreHint::Plain("lines")
        } else {
            MoreHint::Expand("lines")
        },
    );
    append_notices(&mut rows, view.result, width, style);
    rows
}

fn generic(text: &str, mcp: bool, expanded: bool, width: u16, style: Style) -> Vec<Line<'static>> {
    if let Ok(value @ (Value::Object(_) | Value::Array(_))) = serde_json::from_str(text) {
        let (tree, truncated) = json_tree(&value, expanded);
        if mcp || !expanded || !truncated {
            let mut rows = wrap(tree, width, style);
            if !expanded {
                if truncated
                    && !rows.iter().any(|line| line.to_string().contains('…'))
                    && let Some(last) = rows.last_mut()
                {
                    *last = block_line("…", style);
                }
                rows.extend(wrap(["(Ctrl + O to expand)".into()], width, style));
            } else if truncated {
                rows.extend(wrap(["…".into()], width, style));
            }
            return rows;
        }
    }
    let limit = if expanded {
        if mcp { MCP_EXPANDED_LINES } else { usize::MAX }
    } else {
        MCP_LINES
    };
    let mut rows = head(
        text,
        limit,
        false,
        !expanded,
        width,
        style,
        if expanded {
            MoreHint::Plain("lines")
        } else {
            MoreHint::Expand("lines")
        },
    );
    if !expanded && logical(text).len() <= limit && !text.is_empty() {
        rows.extend(wrap(["(Ctrl + O to expand)".into()], width, style));
    }
    rows
}

// 与上游相同：根对象从深度 1 开始，容器显示大小，超深或超行时保留省略标记。
fn json_tree(value: &Value, expanded: bool) -> (Vec<String>, bool) {
    let mut tree = JsonTree {
        lines: Vec::new(),
        truncated: false,
        depth: if expanded { 6 } else { 2 },
        max: if expanded { 200 } else { 6 },
        scalar: if expanded { 2000 } else { 60 },
    };
    match value {
        Value::Object(map) if map.is_empty() => tree.push("{}".into()),
        Value::Array(items) if items.is_empty() => tree.push("[]".into()),
        Value::Object(map) => {
            for (key, value) in map {
                tree.visit(key, value, 1);
            }
        }
        Value::Array(items) => {
            for (i, value) in items.iter().enumerate() {
                tree.visit(&format!("[{i}]"), value, 1);
            }
        }
        _ => tree.visit("value", value, 0),
    }
    (tree.lines, tree.truncated)
}

struct JsonTree {
    lines: Vec<String>,
    truncated: bool,
    depth: usize,
    max: usize,
    scalar: usize,
}

impl JsonTree {
    fn push(&mut self, text: String) {
        if self.lines.len() < self.max {
            self.lines.push(text);
        } else {
            self.truncated = true;
        }
    }
    fn visit(&mut self, key: &str, value: &Value, depth: usize) {
        if self.lines.len() >= self.max {
            self.truncated = true;
            return;
        }
        let prefix = format!("{}{}", "  ".repeat(depth.saturating_sub(1)), key);
        match value {
            Value::Object(map) => {
                self.push(format!("{prefix} {{{}}}", map.len()));
                if depth >= self.depth && !map.is_empty() {
                    self.push(format!("{}…", "  ".repeat(depth)));
                    self.truncated = true;
                } else {
                    for (index, (key, value)) in map.iter().enumerate() {
                        self.visit(key, value, depth + 1);
                        if self.lines.len() >= self.max {
                            self.truncated |= index + 1 < map.len();
                            break;
                        }
                    }
                }
            }
            Value::Array(items) => {
                self.push(format!("{prefix} [{}]", items.len()));
                if depth >= self.depth && !items.is_empty() {
                    self.push(format!("{}…", "  ".repeat(depth)));
                    self.truncated = true;
                } else {
                    for (i, value) in items.iter().enumerate() {
                        self.visit(&format!("[{i}]"), value, depth + 1);
                        if self.lines.len() >= self.max {
                            self.truncated |= i + 1 < items.len();
                            break;
                        }
                    }
                }
            }
            Value::String(text) => {
                let lines: Vec<_> = text.split('\n').collect();
                let available = self.max.saturating_sub(self.lines.len()).max(1);
                let shown = if lines.len() > 1 {
                    lines.len().min(available.saturating_sub(1).max(1))
                } else {
                    1
                };
                for (index, line) in lines[..shown].iter().enumerate() {
                    let scalar = clip(line, self.scalar, true);
                    self.truncated |= scalar != *line;
                    self.push(if index == 0 {
                        format!(
                            "{prefix}: \"{scalar}{}",
                            if shown == lines.len() && shown == 1 {
                                "\""
                            } else {
                                ""
                            }
                        )
                    } else {
                        format!(
                            "{}{}{}",
                            " ".repeat(Line::raw(format!("{prefix}: \"")).width()),
                            scalar,
                            if index + 1 == lines.len() { "\"" } else { "" }
                        )
                    });
                }
                if shown < lines.len() {
                    self.truncated = true;
                    self.push(format!(
                        "{}…({} more lines)\"",
                        " ".repeat(Line::raw(format!("{prefix}: \"")).width()),
                        lines.len() - shown
                    ));
                }
            }
            _ => self.push(format!("{prefix}: {value}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn text(rows: Vec<Line<'static>>) -> String {
        rows.iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn output(name: &str, content: &str, expanded: bool, full: bool, width: u16) -> String {
        text(body(
            View {
                name,
                text: content,
                arguments: &Value::Null,
                result: None,
                running: false,
                failed: false,
                expanded,
                context: Context {
                    full,
                    ..Context::default()
                },
            },
            width,
            Style::default(),
        ))
    }

    #[test]
    fn search_caps_and_long_rows_match_ansi_fallback() {
        let content = (0..10)
            .map(|i| format!("item-{i:02}-{}", "中e\u{301}😀".repeat(20)))
            .collect::<Vec<_>>()
            .join("\n");
        let grep = output("grep", &content, false, false, 70);
        assert_eq!(grep.lines().count(), 6);
        assert!(grep.contains("item-04") && !grep.contains("item-05"));
        assert!(grep.contains("5 more items"));
        let find = output("find", &content, false, false, 70);
        assert_eq!(find.lines().count(), 9);
        assert!(find.contains("item-07") && !find.contains("item-08"));
        for name in ["grep", "find"] {
            let expanded = output(name, &content, true, false, 70);
            assert_eq!(expanded.lines().count(), 10);
            assert!(expanded.contains("item-09"));
            assert!(expanded.lines().all(|line| Line::raw(line).width() <= 70));
            let full = output(name, &content, true, true, 70);
            assert!(full.lines().count() > 10);
            assert!(full.contains("e\u{301}"));
            let tabs = output(name, "\t\t\t\t\t中😀 e\u{301}", true, false, 8);
            assert_eq!(tabs.lines().count(), 1);
            assert!(Line::raw(tabs).width() <= 8);
        }
    }

    #[test]
    fn bash_tail_is_visual_and_changes_with_terminal_height() {
        let content = format!("{}last", "中😀".repeat(80));
        let context = Context {
            terminal_rows: 24,
            ..Context::default()
        };
        let view = View {
            name: "bash",
            text: &content,
            arguments: &Value::Null,
            result: None,
            running: true,
            failed: false,
            expanded: false,
            context,
        };
        let rows = body(view, 40, Style::default());
        assert_eq!(rows.len(), 7);
        let shown = text(rows);
        assert!(shown.contains("showing 6") && shown.contains("last"));
        assert_eq!(
            Context {
                terminal_rows: 80,
                ..context
            }
            .window(),
            60
        );
        let command = (0..40)
            .map(|i| format!("command-{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let preview = command.clone();
        let shown = super::command(&command, false, context);
        assert_eq!(shown.len(), 6);
        assert!(shown.last().unwrap().contains("command-39"));
        assert_eq!(super::command(&preview, true, context).len(), 40);
    }

    #[test]
    fn mcp_extracts_multiline_content_and_bounds_expansion() {
        let content = (0..20)
            .map(|i| format!("mcp-line-{i:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        let result = json!({"ok":true,"result":{"content":[{"type":"text","text":content}]}});
        for (expanded, limit) in [(false, 4), (true, 12)] {
            let rows = text(body(
                View {
                    name: "mcp__test__tool",
                    text: "wrapped",
                    arguments: &Value::Null,
                    result: Some(&result),
                    running: false,
                    failed: false,
                    expanded,
                    context: Context::default(),
                },
                100,
                Style::default(),
            ));
            assert!(rows.contains(&format!("mcp-line-{:02}", limit - 1)));
            assert!(!rows.contains(&format!("mcp-line-{limit:02}")));
            assert_eq!(rows.contains("Ctrl + O to expand"), !expanded);
        }
        assert!(output("mcp__test__tool", &content, true, true, 100).contains("mcp-line-19"));
    }

    #[test]
    fn json_limits_keep_multiline_strings_and_generic_full_fallback() {
        let value = json!({"a":{"b":{"c":{"d":{"e":{"f":{"deep":"kept in full fallback"}}}}}},"message":"first\nsecond\nthird"});
        let content = value.to_string();
        let collapsed = output("mcp__test__json", &content, false, false, 100);
        assert!(!collapsed.contains("kept in full fallback"));
        assert!(collapsed.contains('…'));
        let full = output("custom", &content, true, false, 100);
        assert!(full.contains("kept in full fallback"));
        let (tree, _) = json_tree(&json!({"message":"first\nsecond\nthird"}), false);
        assert!(text(wrap(tree, 100, Style::default())).contains("second"));
        let wide = json!({"long":"中".repeat(2000)}).to_string();
        assert!(!output("mcp__test__json", &wide, false, false, 100).contains(&"中".repeat(40)));
        let many = json!({"items":(0..250).collect::<Vec<_>>()});
        let (tree, truncated) = json_tree(&many, true);
        assert!(truncated && tree.len() <= 200);
        assert!(output("custom", &many.to_string(), true, false, 100).contains("249"));
    }

    #[test]
    fn plan_and_write_views_use_domain_budgets_without_losing_full_content() {
        let result = json!({"matches":(0..40).map(|i|json!({"path":"file.rs","line":i+1,"text":format!("hit-{i:02}")})).collect::<Vec<_>>(),"next_offset":null,"truncated":false});
        for (expanded, limit) in [(false, 6), (true, 24)] {
            let rows = body(
                View {
                    name: "plan_search",
                    text: "",
                    arguments: &Value::Null,
                    result: Some(&result),
                    running: false,
                    failed: false,
                    expanded,
                    context: Context::default(),
                },
                100,
                Style::default(),
            );
            assert_eq!(rows.len(), limit);
            assert!(!text(rows).contains("hit-39"));
        }
        let content = (0..20)
            .map(|i| format!("written-{i:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        let args = json!({"content":content});
        for running in [false, true] {
            let rows = text(body(
                View {
                    name: "write",
                    text: "",
                    arguments: &args,
                    result: None,
                    running,
                    failed: false,
                    expanded: false,
                    context: Context::default(),
                },
                100,
                Style::default(),
            ));
            assert_eq!(rows.contains("written-19"), running);
            assert_eq!(rows.contains("written-00"), !running);
        }
        assert!(clip("e\u{301}😀中", 3, false).ends_with('😀'));
    }
}
