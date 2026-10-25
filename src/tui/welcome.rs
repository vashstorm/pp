use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};

const FIRST: Color = Color::Rgb(215, 175, 95);
const SECOND: Color = Color::Rgb(122, 162, 247);
const SEPARATOR: Color = Color::Rgb(62, 68, 80);
const MUTED: Color = Color::Rgb(110, 117, 129);
const SESSION_TIPS: [(&str, &str); 4] = [
    ("/model", "switch model, keep chat"),
    ("/new", "clear chat, keep model"),
    ("/resume", "reopen a saved session"),
    ("/plan", "plan, then approve edits"),
];
const KEYBOARD_TIPS: [(&str, &str); 4] = [
    ("Enter", "send a message"),
    ("Ctrl + J", "insert a newline"),
    ("Esc", "cancel the running task"),
    ("Shift + Tab", "cycle thinking levels"),
];
const WORDMARK_COLOR: Color = Color::Rgb(235, 235, 235);
const WORDMARK: [&str; 5] = [
    "██████    ██████  ",
    "██    ██  ██    ██",
    "██████    ██████  ",
    "██        ██      ",
    "██        ██      ",
];
const VERSION: &str = concat!("v", env!("CARGO_PKG_VERSION"));
const LOGO: [(&str, &str); 7] = [
    ("              ▄▄", "▄█"),
    ("        ▄▄▄██▀▀", "██"),
    ("  ▄▄▄██████▀ ", "▄███"),
    (" ▀▀█████▀  ", "▄████"),
    ("     ▀█  ", "▄█████▀"),
    ("      █▀ ", "▀█████"),
    ("            ", "▀█▀"),
];

fn tip_spans((key, description): (&str, &str), width: u16) -> Vec<Span<'static>> {
    vec![
        Span::styled(
            if width >= 34 {
                format!("{key:<12}")
            } else {
                format!("{key}  ")
            },
            Style::default().fg(SECOND).add_modifier(Modifier::BOLD),
        ),
        Span::styled(description.to_owned(), Style::default().fg(Color::Gray)),
    ]
}

fn help_lines(width: u16) -> Vec<Line<'static>> {
    let hints = if width >= 44 {
        vec!["/tips  All shortcuts   ·   /help  Commands"]
    } else {
        vec!["/tips  All shortcuts", "/help  Commands"]
    };
    hints
        .into_iter()
        .map(|hint| Line::styled(hint, Style::default().fg(MUTED)))
        .collect()
}

fn hint_lines(width: u16, height: u16) -> Vec<Line<'static>> {
    let title_style = Style::default().fg(FIRST);
    let rule_style = Style::default().fg(SEPARATOR);
    let mut lines = Vec::new();
    if width >= 76 {
        let column_width = usize::from((width - 4) / 2);
        lines.push(Line::from(vec![
            Span::styled(
                format!("{:<column_width$}    ", "Session & workflow"),
                title_style,
            ),
            Span::styled("Keyboard", title_style),
        ]));
        lines.push(Line::styled(
            format!(
                "{}    {}",
                "─".repeat(column_width),
                "─".repeat(column_width)
            ),
            rule_style,
        ));
        for (session, keyboard) in SESSION_TIPS.into_iter().zip(KEYBOARD_TIPS) {
            let mut spans = tip_spans(session, column_width as u16);
            let used = spans.iter().map(Span::width).sum::<usize>();
            spans.push(Span::raw(" ".repeat(column_width - used + 4)));
            spans.extend(tip_spans(keyboard, column_width as u16));
            lines.push(Line::from(spans));
        }
    } else {
        for (title, tips) in [
            ("Session & workflow", SESSION_TIPS),
            ("Keyboard", KEYBOARD_TIPS),
        ] {
            if !lines.is_empty() {
                lines.push(Line::default());
            }
            lines.push(Line::styled(title, title_style));
            lines.push(Line::styled("─".repeat(usize::from(width)), rule_style));
            lines.extend(
                tips.into_iter()
                    .map(|tip| Line::from(tip_spans(tip, width))),
            );
        }
    }
    lines.push(Line::default());
    lines.extend(help_lines(width));
    if Paragraph::new(lines.clone())
        .wrap(Wrap { trim: false })
        .line_count(width)
        <= usize::from(height)
    {
        return lines;
    }

    // 小窗口优先保留输入操作和帮助入口，不让装饰挤占输入区。
    let mut compact = vec![
        Line::from(tip_spans(("Enter", "send"), width)),
        Line::from(tip_spans(("Ctrl + J", "newline"), width)),
    ];
    compact.extend(help_lines(width));
    compact
}

pub(super) fn draw(frame: &mut Frame, area: Rect) -> Rect {
    if area.width == 0 || area.height == 0 {
        return area;
    }
    let compact = area.width < 40 || area.height < 11;
    let mut lines = if compact {
        vec![
            Line::from(vec![
                Span::styled("", Style::default().fg(FIRST)),
                Span::raw("  "),
                Span::styled("PP", Style::default().fg(WORDMARK_COLOR)),
            ]),
            Line::styled(VERSION, Style::default().fg(Color::DarkGray)),
        ]
    } else {
        LOGO.into_iter()
            .enumerate()
            .map(|(row, (first, second))| {
                let (label, color) = if row == LOGO.len() - 1 {
                    (VERSION, Color::DarkGray)
                } else {
                    (WORDMARK.get(row).copied().unwrap_or(""), WORDMARK_COLOR)
                };
                Line::from(vec![
                    Span::styled(first, Style::default().fg(FIRST)),
                    Span::styled(second, Style::default().fg(SECOND)),
                    Span::raw(" ".repeat(22 - first.chars().count() - second.chars().count())),
                    Span::styled(label, Style::default().fg(color)),
                    Span::raw(" ".repeat(18_usize.saturating_sub(label.chars().count()))),
                ])
            })
            .collect::<Vec<_>>()
    };
    lines.push(Line::default());
    let width = area.width.min(76);
    let brand = Paragraph::new(lines).centered().wrap(Wrap { trim: false });
    let brand_height = u16::try_from(brand.line_count(width))
        .unwrap_or(u16::MAX)
        .min(area.height);
    let content_width = area.width.min(80);
    let inset = if content_width >= 8 { 2 } else { 0 };
    let tips_width = content_width - inset * 2;
    let tips = Paragraph::new(hint_lines(
        tips_width,
        area.height.saturating_sub(brand_height),
    ))
    .wrap(Wrap { trim: false });
    let tips_height = u16::try_from(tips.line_count(tips_width))
        .unwrap_or(u16::MAX)
        .min(area.height.saturating_sub(brand_height));
    let height = brand_height + tips_height;
    let top = if compact { 0 } else { (area.height / 8).min(2) };
    let top = top.min(area.height.saturating_sub(height));
    frame.render_widget(
        brand,
        Rect::new(
            area.x + (area.width - width) / 2,
            area.y + top,
            width,
            brand_height,
        ),
    );
    frame.render_widget(
        tips,
        Rect::new(
            area.x + (area.width - content_width) / 2 + inset,
            area.y + top + brand_height,
            tips_width,
            tips_height,
        ),
    );

    // 欢迎内容下方仍可显示重置提示，输入区始终由主布局固定在底部。
    let used = (top + height + 1).min(area.height);
    Rect::new(area.x, area.y + used, area.width, area.height - used)
}
