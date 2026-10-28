use super::transcript_style as palette;
use crate::events::{DiffLine, DiffLineKind};
use ratatui::{style::Style, text::Span};
use std::{collections::HashMap, ops::Range};

/// 只比较连续替换段中对应行的公共前后缀，不在绘制时重新读取文件。
pub(super) fn highlights(lines: &[DiffLine]) -> HashMap<usize, Range<usize>> {
    let mut result = HashMap::new();
    let mut start = 0;
    while start < lines.len() {
        if lines[start].kind == DiffLineKind::Context {
            start += 1;
            continue;
        }
        let end = (start..lines.len())
            .find(|&i| lines[i].kind == DiffLineKind::Context)
            .unwrap_or(lines.len());
        let old = (start..end).filter(|&i| lines[i].kind == DiffLineKind::Removed);
        let new = (start..end).filter(|&i| lines[i].kind == DiffLineKind::Added);
        for (old, new) in old.zip(new) {
            let a = lines[old].text.trim_end_matches(['\r', '\n']);
            let b = lines[new].text.trim_end_matches(['\r', '\n']);
            let prefix: usize = a
                .chars()
                .zip(b.chars())
                .take_while(|(a, b)| a == b)
                .map(|(c, _)| c.len_utf8())
                .sum();
            let suffix: usize = a[prefix..]
                .chars()
                .rev()
                .zip(b[prefix..].chars().rev())
                .take_while(|(a, b)| a == b)
                .map(|(c, _)| c.len_utf8())
                .sum();
            // 整行替换已经用前景色标识；只强调具有公共正文的局部变化。
            if !a[..prefix].trim().is_empty() || suffix > 0 {
                result.insert(old, prefix..a.len() - suffix);
                result.insert(new, prefix..b.len() - suffix);
            }
        }
        start = end;
    }
    result
}

pub(super) fn spans(
    text: &str,
    path: &str,
    kind: DiffLineKind,
    changed: Option<&Range<usize>>,
) -> Vec<Span<'static>> {
    let color = match kind {
        DiffLineKind::Added => palette::EDIT_ADDED,
        DiffLineKind::Removed => palette::EDIT_REMOVED,
        DiffLineKind::Context => palette::EDIT_TEXT,
    };
    let base = Style::default().fg(color).bg(palette::EDIT_BG);
    if kind != DiffLineKind::Context {
        let indent = text.len() - text.trim_start_matches([' ', '\t']).len();
        let mut spans = vec![Span::styled(
            text[..indent]
                .chars()
                .map(|c| if c == '\t' { "····" } else { "·" })
                .collect::<String>(),
            base,
        )];
        for (index, c) in text[indent..].char_indices() {
            let start = indent + index;
            let highlighted = changed.is_some_and(|range| range.contains(&start));
            let style = if highlighted {
                base.fg(palette::EDIT_BG).bg(color)
            } else {
                base
            };
            if let Some(previous) = spans.last_mut().filter(|span| span.style == style) {
                previous.content.to_mut().push(c);
            } else {
                spans.push(Span::styled(c.to_string(), style));
            }
        }
        return spans;
    }
    let extension = std::path::Path::new(path)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    if !matches!(
        extension,
        "rs" | "js"
            | "ts"
            | "jsx"
            | "tsx"
            | "py"
            | "go"
            | "c"
            | "h"
            | "cpp"
            | "hpp"
            | "java"
            | "json"
            | "toml"
            | "yaml"
            | "yml"
            | "sh"
    ) {
        return vec![Span::styled(text.to_owned(), base)];
    }
    // 轻量词法着色仅影响上下文；不声称解析完整语言语法。
    let mut result = Vec::new();
    let mut chars = text.char_indices().peekable();
    while let Some((start, c)) = chars.next() {
        let mut end = start + c.len_utf8();
        let color = if text[start..].starts_with("//") || c == '#' {
            end = text.len();
            for _ in chars.by_ref() {}
            palette::EDIT_META
        } else if c == '"' || (c == '\'' && extension != "rs") {
            let mut escaped = false;
            for (index, next) in chars.by_ref() {
                end = index + next.len_utf8();
                if next == c && !escaped {
                    break;
                }
                escaped = next == '\\' && !escaped;
            }
            palette::EDIT_STRING
        } else if c.is_alphanumeric() || c == '_' {
            while let Some(&(index, next)) = chars.peek() {
                if !next.is_alphanumeric() && next != '_' {
                    break;
                }
                end = index + next.len_utf8();
                chars.next();
            }
            let token = &text[start..end];
            if matches!(
                token,
                "let"
                    | "mut"
                    | "const"
                    | "static"
                    | "fn"
                    | "pub"
                    | "use"
                    | "mod"
                    | "crate"
                    | "self"
                    | "super"
                    | "impl"
                    | "trait"
                    | "struct"
                    | "enum"
                    | "type"
                    | "where"
                    | "for"
                    | "in"
                    | "while"
                    | "loop"
                    | "if"
                    | "else"
                    | "match"
                    | "return"
                    | "break"
                    | "continue"
                    | "async"
                    | "await"
                    | "move"
                    | "ref"
                    | "unsafe"
                    | "as"
                    | "true"
                    | "false"
                    | "null"
                    | "None"
                    | "def"
                    | "class"
                    | "import"
                    | "from"
                    | "function"
                    | "var"
                    | "new"
                    | "export"
                    | "package"
                    | "func"
            ) {
                palette::EDIT_KEYWORD
            } else if c.is_ascii_digit() {
                palette::EDIT_NUMBER
            } else if c.is_uppercase() {
                palette::EDIT_TYPE
            } else {
                palette::EDIT_TEXT
            }
        } else if matches!(c, '=' | '&' | '|' | '<' | '>') {
            palette::EDIT_KEYWORD
        } else {
            palette::EDIT_TEXT
        };
        result.push(Span::styled(text[start..end].to_owned(), base.fg(color)));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_inline_highlights_keep_utf8_boundaries_and_separate_exchanges() {
        let line = |kind, text: &str| DiffLine {
            kind,
            text: text.into(),
            old_line: None,
            new_line: None,
        };
        let lines = [
            line(DiffLineKind::Removed, "    let 名称 = \"旧😀\";\r\n"),
            line(DiffLineKind::Added, "    let 名称 = \"新🌟\";\n"),
            line(DiffLineKind::Context, "separator\n"),
            line(DiffLineKind::Added, "unpaired insertion\n"),
        ];
        let ranges = highlights(&lines);
        assert_eq!(&lines[0].text[ranges[&0].clone()], "旧😀");
        assert_eq!(&lines[1].text[ranges[&1].clone()], "新🌟");
        assert!(!ranges.contains_key(&3));
        let result = spans(
            lines[1].text.trim_end(),
            "main.rs",
            DiffLineKind::Added,
            ranges.get(&1),
        );
        let text: String = result.iter().map(|span| span.content.as_ref()).collect();
        assert_eq!(text, "····let 名称 = \"新🌟\";");
        assert!(
            result
                .iter()
                .any(|span| span.content == "新🌟" && span.style.bg == Some(palette::EDIT_ADDED))
        );
        let endings = [
            line(DiffLineKind::Removed, "same\r\n"),
            line(DiffLineKind::Added, "same\n"),
        ];
        assert!(highlights(&endings).values().all(Range::is_empty));
    }
}
