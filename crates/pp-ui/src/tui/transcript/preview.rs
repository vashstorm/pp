//! 文件工具参数预览：把模型生成中的 `edit`／`write` 参数渲染成临时卡片。
//! 预览不解析成可执行工具调用，也不读取磁盘；完整调用仍由原执行链路校验。
use super::{
    Color, EntryBody, EntryKind, EntryStatus, Line, Style, Transcript, TranscriptEntry, palette,
    wrap_block,
};

/// `edit` 预览的正文上限；`write` 不设上限，只靠尾部折叠控制卡片高度。
const LIMIT: usize = 64 * 1024;
/// 未展开时最多显示的行数；超出时保留末尾行并加提示，正好铺满时不加提示。
const COLLAPSED_LINES: usize = 40;
/// 折叠时保留的末尾内容行数，其余位置留给尾部提示。
const TAIL_LINES: usize = COLLAPSED_LINES - 1;
const TAIL_HINT: &str = "… preview tail · Ctrl+O to expand";
const LIMIT_HINT: &str = "… preview limit reached";

impl Transcript {
    /// 追加模型生成的参数分片。`edit` 保持 64 KiB 上限，`write` 完整累积。
    pub(super) fn preview_arguments(&mut self, index: u32, tool: &str, text: &str) {
        if text.is_empty() || self.replaying {
            return;
        }
        let entry_index = *self.argument_previews.entry(index).or_insert_with(|| {
            let mut entry = TranscriptEntry::new(
                EntryKind::Diff,
                String::new(),
                String::new(),
                Some(tool.to_owned()),
            );
            entry.body = EntryBody::ArgumentsPreview { truncated: false };
            entry.status = EntryStatus::Running;
            entry.expanded = self.tools_expanded;
            self.read_groups_key = None;
            self.entries.push(entry);
            self.entries.len() - 1
        });
        let entry = &mut self.entries[entry_index];
        let EntryBody::ArgumentsPreview { truncated } = &mut entry.body else {
            return;
        };
        if *truncated {
            return;
        }
        if tool == "edit" {
            let mut end = text.len().min(LIMIT.saturating_sub(entry.text.len()));
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            if end > 0 {
                entry.text.push_str(&text[..end]);
                entry.layout = None;
            }
            if end < text.len() {
                *truncated = true;
                entry.layout = None;
            }
        } else {
            // `write` 预览不设字节上限：正文完整累积，只由 `layout` 的尾部折叠限制行数。
            entry.text.push_str(text);
            entry.layout = None;
        }
    }

    pub(super) fn clear_argument_previews(&mut self) {
        for index in std::mem::take(&mut self.argument_previews).into_values() {
            let entry = &mut self.entries[index];
            // 保留索引槽，避免改变助手、滚动锚点和其他工具的索引。
            entry.hidden = true;
            entry.text = String::new();
            entry.body = EntryBody::Plain;
            entry.status = EntryStatus::Complete;
            entry.layout = None;
        }
    }
}

/// 只提取 JSON 字符串。遇到尚未闭合的值时仅解码完整前缀，转义序列与代理对
/// 留待后续分片补齐；绝不通过补全 JSON 来触发工具执行。
fn fields(raw: &str) -> Vec<(String, String)> {
    let bytes = raw.as_bytes();
    let (mut cursor, mut key) = (0, None);
    let mut fields = Vec::new();
    while cursor < bytes.len() {
        if bytes[cursor] != b'"' {
            cursor += 1;
            continue;
        }
        let start = cursor;
        cursor += 1;
        while cursor < bytes.len() {
            match bytes[cursor] {
                b'\\' => cursor = (cursor + 2).min(bytes.len()),
                b'"' => break,
                _ => cursor += 1,
            }
        }
        let closed = cursor < bytes.len();
        let value = if closed {
            serde_json::from_str::<String>(&raw[start..=cursor]).ok()
        } else {
            partial_string(&raw[start..])
        };
        let Some(value) = value else { break };
        cursor += usize::from(closed);
        let next = bytes[cursor..]
            .iter()
            .position(|b| !b.is_ascii_whitespace())
            .map(|offset| cursor + offset);
        if closed && next.is_some_and(|next| bytes[next] == b':') {
            key = Some(value);
            cursor = next.expect("已找到冒号") + 1;
        } else if let Some(key) = key.take() {
            fields.push((key, value));
        }
    }
    fields
}

fn partial_string(raw: &str) -> Option<String> {
    // 最长的不完整尾部是一个 UTF-16 代理对（两个六字节转义）。
    for trim in 0..=12.min(raw.len().saturating_sub(1)) {
        let end = raw.len() - trim;
        if !raw.is_char_boundary(end) {
            continue;
        }
        let mut candidate = raw[..end].to_owned();
        candidate.push('"');
        if let Ok(value) = serde_json::from_str(&candidate) {
            return Some(value);
        }
    }
    None
}

/// 预览字段的行前缀与配色；未知键不参与预览。
fn field_style(key: &str) -> Option<(&'static str, Color)> {
    match key {
        "path" => Some(("File: ", palette::ACTIVITY)),
        "oldText" => Some(("- ", palette::EDIT_REMOVED)),
        "newText" | "content" => Some(("+ ", palette::EDIT_ADDED)),
        "input" => Some(("", palette::ACTIVITY)),
        _ => None,
    }
}

/// `input` 的 `+` 行按新增内容着色。
fn line_style(key: &str, line: &str, color: Color) -> Color {
    if key == "input" && line.starts_with('+') {
        palette::EDIT_ADDED
    } else {
        color
    }
}

fn row(prefix: &str, color: Color, text: &str, base: Style) -> Line<'static> {
    Line::styled(
        format!("{prefix}{}", pp_core::text::display_text(text)),
        base.fg(color),
    )
}

pub(super) fn layout(raw: &str, width: u16, expanded: bool, truncated: bool) -> Vec<Line<'static>> {
    let base = Style::default().bg(palette::EDIT_BG);
    let fields = fields(raw);
    let hint = || Line::styled(LIMIT_HINT, base.fg(palette::MUTED));
    if expanded {
        // 展开：正向物化全部行，`Ctrl + O` 需要完整正文。
        let mut rows = Vec::new();
        for (key, value) in &fields {
            let Some((prefix, color)) = field_style(key) else {
                continue;
            };
            for line in value.split('\n') {
                rows.extend(wrap_block(
                    row(prefix, line_style(key, line, color), line, base),
                    width,
                    "  ",
                ));
            }
        }
        if truncated {
            rows.push(hint());
        }
        return rows;
    }
    // 折叠：从末尾反向排版，触碰到保留行数即停止，每帧 `Line` 分配与保留行数成正比，
    // 与被折叠掉的行数无关。
    let mut rows = Vec::new();
    if truncated {
        rows.push(hint());
    }
    let mut lines = fields
        .iter()
        .rev()
        .filter_map(|(key, value)| {
            field_style(key).map(|style| (key.as_str(), value.as_str(), style))
        })
        .flat_map(|(key, value, (prefix, color))| {
            value
                .rsplit('\n')
                .map(move |line| (prefix, line_style(key, line, color), line))
        });
    while rows.len() < COLLAPSED_LINES {
        let Some((prefix, color, text)) = lines.next() else {
            break;
        };
        let mut wrapped = wrap_block(row(prefix, color, text, base), width, "  ");
        wrapped.append(&mut rows);
        rows = wrapped;
    }
    // 只有确实还有更早内容时才折叠；正好 40 行原样显示，不加提示。
    let folded =
        rows.len() > COLLAPSED_LINES || (rows.len() == COLLAPSED_LINES && lines.next().is_some());
    if folded {
        rows.drain(..rows.len() - TAIL_LINES);
        rows.insert(0, Line::styled(TAIL_HINT, base.fg(palette::MUTED)));
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_preview_decodes_partial_escapes_and_surrogate_pairs() {
        let raw = r#"{"path":"demo.rs","edits":[{"oldText":"old\\text","newText":"中\n\"new\"\t\uD83D\uDE42"}]}"#;
        for end in (0..=raw.len()).filter(|end| raw.is_char_boundary(*end)) {
            let values = fields(&raw[..end]);
            if let Some((_, value)) = values.iter().find(|(key, _)| key == "newText") {
                assert!("中\n\"new\"\t🙂".starts_with(value), "{end}: {value:?}");
            }
        }
        assert_eq!(fields(raw).last().unwrap().1, "中\n\"new\"\t🙂");
        assert_eq!(
            fields(r#"{"input":"[demo#ABCD]\nPUT 1.=1:\n+hello"#)[0].1,
            "[demo#ABCD]\nPUT 1.=1:\n+hello"
        );
    }
}
