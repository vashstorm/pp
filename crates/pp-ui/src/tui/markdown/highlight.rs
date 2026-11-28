//! syntect syntax highlighting of fenced code blocks.
//!
//! The caller guarantees the input text already went through `display_text` sanitization and that tabs are expanded to four columns; this module only does
//! token parsing, the limit guardrails and coloring. A complete line keeps its syntax state, and an append only recomputes the changed last line.
//! The cache is isolated per code block and capped both by entry count and by source text bytes.

use std::cell::Cell;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};

use pp_config::config::Theme as UiTheme;
use ratatui::style::{Color as RatatuiColor, Modifier, Style};
use ratatui::text::Span;
use syntect::easy::HighlightLines;
use syntect::highlighting::{
    Color as SyntectColor, FontStyle, HighlightState, Theme, ThemeItem, ThemeSettings,
};
use syntect::parsing::{ParseState, SyntaxSet};
use unicode_segmentation::UnicodeSegmentation;

use crate::tui::themes;
#[cfg(test)]
use crate::tui::transcript_style::{
    code_block, edit_number, edit_string, syntax_comment, syntax_keyword,
};

/// Line count and byte limits: going over falls back to solid color, keeping the cost of streaming deltas and oversized blocks bounded.
const MAX_LINES: usize = 500;
const MAX_BYTES: usize = 65_536;
/// Process-wide LRU cache capacity.
const CACHE_CAPACITY: usize = 64;

fn syntaxes() -> &'static SyntaxSet {
    static SYNTAXES: OnceLock<SyntaxSet> = OnceLock::new();
    SYNTAXES.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn build_theme(active: UiTheme) -> Theme {
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
    let p = themes::palette_for(active);
    Theme {
        name: Some(format!("pp-tui-{}", active.as_str())),
        author: None,
        settings: ThemeSettings {
            foreground: Some(syntect_color(p.code_block)),
            ..ThemeSettings::default()
        },
        scopes: vec![
            item("keyword", p.syntax_keyword, false),
            item("storage", p.syntax_keyword, false),
            item("comment", p.syntax_comment, true),
            item("string", p.edit_string, false),
            item("constant.character.escape", p.edit_string, false),
            item("constant.numeric", p.edit_number, false),
            item("constant", p.syntax_constant, false),
            item("entity.name.function", p.syntax_function, false),
            item("support.function", p.syntax_function, false),
            item("entity.name.type", p.edit_type, false),
            item("support.class", p.edit_type, false),
            item("support.type", p.edit_type, false),
            item("entity.name.tag", p.syntax_tag, false),
        ],
    }
}

/// Programmatically built theme: scope prefix matching with longer scopes first; unmatched text uses
/// `settings.foreground` (that is `code_block`). No .tmTheme parsing.
fn theme_for(active: UiTheme) -> &'static Theme {
    static THEMES: [OnceLock<Theme>; UiTheme::ALL.len()] =
        [const { OnceLock::new() }; UiTheme::ALL.len()];
    let index = match active {
        UiTheme::Default => 0,
        UiTheme::TokyoNight => 1,
        UiTheme::Catppuccin => 2,
        UiTheme::Dracula => 3,
        UiTheme::Nord => 4,
        UiTheme::Gruvbox => 5,
        UiTheme::SolarizedLight => 6,
        UiTheme::OneDark => 7,
        UiTheme::SolarizedDark => 8,
        UiTheme::Monokai => 9,
        UiTheme::RosePine => 10,
        UiTheme::RosePineDawn => 11,
        UiTheme::Kanagawa => 12,
        UiTheme::Everforest => 13,
        UiTheme::GruvboxLight => 14,
        UiTheme::CatppuccinLatte => 15,
        UiTheme::GitHubDark => 16,
        UiTheme::GitHubLight => 17,
        UiTheme::AyuDark => 18,
        UiTheme::NightOwl => 19,
    };
    THEMES[index].get_or_init(|| build_theme(active))
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

type HighlightedLines = Vec<Arc<Vec<Span<'static>>>>;

pub(super) fn prewarm() {
    let _ = syntaxes();
}
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
    theme: UiTheme,
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
        // A token boundary may fall inside a grapheme cluster; the whole cluster keeps the style of its first character.
        // Work on whole lines so wrap/Ratatui does not drop zero-width combining characters across spans.
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

/// Highlight a whole code block by language token and return the spans of every line with adjacent equal styles merged.
/// No language, unknown token, over-limit or parse failure all return `None` (the caller falls back to solid color).
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
    let active_theme = themes::current_theme();
    let theme = theme_for(active_theme);
    let cached = {
        let mut cache = cache().lock().unwrap_or_else(|e| e.into_inner());
        let exact = cache.iter().position(|item| {
            item.theme == active_theme
                && item.lang == lang
                && item.code == code
                && item.context == context
        });
        if let Some(index) = exact {
            let item = cache.remove(index)?;
            let result = item.result.clone();
            cache.push_back(item);
            return Some(result);
        }
        let index = cache.iter().rposition(|item| {
            item.theme == active_theme
                && item.lang == lang
                && item.context == context
                && item.stable > 0
                && code.starts_with(&item.code[..item.stable])
        });
        index.and_then(|index| cache.remove(index))
    };
    let (mut highlighter, mut lines, offset) = match cached {
        Some(item) => (
            HighlightLines::from_state(theme, item.state.0, item.state.1),
            item.prefix,
            item.stable,
        ),
        None => (HighlightLines::new(syntax, theme), Vec::new(), 0),
    };
    let stable = code.rfind('\n').map_or(0, |index| index + 1);
    for line in code[offset..stable].split_terminator('\n') {
        let fed = format!("{line}\n");
        lines.push(Arc::new(grapheme_spans(
            line,
            highlighter.highlight_line(&fed, syntaxes).ok()?,
        )));
    }
    let state = highlighter.state();
    let prefix = lines.clone();
    let mut highlighter = HighlightLines::from_state(theme, state.0.clone(), state.1.clone());
    let last = &code[stable..];
    let fed = format!("{last}\n");
    lines.push(Arc::new(grapheme_spans(
        last,
        highlighter.highlight_line(&fed, syntaxes).ok()?,
    )));
    let result = Arc::new(lines);
    let mut cache = cache().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(context) = context {
        cache.retain(|item| item.context != Some(context));
    }
    cache.push_back(Cached {
        context,
        theme: active_theme,
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
                        edit_string()
                    } else {
                        edit_number()
                    }),
                    ..syntect::highlighting::Style::default()
                };
                (style, &line[offset..offset + ch.len_utf8()])
            })
            .collect();
        let spans = grapheme_spans(line, regions);
        assert_eq!(spans[0].style.fg, Some(edit_string()));
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
    fn highlight_semantic_tokens_cache_and_fallbacks() {
        let code = "let x = 1; // note\nlet s = \"hi\";";
        let lines = highlight(Some("rust"), code).expect("rust resolves");
        assert_eq!(lines.len(), 2);
        assert_eq!(span_fg_containing(&lines, "let"), Some(syntax_keyword()));
        assert_eq!(span_fg_containing(&lines, "1"), Some(edit_number()));
        assert_eq!(
            span_fg_containing(&lines, "// note"),
            Some(syntax_comment())
        );
        assert_eq!(span_fg_containing(&lines, "\"hi\""), Some(edit_string()));
        for needle in ["let", "1", "// note", "\"hi\""] {
            assert_ne!(span_fg_containing(&lines, needle), Some(code_block()));
        }
        let comment = lines[0]
            .iter()
            .find(|span| span.content.contains("// note"))
            .unwrap();
        assert!(comment.style.add_modifier.contains(Modifier::ITALIC));
        let joined: String = lines
            .iter()
            .flat_map(|spans| spans.iter())
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(joined, code.replace('\n', ""));

        assert!(highlight(None, "let x = 1;").is_none());
        assert!(highlight(Some("notalang"), "let x = 1;").is_none());
        assert!(highlight(Some(""), "let x = 1;").is_none());
        assert!(highlight(Some("text"), "just words").is_none());

        let txt = highlight(Some("txt"), "just words").expect("txt resolves");
        assert_eq!(txt.len(), 1);
        for span in txt[0].iter() {
            assert_eq!(span.style.fg, Some(code_block()));
        }

        let big: String = "let x = 1;\n".repeat(600);
        assert!(highlight(Some("rust"), &big).is_none());
        let wide = "a".repeat(MAX_BYTES + 1);
        assert!(highlight(Some("rust"), &wide).is_none());

        let cached_src = "fn main() {}";
        let first = highlight(Some("rust"), cached_src).expect("rust resolves");
        let second = highlight(Some("rust"), cached_src).expect("rust resolves");
        assert!(Arc::ptr_eq(&first, &second));
    }
}
