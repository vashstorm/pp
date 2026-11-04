//! fenced code block 的 syntect 语法高亮。
//!
//! 输入文本由调用方保证已 `display_text` 脱敏且 tab 已按四列展开；本模块只做
//! token 解析、上限护栏与着色。完整行保存语法状态，追加时只重算变化的末行。
//! 缓存按代码块隔离，采用数量与源文本字节双重上限。

use std::cell::Cell;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};

use ratatui::style::{Color as RatatuiColor, Modifier, Style};
use ratatui::text::Span;
use syntect::easy::HighlightLines;
use syntect::highlighting::{
    Color as SyntectColor, FontStyle, HighlightState, Theme, ThemeItem, ThemeSettings,
};
use syntect::parsing::{ParseState, SyntaxSet};
use unicode_segmentation::UnicodeSegmentation;

use crate::tui::transcript_style::{
    CODE_BLOCK, EDIT_NUMBER, EDIT_STRING, EDIT_TYPE, SYNTAX_COMMENT, SYNTAX_CONSTANT,
    SYNTAX_FUNCTION, SYNTAX_KEYWORD, SYNTAX_TAG,
};

/// 行数与字节上限：超出直接回退纯色，保证流式 delta 与超大块成本有界。
const MAX_LINES: usize = 500;
const MAX_BYTES: usize = 65_536;
/// 进程级 LRU 缓存容量。
const CACHE_CAPACITY: usize = 64;

fn syntaxes() -> &'static SyntaxSet {
    static SYNTAXES: OnceLock<SyntaxSet> = OnceLock::new();
    SYNTAXES.get_or_init(SyntaxSet::load_defaults_newlines)
}

/// 程序化构造的主题：scope 前缀匹配 + 长 scope 优先，未匹配文本使用
/// `settings.foreground`（即 `CODE_BLOCK`）。不解析 .tmTheme。
fn theme() -> &'static Theme {
    static THEME: OnceLock<Theme> = OnceLock::new();
    THEME.get_or_init(|| {
        fn item(scope: &str, fg: RatatuiColor, italic: bool) -> ThemeItem {
            ThemeItem {
                scope: scope.parse().expect("static scope selector"),
                style: syntect::highlighting::StyleModifier {
                    foreground: Some(syntect_color(fg)),
                    background: None,
                    font_style: italic.then_some(FontStyle::ITALIC),
                },
            }
        }
        Theme {
            name: Some("pp-tui".to_string()),
            author: None,
            settings: ThemeSettings {
                foreground: Some(syntect_color(CODE_BLOCK)),
                ..ThemeSettings::default()
            },
            scopes: vec![
                item("keyword", SYNTAX_KEYWORD, false),
                item("storage", SYNTAX_KEYWORD, false),
                item("comment", SYNTAX_COMMENT, true),
                item("string", EDIT_STRING, false),
                item("constant.character.escape", EDIT_STRING, false),
                item("constant.numeric", EDIT_NUMBER, false),
                item("constant", SYNTAX_CONSTANT, false),
                item("entity.name.function", SYNTAX_FUNCTION, false),
                item("support.function", SYNTAX_FUNCTION, false),
                item("entity.name.type", EDIT_TYPE, false),
                item("support.class", EDIT_TYPE, false),
                item("support.type", EDIT_TYPE, false),
                item("entity.name.tag", SYNTAX_TAG, false),
            ],
        }
    })
}

fn syntect_color(color: RatatuiColor) -> SyntectColor {
    match color {
        RatatuiColor::Rgb(r, g, b) => SyntectColor { r, g, b, a: 0xff },
        _ => SyntectColor {
            r: 0,
            g: 0,
            b: 0,
            a: 0xff,
        },
    }
}

fn ratatui_style(style: syntect::highlighting::Style) -> Style {
    let SyntectColor { r, g, b, .. } = style.foreground;
    let mut st = Style::new().fg(RatatuiColor::Rgb(r, g, b));
    if style.font_style.contains(FontStyle::BOLD) {
        st = st.add_modifier(Modifier::BOLD);
    }
    if style.font_style.contains(FontStyle::ITALIC) {
        st = st.add_modifier(Modifier::ITALIC);
    }
    if style.font_style.contains(FontStyle::UNDERLINE) {
        st = st.add_modifier(Modifier::UNDERLINED);
    }
    st
}

type HighlightedLines = Vec<Vec<Span<'static>>>;
thread_local! { static CONTEXT: Cell<(u64, u64, usize)> = const { Cell::new((0, 0, 0)) }; }
pub(super) fn release_context(id: u64) {
    cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|item| item.context.is_none_or(|(owner, _, _)| owner != id));
}
pub(super) fn context(id: u64, epoch: u64) {
    cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|item| {
            item.context
                .is_none_or(|(owner, previous, _)| owner != id || previous == epoch)
        });
    CONTEXT.with(|c| c.set((id, epoch, 0)));
}
struct Cached {
    context: Option<(u64, u64, usize)>,
    lang: String,
    code: String,
    stable: usize,
    state: (HighlightState, ParseState),
    prefix: HighlightedLines,
    result: Arc<HighlightedLines>,
}
fn cache() -> &'static Mutex<VecDeque<Cached>> {
    static CACHE: OnceLock<Mutex<VecDeque<Cached>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(VecDeque::new()))
}

fn grapheme_spans(
    line: &str,
    regions: Vec<(syntect::highlighting::Style, &str)>,
) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut graphemes = line.grapheme_indices(true).peekable();
    let mut region_end = 0;
    for (style, text) in regions {
        region_end += text.len();
        let Some(&(start, _)) = graphemes.peek() else {
            break;
        };
        if start >= region_end {
            continue;
        }
        // token 边界可能落在字素簇内部，完整字素簇沿用首字符的样式。
        // 按整行划分，避免 wrap/Ratatui 丢弃跨 span 的零宽组合字符。
        let mut end = start;
        while let Some(&(offset, grapheme)) = graphemes.peek()
            && offset < region_end
        {
            end = offset + grapheme.len();
            graphemes.next();
        }
        let text = &line[start..end];
        let style = ratatui_style(style);
        if let Some(last) = spans.last_mut()
            && last.style == style
        {
            last.content.to_mut().push_str(text);
        } else {
            spans.push(Span::styled(text.to_string(), style));
        }
    }
    spans
}

/// 按语言 token 高亮完整代码块，返回每行已合并相邻同 style 的 spans。
/// 无语言、未知 token、超限或解析失败均返回 `None`（调用方回退纯色）。
pub(super) fn highlight(lang: Option<&str>, code: &str) -> Option<Arc<HighlightedLines>> {
    let lang = lang.filter(|l| !l.is_empty())?;
    if code.len() > MAX_BYTES || code.split('\n').count() > MAX_LINES {
        return None;
    }
    let syntaxes = syntaxes();
    let syntax = syntaxes
        .find_syntax_by_token(lang)
        .or_else(|| syntaxes.find_syntax_by_token(&lang.to_lowercase()))?;
    let context = CONTEXT.with(|c| {
        let (id, epoch, block) = c.get();
        if id == 0 {
            None
        } else {
            c.set((id, epoch, block + 1));
            Some((id, epoch, block))
        }
    });
    let cached = {
        let mut cache = cache().lock().unwrap_or_else(|e| e.into_inner());
        let exact = cache
            .iter()
            .position(|item| item.lang == lang && item.code == code && item.context == context);
        if let Some(index) = exact {
            let item = cache.remove(index)?;
            let result = item.result.clone();
            cache.push_back(item);
            return Some(result);
        }
        let index = cache.iter().rposition(|item| {
            item.lang == lang
                && item.context == context
                && item.stable > 0
                && code.starts_with(&item.code[..item.stable])
        });
        index.and_then(|index| cache.remove(index))
    };
    let (mut highlighter, mut lines, offset) = match cached {
        Some(item) => (
            HighlightLines::from_state(theme(), item.state.0, item.state.1),
            item.prefix,
            item.stable,
        ),
        None => (HighlightLines::new(syntax, theme()), Vec::new(), 0),
    };
    let stable = code.rfind('\n').map_or(0, |index| index + 1);
    for line in code[offset..stable].split_terminator('\n') {
        let fed = format!("{line}\n");
        lines.push(grapheme_spans(
            line,
            highlighter.highlight_line(&fed, syntaxes).ok()?,
        ));
    }
    let state = highlighter.state();
    let prefix = lines.clone();
    let mut highlighter = HighlightLines::from_state(theme(), state.0.clone(), state.1.clone());
    let last = &code[stable..];
    let fed = format!("{last}\n");
    lines.push(grapheme_spans(
        last,
        highlighter.highlight_line(&fed, syntaxes).ok()?,
    ));
    let result = Arc::new(lines);
    let mut cache = cache().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(context) = context {
        cache.retain(|item| item.context != Some(context));
    }
    cache.push_back(Cached {
        context,
        lang: lang.into(),
        code: code.into(),
        stable,
        state,
        prefix,
        result: result.clone(),
    });
    while cache.len() > CACHE_CAPACITY
        || cache.iter().map(|item| item.code.len()).sum::<usize>() > 8 * 1024 * 1024
    {
        cache.pop_front();
    }
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_boundaries_preserve_complete_graphemes_in_buffer() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        use ratatui::text::Line;
        use ratatui::widgets::Widget;

        let line = "e\u{301}\u{308}👩\u{200d}💻🇨🇳✈\u{fe0f}x";
        let regions = line
            .char_indices()
            .enumerate()
            .map(|(index, (offset, ch))| {
                let style = syntect::highlighting::Style {
                    foreground: syntect_color(if index % 2 == 0 {
                        EDIT_STRING
                    } else {
                        EDIT_NUMBER
                    }),
                    ..syntect::highlighting::Style::default()
                };
                (style, &line[offset..offset + ch.len_utf8()])
            })
            .collect();
        let spans = grapheme_spans(line, regions);
        assert_eq!(spans[0].style.fg, Some(EDIT_STRING));
        let width = Span::raw(line).width() as u16;
        let area = Rect::new(0, 0, width, 1);
        let mut buffer = Buffer::empty(area);
        Line::from(spans).render(area, &mut buffer);
        let mut x = 0;
        for grapheme in line.graphemes(true) {
            assert_eq!(buffer[(x, 0)].symbol(), grapheme);
            x += Span::raw(grapheme).width() as u16;
        }
    }

    fn span_fg_containing(lines: &HighlightedLines, needle: &str) -> Option<RatatuiColor> {
        lines
            .iter()
            .flat_map(|spans| spans.iter())
            .find(|span| span.content.contains(needle))
            .and_then(|span| span.style.fg)
    }

    #[test]
    fn rust_tokens_use_semantic_colors() {
        let code = "let x = 1; // note\nlet s = \"hi\";";
        let lines = highlight(Some("rust"), code).expect("rust resolves");
        assert_eq!(lines.len(), 2);
        assert_eq!(span_fg_containing(&lines, "let"), Some(SYNTAX_KEYWORD));
        assert_eq!(span_fg_containing(&lines, "1"), Some(EDIT_NUMBER));
        assert_eq!(span_fg_containing(&lines, "// note"), Some(SYNTAX_COMMENT));
        assert_eq!(span_fg_containing(&lines, "\"hi\""), Some(EDIT_STRING));
        for needle in ["let", "1", "// note", "\"hi\""] {
            assert_ne!(span_fg_containing(&lines, needle), Some(CODE_BLOCK));
        }
        // 注释叠加 ITALIC。
        let comment = lines[0]
            .iter()
            .find(|span| span.content.contains("// note"))
            .unwrap();
        assert!(comment.style.add_modifier.contains(Modifier::ITALIC));
        // 拼接还原图元。
        let joined: String = lines
            .iter()
            .flat_map(|spans| spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_eq!(joined, code.replace('\n', ""));
    }

    #[test]
    fn unknown_token_and_missing_lang_return_none() {
        assert!(highlight(None, "let x = 1;").is_none());
        assert!(highlight(Some("notalang"), "let x = 1;").is_none());
        assert!(highlight(Some(""), "let x = 1;").is_none());
    }

    #[test]
    fn oversized_block_returns_none() {
        let code: String = "let x = 1;\n".repeat(600);
        assert!(highlight(Some("rust"), &code).is_none());
        let wide = "a".repeat(MAX_BYTES + 1);
        assert!(highlight(Some("rust"), &wide).is_none());
    }

    #[test]
    fn repeated_calls_hit_the_cache() {
        let code = "fn main() {}";
        let first = highlight(Some("rust"), code).expect("rust resolves");
        let second = highlight(Some("rust"), code).expect("rust resolves");
        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn incremental_highlight_matches_full_parse_for_partial_lines() {
        let source = "/* multi\nline */\nlet x = r#\"string\nvalue\"#;\nlet 中文 = \"🙂\";\n";
        for (index, _) in source.char_indices().skip(1) {
            let text = &source[..index];
            context(99881, 1);
            let incremental = highlight(Some("rust"), text).unwrap();
            let syntaxes = syntaxes();
            let mut highlighter =
                HighlightLines::new(syntaxes.find_syntax_by_token("rust").unwrap(), theme());
            let full: HighlightedLines = text
                .split('\n')
                .map(|line| {
                    let fed = format!("{line}\n");
                    grapheme_spans(line, highlighter.highlight_line(&fed, syntaxes).unwrap())
                })
                .collect();
            assert_eq!(*incremental, full, "前缀字节数：{index}");
        }
        context(0, 0);
    }

    #[test]
    fn plain_token_keeps_default_color() {
        // "text" 不在 syntect 收录 token 中，走回退；"txt" 解析到 Plain Text。
        assert!(highlight(Some("text"), "just words").is_none());
        let lines = highlight(Some("txt"), "just words").expect("txt resolves");
        assert_eq!(lines.len(), 1);
        for span in &lines[0] {
            assert_eq!(span.style.fg, Some(CODE_BLOCK));
        }
    }
}
