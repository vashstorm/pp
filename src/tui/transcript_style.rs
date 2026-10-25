use ratatui::style::Color;
use std::borrow::Cow;

pub(super) const ACCENT: Color = Color::Rgb(254, 188, 56);
pub(super) const ACTIVITY: Color = Color::Rgb(190, 198, 212);
pub(super) const MUTED: Color = Color::Rgb(119, 125, 136);
pub(super) const DIM: Color = Color::Rgb(95, 102, 115);
pub(super) const CODE: Color = Color::Rgb(229, 193, 255);
pub(super) const CODE_BLOCK: Color = Color::Rgb(156, 220, 254);
pub(super) const LINK: Color = Color::Rgb(0, 136, 250);
pub(super) const ADDED: Color = Color::Rgb(137, 210, 129);
pub(super) const REMOVED: Color = Color::Rgb(252, 58, 75);
pub(super) const TOOL_PENDING_BG: Color = Color::Rgb(29, 33, 41);
pub(super) const TOOL_SUCCESS_BG: Color = Color::Rgb(22, 26, 31);
pub(super) const TOOL_ERROR_BG: Color = Color::Rgb(41, 29, 29);
pub(super) const ASK_BG: Color = Color::Rgb(40, 42, 54);
pub(super) const ASK_BORDER: Color = Color::Rgb(39, 135, 165);

// 统一使用 Nerd Font Font Awesome 单列字形；加一列空白为图标预留两列。
pub(super) fn tool_icon(name: &str) -> &'static str {
    match name {
        "read" => "\u{f15c}",
        "edit" => "\u{f040}",
        "write" => "\u{f0c7}",
        "bash_task" => "\u{f085}",
        "grep" => "\u{f002}",
        "find" => "\u{f07b}",
        "load_skill" => "\u{f02d}",
        "ask" => "\u{f059}",
        "plan_search" => "\u{f0b0}",
        "plan_find" => "\u{f14e}",
        "plan_write" => "\u{f0cb}",
        "plan_submit" => "\u{f00c}",
        name if name.starts_with("mcp__") => "\u{f1e6}",
        _ => "\u{f12e}",
    }
}

pub(super) fn display_text(text: &str) -> Cow<'_, str> {
    fn unsafe_control(c: char) -> bool {
        (c <= '\u{1f}' && c != '\n' && c != '\t') || c == '\u{7f}'
    }
    if !text.chars().any(unsafe_control) {
        return Cow::Borrowed(text);
    }
    Cow::Owned(
        text.chars()
            .map(|c| if unsafe_control(c) { '\u{fffd}' } else { c })
            .collect(),
    )
}
