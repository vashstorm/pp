use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};

const FIRST: Color = Color::Rgb(215, 175, 95);
const SECOND: Color = Color::Rgb(122, 162, 247);
const LOGO: [(&str, &str); 5] = [
    ("  ▄▀▀▄", "  ▄▀▀▄"),
    (" ▐▌ ▐▌", " ▐▌ ▐▌"),
    (" ▟▛▀▀ ", " ▐▛▀▀ "),
    ("▐▌    ", " ▐▌   "),
    ("▀▘    ", " ▀▘   "),
];

pub(super) fn draw(frame: &mut Frame, area: Rect) -> Rect {
    if area.width == 0 || area.height == 0 {
        return area;
    }
    let compact = area.width < 28 || area.height < 11;
    let mut lines = if compact {
        vec![Line::from(vec![
            Span::styled(
                "P",
                Style::default()
                    .fg(FIRST)
                    .add_modifier(Modifier::BOLD | Modifier::ITALIC),
            ),
            Span::styled(
                "P",
                Style::default()
                    .fg(SECOND)
                    .add_modifier(Modifier::BOLD | Modifier::ITALIC),
            ),
        ])]
    } else {
        LOGO.into_iter()
            .map(|(first, second)| {
                Line::from(vec![
                    Span::styled(first, Style::default().fg(FIRST)),
                    Span::raw("  "),
                    Span::styled(second, Style::default().fg(SECOND)),
                ])
            })
            .collect::<Vec<_>>()
    };
    lines.push(Line::styled(
        concat!("v", env!("CARGO_PKG_VERSION")),
        Style::default().fg(Color::DarkGray),
    ));
    lines.push(Line::default());
    let width = area.width.min(76);
    let brand = Paragraph::new(lines).centered().wrap(Wrap { trim: false });
    let brand_height = u16::try_from(brand.line_count(width))
        .unwrap_or(u16::MAX)
        .min(area.height);
    let inset = if area.width >= 40 { 2 } else { 0 };
    let tips_width = area.width - inset * 2;
    let tips = Paragraph::new(super::tips::lines(tips_width)).wrap(Wrap { trim: false });
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
            area.x + inset,
            area.y + top + brand_height,
            tips_width,
            tips_height,
        ),
    );

    // 欢迎内容下方仍可显示重置提示，输入区始终由主布局固定在底部。
    let used = (top + height + 1).min(area.height);
    Rect::new(area.x, area.y + used, area.width, area.height - used)
}
