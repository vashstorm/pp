use pp_config::config::IconMode;
use pp_core::model::ThinkLevel;
use ratatui::style::Color;

use super::themes;

pub(super) fn accent() -> Color {
    themes::palette().accent
}
pub(super) fn text() -> Color {
    themes::palette().text
}
pub(super) fn activity() -> Color {
    themes::palette().activity
}
pub(super) fn muted() -> Color {
    themes::palette().muted
}
pub(super) fn dim() -> Color {
    themes::palette().dim
}
pub(super) fn code() -> Color {
    themes::palette().code
}
pub(super) fn code_block() -> Color {
    themes::palette().code_block
}
pub(super) fn link() -> Color {
    themes::palette().link
}
pub(super) fn added() -> Color {
    themes::palette().added
}
pub(super) fn md_bg() -> Color {
    themes::palette().md_bg
}
pub(super) fn md_text() -> Color {
    themes::palette().md_text
}
pub(super) fn md_heading() -> Color {
    themes::palette().md_heading
}
pub(super) fn md_code() -> Color {
    themes::palette().md_code
}
pub(super) fn md_strong() -> Color {
    themes::palette().md_strong
}
pub(super) fn md_bullet() -> Color {
    themes::palette().md_bullet
}
pub(super) fn md_border() -> Color {
    themes::palette().md_border
}
pub(super) fn todo_active() -> Color {
    themes::palette().todo_active
}
pub(super) fn error_border() -> Color {
    themes::palette().error_border
}
pub(super) fn removed() -> Color {
    themes::palette().removed
}
pub(super) fn tool_bg() -> Color {
    themes::palette().tool_bg
}
pub(super) fn tool_border() -> Color {
    themes::palette().tool_border
}
#[cfg(test)]
pub(super) fn search_title() -> Color {
    themes::palette().search_title
}
#[cfg(test)]
pub(super) fn shell_title() -> Color {
    themes::palette().shell_title
}
pub(super) fn bash_title() -> Color {
    themes::palette().bash_title
}
pub(super) fn mcp_title() -> Color {
    themes::palette().mcp_title
}
pub(super) fn mcp_server() -> Color {
    themes::palette().mcp_server
}
pub(super) fn mcp_tool() -> Color {
    themes::palette().mcp_tool
}
pub(super) fn tool_pending_bg() -> Color {
    themes::palette().tool_pending_bg
}
pub(super) fn tool_success_bg() -> Color {
    themes::palette().tool_success_bg
}
pub(super) fn tool_error_bg() -> Color {
    themes::palette().tool_error_bg
}
pub(super) fn ask_bg() -> Color {
    themes::palette().ask_bg
}
pub(super) fn ask_border() -> Color {
    themes::palette().ask_border
}
pub(super) fn ask_tab_bg() -> Color {
    themes::palette().ask_tab_bg
}
pub(super) fn read_bg() -> Color {
    themes::palette().tool_bg
}
pub(super) fn read_text() -> Color {
    themes::palette().read_text
}
pub(super) fn read_summary_title() -> Color {
    themes::palette().read_title
}
pub(super) fn read_summary_path() -> Color {
    themes::palette().accent
}
pub(super) fn read_summary_meta() -> Color {
    themes::palette().read_summary_meta
}
pub(super) fn rg_bg() -> Color {
    themes::palette().tool_bg
}
pub(super) fn rg_border() -> Color {
    themes::palette().tool_border
}
pub(super) fn rg_title() -> Color {
    themes::palette().rg_title
}
pub(super) fn rg_text() -> Color {
    themes::palette().rg_text
}
pub(super) fn rg_meta() -> Color {
    themes::palette().rg_meta
}
pub(super) fn tool_command() -> Color {
    themes::palette().tool_command
}
pub(super) fn edit_bg() -> Color {
    themes::palette().tool_bg
}
pub(super) fn edit_border() -> Color {
    themes::palette().tool_border
}
#[cfg(test)]
pub(super) fn edit_title() -> Color {
    themes::palette().edit_title
}
pub(super) fn edit_text() -> Color {
    themes::palette().edit_text
}
pub(super) fn edit_meta() -> Color {
    themes::palette().edit_meta
}
pub(super) fn edit_added() -> Color {
    themes::palette().edit_added
}
pub(super) fn edit_removed() -> Color {
    themes::palette().edit_removed
}
pub(super) fn edit_keyword() -> Color {
    themes::palette().edit_keyword
}
pub(super) fn edit_type() -> Color {
    themes::palette().edit_type
}
pub(super) fn edit_string() -> Color {
    themes::palette().edit_string
}
pub(super) fn edit_number() -> Color {
    themes::palette().edit_number
}
pub(super) fn syntax_keyword() -> Color {
    themes::palette().syntax_keyword
}
pub(super) fn syntax_comment() -> Color {
    themes::palette().syntax_comment
}
pub(super) fn syntax_function() -> Color {
    themes::palette().syntax_function
}

#[cfg(test)]
pub(super) const WAITING_CLOCK_FRAMES: [char; 12] = [
    '\u{f1456}',
    '\u{f144b}',
    '\u{f144c}',
    '\u{f144d}',
    '\u{f144e}',
    '\u{f144f}',
    '\u{f1450}',
    '\u{f1451}',
    '\u{f1452}',
    '\u{f1453}',
    '\u{f1454}',
    '\u{f1455}',
];

pub(super) fn waiting_clock(milliseconds: u128) -> &'static str {
    const NERD_FRAMES: [&str; 12] = [
        "\u{f1456}",
        "\u{f144b}",
        "\u{f144c}",
        "\u{f144d}",
        "\u{f144e}",
        "\u{f144f}",
        "\u{f1450}",
        "\u{f1451}",
        "\u{f1452}",
        "\u{f1453}",
        "\u{f1454}",
        "\u{f1455}",
    ];
    const ASCII_FRAMES: [&str; 4] = ["|", "/", "-", "\\"];
    if themes::is_ascii() {
        ASCII_FRAMES[(milliseconds / 120 % ASCII_FRAMES.len() as u128) as usize]
    } else {
        NERD_FRAMES[(milliseconds / 120 % NERD_FRAMES.len() as u128) as usize]
    }
}

pub(super) fn tool_accent(name: &str) -> Color {
    let p = themes::palette();
    match name {
        "read" => p.read_title,
        "rg" | "plan_search" => p.search_title,
        "find" | "glob" | "plan_find" => p.glob_title,
        "edit" => p.edit_title,
        "write" => p.write_title,
        "bash" | "bash_task" | "eval" => p.shell_title,
        "ask" => p.ask_title,
        "todo" | "plan_submit" => p.todo_title,
        name if name.starts_with("mcp__") => p.mcp_title,
        _ => p.activity,
    }
}

// 与底部状态栏一致，默认使用 Nerd Font 私用区单列字形；ASCII 模式切换为可打印 ASCII 字符。
// rg 取 Font Awesome 放大镜（nf-fa-search，U+F002），其余为 Material Design 字形。
pub(super) fn tool_icon_for(name: &str, icons: IconMode) -> &'static str {
    match icons {
        IconMode::Nerd => match name {
            "read" => "\u{f0219}",
            "edit" => "\u{f03eb}",
            "write" => "\u{f0193}",
            "bash_task" => "\u{f08d6}",
            "eval" => "\u{f0320}",
            "rg" => "\u{f002}",
            "find" | "glob" => "\u{f024b}",
            "ask" => "\u{f02d7}",
            "plan_search" => "\u{f0232}",
            "plan_find" => "\u{f018b}",
            "plan_submit" => "\u{f012c}",
            "todo" => "\u{f0756}",
            name if name.starts_with("mcp__") => "\u{f06a5}",
            _ => "\u{f1064}",
        },
        IconMode::Ascii => match name {
            "read" => "R",
            "edit" => "E",
            "write" => "W",
            "bash_task" => "&",
            "eval" => "P",
            "rg" => "/",
            "find" | "glob" => "*",
            "ask" => "?",
            "plan_search" => "/",
            "plan_find" => "*",
            "plan_submit" => "P",
            "todo" => "#",
            name if name.starts_with("mcp__") => "@",
            _ => "$",
        },
    }
}

pub(super) fn tool_icon(name: &str) -> &'static str {
    tool_icon_for(name, themes::current_icons())
}

pub(super) fn done_icon(icons: IconMode) -> &'static str {
    match icons {
        IconMode::Nerd => "\u{f00c}",
        IconMode::Ascii => "+",
    }
}

pub(super) fn fail_icon(icons: IconMode) -> &'static str {
    match icons {
        IconMode::Nerd => "\u{f00d}",
        IconMode::Ascii => "x",
    }
}

pub(super) fn file_icon(path: &str, icons: IconMode) -> &'static str {
    match icons {
        IconMode::Nerd => {
            if path.ends_with(".rs") {
                "\u{f1617} "
            } else {
                "\u{f022e} "
            }
        }
        IconMode::Ascii => "- ",
    }
}

/// `mcp__<server>__<tool>` 拆出服务名与工具名；两段都非空才算合法的 MCP 名称。
pub(super) fn mcp_parts(name: &str) -> Option<(&str, &str)> {
    let (server, tool) = name.strip_prefix("mcp__")?.split_once("__")?;
    (!server.is_empty() && !tool.is_empty()).then_some((server, tool))
}

/// MCP 卡片标题文本：`mcp__server__tool` → `mcp server tool`；非 MCP 名称返回 `None`。
pub(super) fn mcp_label(name: &str) -> Option<String> {
    let (server, tool) = mcp_parts(name)?;
    Some(format!(
        "mcp {} {}",
        pp_core::text::display_text(server),
        pp_core::text::display_text(tool)
    ))
}

// 思考级别统一使用 Nerd Font 图标，状态栏与角色面板共用同一组字形；ASCII 模式使用阶梯字符。
pub(super) fn think_icon_for(level: ThinkLevel, icons: IconMode) -> &'static str {
    match icons {
        IconMode::Nerd => match level {
            ThinkLevel::None => "\u{f0766}",
            ThinkLevel::Low => "\u{f0a9f}",
            ThinkLevel::Medium => "\u{f0aa1}",
            ThinkLevel::High => "\u{f0aa3}",
            ThinkLevel::Xhigh => "\u{f0aa5}",
            ThinkLevel::Max => "\u{f06d}",
        },
        IconMode::Ascii => match level {
            ThinkLevel::None => ".",
            ThinkLevel::Low => "1",
            ThinkLevel::Medium => "2",
            ThinkLevel::High => "3",
            ThinkLevel::Xhigh => "4",
            ThinkLevel::Max => "!",
        },
    }
}

pub(super) fn think_icon(level: ThinkLevel) -> &'static str {
    think_icon_for(level, themes::current_icons())
}
