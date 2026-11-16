//! `/context` 独立 TUI 面板渲染：
//! 顶部三行摘要 + 左侧 25×16 占比网格 + 右侧紧凑 12 行列表（数值紧跟文字）+ 原地展开子项。

use super::*;
use crate::tui::{
    statusline::abbreviate_tokens,
    themes::{self, Palette, palette},
};
use pp_runtime::agent::context_diagnosis::{CategoryKey, CategorySummary};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};

const GRID_COLS: usize = 25;
const GRID_ROWS: usize = 16;
const GRID_CELLS: usize = GRID_COLS * GRID_ROWS;
const GRID_COLUMN_WIDTH: u16 = (GRID_COLS as u16) * 2 + 1;

const NOTE_TEXT: &str =
    "Category totals split Used by estimated share; the entries inside may not add up to them.";

#[derive(Clone, Copy)]
enum GridCell {
    Category(usize),
    Free,
    Buffer,
}

impl Panel {
    pub(in crate::tui) fn draw(&mut self, frame: &mut Frame, area: Rect) {
        self.area = area;
        self.list_area = Rect::default();
        if area.is_empty() {
            return;
        }
        let p = palette();
        frame.render_widget(Clear, area);
        frame.render_widget(
            Block::default().style(Style::default().bg(p.panel_bg).fg(p.text)),
            area,
        );
        if area.width < 12 || area.height < 8 {
            return;
        }

        let outer = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(p.panel_border));
        let box_inner = outer.inner(area);
        frame.render_widget(outer, area);
        if box_inner.width < 8 || box_inner.height < 6 {
            return;
        }

        // 左右各留 1 列内边距，视觉更舒展。
        let inner = if box_inner.width >= 16 {
            Rect::new(
                box_inner.x + 1,
                box_inner.y,
                box_inner.width - 2,
                box_inner.height,
            )
        } else {
            box_inner
        };

        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(3),
                Constraint::Length(1),
            ])
            .split(inner);

        self.draw_header(frame, sections[0], p);
        self.draw_body(frame, sections[1], p);
        self.draw_footer(frame, sections[2], p);
    }

    fn draw_header(&self, frame: &mut Frame, area: Rect, p: &Palette) {
        let Some(report) = &self.report else {
            let msg = if self.loading {
                "Inspecting current session context..."
            } else {
                "Context diagnostics unavailable."
            };
            frame.render_widget(
                Line::styled(
                    "Context",
                    Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
                ),
                Rect::new(area.x, area.y, area.width, 1),
            );
            frame.render_widget(
                Line::styled(msg, Style::default().fg(p.muted)),
                Rect::new(area.x, area.y + 1, area.width, 1),
            );
            return;
        };

        let width = usize::from(area.width);
        let title_line = Line::from(vec![
            Span::styled(
                "Context",
                Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" · ", Style::default().fg(p.muted)),
            Span::styled(
                truncate_chars(
                    &format!(
                        "{} ({})",
                        display_text(&report.model_reference),
                        report.think_level.label()
                    ),
                    width.saturating_sub(10),
                ),
                Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
            ),
        ]);
        frame.render_widget(title_line, Rect::new(area.x, area.y, area.width, 1));

        let used_str = abbreviate_tokens(report.active_tokens);
        let line2_text = match report.context_window {
            Some(window) if window > 0 => {
                let pct = report.active_tokens as f64 * 100.0 / window as f64;
                let pct_label = if report.active_tokens == 0 {
                    "0%".to_owned()
                } else if pct < 0.5 {
                    "<1%".to_owned()
                } else {
                    format!("{pct:.0}%")
                };
                format!(
                    "Used {used_str} of {} tokens · {pct_label}",
                    abbreviate_tokens(window)
                )
            }
            _ => format!("Used {used_str} tokens · context_window unconfigured"),
        };
        frame.render_widget(
            Line::styled(
                truncate_chars(&line2_text, width),
                Style::default().fg(p.muted),
            ),
            Rect::new(area.x, area.y + 1, area.width, 1),
        );

        let line3_text = match report.context_window {
            Some(window) if window > 0 && report.auto_compact => {
                let threshold_tokens = window * u64::from(report.threshold_percent) / 100;
                if report.active_tokens <= threshold_tokens {
                    let left = threshold_tokens - report.active_tokens;
                    format!(
                        "{} left before auto-compaction at {}%",
                        abbreviate_tokens(left),
                        report.threshold_percent
                    )
                } else {
                    format!(
                        "Auto-compaction threshold reached ({}%)",
                        report.threshold_percent
                    )
                }
            }
            Some(window) if window > 0 => {
                let free = window.saturating_sub(report.active_tokens);
                format!(
                    "Auto-compaction disabled · {} free",
                    abbreviate_tokens(free)
                )
            }
            _ => "Auto-compaction inactive (context_window unconfigured)".to_owned(),
        };
        frame.render_widget(
            Line::styled(
                truncate_chars(&line3_text, width),
                Style::default().fg(p.muted),
            ),
            Rect::new(area.x, area.y + 2, area.width, 1),
        );
    }

    fn draw_body(&mut self, frame: &mut Frame, area: Rect, p: &Palette) {
        let Some(report) = self.report.clone() else {
            return;
        };

        let right_area = if area.width >= GRID_COLUMN_WIDTH + 32 && area.height >= GRID_ROWS as u16
        {
            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Length(GRID_COLUMN_WIDTH), Constraint::Min(24)])
                .split(area);
            draw_grid(frame, cols[0], &report, p);
            cols[1]
        } else {
            area
        };

        let note_para = Paragraph::new(Line::styled(NOTE_TEXT, Style::default().fg(p.muted)))
            .wrap(Wrap { trim: true });
        let note_height = if right_area.width >= 20 && right_area.height >= 14 {
            (note_para.line_count(right_area.width) as u16).min(3)
        } else {
            0
        };

        let rows = self.visible_rows();
        let max_list_height = right_area.height.saturating_sub(note_height);
        let list_height = (rows.len() as u16).min(max_list_height).max(1);
        let list_rect = Rect::new(right_area.x, right_area.y, right_area.width, list_height);
        self.list_area = list_rect;
        self.draw_list(frame, list_rect, &report, &rows, p);

        if note_height > 0 {
            let note_y = list_rect.bottom();
            let avail = right_area.bottom().saturating_sub(note_y);
            if avail > 0 {
                frame.render_widget(
                    note_para,
                    Rect::new(
                        right_area.x,
                        note_y,
                        right_area.width,
                        avail.min(note_height),
                    ),
                );
            }
        }
    }

    fn draw_list(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        report: &ContextReport,
        rows: &[RowKind],
        p: &Palette,
    ) {
        let visible = usize::from(area.height).max(1);
        self.visible_height = visible;
        let total = rows.len();
        if total == 0 {
            return;
        }
        if self.cursor >= total {
            self.cursor = total - 1;
        }
        let max_scroll = total.saturating_sub(visible);
        if self.scroll > max_scroll {
            self.scroll = max_scroll;
        }
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        } else if self.cursor >= self.scroll + visible {
            self.scroll = self.cursor + 1 - visible;
        }

        let ascii = themes::is_ascii();
        let filled_glyph = if ascii { "#" } else { "⛁" };
        let free_glyph = if ascii { "." } else { "⛶" };
        let buffer_glyph = if ascii { "x" } else { "⛝" };
        let expanded_mark = if ascii { " v" } else { " ▾" };

        let denom = report
            .context_window
            .filter(|&w| w > 0)
            .unwrap_or_else(|| report.active_tokens.max(1));
        let buffer_tokens = capacity_buffer_tokens(report);
        let free_tokens = capacity_free_tokens(report, buffer_tokens);

        let width = usize::from(area.width);
        let mut lines = Vec::with_capacity(visible);

        for (idx, kind) in rows.iter().enumerate().skip(self.scroll).take(visible) {
            let selected = idx == self.cursor;
            let cursor_str = if selected { "> " } else { "  " };
            let cursor_style = if selected {
                Style::default().fg(p.accent).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(p.muted)
            };

            let line = match *kind {
                RowKind::Category(cat_idx) => {
                    let cat = &report.categories[cat_idx];
                    let color = category_color(cat.key, p);
                    let tok_str = abbreviate_tokens(cat.tokens);
                    let pct_str = format_pct(cat.tokens, denom, cat.in_context);
                    let suffix = if self.expanded.get(cat_idx).copied().unwrap_or(false) {
                        expanded_mark
                    } else {
                        ""
                    };
                    let metrics = format!("  {tok_str}  {pct_str}");
                    let prefix_width = 4; // cursor (2) + icon (1) + space (1)
                    let name_budget = width
                        .saturating_sub(prefix_width + metrics.chars().count())
                        .max(4);
                    let raw_name = format!("{}{suffix}", cat.name);
                    let name_text = truncate_chars(&raw_name, name_budget);

                    let name_style = if selected {
                        Style::default().fg(p.text).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(p.text)
                    };
                    let tok_style = if cat.tokens > 0 {
                        Style::default().fg(p.text)
                    } else {
                        Style::default().fg(p.muted)
                    };

                    Line::from(vec![
                        Span::styled(cursor_str, cursor_style),
                        Span::styled(format!("{filled_glyph} "), Style::default().fg(color)),
                        Span::styled(name_text, name_style),
                        Span::styled(format!("  {tok_str}"), tok_style),
                        Span::styled(format!("  {pct_str}"), Style::default().fg(p.muted)),
                    ])
                }
                RowKind::Child(cat_idx, item_idx) => {
                    let cat = &report.categories[cat_idx];
                    let item = &cat.items[item_idx];
                    let tok_str = abbreviate_tokens(item.tokens);
                    let pct_str = format_pct(item.tokens, denom, cat.in_context);
                    let metrics = format!("  {tok_str}  {pct_str}");
                    let child_prefix = if selected { ">   " } else { "    " };
                    let name_budget = width.saturating_sub(4 + metrics.chars().count()).max(4);

                    let clean_title = display_text(&item.title);
                    let label = if !item.subtitle.is_empty()
                        && name_budget > clean_title.chars().count() + 8
                    {
                        format!("{clean_title} · {}", display_text(&item.subtitle))
                    } else {
                        clean_title.into_owned()
                    };
                    let label_text = truncate_chars(&label, name_budget);

                    let title_style = if selected {
                        Style::default().fg(p.text).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(p.activity)
                    };
                    let tok_style = if item.tokens > 0 {
                        Style::default().fg(p.activity)
                    } else {
                        Style::default().fg(p.muted)
                    };

                    Line::from(vec![
                        Span::styled(child_prefix, cursor_style),
                        Span::styled(label_text, title_style),
                        Span::styled(format!("  {tok_str}"), tok_style),
                        Span::styled(format!("  {pct_str}"), Style::default().fg(p.muted)),
                    ])
                }
                RowKind::AutoCompactBuffer => {
                    let (tok_str, pct_str) = match buffer_tokens {
                        Some(tokens) => {
                            (abbreviate_tokens(tokens), format_pct(tokens, denom, true))
                        }
                        None => ("0".to_owned(), "0.0%".to_owned()),
                    };
                    let metrics = format!("  {tok_str}  {pct_str}");
                    let name_budget = width.saturating_sub(4 + metrics.chars().count()).max(4);
                    let name_text = truncate_chars("Auto-Compact Buffer", name_budget);
                    let name_style = if selected {
                        Style::default().fg(p.text).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(p.text)
                    };
                    Line::from(vec![
                        Span::styled(cursor_str, cursor_style),
                        Span::styled(format!("{buffer_glyph} "), Style::default().fg(p.accent)),
                        Span::styled(name_text, name_style),
                        Span::styled(format!("  {tok_str}"), Style::default().fg(p.muted)),
                        Span::styled(format!("  {pct_str}"), Style::default().fg(p.muted)),
                    ])
                }
                RowKind::FreeSpace => {
                    let (tok_str, pct_str) = match free_tokens {
                        Some(tokens) => {
                            (abbreviate_tokens(tokens), format_pct(tokens, denom, true))
                        }
                        None => ("—".to_owned(), "—".to_owned()),
                    };
                    let metrics = format!("  {tok_str}  {pct_str}");
                    let name_budget = width.saturating_sub(4 + metrics.chars().count()).max(4);
                    let name_text = truncate_chars("Free Space", name_budget);
                    let name_style = if selected {
                        Style::default().fg(p.text).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(p.text)
                    };
                    Line::from(vec![
                        Span::styled(cursor_str, cursor_style),
                        Span::styled(format!("{free_glyph} "), Style::default().fg(p.dim)),
                        Span::styled(name_text, name_style),
                        Span::styled(format!("  {tok_str}"), Style::default().fg(p.muted)),
                        Span::styled(format!("  {pct_str}"), Style::default().fg(p.muted)),
                    ])
                }
            };
            lines.push(line);
        }

        frame.render_widget(Paragraph::new(lines), area);
    }

    fn draw_footer(&self, frame: &mut Frame, area: Rect, p: &Palette) {
        let line = if let Some(error) = &self.error {
            Line::styled(error.as_str(), Style::default().fg(p.removed))
        } else {
            Line::styled(
                truncate_chars(
                    "↑↓ move · Enter/→ expand · ←/Backspace collapse · r refresh · Esc close",
                    usize::from(area.width),
                ),
                Style::default().fg(p.muted),
            )
        };
        frame.render_widget(line, area);
    }
}

fn draw_grid(frame: &mut Frame, area: Rect, report: &ContextReport, p: &Palette) {
    let cells = build_grid_cells(report);
    let ascii = themes::is_ascii();
    let filled_glyph = if ascii { "#" } else { "⛁" };
    let free_glyph = if ascii { "." } else { "⛶" };
    let buffer_glyph = if ascii { "x" } else { "⛝" };

    let mut lines = Vec::with_capacity(GRID_ROWS);
    for row in 0..GRID_ROWS {
        let mut spans = Vec::with_capacity(GRID_COLS * 2 - 1);
        for col in 0..GRID_COLS {
            if col > 0 {
                spans.push(Span::raw(" "));
            }
            let cell = cells[row * GRID_COLS + col];
            let (glyph, color) = match cell {
                GridCell::Category(cat_idx) => (
                    filled_glyph,
                    category_color(report.categories[cat_idx].key, p),
                ),
                GridCell::Free => (free_glyph, p.dim),
                GridCell::Buffer => (buffer_glyph, p.accent),
            };
            spans.push(Span::styled(glyph, Style::default().fg(color)));
        }
        lines.push(Line::from(spans));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn build_grid_cells(report: &ContextReport) -> [GridCell; GRID_CELLS] {
    let mut cells = [GridCell::Free; GRID_CELLS];
    let buffer_cells = if report.context_window.is_some_and(|w| w > 0) && report.auto_compact {
        let pct = usize::from(100u8.saturating_sub(report.threshold_percent.min(100)));
        (pct * GRID_CELLS + 50) / 100
    } else {
        0
    };
    let usable_cells = GRID_CELLS.saturating_sub(buffer_cells);

    let used_cells = match report.context_window {
        Some(window) if window > 0 => {
            if report.active_tokens == 0 {
                0
            } else {
                let raw = ((report.active_tokens as f64 / window as f64) * GRID_CELLS as f64)
                    .round() as usize;
                raw.clamp(1, GRID_CELLS)
            }
        }
        _ => {
            if report.active_tokens == 0 {
                0
            } else {
                GRID_CELLS
            }
        }
    };

    let cat_counts = allocate_category_cells(&report.categories, used_cells);
    let mut cursor = 0usize;
    for (cat_idx, count) in cat_counts.into_iter().enumerate() {
        for _ in 0..count {
            if cursor >= GRID_CELLS {
                break;
            }
            cells[cursor] = GridCell::Category(cat_idx);
            cursor += 1;
        }
    }

    // `usable_cells` 之后的格子在未被实际占用覆盖时渲染为 Auto-Compact Buffer (`⛝`)。
    for cell in cells.iter_mut().skip(usable_cells.max(cursor)) {
        *cell = GridCell::Buffer;
    }

    cells
}

fn allocate_category_cells(categories: &[CategorySummary], target_cells: usize) -> Vec<usize> {
    let mut counts = vec![0usize; categories.len()];
    if target_cells == 0 {
        return counts;
    }

    let in_ctx: Vec<(usize, u64)> = categories
        .iter()
        .enumerate()
        .filter(|(_, cat)| cat.in_context && cat.tokens > 0)
        .map(|(idx, cat)| (idx, cat.tokens))
        .collect();
    if in_ctx.is_empty() {
        return counts;
    }

    let total: u128 = in_ctx.iter().map(|&(_, t)| u128::from(t)).sum();
    let target_u128 = u128::from(target_cells as u64);
    let mut assigned = 0usize;
    let mut remainders = Vec::with_capacity(in_ctx.len());

    for &(idx, tokens) in &in_ctx {
        let exact = target_u128 * u128::from(tokens);
        let share = (exact / total) as usize;
        let rem = exact % total;
        counts[idx] = share;
        assigned += share;
        remainders.push((idx, rem, tokens));
    }

    remainders.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| b.2.cmp(&a.2))
            .then_with(|| a.0.cmp(&b.0))
    });
    for (idx, _, _) in remainders
        .into_iter()
        .take(target_cells.saturating_sub(assigned))
    {
        counts[idx] += 1;
    }

    // 尽量为极小但非零的分类（如 53 token 的 Instruction Files）保留至少 1 格视觉提示，
    // 从格数最多的分类挪出 1 格，直到非零分类都有格子或不可再挪。
    for &(idx, _) in &in_ctx {
        if counts[idx] > 0 {
            continue;
        }
        if let Some((donor_idx, _)) = counts
            .iter()
            .enumerate()
            .filter(|(_, c)| **c > 1)
            .max_by_key(|(_, c)| **c)
        {
            counts[donor_idx] -= 1;
            counts[idx] = 1;
        }
    }

    counts
}

fn capacity_buffer_tokens(report: &ContextReport) -> Option<u64> {
    let window = report.context_window.filter(|&w| w > 0)?;
    if !report.auto_compact {
        return Some(0);
    }
    let threshold = window * u64::from(report.threshold_percent.min(100)) / 100;
    Some(window.saturating_sub(threshold))
}

fn capacity_free_tokens(report: &ContextReport, buffer_tokens: Option<u64>) -> Option<u64> {
    let window = report.context_window.filter(|&w| w > 0)?;
    let buffer = buffer_tokens.unwrap_or(0);
    Some(
        window
            .saturating_sub(report.active_tokens)
            .saturating_sub(buffer),
    )
}

fn format_pct(tokens: u64, denom: u64, in_context: bool) -> String {
    if !in_context {
        return "gen".to_owned();
    }
    if tokens == 0 || denom == 0 {
        return "0.0%".to_owned();
    }
    let pct = tokens as f64 * 100.0 / denom as f64;
    if pct < 0.05 {
        "<0.1%".to_owned()
    } else {
        format!("{pct:.1}%")
    }
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let mut out = String::new();
    for ch in text.chars().take(max_chars.saturating_sub(1)) {
        out.push(ch);
    }
    if max_chars > 0 {
        out.push('…');
    }
    out
}

fn category_color(key: CategoryKey, p: &Palette) -> Color {
    match key {
        CategoryKey::System => p.bash_title,
        CategoryKey::InstructionFiles => p.read_title,
        CategoryKey::Skills => p.added,
        CategoryKey::BuiltInTools => p.mcp_server,
        CategoryKey::CustomTools => p.code,
        CategoryKey::UserMessages => p.attachment_color,
        CategoryKey::AssistantMessages => p.text,
        CategoryKey::Thinking => p.status_cache_read,
        CategoryKey::ToolCalls => p.md_heading,
        CategoryKey::ToolOutput => p.glob_title,
    }
}
