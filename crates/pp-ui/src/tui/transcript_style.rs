use pp_core::model::ThinkLevel;
use ratatui::style::Color;

pub(super) const ACCENT: Color = Color::Rgb(254, 188, 56);
pub(super) const TEXT: Color = Color::Rgb(250, 250, 250);
pub(super) const ACTIVITY: Color = Color::Rgb(190, 198, 212);
pub(super) const MUTED: Color = Color::Rgb(119, 125, 136);
pub(super) const DIM: Color = Color::Rgb(95, 102, 115);
pub(super) const CODE: Color = Color::Rgb(229, 193, 255);
pub(super) const CODE_BLOCK: Color = Color::Rgb(156, 220, 254);
pub(super) const LINK: Color = Color::Rgb(0, 136, 250);
pub(super) const ADDED: Color = Color::Rgb(137, 210, 129);
pub(super) const TODO_ACTIVE: Color = Color::Rgb(255, 230, 109);
pub(super) const ERROR_BORDER: Color = Color::Rgb(224, 108, 117);
pub(super) const REMOVED: Color = Color::Rgb(252, 58, 75);
// 工具共享中性底色与边框；主题色只用于名称、图标等少量标识。
pub(super) const TOOL_BG: Color = Color::Rgb(22, 26, 31);
pub(super) const TOOL_BORDER: Color = Color::Rgb(65, 72, 83);
pub(super) const READ_TITLE: Color = Color::Rgb(145, 183, 218);
pub(super) const SEARCH_TITLE: Color = Color::Rgb(131, 184, 172);
pub(super) const GLOB_TITLE: Color = Color::Rgb(156, 185, 201);
pub(super) const WRITE_TITLE: Color = Color::Rgb(165, 189, 143);
pub(super) const SHELL_TITLE: Color = Color::Rgb(197, 187, 139);
pub(super) const BASH_TITLE: Color = Color::Rgb(244, 191, 84);
pub(super) const SKILL_TITLE: Color = Color::Rgb(182, 160, 210);
pub(super) const MCP_TITLE: Color = Color::Rgb(194, 158, 175);
// MCP 卡片标题分段配色：`mcp` 前缀、服务名与工具名各用独立色相，避免整串名称连写。
pub(super) const MCP_SERVER: Color = Color::Rgb(160, 196, 224);
pub(super) const MCP_TOOL: Color = Color::Rgb(156, 220, 254);
pub(super) const TOOL_PENDING_BG: Color = Color::Rgb(29, 33, 41);
pub(super) const TOOL_SUCCESS_BG: Color = Color::Rgb(22, 26, 31);
pub(super) const TOOL_ERROR_BG: Color = Color::Rgb(41, 29, 29);
pub(super) const ASK_BG: Color = Color::Rgb(40, 42, 54);
pub(super) const ASK_BORDER: Color = Color::Rgb(39, 135, 165);
pub(super) const ASK_TAB_BG: Color = Color::Rgb(60, 63, 75);
pub(super) const READ_BG: Color = TOOL_BG;
pub(super) const READ_BORDER: Color = TOOL_BORDER;
pub(super) const READ_TEXT: Color = Color::Rgb(190, 190, 185);
pub(super) const READ_ACCENT: Color = Color::Rgb(204, 153, 51);
pub(super) const READ_SUMMARY_BG: Color = TOOL_BG;
pub(super) const READ_SUMMARY_TITLE: Color = READ_TITLE;
pub(super) const READ_SUMMARY_PATH: Color = ACCENT;
pub(super) const READ_SUMMARY_META: Color = Color::Rgb(96, 102, 114);
pub(super) const RG_BG: Color = TOOL_BG;
pub(super) const RG_BORDER: Color = TOOL_BORDER;
pub(super) const RG_TITLE: Color = Color::Rgb(156, 220, 254);
pub(super) const RG_TEXT: Color = Color::Rgb(190, 198, 212);
pub(super) const RG_META: Color = Color::Rgb(119, 125, 136);
pub(super) const TOOL_COMMAND: Color = Color::Rgb(220, 226, 235);
pub(super) const EDIT_BG: Color = TOOL_BG;
pub(super) const EDIT_BORDER: Color = TOOL_BORDER;
pub(super) const EDIT_TITLE: Color = Color::Rgb(244, 191, 84);
pub(super) const EDIT_TEXT: Color = Color::Rgb(248, 248, 243);
pub(super) const EDIT_META: Color = Color::Rgb(120, 125, 135);
pub(super) const EDIT_ADDED: Color = Color::Rgb(153, 208, 137);
pub(super) const EDIT_REMOVED: Color = Color::Rgb(232, 76, 82);
pub(super) const EDIT_KEYWORD: Color = Color::Rgb(103, 154, 209);
pub(super) const EDIT_TYPE: Color = Color::Rgb(170, 218, 250);
pub(super) const EDIT_STRING: Color = Color::Rgb(197, 148, 124);
pub(super) const EDIT_NUMBER: Color = Color::Rgb(220, 220, 175);
// Markdown 代码块 syntect 语义高亮配色（VS Code Dark+ 风格）。
pub(super) const SYNTAX_KEYWORD: Color = Color::Rgb(197, 134, 192);
pub(super) const SYNTAX_COMMENT: Color = Color::Rgb(106, 153, 85);
pub(super) const SYNTAX_FUNCTION: Color = Color::Rgb(220, 220, 170);
pub(super) const SYNTAX_CONSTANT: Color = Color::Rgb(79, 193, 255);
pub(super) const SYNTAX_TAG: Color = Color::Rgb(86, 156, 214);

pub(super) fn waiting_clock(milliseconds: u128) -> &'static str {
    const FRAMES: [&str; 4] = ["🕛", "🕒", "🕕", "🕘"];
    FRAMES[(milliseconds / 500 % FRAMES.len() as u128) as usize]
}

pub(super) fn tool_accent(name: &str) -> Color {
    match name {
        "read" => READ_TITLE,
        "rg" | "plan_search" => SEARCH_TITLE,
        "find" | "glob" | "plan_find" => GLOB_TITLE,
        "edit" => EDIT_TITLE,
        "write" => WRITE_TITLE,
        "bash" | "bash_task" => SHELL_TITLE,
        "ask" => Color::Rgb(140, 191, 195),
        "todo" | "plan_submit" => Color::Rgb(168, 173, 208),
        name if name.starts_with("mcp__") => MCP_TITLE,
        _ => ACTIVITY,
    }
}

// 与底部状态栏一致，使用 Nerd Font Material Design 双列字形；图标后保留一列空白。
pub(super) fn tool_icon(name: &str) -> &'static str {
    match name {
        "read" => "\u{f0219}",
        "edit" => "\u{f03eb}",
        "write" => "\u{f0193}",
        "bash_task" => "\u{f08d6}",
        "rg" => "\u{f0349}",
        "find" | "glob" => "\u{f024b}",
        "ask" => "\u{f02d7}",
        "plan_search" => "\u{f0232}",
        "plan_find" => "\u{f018b}",
        "plan_submit" => "\u{f012c}",
        "todo" => "\u{f0756}",
        name if name.starts_with("mcp__") => "\u{f06a5}",
        _ => "\u{f1064}",
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

// 思考级别统一使用 Nerd Font 图标，状态栏与角色面板共用同一组字形。
// none 到 xhigh 为 Material Design 圆环阶梯；max 改用 Font Awesome 火苗（U+F06D 的
// nf-fa-fire，单列实心火焰，与描边圆环阶梯形成最大对比）。
pub(super) fn think_icon(level: ThinkLevel) -> &'static str {
    match level {
        ThinkLevel::None => "\u{f0766}",
        ThinkLevel::Low => "\u{f0a9f}",
        ThinkLevel::Medium => "\u{f0aa1}",
        ThinkLevel::High => "\u{f0aa3}",
        ThinkLevel::Xhigh => "\u{f0aa5}",
        ThinkLevel::Max => "\u{f06d}",
    }
}
