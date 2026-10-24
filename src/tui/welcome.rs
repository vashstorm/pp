use super::statusline::ACCENT;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};

const LOGO: [&str; 5] = [
    "█▀▀█  █▀▀█",
    "█  █  █  █",
    "█▀▀▀  █▀▀▀",
    "█     █   ",
    "▀     ▀   ",
];
pub(super) fn draw(frame: &mut Frame, area: Rect) -> Rect {
    if area.width == 0 || area.height == 0 {
        return area;
    }
    let compact = area.width < 28 || area.height < 11;
    let mut lines = if compact {
        vec![Line::from(Span::styled(
            "pp",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ))]
    } else {
        LOGO.into_iter()
            .map(|text| Line::styled(text, Style::default().fg(ACCENT)))
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
    let tips = Paragraph::new(
        std::iter::once(Line::styled("Tips:", Style::default().fg(ACCENT)))
            .chain(
                crate::tips::TIPS
                    .iter()
                    .map(|&tip| Line::styled(tip, Style::default().fg(Color::DarkGray))),
            )
            .collect::<Vec<_>>(),
    )
    .wrap(Wrap { trim: false });
    let tips_height = u16::try_from(tips.line_count(area.width))
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
        Rect::new(area.x, area.y + top + brand_height, area.width, tips_height),
    );

    // 欢迎内容下方仍可显示重置提示，输入区始终由主布局固定在底部。
    let used = (top + height + 1).min(area.height);
    Rect::new(area.x, area.y + used, area.width, area.height - used)
}
