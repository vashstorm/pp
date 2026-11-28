//! Standalone display module for the `eval` tool: language detection (Python / JavaScript / fallback), code syntax highlighting and the output tail window.

use super::{Context, View, palette, tail_output, wrap_block};
use crate::tui::transcript::card::{self, CardSurface};
use pp_config::config::IconMode;
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use serde_json::Value;

const EVAL_LINES: usize = 10;

/// Target language of the `eval` tool; Python and JavaScript are distinguished explicitly and a smooth fallback is kept for future languages.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::tui) enum EvalLang {
    Python,
    JavaScript,
    Other(String),
    Unknown,
}

impl EvalLang {
    pub(in crate::tui) fn from_raw(raw: Option<&str>) -> Self {
        let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
            return Self::Unknown;
        };
        let lower = raw.to_ascii_lowercase();
        match lower.as_str() {
            "py" | "python" => Self::Python,
            "js" | "javascript" => Self::JavaScript,
            _ => Self::Other(pp_core::text::display_text(raw).into_owned()),
        }
    }

    pub(in crate::tui) fn from_entry(arguments: &Value, result: Option<&Value>) -> Self {
        match Self::from_raw(arguments["language"].as_str()) {
            Self::Unknown => Self::from_raw(result.and_then(|r| r["language"].as_str())),
            lang => lang,
        }
    }

    pub(in crate::tui) fn label(&self) -> &str {
        match self {
            Self::Python => "Python",
            Self::JavaScript => "JavaScript",
            Self::Other(name) => name.as_str(),
            Self::Unknown => "Eval",
        }
    }

    pub(in crate::tui) fn icon(&self, icons: IconMode) -> &str {
        match (self, icons) {
            (Self::Python, IconMode::Nerd) => "\u{f0320}",
            (Self::JavaScript, IconMode::Nerd) => "\u{f031e}",
            (Self::Other(_) | Self::Unknown, IconMode::Nerd) => "\u{f0169}",
            (Self::Python, IconMode::Ascii) => "P",
            (Self::JavaScript, IconMode::Ascii) => "J",
            (Self::Other(_) | Self::Unknown, IconMode::Ascii) => "E",
        }
    }

    pub(in crate::tui) fn accent(&self) -> Color {
        match self {
            Self::Python => palette::eval_py_title(),
            Self::JavaScript => palette::eval_js_title(),
            Self::Other(_) | Self::Unknown => palette::eval_title(),
        }
    }

    pub(in crate::tui) fn icon_color(&self) -> Color {
        match self {
            Self::Python => palette::eval_py_icon(),
            Self::JavaScript => palette::eval_js_icon(),
            Self::Other(_) | Self::Unknown => palette::eval_title(),
        }
    }

    pub(in crate::tui) fn highlight_token(&self) -> Option<&str> {
        match self {
            Self::Python => Some("python"),
            Self::JavaScript => Some("javascript"),
            Self::Other(name) => Some(name.as_str()),
            Self::Unknown => None,
        }
    }
}

fn sanitize_for_highlight(text: &str) -> String {
    let safe = pp_core::text::display_text(text);
    if !safe.contains('\t') {
        return safe.into_owned();
    }
    let mut out = String::with_capacity(safe.len());
    for (index, line) in safe.split('\n').enumerate() {
        if index > 0 {
            out.push('\n');
        }
        let mut column = 0usize;
        for grapheme in Span::raw(line).styled_graphemes(Style::default()) {
            if grapheme.symbol == "\t" {
                let spaces = 4 - column % 4;
                for _ in 0..spaces {
                    out.push(' ');
                }
                column += spaces;
            } else {
                out.push_str(grapheme.symbol);
                column += Span::raw(grapheme.symbol).width();
            }
        }
    }
    out
}

/// Lay out the title (if any) of the `eval` card and the code body highlighted for its language.
pub(in crate::tui) fn eval_code_lines(
    lang: &EvalLang,
    title: Option<&str>,
    code: &str,
    expanded: bool,
    context: Context,
    width: u16,
) -> Vec<Line<'static>> {
    let bg = card::surface_bg(CardSurface::Eval);
    let with_bg = |s: Style| match bg {
        Some(bg) => s.bg(bg),
        None => s,
    };
    let mut rows = Vec::new();
    if let Some(title) = title.map(str::trim).filter(|s| !s.is_empty()) {
        let header = Line::from(vec![
            Span::styled("▸ ", with_bg(Style::default().fg(lang.accent()))),
            Span::styled(
                pp_core::text::display_text(title).into_owned(),
                with_bg(
                    Style::default()
                        .fg(palette::eval_title())
                        .add_modifier(Modifier::BOLD),
                ),
            ),
        ]);
        rows.extend(wrap_block(header, width, ""));
    }

    let code = code.trim_end();
    if code.is_empty() {
        return rows;
    }

    let sanitized = sanitize_for_highlight(code);
    let highlighted = lang
        .highlight_token()
        .and_then(|token| crate::tui::markdown::highlight_code_document(token, &sanitized));

    let base_style = with_bg(Style::default().fg(palette::tool_command()));
    let bg_only = with_bg(Style::default());
    let mut code_lines: Vec<Line<'static>> = if let Some(lines) = highlighted {
        lines
            .iter()
            .map(|spans| {
                if spans.is_empty() {
                    Line::styled(String::new(), base_style)
                } else {
                    Line::from((**spans).clone()).style(bg_only)
                }
            })
            .collect()
    } else {
        sanitized
            .split('\n')
            .map(|line| Line::styled(line.to_owned(), base_style))
            .collect()
    };

    let max = context.window();
    if !expanded && !context.full && code_lines.len() > max {
        let hidden = code_lines.len() - max + 1;
        code_lines.drain(..hidden);
        code_lines.insert(
            0,
            Line::styled(
                format!("… {hidden} earlier lines (Ctrl + O to expand)"),
                with_bg(Style::default().fg(palette::muted())),
            ),
        );
    }

    for line in code_lines {
        rows.extend(wrap_block(line, width, ""));
    }
    rows
}

/// Lay out the output body of the `eval` card (stdout / `[stderr]` coloring and the folded tail window).
pub(in crate::tui) fn eval_body(view: &View<'_>, width: u16, style: Style) -> Vec<Line<'static>> {
    let expanded = view.expanded || view.context.full;
    tail_output(
        view.text,
        EVAL_LINES,
        expanded,
        view.context,
        width,
        style,
        palette::muted(),
    )
}
