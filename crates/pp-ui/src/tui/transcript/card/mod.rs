use super::*;
use pp_config::config::ToolStyle;

pub(in crate::tui) mod anatomy;
mod badge;
mod classic;
pub(in crate::tui) mod glyphs;
mod hanging;
mod ledger;
mod rail;
mod slab;
mod styled;
pub(in crate::tui) mod tone;

pub(super) use anatomy::ReadItem;

#[derive(Clone, Copy)]
pub(in crate::tui) enum CardSurface {
    Bash,
    BashTask,
    Eval,
    Edit,
    Write,
    Rg,
    Glob,
    WebSearch,
}

impl CardSurface {
    pub(in crate::tui) fn from_search_tool(name: &str) -> Self {
        match name {
            "web_search" => Self::WebSearch,
            "glob" | "find" | "plan_find" => Self::Glob,
            _ => Self::Rg,
        }
    }

    pub(in crate::tui) fn from_file_tool(name: &str) -> Self {
        if name == "write" {
            Self::Write
        } else {
            Self::Edit
        }
    }

    pub(super) fn tool_name(self) -> &'static str {
        match self {
            Self::Bash => "bash",
            Self::BashTask => "bash_task",
            Self::Eval => "eval",
            Self::Edit => "edit",
            Self::Write => "write",
            Self::Rg => "rg",
            Self::Glob => "glob",
            Self::WebSearch => "web_search",
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct RowCtx {
    pub index: usize,
    pub total: usize,
    pub is_first_body: bool,
}

pub(super) fn current_style() -> ToolStyle {
    crate::tui::themes::current_tool_style()
}

pub(super) fn is_classic(style: ToolStyle) -> bool {
    matches!(style, ToolStyle::Classic | ToolStyle::Classic2)
}

pub(super) fn is_tool_card(entry: &TranscriptEntry) -> bool {
    entry.tool.is_some() || entry.kind == EntryKind::Lsp
}

pub(super) fn entry_padding(entry: &TranscriptEntry, compact: bool, width: u16) -> u16 {
    if compact {
        return 0;
    }
    let style = current_style();
    if !is_tool_card(entry) || is_classic(style) {
        return classic::padding(entry);
    }
    styled::metrics(style, width).padding()
}

pub(super) fn build_non_classic_layout(
    entry: &mut TranscriptEntry,
    width: u16,
) -> Option<Vec<LayoutLine>> {
    let style = current_style();
    if !is_tool_card(entry) || is_classic(style) {
        return None;
    }
    Some(styled::build_layout(style, entry, width))
}

pub(in crate::tui) fn surface_bg(surface: CardSurface) -> Option<Color> {
    match current_style() {
        style if is_classic(style) => Some(classic::surface_bg(surface, style)),
        ToolStyle::Slab => Some(tone::slab_body_bg(crate::tui::themes::palette())),
        _ => None,
    }
}

pub(in crate::tui) fn surface_style(surface: CardSurface, fg: Color) -> Style {
    let style = Style::default().fg(fg);
    match surface_bg(surface) {
        Some(bg) => style.bg(bg),
        None => style,
    }
}

pub(in crate::tui) fn contrast_bg_fg(highlight_bg: Color) -> Color {
    match current_style() {
        ToolStyle::Classic | ToolStyle::Classic2 => palette::edit_bg(),
        ToolStyle::Slab => tone::slab_body_bg(crate::tui::themes::palette()),
        ToolStyle::Rail | ToolStyle::Badge | ToolStyle::Hanging | ToolStyle::Ledger => {
            tone::contrast_fg(
                highlight_bg,
                &[
                    palette::md_bg(),
                    Color::Rgb(16, 18, 24),
                    Color::Rgb(250, 251, 254),
                ],
            )
        }
    }
}

pub(super) fn entry_background(entry: &TranscriptEntry) -> Option<Color> {
    let style = current_style();
    if !is_tool_card(entry) || is_classic(style) {
        return classic::background(entry, style);
    }
    match style {
        ToolStyle::Classic | ToolStyle::Classic2 => classic::background(entry, style),
        ToolStyle::Slab => Some(tone::slab_body_bg(crate::tui::themes::palette())),
        ToolStyle::Rail | ToolStyle::Badge | ToolStyle::Hanging | ToolStyle::Ledger => None,
    }
}

pub(super) fn paint_chrome(
    entry: &TranscriptEntry,
    line: &LayoutLine,
    ctx: RowCtx,
    row: u16,
    area: Rect,
    buffer: &mut Buffer,
) {
    let style = current_style();
    if !is_tool_card(entry) || is_classic(style) {
        classic::paint_chrome(entry, line, row, area, buffer, style);
        return;
    }
    styled::paint_chrome(style, entry, line, ctx, row, area, buffer);
}

pub(super) fn text_target(
    entry: &TranscriptEntry,
    line: &LayoutLine,
    inset: u16,
    area_width: u16,
    before_divider: bool,
) -> Option<(u16, u16)> {
    let style = current_style();
    if !is_tool_card(entry) || is_classic(style) {
        return classic::text_target(entry, line, inset, area_width, before_divider, style);
    }
    styled::text_target(style, line, area_width)
}
