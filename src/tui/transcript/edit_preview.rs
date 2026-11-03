//! 参数预览不解析成可执行工具调用，也不读取磁盘；完整调用仍由原执行链路校验。
use super::{
    EntryBody, EntryKind, EntryStatus, Line, Style, Transcript, TranscriptEntry, palette,
    wrap_block,
};

const LIMIT: usize = 64 * 1024;

impl Transcript {
    pub(super) fn preview_edit(&mut self, index: u32, text: &str) {
        if text.is_empty() || self.replaying {
            return;
        }
        let entry_index = *self.edit_previews.entry(index).or_insert_with(|| {
            let mut entry = TranscriptEntry::new(
                EntryKind::Diff,
                String::new(),
                String::new(),
                Some("edit".into()),
            );
            entry.body = EntryBody::EditPreview { truncated: false };
            entry.status = EntryStatus::Running;
            entry.expanded = self.tools_expanded;
            self.read_groups_key = None;
            self.entries.push(entry);
            self.entries.len() - 1
        });
        let entry = &mut self.entries[entry_index];
        let EntryBody::EditPreview { truncated } = &mut entry.body else {
            return;
        };
        if *truncated {
            return;
        }
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
    }

    pub(super) fn clear_edit_previews(&mut self) {
        for index in std::mem::take(&mut self.edit_previews).into_values() {
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

pub(super) fn layout(raw: &str, width: u16, expanded: bool, truncated: bool) -> Vec<Line<'static>> {
    let base = Style::default().bg(palette::EDIT_BG);
    let mut rows = Vec::new();
    for (key, value) in fields(raw) {
        let (prefix, color) = match key.as_str() {
            "path" => ("File: ", palette::ACTIVITY),
            "oldText" => ("- ", palette::EDIT_REMOVED),
            "newText" => ("+ ", palette::EDIT_ADDED),
            "input" => ("", palette::ACTIVITY),
            _ => continue,
        };
        for line in value.split('\n') {
            let color = if key == "input" && line.starts_with('+') {
                palette::EDIT_ADDED
            } else {
                color
            };
            rows.extend(wrap_block(
                Line::styled(
                    format!("{prefix}{}", palette::display_text(line)),
                    base.fg(color),
                ),
                width,
                "  ",
            ));
        }
    }
    if truncated {
        rows.push(Line::styled(
            "… preview limit reached",
            base.fg(palette::MUTED),
        ));
    }
    if !expanded && rows.len() > 40 {
        rows.drain(..rows.len() - 39);
        rows.insert(
            0,
            Line::styled("… preview tail · Ctrl+O to expand", base.fg(palette::MUTED)),
        );
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_preview_decodes_partial_escapes_and_surrogate_pairs() {
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
