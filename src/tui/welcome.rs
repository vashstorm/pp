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
const TIP: &str = "/help for commands · /model to choose a model · /effort to set thinking";
const KEYS_TAIL: &str =
    " · Shift-Tab thinking · Ctrl-P model · F1 help · Ctrl-J newline · Ctrl-C quit";

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
    lines.push(Line::from(vec![
        Span::styled("Tip: ", Style::default().fg(ACCENT)),
        Span::styled(TIP, Style::default().fg(Color::DarkGray)),
    ]));
    lines.push(Line::styled(
        format!("{}{KEYS_TAIL}", super::STEERING_HINT),
        Style::default().fg(Color::DarkGray),
    ));

    let width = area.width.min(76);
    let content = Paragraph::new(lines).centered().wrap(Wrap { trim: false });
    let height = u16::try_from(content.line_count(width))
        .unwrap_or(u16::MAX)
        .min(area.height);
    let top = if compact { 0 } else { (area.height / 8).min(2) };
    let top = top.min(area.height.saturating_sub(height));
    frame.render_widget(
        content,
        Rect::new(
            area.x + (area.width - width) / 2,
            area.y + top,
            width,
            height,
        ),
    );

    // 欢迎内容下方仍可显示重置提示，输入区始终由主布局固定在底部。
    let used = (top + height + 1).min(area.height);
    Rect::new(area.x, area.y + used, area.width, area.height - used)
}
