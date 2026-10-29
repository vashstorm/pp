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
pub(super) const ASK_TAB_BG: Color = Color::Rgb(60, 63, 75);
pub(super) const READ_BG: Color = Color::Rgb(22, 26, 31);
pub(super) const READ_BORDER: Color = Color::Rgb(76, 85, 98);
pub(super) const READ_TEXT: Color = Color::Rgb(190, 190, 185);
pub(super) const READ_ACCENT: Color = Color::Rgb(204, 153, 51);
pub(super) const READ_SUMMARY_BG: Color = Color::Rgb(40, 42, 53);
pub(super) const READ_SUMMARY_TITLE: Color = Color::Rgb(248, 248, 243);
pub(super) const READ_SUMMARY_PATH: Color = Color::Rgb(244, 191, 84);
pub(super) const READ_SUMMARY_META: Color = Color::Rgb(96, 102, 114);
pub(super) const GREP_BG: Color = Color::Rgb(40, 42, 53);
pub(super) const GREP_TITLE: Color = Color::Rgb(212, 212, 212);
pub(super) const GREP_TEXT: Color = Color::Rgb(120, 125, 135);
pub(super) const GREP_META: Color = Color::Rgb(96, 102, 114);
pub(super) const EDIT_BG: Color = Color::Rgb(23, 26, 31);
pub(super) const EDIT_BORDER: Color = Color::Rgb(62, 66, 73);
pub(super) const EDIT_TITLE: Color = Color::Rgb(244, 191, 84);
pub(super) const EDIT_TEXT: Color = Color::Rgb(248, 248, 243);
pub(super) const EDIT_META: Color = Color::Rgb(120, 125, 135);
pub(super) const EDIT_ADDED: Color = Color::Rgb(153, 208, 137);
pub(super) const EDIT_REMOVED: Color = Color::Rgb(232, 76, 82);
pub(super) const EDIT_KEYWORD: Color = Color::Rgb(103, 154, 209);
pub(super) const EDIT_TYPE: Color = Color::Rgb(170, 218, 250);
pub(super) const EDIT_STRING: Color = Color::Rgb(197, 148, 124);
pub(super) const EDIT_NUMBER: Color = Color::Rgb(220, 220, 175);

// 统一使用 Nerd Font Font Awesome 单列字形；加一列空白为图标预留两列。
pub(super) fn tool_icon(name: &str) -> &'static str {
    match name {
        "read" => "\u{f15c}",
        "edit" => "\u{f040}",
        "write" => "\u{f0c7}",
        "bash_task" => "\u{f085}",
        "grep" => "\u{f002}",
        "find" | "glob" => "\u{f07b}",
        "ask" => "\u{f059}",
        "plan_search" => "\u{f0b0}",
        "plan_find" => "\u{f14e}",
        "plan_write" => "\u{f0cb}",
        "plan_submit" => "\u{f00c}",
        "todo" => "\u{f0ae}",
        name if name.starts_with("mcp__") => "\u{f1e6}",
        _ => "\u{f12e}",
    }
}

pub(crate) fn display_text(text: &str) -> Cow<'_, str> {
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
