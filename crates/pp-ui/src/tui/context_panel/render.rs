//! `/context` 独立 TUI 面板渲染：顶部核心指标、左侧分类栏、右侧全高列表或条目详情视图。

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
        let line2 = header_usage_line(report, p);
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
        if let Some(report) = &self.report
            && report.empty_session
        {
            self.sidebar_area = Rect::default();
            self.list_area = Rect::default();
            self.detail_area = Rect::default();
            self.detail_max_scroll = 0;
            draw_empty_state(frame, area, report, p);
            return;
        }

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

        match self.view {
            ViewMode::List => {
                self.detail_area = Rect::default();
                self.detail_max_scroll = 0;
                let panes = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Length(2), Constraint::Min(2)])
                    .split(right);
                self.draw_list_header(frame, panes[0], p);
                self.list_area = panes[1];
                self.draw_list(frame, panes[1], p);
            }
            ViewMode::Detail => {
                self.list_area = Rect::default();
                let panes = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Length(2), Constraint::Min(2)])
                    .split(right);
                self.draw_detail_header(frame, panes[0], p);
                self.detail_area = panes[1];
                self.draw_detail_body(frame, panes[1], p);
            }
        }
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

    fn draw_list_header(&self, frame: &mut Frame, area: Rect, p: &Palette) {
        let summary_text = match &self.report {
            None => " Loading...".to_owned(),
            Some(report) if self.category == Category::Overview => format!(
                " Overview · {} categories · ~{} active tokens (Enter to open)",
                report.categories.len(),
                abbreviate_tokens(report.active_tokens)
            ),
            Some(report) => {
                let tokens = self.category_tokens(self.category);
                let items = self.current_items().len();
                if self.category == Category::Thinking && !report.thinking_in_context {
                    format!(
                        " {} · {} turn(s) · ~{} tokens (model output · Enter for detail)",
                        self.category.label(),
                        report.thinking_turns,
                        abbreviate_tokens(tokens),
                    )
                } else {
                    let denom = report.active_tokens.max(1) as f64;
                    let pct = tokens as f64 * 100.0 / denom;
                    format!(
                        " {} · {} entries · ~{} tokens ({pct:.1}% of context · Enter for detail)",
                        self.category.label(),
                        items,
                        abbreviate_tokens(tokens),
                    )
                }
            }
        };
        let sort_badge = if self.report.is_some() {
            format!(" [Sort: {}] ", self.sort.label())
        } else {
            String::new()
        };
        let width = usize::from(area.width);
        let fill = width.saturating_sub(sort_badge.chars().count());
        frame.render_widget(
            Line::styled(
                truncate_chars(&summary_text, width),
                Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
            ),
            Rect::new(area.x, area.y, area.width, 1),
        );
        frame.render_widget(
            Line::from(vec![
                Span::styled("─".repeat(fill), Style::default().fg(p.panel_border)),
                Span::styled(sort_badge, Style::default().fg(p.accent)),
            ]),
            Rect::new(area.x, area.y + 1, area.width, 1),
        );
    }

    fn draw_list(&mut self, frame: &mut Frame, area: Rect, p: &Palette) {
        let visible = usize::from(area.height).max(1);
        self.list_visible = visible;
        let item_count = self.current_items().len();
        if item_count == 0 {
            let msg = if self.loading {
                "Loading..."
            } else {
                "No items recorded in this category."
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
        let sep_idx = self.overview_separator_index();
        let total_visual = item_count + usize::from(sep_idx.is_some());
        if self.list_index >= item_count {
            self.list_index = item_count - 1;
        }
        let selected_visual = if sep_idx.is_some_and(|sep| self.list_index >= sep) {
            self.list_index + 1
        } else {
            self.list_index
        };
        let max_scroll = total_visual.saturating_sub(visible);
        if self.list_scroll > max_scroll {
            self.list_scroll = max_scroll;
        }
        if selected_visual < self.list_scroll {
            self.list_scroll = selected_visual;
        } else if selected_visual >= self.list_scroll + visible {
            self.list_scroll = selected_visual + 1 - visible;
        }
        let items = self.current_items();
        let denom = self.report.as_ref().map_or(1, |r| r.active_tokens.max(1)) as f64;
        let width = usize::from(area.width);
        let mut lines = Vec::with_capacity(visible);

        for visual_idx in self.list_scroll..(self.list_scroll + visible).min(total_visual) {
            if sep_idx == Some(visual_idx) {
                lines.push(Line::styled(
                    "─".repeat(width),
                    Style::default().fg(p.panel_border),
                ));
                continue;
            }
            let idx = if sep_idx.is_some_and(|sep| visual_idx > sep) {
                visual_idx - 1
            } else {
                visual_idx
            };
            let item = items[idx];
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
            let tok_str = if item.tokens > 0 {
                format!("~{}", abbreviate_tokens(item.tokens))
            } else {
                "0".into()
            };
            let in_ctx = self.current_row_in_context(idx);
            let pct = item.tokens as f64 * 100.0 / denom;
            let pct_str = if in_ctx {
                format!("{pct:>4.1}%")
            } else {
                "  gen".into()
            };
            let show_bar = width >= 66;
            let bar = if show_bar {
                if in_ctx {
                    format!(" {}", proportion_bar(pct / 100.0, 8))
                } else {
                    " ────────".into()
                }
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

    fn draw_detail_header(&self, frame: &mut Frame, area: Rect, p: &Palette) {
        let width = usize::from(area.width);
        let back_hint = " [Esc/Backspace: Back] ";
        let title_str = self
            .current_items()
            .get(self.list_index)
            .map(|item| {
                format!(
                    " [{}] {} (~{} tok, {} B)",
                    item.kind_label,
                    display_text(&item.title),
                    item.tokens,
                    item.bytes
                )
            })
            .unwrap_or_else(|| " Detail ".to_owned());
        let subtitle_str = self
            .current_items()
            .get(self.list_index)
            .map(|item| display_text(&item.subtitle).into_owned())
            .unwrap_or_default();

        let title_budget = width.saturating_sub(back_hint.chars().count() + 1);
        let left_title = truncate_chars(&title_str, title_budget);
        let gap = width.saturating_sub(left_title.chars().count() + back_hint.chars().count());
        frame.render_widget(
            Line::from(vec![
                Span::styled(
                    left_title,
                    Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
                ),
                Span::styled(" ".repeat(gap), Style::default()),
                Span::styled(back_hint, Style::default().fg(p.muted)),
            ]),
            Rect::new(area.x, area.y, area.width, 1),
        );

        let sub_tag = if subtitle_str.is_empty() {
            String::new()
        } else {
            format!(
                " {} ",
                truncate_chars(&subtitle_str, width.saturating_sub(6))
            )
        };
        let fill = width.saturating_sub(sub_tag.chars().count());
        frame.render_widget(
            Line::from(vec![
                Span::styled(sub_tag, Style::default().fg(p.muted)),
                Span::styled("─".repeat(fill), Style::default().fg(p.panel_border)),
            ]),
            Rect::new(area.x, area.y + 1, area.width, 1),
        );
    }

    fn draw_detail_body(&mut self, frame: &mut Frame, area: Rect, p: &Palette) {
        self.detail_visible = usize::from(area.height).max(1);
        let Some(item) = self.current_items().get(self.list_index).copied() else {
            self.detail_max_scroll = 0;
            self.detail_scroll = 0;
            return;
        };
        let sanitized = display_text(&item.preview).into_owned();
        let para = Paragraph::new(sanitized.as_str())
            .style(Style::default().fg(p.text))
            .wrap(Wrap { trim: false });
        let total_lines = para.line_count(area.width.max(1));
        self.detail_max_scroll = total_lines.saturating_sub(self.detail_visible);
        self.detail_scroll = self.detail_scroll.min(self.detail_max_scroll);

        frame.render_widget(
            para.scroll((self.detail_scroll.min(u16::MAX as usize) as u16, 0)),
            area,
        );
    }

    fn draw_footer(&self, frame: &mut Frame, area: Rect, p: &Palette) {
        let line = if let Some(error) = &self.error {
            Line::styled(error.as_str(), Style::default().fg(p.removed))
        } else if self.report.as_ref().is_some_and(|r| r.empty_session) {
            Line::styled(
                "No messages sent yet · r refresh · Esc close",
                Style::default().fg(p.muted),
            )
        } else if self.view == ViewMode::Detail {
            let scroll_info = if self.detail_max_scroll > 0 {
                format!(
                    "Detail {}/{} · ",
                    self.detail_scroll + 1,
                    self.detail_max_scroll + 1
                )
            } else {
                "Detail · ".into()
            };
            Line::styled(
                format!("{scroll_info}↑↓/PgUp/PgDn scroll · Esc/Backspace back · r refresh"),
                Style::default().fg(p.muted),
            )
        } else {
            let focus_hint = match self.focus {
                Focus::Sidebar => "Focus: Category",
                Focus::List => "Focus: List",
            };
            let back_hint = if self.category != Category::Overview {
                " · Backspace overview"
            } else {
                ""
            };
            Line::styled(
                format!(
                    "{focus_hint} · Tab/←/→ focus · ↑↓ move · Enter open · s sort{back_hint} · r refresh · Esc close"
                ),
                Style::default().fg(p.muted),
            )
        };
        frame.render_widget(line, area);
    }
}

fn draw_empty_state(frame: &mut Frame, area: Rect, report: &ContextReport, p: &Palette) {
    let lines = vec![
        Line::raw(""),
        Line::styled(
            "  No Context Data Yet",
            Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
        ),
        Line::raw(""),
        Line::styled(
            "  No messages have been sent in this session yet.",
            Style::default().fg(p.text),
        ),
        Line::styled(
            "  Context statistics are collected from actual session requests only.",
            Style::default().fg(p.muted),
        ),
        Line::raw(""),
        Line::styled(
            "  Send your first message, or use /resume to load an existing session.",
            Style::default().fg(p.muted),
        ),
        Line::raw(""),
        Line::styled(
            format!(
                "  Current Model: {} ({}) · Protocol: {} · Mode: {} · Thinking: {}",
                display_text(&report.model_reference),
                display_text(&report.model_name),
                report.protocol,
                report.work_mode,
                report.think_level.label(),
            ),
            Style::default().fg(p.muted),
        ),
        Line::styled(
            format!(
                "  Auto-Compact: {}",
                if !report.auto_compact {
                    "off".into()
                } else if let Some(w) = report.context_window {
                    format!(
                        "on at {}% ({} window)",
                        report.threshold_percent,
                        abbreviate_tokens(w)
                    )
                } else {
                    "inactive (context_window unconfigured)".into()
                }
            ),
            Style::default().fg(p.muted),
        ),
    ];
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn header_capacity_line(report: &ContextReport, width: u16, p: &Palette) -> Line<'static> {
    let mut spans = Vec::new();
    let active_str = abbreviate_tokens(report.active_tokens);
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
                    "Context: ~{active_str}/{} ({pct:.1}%)",
                    abbreviate_tokens(window)
                ),
                Style::default().fg(p.text).add_modifier(Modifier::BOLD),
            ));
        }
        None => {
            spans.push(Span::styled(
                format!("Context: ~{active_str} / ?"),
                Style::default().fg(p.text).add_modifier(Modifier::BOLD),
            ));
        }
    }
    let compact_str = if report.empty_session {
        " · No messages sent yet".into()
    } else if !report.auto_compact {
        " · Auto-compact: off".into()
    } else if report.context_window.is_some() {
        format!(" · Auto-compact: {}%", report.threshold_percent)
    } else {
        " · context_window unconfigured".into()
    };
    spans.push(Span::styled(compact_str, Style::default().fg(p.muted)));
    Line::from(spans)
}

fn header_usage_line(report: &ContextReport, p: &Palette) -> Line<'static> {
    if report.empty_session {
        return Line::styled(
            format!(
                "Model: {} · Mode: {} · Thinking: {} · Auto-compact: {}",
                display_text(&report.model_reference),
                report.work_mode,
                report.think_level.label(),
                if report.auto_compact {
                    format!("{}%", report.threshold_percent)
                } else {
                    "off".into()
                }
            ),
            Style::default().fg(p.muted),
        );
    }

    let mut spans = Vec::new();
    match report.last_request {
        Some(usage) => {
            spans.push(Span::styled("Last req: ", Style::default().fg(p.muted)));
            spans.push(Span::styled(
                format!(
                    "in {} · out {}",
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
                    format!(" · cache {}{hit}", abbreviate_tokens(cached)),
                    Style::default().fg(p.status_cache_read),
                ));
            }
        }
        None => {
            spans.push(Span::styled("Last req: none", Style::default().fg(p.muted)));
        }
    }

    spans.push(Span::styled(
        format!(
            " · Session out: {}",
            abbreviate_tokens(report.session_output_tokens)
        ),
        Style::default().fg(p.muted),
    ));

    let thinking_str = if report.thinking_turns > 0 {
        format!(
            " · Thinking: ~{} ({}t)",
            abbreviate_tokens(report.thinking_tokens),
            report.thinking_turns
        )
    } else {
        " · Thinking: 0".into()
    };
    spans.push(Span::styled(
        thinking_str,
        Style::default().fg(if report.thinking_turns > 0 {
            p.code
        } else {
            p.muted
        }),
    ));
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
        "Thinking" => p.code,
        "Summary" => p.plan_mode,
        "Summary Row" => p.accent,
        "User" => p.status_input,
        "Assistant" => p.md_text,
        _ => p.muted,
    }
}
