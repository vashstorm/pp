use super::super::*;
use super::{badge, hanging, ledger, rail, slab};
use pp_config::config::ToolStyle;

#[derive(Clone, Copy)]
pub(super) struct Metrics {
    pub left_gutter: u16,
    pub right_gutter: u16,
}

impl Metrics {
    pub(super) fn padding(self) -> u16 {
        self.left_gutter + self.right_gutter
    }
}

pub(super) fn metrics(style: ToolStyle, width: u16) -> Metrics {
    match style {
        ToolStyle::Classic | ToolStyle::Classic2 => Metrics {
            left_gutter: 2,
            right_gutter: 0,
        },
        ToolStyle::Rail | ToolStyle::Slab | ToolStyle::Badge => {
            if width < 10 {
                Metrics {
                    left_gutter: 2,
                    right_gutter: 0,
                }
            } else {
                Metrics {
                    left_gutter: 3,
                    right_gutter: 1,
                }
            }
        }
        ToolStyle::Hanging => {
            if width < 10 {
                Metrics {
                    left_gutter: 2,
                    right_gutter: 0,
                }
            } else {
                Metrics {
                    left_gutter: 4,
                    right_gutter: 1,
                }
            }
        }
        ToolStyle::Ledger => {
            if width < 16 {
                Metrics {
                    left_gutter: 2,
                    right_gutter: 0,
                }
            } else {
                Metrics {
                    left_gutter: 8,
                    right_gutter: 1,
                }
            }
        }
    }
}

fn has_important_footer(entry: &TranscriptEntry) -> bool {
    if !entry.footer_warning.is_empty() {
        return true;
    }
    entry.footer.split(" · ").any(|seg| {
        let seg = seg.trim();
        seg.starts_with("Exit:")
            || seg.starts_with("Artifact:")
            || seg.starts_with("Task ")
            || seg.starts_with("Cell #")
            || matches!(seg, "cancelled" | "timed out" | "failed" | "reset")
    })
}

pub(super) fn build_layout(
    style: ToolStyle,
    entry: &mut TranscriptEntry,
    width: u16,
) -> Vec<LayoutLine> {
    let border = |source| LayoutLine {
        content: LayoutContent::Source {
            source,
            range: 0..0,
            title_end: false,
        },
        height: 1,
    };

    let mut lines = vec![border(Source::Top)];
    let (input_lines, output_lines) =
        entry.collect_non_classic_sections(width, style == ToolStyle::Hanging);

    let has_input = !input_lines.is_empty();
    let has_output = !output_lines.is_empty();
    let has_body = has_input || has_output;
    let has_footer = match style {
        ToolStyle::Hanging => has_important_footer(entry),
        _ => !entry.footer.is_empty() || !entry.footer_warning.is_empty(),
    };

    if style == ToolStyle::Ledger && (has_body || has_footer) {
        lines.push(border(Source::Divider));
    }

    if has_input {
        lines.extend(input_lines);
    }

    if has_input && has_output {
        lines.push(border(Source::Divider));
    }

    if has_output {
        lines.extend(output_lines);
    }

    if has_footer {
        lines.push(border(Source::Bottom));
    }

    lines
}

pub(super) fn paint_chrome(
    style: ToolStyle,
    entry: &TranscriptEntry,
    line: &LayoutLine,
    ctx: super::RowCtx,
    row: u16,
    area: Rect,
    buffer: &mut Buffer,
) {
    match style {
        ToolStyle::Classic | ToolStyle::Classic2 => unreachable!(),
        ToolStyle::Rail => rail::paint_chrome(entry, line, ctx, row, area, buffer),
        ToolStyle::Slab => slab::paint_chrome(entry, line, ctx, row, area, buffer),
        ToolStyle::Badge => badge::paint_chrome(entry, line, ctx, row, area, buffer),
        ToolStyle::Hanging => hanging::paint_chrome(entry, line, ctx, row, area, buffer),
        ToolStyle::Ledger => ledger::paint_chrome(entry, line, ctx, row, area, buffer),
    }
}

pub(super) fn text_target(
    style: ToolStyle,
    line: &LayoutLine,
    area_width: u16,
) -> Option<(u16, u16)> {
    if matches!(
        line.source(),
        Some(Source::Top | Source::Divider | Source::Bottom | Source::ReadItem(_))
    ) {
        return None;
    }
    let m = metrics(style, area_width);
    let pad = m.padding();
    if area_width <= pad {
        return Some((0, area_width));
    }
    Some((m.left_gutter, area_width - pad))
}
