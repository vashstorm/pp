//! `/context` 独立 TUI 面板渲染：顶部容量/缓存摘要、左侧分类栏、右上占比列表与右下正文预览。

use super::*;
use crate::tui::{
    settings::render::sidebar_width,
    statusline::abbreviate_tokens,
    themes::{Palette, palette},
};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};

impl Panel {
    pub(in crate::tui) fn draw(&mut self, frame: &mut Frame, area: Rect) {
        self.area = area;
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

        let title = match &self.report {
            Some(report) => format!(
                " Context Diagnostics · {} · {} · {} ",
                display_text(&report.model_reference),
                report.protocol,
                report.work_mode
            ),
            None => " Context Diagnostics ".to_owned(),
        };
        let outer = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(p.panel_border))
            .title(Line::styled(
                title,
                Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
            ));
        let inner = outer.inner(area);
        frame.render_widget(outer, area);
        if inner.width < 8 || inner.height < 6 {
            return;
        }

        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(4),
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
                Line::styled(msg, Style::default().fg(p.muted)),
                Rect::new(area.x, area.y, area.width, 1),
            );
            frame.render_widget(
                Line::styled(
                    "─".repeat(usize::from(area.width)),
                    Style::default().fg(p.panel_border),
                ),
                Rect::new(area.x, area.y + 2, area.width, 1),
            );
            return;
        };

        let line1 = header_capacity_line(report, area.width, p);
        let line2 = header_cache_and_compact_line(report, p);
        frame.render_widget(line1, Rect::new(area.x, area.y, area.width, 1));
        frame.render_widget(line2, Rect::new(area.x, area.y + 1, area.width, 1));
        frame.render_widget(
            Line::styled(
                "─".repeat(usize::from(area.width)),
                Style::default().fg(p.panel_border),
            ),
            Rect::new(area.x, area.y + 2, area.width, 1),
        );
    }

    fn draw_body(&mut self, frame: &mut Frame, area: Rect, p: &Palette) {
        let sb_width = sidebar_width(area.width);
        let right = if sb_width > 0 {
            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Length(sb_width),
                    Constraint::Length(1),
                    Constraint::Min(16),
                ])
                .split(area);
            self.sidebar_area = cols[0];
            self.draw_sidebar(frame, cols[0], p);
            frame.render_widget(
                Paragraph::new(vec![Line::raw("│"); usize::from(cols[1].height)])
                    .style(Style::default().fg(p.panel_border)),
                cols[1],
            );
            cols[2]
        } else {
            self.sidebar_area = Rect::default();
            let narrow = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(1), Constraint::Min(3)])
                .split(area);
            frame.render_widget(
                Line::styled(
                    format!(" Category: {} (←/→ switch)", self.category.label()),
                    Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
                ),
                narrow[0],
            );
            narrow[1]
        };

        let list_height = ((right.height * 45) / 100)
            .max(4)
            .min(right.height.saturating_sub(4));
        let panes = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(list_height),
                Constraint::Length(1),
                Constraint::Min(2),
            ])
            .split(right);

        self.list_area = panes[0];
        self.preview_area = panes[2];

        self.draw_list(frame, panes[0], p);
        self.draw_preview_divider(frame, panes[1], p);
        self.draw_preview(frame, panes[2], p);
    }

    fn draw_sidebar(&self, frame: &mut Frame, area: Rect, p: &Palette) {
        let mut lines = Vec::with_capacity(Category::ALL.len());
        let width = usize::from(area.width);
        for &category in &Category::ALL {
            let selected = category == self.category;
            let prefix = if selected { "❯ " } else { "  " };
            let badge = if self.report.is_some() {
                let tokens = self.category_tokens(category);
                if tokens > 0 {
                    abbreviate_tokens(tokens)
                } else {
                    "—".into()
                }
            } else {
                String::new()
            };
            let style = if selected && self.focus == Focus::Sidebar {
                Style::default()
                    .bg(p.panel_selected_bg)
                    .fg(p.panel_selected_fg)
                    .add_modifier(Modifier::BOLD)
            } else if selected {
                Style::default().fg(p.accent).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(p.text)
            };
            let badge_style = if selected && self.focus == Focus::Sidebar {
                style
            } else {
                Style::default().fg(p.muted)
            };
            let label = category.label();
            let used = 2 + label.chars().count();
            let gap = if !badge.is_empty() && width > used + badge.len() + 1 {
                " ".repeat(width - used - badge.len() - 1)
            } else {
                " ".into()
            };
            lines.push(Line::from(vec![
                Span::styled(format!("{prefix}{label}"), style),
                Span::styled(gap, style),
                Span::styled(badge, badge_style),
            ]));
        }
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn draw_list(&mut self, frame: &mut Frame, area: Rect, p: &Palette) {
        let visible = usize::from(area.height).max(1);
        self.list_visible = visible;
        let item_count = self.current_items().len();
        if item_count == 0 {
            let msg = if self.loading {
                "Loading..."
            } else {
                "No items in this category."
            };
            frame.render_widget(
                Paragraph::new(Line::styled(
                    format!(" {msg}"),
                    Style::default().fg(p.muted),
                )),
                area,
            );
            return;
        }
        if self.list_index >= item_count {
            self.list_index = item_count - 1;
        }
        if self.list_index < self.list_scroll {
            self.list_scroll = self.list_index;
        } else if self.list_index >= self.list_scroll + visible {
            self.list_scroll = self.list_index + 1 - visible;
        }
        let items = self.current_items();

        let denom = self.report.as_ref().map_or(1, |r| r.prepared_tokens.max(1)) as f64;
        let width = usize::from(area.width);
        let mut lines = Vec::with_capacity(visible);
        for (idx, item) in items
            .iter()
            .enumerate()
            .skip(self.list_scroll)
            .take(visible)
        {
            let selected = idx == self.list_index;
            let focused_sel = selected && self.focus == Focus::List;
            let row_style = if focused_sel {
                Style::default()
                    .bg(p.panel_selected_bg)
                    .fg(p.panel_selected_fg)
                    .add_modifier(Modifier::BOLD)
            } else if selected {
                Style::default()
                    .bg(p.panel_alt_bg)
                    .fg(p.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(p.text)
            };

            let prefix = if selected { "❯ " } else { "  " };
            let kind = format!("[{}]", item.kind_label);
            let tok_str = format!("~{}", abbreviate_tokens(item.tokens));
            let pct = item.tokens as f64 * 100.0 / denom;
            let pct_str = format!("{pct:>4.1}%");
            let show_bar = width >= 66;
            let bar = if show_bar {
                format!(" {}", proportion_bar(pct / 100.0, 8))
            } else {
                String::new()
            };

            let right_part = format!(" {tok_str:>6} {pct_str}{bar}");
            let left_prefix_len = 2 + kind.chars().count() + 1;
            let title_budget = width
                .saturating_sub(left_prefix_len + right_part.chars().count() + 1)
                .max(8);

            let clean_title = display_text(&item.title);
            let mut title_text = clean_title.into_owned();
            if !item.subtitle.is_empty() && title_budget > title_text.chars().count() + 6 {
                let clean_sub = display_text(&item.subtitle);
                title_text.push_str(" · ");
                title_text.push_str(&clean_sub);
            }
            let truncated_title = truncate_chars(&title_text, title_budget);
            let padded_title = format!("{truncated_title:<title_budget$}");

            let kind_style = if focused_sel {
                row_style
            } else {
                Style::default().fg(kind_color(&item.kind_label, p))
            };
            let right_style = if focused_sel {
                row_style
            } else {
                Style::default().fg(p.muted)
            };

            lines.push(Line::from(vec![
                Span::styled(prefix, row_style),
                Span::styled(kind, kind_style),
                Span::styled(" ", row_style),
                Span::styled(padded_title, row_style),
                Span::styled(right_part, right_style),
            ]));
        }
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn draw_preview_divider(&self, frame: &mut Frame, area: Rect, p: &Palette) {
        let sort_badge = if !matches!(self.category, Category::Overview | Category::Largest) {
            format!(" [Sort: {}] ", self.sort.label())
        } else {
            String::new()
        };
        let item_label = self
            .current_items()
            .get(self.list_index)
            .map(|item| {
                format!(
                    " Preview: {} (~{} tok, {} B) ",
                    display_text(&item.title),
                    item.tokens,
                    item.bytes
                )
            })
            .unwrap_or_else(|| " Preview ".to_owned());
        let focus_tag = if self.focus == Focus::Preview {
            "[ACTIVE] "
        } else {
            ""
        };
        let width = usize::from(area.width);
        let title_str = truncate_chars(
            &format!("{focus_tag}{item_label}"),
            width.saturating_sub(sort_badge.chars().count() + 2),
        );
        let fill = width.saturating_sub(title_str.chars().count() + sort_badge.chars().count());
        let border_fg = if self.focus == Focus::Preview {
            p.accent
        } else {
            p.panel_border
        };
        frame.render_widget(
            Line::from(vec![
                Span::styled(
                    title_str,
                    Style::default()
                        .fg(if self.focus == Focus::Preview {
                            p.accent
                        } else {
                            p.text
                        })
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("─".repeat(fill), Style::default().fg(border_fg)),
                Span::styled(sort_badge, Style::default().fg(p.accent)),
            ]),
            area,
        );
    }

    fn draw_preview(&mut self, frame: &mut Frame, area: Rect, p: &Palette) {
        self.preview_visible = usize::from(area.height).max(1);
        let Some(item) = self.current_items().get(self.list_index).copied() else {
            self.preview_max_scroll = 0;
            self.preview_scroll = 0;
            return;
        };
        let sanitized = display_text(&item.preview).into_owned();
        let para = Paragraph::new(sanitized.as_str())
            .style(Style::default().fg(p.text))
            .wrap(Wrap { trim: false });
        let total_lines = para.line_count(area.width.max(1));
        self.preview_max_scroll = total_lines.saturating_sub(self.preview_visible);
        self.preview_scroll = self.preview_scroll.min(self.preview_max_scroll);

        frame.render_widget(
            para.scroll((self.preview_scroll.min(u16::MAX as usize) as u16, 0)),
            area,
        );
    }

    fn draw_footer(&self, frame: &mut Frame, area: Rect, p: &Palette) {
        let line = if let Some(error) = &self.error {
            Line::styled(error.as_str(), Style::default().fg(p.removed))
        } else {
            let focus_hint = match self.focus {
                Focus::Sidebar => "Focus: Category",
                Focus::List => "Focus: Items",
                Focus::Preview => "Focus: Preview",
            };
            let scroll_info = if self.preview_max_scroll > 0 {
                format!(
                    " · Preview {}/{}",
                    self.preview_scroll + 1,
                    self.preview_max_scroll + 1
                )
            } else {
                String::new()
            };
            Line::styled(
                format!(
                    "{focus_hint}{scroll_info} · Tab/←/→ focus · ↑↓ move/scroll · Enter open/preview · s sort · r refresh · Esc close"
                ),
                Style::default().fg(p.muted),
            )
        };
        frame.render_widget(line, area);
    }
}

fn header_capacity_line(report: &ContextReport, width: u16, p: &Palette) -> Line<'static> {
    let mut spans = Vec::new();
    let active_str = abbreviate_tokens(report.active_tokens);
    let prep_str = abbreviate_tokens(report.prepared_tokens);
    match report.context_window {
        Some(window) => {
            let ratio = report.active_tokens as f64 / window as f64;
            let bar = proportion_bar(ratio, 14);
            let pct = ratio * 100.0;
            let bar_color = if pct >= f64::from(report.threshold_percent) {
                p.removed
            } else if pct >= f64::from(report.threshold_percent) * 0.75 {
                p.accent
            } else {
                p.added
            };
            if width >= 70 {
                spans.push(Span::styled(
                    format!("[{bar}] "),
                    Style::default().fg(bar_color),
                ));
            }
            spans.push(Span::styled(
                format!(
                    "Active: ~{active_str}/{} ({pct:.1}%)",
                    abbreviate_tokens(window)
                ),
                Style::default().fg(p.text).add_modifier(Modifier::BOLD),
            ));
            if report.empty_session || report.prepared_tokens != report.active_tokens {
                spans.push(Span::styled(
                    format!(" · Prepared: ~{prep_str}"),
                    Style::default().fg(p.accent),
                ));
            }
            let compact_str = if report.auto_compact {
                let threshold_tokens = window * u64::from(report.threshold_percent) / 100;
                let headroom = threshold_tokens.saturating_sub(report.active_tokens);
                format!(
                    " · Auto-compact: {}% (headroom ~{})",
                    report.threshold_percent,
                    abbreviate_tokens(headroom)
                )
            } else {
                " · Auto-compact: off".into()
            };
            spans.push(Span::styled(compact_str, Style::default().fg(p.muted)));
        }
        None => {
            spans.push(Span::styled(
                format!("Active: ~{active_str} / ?"),
                Style::default().fg(p.text).add_modifier(Modifier::BOLD),
            ));
            if report.empty_session || report.prepared_tokens != report.active_tokens {
                spans.push(Span::styled(
                    format!(" · Prepared: ~{prep_str}"),
                    Style::default().fg(p.accent),
                ));
            }
            spans.push(Span::styled(
                " · context_window unconfigured",
                Style::default().fg(p.muted),
            ));
        }
    }
    Line::from(spans)
}

fn header_cache_and_compact_line(report: &ContextReport, p: &Palette) -> Line<'static> {
    let mut spans = Vec::new();
    match report.last_request {
        Some(usage) => {
            spans.push(Span::styled("Last req: ", Style::default().fg(p.muted)));
            spans.push(Span::styled(
                format!(
                    "in {} / out {}",
                    abbreviate_tokens(usage.input_tokens),
                    abbreviate_tokens(usage.output_tokens)
                ),
                Style::default().fg(p.status_input),
            ));
            if let Some(cached) = usage.cached_tokens {
                let hit = if usage.input_tokens > 0 {
                    format!(
                        " ({:.0}%)",
                        cached as f64 * 100.0 / usage.input_tokens as f64
                    )
                } else {
                    String::new()
                };
                spans.push(Span::styled(
                    format!(" · cache read {}{hit}", abbreviate_tokens(cached)),
                    Style::default().fg(p.status_cache_read),
                ));
            }
            if let Some(write) = usage.cache_write_tokens {
                spans.push(Span::styled(
                    format!(" / write {}", abbreviate_tokens(write)),
                    Style::default().fg(p.status_cache_write),
                ));
            }
        }
        None => {
            spans.push(Span::styled(
                "Last req: none yet",
                Style::default().fg(p.muted),
            ));
        }
    }
    let compact_summary = match &report.compaction.status {
        CompactionStatus::NotEnoughHistory => " · Compact: <2 exchanges".into(),
        CompactionStatus::UnsupportedHistory => " · Compact: unsupported history".into(),
        CompactionStatus::Ready {
            compressible_exchanges,
            compressible_tokens,
            retained_exchanges,
            ..
        } => format!(
            " · Compact: {compressible_exchanges} exch (~{}) → keep {retained_exchanges}",
            abbreviate_tokens(*compressible_tokens)
        ),
    };
    spans.push(Span::styled(compact_summary, Style::default().fg(p.muted)));
    Line::from(spans)
}

fn proportion_bar(ratio: f64, width: usize) -> String {
    let filled = ((ratio.clamp(0.0, 1.0) * width as f64).round() as usize).min(width);
    format!("{}{}", "█".repeat(filled), "░".repeat(width - filled))
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

fn kind_color(kind: &str, p: &Palette) -> Color {
    match kind {
        "MCP" => p.mcp_tool,
        "Built-in" => p.read_title,
        "AGENTS.md" | "Rule" | "Boundary" => p.md_heading,
        "Skill Index" | "Skill Body" => p.md_code,
        "Tool Result" | "Tool Image" => p.status_output,
        "Tool Call" => p.bash_title,
        "Reasoning" => p.code,
        "Summary" => p.plan_mode,
        "Summary Row" => p.accent,
        "User" => p.status_input,
        "Assistant" => p.md_text,
        _ => p.muted,
    }
}
