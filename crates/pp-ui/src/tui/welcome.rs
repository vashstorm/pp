use super::{statusline, themes};
use pp_config::config::IconMode;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};

pub(super) struct WelcomeContext<'a> {
    pub(super) mcp_tips: &'a [String],
    /// 对话区已有正文时为真：此时不再占用顶部留白，优先把行让给正文。
    pub(super) reserve_trailing: bool,
}

fn palette() -> &'static themes::Palette {
    themes::palette()
}

fn first() -> Color {
    palette().welcome_first
}

fn second() -> Color {
    palette().welcome_second
}

fn separator() -> Color {
    palette().welcome_separator
}

fn muted() -> Color {
    palette().welcome_muted
}

fn body_text() -> Color {
    palette().activity
}

fn success() -> Color {
    palette().added
}

fn failed() -> Color {
    palette().removed
}

fn wordmark_color() -> Color {
    palette().welcome_wordmark
}

fn card_bg() -> Color {
    palette().welcome_card_bg
}

fn mcp_tip_line(line: &str) -> Line<'static> {
    let base = Style::default().fg(muted());
    if line.starts_with("MCP: ") {
        for (marker, color) in [("✓", success()), ("✗", failed())] {
            if let Some(prefix) = line.strip_suffix(marker) {
                return Line::from(vec![
                    Span::styled(prefix.to_owned(), base),
                    Span::styled(marker, Style::default().fg(color)),
                ]);
            }
        }
    }
    Line::styled(line.to_owned(), base)
}

const SESSION_TIPS: [(&str, &str); 4] = [
    ("/model", "switch model, keep chat"),
    ("/new", "clear chat, keep model"),
    ("/resume", "reopen a saved session"),
    ("/plan", "plan, then approve edits"),
];
const KEYBOARD_TIPS: [(&str, &str); 4] = [
    ("Enter", "send a message"),
    ("Shift + Enter", "insert a newline"),
    ("Esc", "cancel the running task"),
    ("Shift + Tab", "cycle thinking levels"),
];
const WORDMARK: [&str; 5] = [
    "██████    ██████  ",
    "██    ██  ██    ██",
    "██████    ██████  ",
    "██        ██      ",
    "██        ██      ",
];
const TAGLINE: &str = "Paper Plane";
const VERSION: &str = concat!("v", env!("CARGO_PKG_VERSION"));
/// 欢迎页顶部留白行数：仅在内容之外还有富余行时生效。
const TOP_PADDING: u16 = 2;
const LOGO: [(&str, &str); 7] = [
    ("             ▄▄", "▄█"),
    ("       ▄▄▄██▀▀", "██"),
    (" ▄▄▄██████▀ ", "▄███"),
    ("▀▀█████▀  ", "▄████"),
    ("    ▀█  ", "▄█████▀"),
    ("     █▀ ", "▀█████"),
    ("           ", "▀█▀"),
];

fn session_icon(icons: IconMode) -> &'static str {
    match icons {
        IconMode::Nerd => "\u{f489} ",
        IconMode::Ascii => "> ",
    }
}

fn keyboard_icon(icons: IconMode) -> &'static str {
    match icons {
        IconMode::Nerd => "\u{f030c} ",
        IconMode::Ascii => "* ",
    }
}

fn tip_spans((key, description): (&str, &str), key_width: usize) -> Vec<Span<'static>> {
    vec![
        Span::styled(
            format!("{key:<key_width$}"),
            Style::default().fg(second()).add_modifier(Modifier::BOLD),
        ),
        Span::styled(description.to_owned(), Style::default().fg(body_text())),
    ]
}

fn footer_lines(width: u16) -> Vec<Line<'static>> {
    let key_style = Style::default().fg(second()).add_modifier(Modifier::BOLD);
    let desc_style = Style::default().fg(muted());
    let dot_style = Style::default().fg(separator());
    if width >= 44 {
        vec![Line::from(vec![
            Span::styled("/tips", key_style),
            Span::styled("  All shortcuts", desc_style),
            Span::styled("   ·   ", dot_style),
            Span::styled("/help", key_style),
            Span::styled("  Commands", desc_style),
        ])]
    } else {
        vec![
            Line::from(vec![
                Span::styled("/tips", key_style),
                Span::styled("  All shortcuts", desc_style),
            ]),
            Line::from(vec![
                Span::styled("/help", key_style),
                Span::styled("  Commands", desc_style),
            ]),
        ]
    }
}

fn two_column_card_lines(card_width: u16, inset: u16) -> Vec<Line<'static>> {
    let icons = themes::current_icons();
    let title_style = Style::default().fg(first()).add_modifier(Modifier::BOLD);
    let rule_style = Style::default().fg(separator());
    let pad = " ".repeat(usize::from(inset));
    let inner = usize::from(card_width.saturating_sub(inset * 2));
    // 中间竖线占用 3 列：" │ "；宽屏平分左右列使竖线居于卡片正中（对齐顶部 logo 与 PP 间隙），
    // 极窄双栏则优先保留右列 38 列防止快捷键描述折行。
    let left_col = if inner >= 79 {
        (inner - 3) / 2
    } else {
        (inner.saturating_sub(3)).saturating_sub(38).max(34)
    };
    let right_col = inner.saturating_sub(3 + left_col);

    let mut lines = Vec::with_capacity(8);
    lines.push(Line::styled(
        "─".repeat(usize::from(card_width)),
        rule_style,
    ));

    let left_title = format!("{}Session & workflow", session_icon(icons));
    let right_title = format!("{}Keyboard", keyboard_icon(icons));
    let left_title_w = Span::raw(&left_title).width();
    let right_title_w = Span::raw(&right_title).width();
    lines.push(Line::from(vec![
        Span::raw(pad.clone()),
        Span::styled(left_title, title_style),
        Span::raw(" ".repeat(left_col.saturating_sub(left_title_w))),
        Span::styled(" │ ", rule_style),
        Span::styled(right_title, title_style),
        Span::raw(" ".repeat(right_col.saturating_sub(right_title_w))),
        Span::raw(pad.clone()),
    ]));

    lines.push(Line::from(vec![
        Span::raw(pad.clone()),
        Span::styled("─".repeat(left_col), rule_style),
        Span::styled(" │ ", rule_style),
        Span::styled("─".repeat(right_col), rule_style),
        Span::raw(pad.clone()),
    ]));

    for (session, keyboard) in SESSION_TIPS.into_iter().zip(KEYBOARD_TIPS) {
        let mut spans = vec![Span::raw(pad.clone())];
        let left_spans = tip_spans(session, 10);
        let left_used = left_spans.iter().map(Span::width).sum::<usize>();
        spans.extend(left_spans);
        spans.push(Span::raw(" ".repeat(left_col.saturating_sub(left_used))));
        spans.push(Span::styled(" │ ", rule_style));
        let right_spans = tip_spans(keyboard, 15);
        let right_used = right_spans.iter().map(Span::width).sum::<usize>();
        spans.extend(right_spans);
        spans.push(Span::raw(" ".repeat(right_col.saturating_sub(right_used))));
        spans.push(Span::raw(pad.clone()));
        lines.push(Line::from(spans));
    }

    lines.push(Line::styled(
        "─".repeat(usize::from(card_width)),
        rule_style,
    ));
    lines
}

fn single_column_card_lines(card_width: u16, inset: u16) -> Vec<Line<'static>> {
    let icons = themes::current_icons();
    let title_style = Style::default().fg(first()).add_modifier(Modifier::BOLD);
    let rule_style = Style::default().fg(separator());
    let pad = " ".repeat(usize::from(inset));
    let inner = usize::from(card_width.saturating_sub(inset * 2));
    let key_width = if inner >= 38 { 15 } else { 14 };

    let mut lines = Vec::new();
    lines.push(Line::styled(
        "─".repeat(usize::from(card_width)),
        rule_style,
    ));
    for (idx, (icon, title, tips)) in [
        (session_icon(icons), "Session & workflow", SESSION_TIPS),
        (keyboard_icon(icons), "Keyboard", KEYBOARD_TIPS),
    ]
    .into_iter()
    .enumerate()
    {
        if idx > 0 {
            lines.push(Line::default());
        }
        lines.push(Line::from(vec![
            Span::raw(pad.clone()),
            Span::styled(format!("{icon}{title}"), title_style),
        ]));
        lines.push(Line::from(vec![
            Span::raw(pad.clone()),
            Span::styled("─".repeat(inner), rule_style),
        ]));
        for tip in tips {
            let mut spans = vec![Span::raw(pad.clone())];
            spans.extend(tip_spans(tip, key_width));
            lines.push(Line::from(spans));
        }
    }
    lines.push(Line::styled(
        "─".repeat(usize::from(card_width)),
        rule_style,
    ));
    lines
}

fn compact_hint_lines(width: u16) -> Vec<Line<'static>> {
    let key_width = if width >= 34 { 15 } else { 14 };
    let mut compact = vec![
        Line::from(tip_spans(("Enter", "send"), key_width)),
        Line::from(tip_spans(("Shift + Enter", "newline"), key_width)),
    ];
    compact.extend(footer_lines(width));
    compact
}

pub(super) fn draw(frame: &mut Frame, area: Rect, ctx: WelcomeContext<'_>) -> Rect {
    if area.width == 0 || area.height == 0 {
        return area;
    }
    let compact = area.width < 40 || area.height < 11;
    let first_color = first();
    let second_color = second();
    let wordmark = wordmark_color();
    let app_glyph = statusline::app_icon(themes::current_icons());
    let mut brand_lines = if compact {
        vec![
            Line::from(vec![
                Span::styled(app_glyph, Style::default().fg(first_color)),
                Span::raw("  "),
                Span::styled("PP", Style::default().fg(wordmark)),
            ]),
            Line::styled(VERSION, Style::default().fg(muted())),
        ]
    } else {
        let right_col_width = [WORDMARK[0], TAGLINE, VERSION]
            .iter()
            .map(|s| s.chars().count())
            .max()
            .unwrap_or(0);
        LOGO.into_iter()
            .enumerate()
            .map(|(row, (left, right))| {
                let (label, style) = if row == LOGO.len() - 1 {
                    (VERSION, Style::default().fg(muted()))
                } else if row == LOGO.len() - 2 {
                    (TAGLINE, Style::default().fg(body_text()))
                } else {
                    (
                        WORDMARK.get(row).copied().unwrap_or(""),
                        Style::default().fg(wordmark).add_modifier(Modifier::BOLD),
                    )
                };
                Line::from(vec![
                    Span::styled(left, Style::default().fg(first_color)),
                    Span::styled(right, Style::default().fg(second_color)),
                    Span::raw(" ".repeat(20 - left.chars().count() - right.chars().count())),
                    Span::styled(label, style),
                    Span::raw(" ".repeat(right_col_width.saturating_sub(label.chars().count()))),
                ])
            })
            .collect::<Vec<_>>()
    };
    brand_lines.push(Line::default());

    let brand_width = area.width;
    let brand = Paragraph::new(brand_lines)
        .centered()
        .wrap(Wrap { trim: false });
    let brand_height = u16::try_from(brand.line_count(brand_width))
        .unwrap_or(u16::MAX)
        .min(area.height);

    let (card_width, inset) = if area.width >= 88 {
        ((area.width * 3 / 4).clamp(84, 120), 2)
    } else if area.width >= 78 {
        (area.width.min(82), 1)
    } else if area.width >= 44 {
        (area.width - 4, 2)
    } else {
        (area.width, 0)
    };
    let tips_width = card_width.saturating_sub(inset * 2);
    let two_col = tips_width >= 75;

    let card_lines = if two_col {
        two_column_card_lines(card_width, inset)
    } else {
        single_column_card_lines(card_width, inset)
    };
    let footer = footer_lines(tips_width);

    let card_widget = Paragraph::new(card_lines)
        .style(Style::default().bg(card_bg()))
        .wrap(Wrap { trim: false });
    let card_line_count = u16::try_from(card_widget.line_count(card_width)).unwrap_or(u16::MAX);
    let footer_widget = Paragraph::new(footer).wrap(Wrap { trim: false });
    let footer_height = u16::try_from(footer_widget.line_count(tips_width)).unwrap_or(u16::MAX);

    let mcp_lines: Vec<Line<'static>> =
        ctx.mcp_tips.iter().map(|line| mcp_tip_line(line)).collect();
    let mcp_widget =
        (!mcp_lines.is_empty()).then(|| Paragraph::new(mcp_lines).wrap(Wrap { trim: false }));
    let mcp_raw_height = mcp_widget.as_ref().map_or(0, |p| {
        u16::try_from(p.line_count(tips_width)).unwrap_or(u16::MAX)
    });

    let available_for_body = area.height.saturating_sub(brand_height);
    let use_full_card = card_line_count + 1 + footer_height <= available_for_body;

    let (body_card_height, body_footer, body_footer_height, footer_gap) = if use_full_card {
        (card_line_count, footer_widget, footer_height, 1_u16)
    } else {
        let compact_para =
            Paragraph::new(compact_hint_lines(tips_width)).wrap(Wrap { trim: false });
        let compact_h = u16::try_from(compact_para.line_count(tips_width))
            .unwrap_or(u16::MAX)
            .min(available_for_body);
        (0, compact_para, compact_h, 0_u16)
    };

    let after_tips = area
        .height
        .saturating_sub(brand_height + body_card_height + footer_gap + body_footer_height);
    let (mcp_gap, mcp_height) = if mcp_raw_height == 0 || after_tips == 0 {
        (0, 0)
    } else if after_tips == 1 {
        (0, 1)
    } else {
        (1, mcp_raw_height.min(after_tips - 1))
    };

    let total_height =
        brand_height + body_card_height + footer_gap + body_footer_height + mcp_gap + mcp_height;
    // 欢迎页上对齐：标准版在确有富余行时保留 2 行顶部留白；紧凑版、矮窗口与
    // 对话区已有正文时贴住首行，避免裁切下方正文。
    let top = if compact || ctx.reserve_trailing {
        0
    } else {
        area.height.saturating_sub(total_height).min(TOP_PADDING)
    };

    let card_x = area.x + (area.width - card_width) / 2;
    let content_x = card_x + inset;
    let mut y = area.y + top;

    frame.render_widget(
        brand,
        Rect::new(
            area.x + (area.width - brand_width) / 2,
            y,
            brand_width,
            brand_height,
        ),
    );
    y += brand_height;

    if use_full_card && body_card_height > 0 {
        let card_rect = Rect::new(card_x, y, card_width, body_card_height);
        frame
            .buffer_mut()
            .set_style(card_rect, Style::default().bg(card_bg()));
        frame.render_widget(card_widget, card_rect);
        y += body_card_height + footer_gap;
    }

    if body_footer_height > 0 {
        frame.render_widget(
            body_footer,
            Rect::new(content_x, y, tips_width, body_footer_height),
        );
        y += body_footer_height;
    }

    if let Some(mcp_para) = mcp_widget
        && mcp_height > 0
    {
        frame.render_widget(
            mcp_para,
            Rect::new(content_x, y + mcp_gap, tips_width, mcp_height),
        );
    }

    // 欢迎内容下方仍可显示重置提示，输入区始终由主布局固定在底部。
    let used = (top + total_height + 1).min(area.height);
    Rect::new(area.x, area.y + used, area.width, area.height - used)
}
