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
    Eval,
    Edit,
    Search,
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

pub(super) fn is_tool_card(entry: &TranscriptEntry) -> bool {
    entry.tool.is_some() || entry.kind == EntryKind::Lsp
}

pub(super) fn entry_padding(entry: &TranscriptEntry, compact: bool, width: u16) -> u16 {
    if compact {
        return 0;
    }
    let style = current_style();
    if !is_tool_card(entry) || style == ToolStyle::Classic {
        return classic::padding(entry);
    }
    styled::metrics(style, width).padding()
}

pub(super) fn build_non_classic_layout(
    entry: &mut TranscriptEntry,
    width: u16,
) -> Option<Vec<LayoutLine>> {
    let style = current_style();
    if !is_tool_card(entry) || style == ToolStyle::Classic {
        return None;
    }
    Some(styled::build_layout(style, entry, width))
}

pub(in crate::tui) fn surface_bg(surface: CardSurface) -> Option<Color> {
    match current_style() {
        ToolStyle::Classic => Some(classic::surface_bg(surface)),
        ToolStyle::Slab => Some(tone::slab_body_bg(crate::tui::themes::palette())),
        ToolStyle::Rail | ToolStyle::Badge | ToolStyle::Hanging | ToolStyle::Ledger => None,
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
        ToolStyle::Classic => palette::edit_bg(),
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
    if !is_tool_card(entry) || current_style() == ToolStyle::Classic {
        return classic::background(entry);
    }
    match current_style() {
        ToolStyle::Classic => classic::background(entry),
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
    if !is_tool_card(entry) || style == ToolStyle::Classic {
        classic::paint_chrome(entry, line, row, area, buffer);
        return;
    }
    styled::paint_chrome(style, entry, line, ctx, row, area, buffer);
}

pub(super) fn text_target(
    entry: &TranscriptEntry,
    line: &LayoutLine,
    inset: u16,
    area_width: u16,
) -> Option<(u16, u16)> {
    let style = current_style();
    if !is_tool_card(entry) || style == ToolStyle::Classic {
        return classic::text_target(entry, line, inset, area_width);
    }
    styled::text_target(style, line, area_width)
}
