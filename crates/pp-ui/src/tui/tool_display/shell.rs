//! Shell 语义行的终端布局；语法与执行状态由 runtime 提供。

use super::{Context, palette, wrap_block};
use pp_core::shell_display::{ShellPresentation, ShellRow, ShellState, ShellStyle};
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use std::collections::HashSet;

pub(in crate::tui) fn command(
    snapshot: &ShellPresentation,
    expanded: bool,
    context: Context,
    width: u16,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut index = 0;
    while index < snapshot.rows.len() {
        let row = &snapshot.rows[index];
        let language = document_language(row);
        let end = if language.is_some() {
            (index + 1..snapshot.rows.len())
                .find(|next| {
                    document_language(&snapshot.rows[*next]) != language
                        || snapshot.rows[*next].depth != row.depth
                })
                .unwrap_or(snapshot.rows.len())
        } else {
            index + 1
        };
        let highlighted = language.and_then(|language| {
            let source = snapshot.rows[index..end]
                .iter()
                .map(|row| safe(&row.segments[0].text))
                .collect::<Vec<_>>()
                .join("\n");
            crate::tui::markdown::highlight_shell_document(language, &source)
        });
        for (offset, row) in snapshot.rows[index..end].iter().enumerate() {
            let code = highlighted
                .as_ref()
                .and_then(|lines| lines.get(offset))
                .map(|spans| spans.as_slice());
            lines.extend(
                layout_row(row, snapshot.running, width, code)
                    .into_iter()
                    .map(|line| fit_line(line, width.max(1))),
            );
        }
        index = end;
    }
    let max = context.window();
    if !expanded && !context.full && lines.len() > max {
        let hidden = lines.len() - max + 1;
        lines.drain(..hidden);
        lines.insert(
            0,
            Line::styled(
                format!("… {hidden} earlier lines (Ctrl + O to expand)"),
                Style::default().fg(palette::muted()),
            ),
        );
    }
    lines
}

fn layout_row(
    row: &ShellRow,
    running: bool,
    width: u16,
    document: Option<&[Span<'static>]>,
) -> Vec<Line<'static>> {
    let failed_segments = row.segments.iter().any(|s| s.state == ShellState::Fail);
    let failed = row.state == ShellState::Fail && !failed_segments && !row.block;
    let active = row.segments.iter().any(|s| s.state == ShellState::Running);
    let mut lead = Line::from(vec![Span::styled(
        if failed {
            "× "
        } else if active && running {
            "▶ "
        } else {
            "  "
        },
        Style::default().fg(if failed {
            palette::removed()
        } else {
            palette::accent()
        }),
    )]);
    lead.spans.push(Span::raw(
        "  ".repeat(usize::from(row.depth).min(usize::from(width) / 4)),
    ));
    if !row.effects.is_empty() {
        lead.spans.push(Span::styled(
            format!("{} ", safe(&row.effects)),
            Style::default().fg(palette::accent()),
        ));
    }
    let mut body = Line::default();
    let mut failures = HashSet::new();
    for segment in &row.segments {
        let inactive = matches!(segment.state, ShellState::Pending | ShellState::Skipped)
            || (running && segment.state != ShellState::Running);
        let mut style = Style::default().fg(match segment.style {
            ShellStyle::Text => palette::tool_command(),
            ShellStyle::Command => palette::syntax_function(),
            ShellStyle::Keyword => palette::syntax_keyword(),
            ShellStyle::Variable => palette::edit_type(),
            ShellStyle::String => palette::edit_string(),
            ShellStyle::Number => palette::edit_number(),
            ShellStyle::Operator => palette::muted(),
            ShellStyle::Badge => palette::accent(),
            ShellStyle::Comment => palette::syntax_comment(),
        });
        if inactive {
            style = style.add_modifier(Modifier::DIM);
        } else if running && segment.state == ShellState::Running {
            style = style.add_modifier(Modifier::BOLD);
        }
        if segment.state == ShellState::Fail
            && let Some(node) = segment.node
            && failures.insert(node)
        {
            body.spans
                .push(Span::styled("× ", Style::default().fg(palette::removed())));
        }
        let text = safe(&segment.text);
        let highlighted = document.map(<[Span<'static>]>::to_vec).or_else(|| {
            segment
                .language
                .as_deref()
                .and_then(|lang| crate::tui::markdown::highlight_shell_segment(lang, &text))
        });
        if let Some(highlighted) = highlighted {
            for mut span in highlighted {
                if inactive {
                    span.style = span.style.add_modifier(Modifier::DIM);
                } else if active && running {
                    span.style = span.style.add_modifier(Modifier::BOLD);
                }
                body.spans.push(span);
            }
        } else {
            body.spans.push(Span::styled(text, style));
        }
    }
    if !row.meta.is_empty() {
        body.spans.push(Span::styled(
            format!("  {}", safe(&row.meta)),
            Style::default().fg(if row.state == ShellState::Fail {
                palette::removed()
            } else {
                palette::muted()
            }),
        ));
    }
    let width = width.max(1);
    let lead_width = lead.width();
    if lead_width >= usize::from(width) {
        return wrap_block(body, width, "");
    }
    let hanging = " ".repeat(lead_width);
    wrap_block(body, width - lead_width as u16, "")
        .into_iter()
        .enumerate()
        .map(|(index, mut line)| {
            let mut spans = if index == 0 {
                lead.spans.clone()
            } else {
                vec![Span::raw(hanging.clone())]
            };
            spans.append(&mut line.spans);
            Line::from(spans)
        })
        .collect()
}

fn safe(text: &str) -> String {
    pp_core::text::display_text(text).replace(['\n', '\r'], " ")
}

/// 极窄窗口容不下双列字形时使用单列占位，避免越过右边界。
fn fit_line(mut line: Line<'static>, width: u16) -> Line<'static> {
    if line.width() <= usize::from(width) {
        return line;
    }
    let mut left = usize::from(width);
    let mut spans = Vec::new();
    for span in line.spans {
        if left == 0 {
            break;
        }
        let text = super::clip(&span.content, left, false);
        let text = if text.is_empty() && !span.content.is_empty() {
            "�".to_owned()
        } else {
            text
        };
        left = left.saturating_sub(Line::raw(&text).width());
        spans.push(Span::styled(text, span.style));
    }
    line.spans = spans;
    line
}

fn document_language(row: &ShellRow) -> Option<&str> {
    if row.node.is_none() && row.segments.len() == 1 {
        row.segments[0].language.as_deref()
    } else {
        None
    }
}
