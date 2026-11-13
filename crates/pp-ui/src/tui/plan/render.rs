//! 计划面板绘制：正文／目录／操作区布局与提示行。

use pp_core::context::ContextUsage;
use pp_core::plan::PlanStatus;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders},
};

use crate::tui::markdown;
use crate::tui::statusline::abbreviate_tokens;
use crate::tui::transcript_style::{ACCENT, DIM, MUTED};

use super::{ACTIONS, BodyLayout, Focus, HINT, Panel, READONLY_HINT, Tab};

/// 按字符数截断纯显示文本，超宽时以省略号结尾。
/// Highlighted choice/tab chip shared by the plan panel tabs.
fn choice_span(label: &str, selected: bool) -> Span<'static> {
    Span::styled(
        format!(" {label} "),
        if selected {
            Style::default()
                .fg(ACCENT)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED)
        } else {
            Style::default().fg(MUTED)
        },
    )
}

fn fit_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let mut truncated: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    truncated.push('…');
    truncated
}

impl Panel {
    fn hint(&self, fallback: &str) -> Line<'static> {
        if let Some(error) = &self.error {
            Line::styled(
                error.rsplit(": ").next().unwrap_or(error).to_owned(),
                Style::default().fg(crate::tui::transcript_style::ERROR_BORDER),
            )
        } else {
            Line::styled(format!(" {fallback}"), Style::default().fg(MUTED))
        }
    }

    fn border_color(&self, focus: Focus) -> Color {
        if self.focus == focus { ACCENT } else { MUTED }
    }

    fn layout(&mut self, tab: Tab, width: u16) -> &BodyLayout {
        let content: &str = match tab {
            Tab::Plan => &self.document.content,
            Tab::Review => self
                .report
                .as_ref()
                .map_or("", |report| report.content.as_str()),
        };
        let slot = &mut self.layouts[tab.index()];
        if slot.as_ref().is_none_or(|cached| cached.width != width) {
            let (lines, headings) = markdown::parse(content).layout_with_headings(width);
            *slot = Some(BodyLayout {
                width,
                lines,
                headings,
            });
        }
        slot.as_ref().expect("正文布局已初始化")
    }

    fn status_label(&self) -> (&'static str, Color) {
        match self.document.status {
            PlanStatus::Draft => ("Draft", DIM),
            PlanStatus::Ready => ("Ready", ACCENT),
            PlanStatus::Executed => ("Executed", MUTED),
            PlanStatus::Completed => ("Completed", MUTED),
            PlanStatus::Abandoned => ("Abandoned", MUTED),
        }
    }

    pub(in crate::tui) fn draw(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        progress: (usize, usize),
        usage: Option<&ContextUsage>,
    ) {
        if area.width < 6 || area.height < 3 {
            return;
        }
        let has_actions = self.token.is_some() && !self.readonly();
        let actions_height = if has_actions && area.height >= 7 {
            (ACTIONS.len() as u16 + 3).min(area.height - 4)
        } else {
            0
        };
        let preview_area = Rect::new(
            area.x,
            area.y,
            area.width,
            area.height
                .saturating_sub(actions_height + u16::from(actions_height > 0)),
        );
        let actions_area = Rect::new(
            area.x,
            area.bottom() - actions_height,
            area.width,
            actions_height,
        );
        // 左侧固定列宽，窄窗口收起目录，确保正文仍有可用空间。
        let toc_width = if area.width >= 60 {
            26.min(area.width / 3)
        } else {
            0
        };
        self.toc_visible = toc_width > 0;
        if !self.toc_visible && self.focus == Focus::Toc {
            self.focus = Focus::Body;
        }
        let toc_area = Rect::new(area.x, preview_area.y, toc_width, preview_area.height);
        let gap = u16::from(toc_width > 0);
        let preview = Rect::new(
            area.x + toc_width + gap,
            preview_area.y,
            area.width.saturating_sub(toc_width + gap),
            preview_area.height,
        );
        let title = if self.token.is_none() || self.readonly() {
            format!(" Plan · {}/{} ", progress.0, progress.1)
        } else {
            format!(" Plan · r{} ", self.document.revision)
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Plain)
            .border_style(Style::default().fg(self.border_color(Focus::Body)))
            .title(Span::styled(
                title,
                Style::default()
                    .fg(self.border_color(Focus::Body))
                    .add_modifier(Modifier::BOLD),
            ));
        let mut body = block.inner(preview);
        frame.render_widget(block, preview);

        // 头部：Plan 标签显示相对路径 + 状态；Review 标签显示报告修订与
        // 模型来源，旧修订报告明确标注 outdated（宽度不足时截断模型引用）。
        if body.height > 0 {
            let (label, color) = self.status_label();
            let mut spans = Vec::new();
            match (&self.tab, &self.report) {
                (Tab::Review, Some(report)) => {
                    let prefix = format!(" Review r{}", report.revision);
                    let outdated = report.revision < self.document.revision;
                    let suffix_len = usize::from(outdated) * " · outdated".len();
                    let budget = (body.width as usize)
                        .saturating_sub(prefix.chars().count() + 3 + suffix_len);
                    spans.push(Span::styled(prefix, Style::default().fg(MUTED)));
                    spans.push(Span::raw(" · "));
                    spans.push(Span::styled(
                        fit_chars(&report.model_reference, budget),
                        Style::default().fg(MUTED),
                    ));
                    if outdated {
                        spans.push(Span::styled(
                            " · outdated",
                            Style::default().fg(DIM).add_modifier(Modifier::BOLD),
                        ));
                    }
                }
                _ => {
                    // 未保存的新绑定不显示假路径。
                    let path = self
                        .document
                        .path()
                        .unwrap_or_else(|| "Not saved yet".to_owned());
                    spans.push(Span::styled(format!(" {path}"), Style::default().fg(MUTED)));
                }
            }
            spans.push(Span::raw(" · "));
            spans.push(Span::styled(
                label,
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ));
            let line = Line::from(spans);
            frame.render_widget(line, Rect::new(body.x, body.y, body.width, 1));
            body.y += 1;
            body.height -= 1;
        }

        // 报告标签行；没有报告时不占用行高。
        if self.report.is_some() && body.height > 0 {
            frame.render_widget(
                Line::from(vec![
                    choice_span("Plan", self.tab == Tab::Plan),
                    choice_span("Review", self.tab == Tab::Review),
                ]),
                Rect::new(body.x, body.y, body.width, 1),
            );
            body.y += 1;
            body.height -= 1;
        }

        // 只读提示留在预览内部；五项操作在独立外框中绘制。
        let footer_height = u16::from(!has_actions).min(body.height);
        let footer = Rect::new(
            body.x,
            body.y + body.height.saturating_sub(footer_height),
            body.width,
            footer_height,
        );
        body.height -= footer_height;

        // 正文：markdown 布局缓存 + 滚动。
        let viewport = body.height as usize;
        self.viewport = viewport;
        let tab = self.tab;
        let slot_scroll = self.scroll[tab.index()];
        let empty_plan = self.document.content.is_empty();
        let layout = self.layout(tab, body.width);
        let total = layout.lines.len();
        let scroll = slot_scroll.min(total.saturating_sub(viewport));
        if body.height > 0 {
            if total == 0 && tab == Tab::Plan && empty_plan {
                frame.render_widget(
                    Line::styled("No plan content yet.", Style::default().fg(MUTED)),
                    body,
                );
            } else {
                for (row, line) in layout.lines.iter().enumerate().skip(scroll).take(viewport) {
                    frame.render_widget(
                        line,
                        Rect::new(body.x, body.y + (row - scroll) as u16, body.width, 1),
                    );
                }
            }
        }
        // 布局借用结束后再落滚动位置，下一帧继续从此处显示。
        self.scroll[tab.index()] = scroll;
        if toc_width > 0 {
            self.draw_toc(frame, toc_area);
        }

        if footer_height > 0 {
            frame.render_widget(self.hint(READONLY_HINT), footer);
        }
        if actions_height > 0 {
            self.draw_actions(frame, actions_area, usage);
        }
    }

    fn draw_toc(&mut self, frame: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(self.border_color(Focus::Toc)))
            .title(" TOC ");
        let inner = block.inner(area);
        frame.render_widget(block, area);
        self.toc_viewport = usize::from(inner.height);
        let index = self.tab.index();
        let Some(layout) = &self.layouts[index] else {
            return;
        };
        if layout.headings.is_empty() {
            self.toc_selected[index] = 0;
            if self.focus == Focus::Toc {
                self.focus = Focus::Body;
            }
            frame.render_widget(
                Line::styled("No headings", Style::default().fg(MUTED)),
                inner,
            );
            return;
        }
        let active = layout
            .headings
            .partition_point(|heading| heading.row <= self.scroll[index])
            .saturating_sub(1);
        let selected = if self.focus == Focus::Toc {
            self.toc_selected[index].min(layout.headings.len() - 1)
        } else {
            active
        };
        self.toc_selected[index] = selected;
        let first = selected.saturating_add(1).saturating_sub(self.toc_viewport);
        let base_level = layout
            .headings
            .iter()
            .map(|heading| heading.level)
            .min()
            .unwrap_or(1);
        for (row, (position, heading)) in layout
            .headings
            .iter()
            .enumerate()
            .skip(first)
            .take(self.toc_viewport)
            .enumerate()
        {
            let style = if position == selected {
                let style = Style::default().fg(ACCENT).add_modifier(Modifier::BOLD);
                if self.focus == Focus::Toc {
                    style.add_modifier(Modifier::REVERSED)
                } else {
                    style
                }
            } else {
                Style::default().fg(MUTED)
            };
            let prefix = if position == selected { "❯ " } else { "  " };
            let indent = "  ".repeat(usize::from(heading.level.saturating_sub(base_level)));
            frame.render_widget(
                Line::styled(format!("{prefix}{indent}{}", heading.title), style),
                Rect::new(inner.x, inner.y + row as u16, inner.width, 1),
            );
        }
    }

    fn draw_actions(&self, frame: &mut Frame, area: Rect, usage: Option<&ContextUsage>) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(self.border_color(Focus::Actions)))
            .title(Span::styled(
                " Actions ",
                Style::default()
                    .fg(self.border_color(Focus::Actions))
                    .add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let show_hint = inner.height > ACTIONS.len() as u16;
        let visible = usize::from(inner.height.saturating_sub(u16::from(show_hint)));
        // 短窗口只显示部分操作时，仍保证当前选项处于可见范围。
        let first = self.action.saturating_add(1).saturating_sub(visible);
        for (row, (index, label)) in ACTIONS
            .iter()
            .enumerate()
            .skip(first)
            .take(visible)
            .enumerate()
        {
            let selected = self.action == index && self.action_enabled(index);
            let style = if !self.action_enabled(index) {
                Style::default().fg(DIM)
            } else if selected {
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let marker = if selected { "❯ " } else { "  " };
            let context = if index == 1 {
                let used = usage.map_or_else(
                    || "?".to_owned(),
                    |usage| abbreviate_tokens(usage.estimated_tokens),
                );
                let window = usage
                    .and_then(|usage| usage.context_window)
                    .map_or_else(|| "?".to_owned(), abbreviate_tokens);
                format!(" ({used}/{window})")
            } else {
                String::new()
            };
            frame.render_widget(
                Line::styled(format!("{marker}{label}{context}"), style),
                Rect::new(inner.x, inner.y + row as u16, inner.width, 1),
            );
        }
        if show_hint {
            frame.render_widget(
                self.hint(HINT),
                Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::*;
    use super::super::{ACTIONS, Action, Focus, Panel};
    use crossterm::event::KeyCode;
    use pp_core::plan::PlanStatus;

    #[test]
    fn plan_preview_and_actions_have_separate_frames_and_spacing() {
        let mut panel = review_panel(PlanStatus::Ready);
        let rows = render_panel(&mut panel, 26);
        assert!(rows[0].starts_with("┌ TOC"));
        assert!(rows[0].contains("┌ Plan"));
        assert!(rows[16].starts_with('└'));
        assert!(rows[17].trim().is_empty());
        assert!(rows[18].starts_with("╭ Actions"));
        assert!(rows[25].starts_with('╰'));
        for (index, label) in ACTIONS.iter().enumerate() {
            assert!(rows[19 + index].contains(label));
        }
        let mut completed = Panel::progress(document(PlanStatus::Completed, "- [x] step"));
        let rows = render_panel(&mut completed, 26);
        assert!(!rows.iter().any(|row| row.contains("Actions")));
        assert!(rows[25].starts_with('└'));
    }

    #[test]
    fn plan_toc_jump_uses_current_wrapping_and_tracks_body_scroll() {
        let content = format!(
            "# Overview\n\n{}\n\n## Implementation\n\n{}",
            "Long paragraph content. ".repeat(100),
            "- detail\n".repeat(40)
        );
        let mut panel = Panel::progress(document(PlanStatus::Executed, &content));
        let rows = render_panel(&mut panel, 14);
        assert!(rows[0].contains("TOC") && rows[0].contains("Plan"));
        assert!(rows.iter().any(|row| row.contains("    Implementation")));
        panel.key(key_event(KeyCode::Tab));
        assert_eq!(panel.focus, Focus::Toc);
        panel.key(key_event(KeyCode::Down));
        assert_eq!(panel.scroll[0], 0);
        for width in [100, 70] {
            render_panel_size(&mut panel, width, 14);
            assert_eq!(panel.key(key_event(KeyCode::Enter)), Action::None);
            let rows = render_panel_size(&mut panel, width, 14);
            let body_x = usize::from(26.min(width / 3) + 2);
            assert!(
                rows[2]
                    .chars()
                    .skip(body_x)
                    .collect::<String>()
                    .starts_with("Implementation")
            );
        }
        panel.key(key_event(KeyCode::Tab));
        assert_eq!(panel.focus, Focus::Body);
        panel.wheel(true, usize::MAX);
        render_panel(&mut panel, 14);
        assert_eq!(panel.scroll[0], 0);
        assert_eq!(panel.toc_selected[0], 0);
        panel.wheel(false, usize::MAX);
        render_panel(&mut panel, 14);
        assert!(panel.scroll[0] > 0);
        assert_eq!(panel.toc_selected[0], 1);
        panel.key(key_event(KeyCode::BackTab));
        assert_eq!(panel.focus, Focus::Toc);
        render_panel_size(&mut panel, 40, 14);
        assert_eq!(panel.focus, Focus::Body);
        assert!(!panel.toc_visible);
    }

    #[test]
    fn plan_toc_scrolls_long_outlines_and_handles_removed_headings() {
        let content = (0..30)
            .map(|index| format!("## Section {index}\n\nbody\n\n"))
            .collect::<String>();
        let mut panel = Panel::progress(document(PlanStatus::Ready, &content));
        render_panel(&mut panel, 10);
        panel.key(key_event(KeyCode::Tab));
        panel.key(key_event(KeyCode::End));
        let rows = render_panel(&mut panel, 10);
        assert!(rows.iter().any(|row| row.contains("❯ Section 29")));
        panel.key(key_event(KeyCode::Home));
        let rows = render_panel(&mut panel, 10);
        assert!(rows.iter().any(|row| row.contains("❯ Section 0")));
        panel.sync_document(&document(PlanStatus::Ready, "Content without headings"));
        let rows = render_panel(&mut panel, 10);
        assert!(rows.iter().any(|row| row.contains("No headings")));
        assert_eq!(panel.focus, Focus::Body);
        panel.key(key_event(KeyCode::Tab));
        assert_eq!(panel.focus, Focus::Body);
    }

    #[test]
    fn plan_short_actions_frame_keeps_the_selected_option_visible() {
        let mut panel = review_panel(PlanStatus::Ready);
        panel.key(key_event(KeyCode::Char('5')));
        let rows = render_panel(&mut panel, 10);
        assert!(
            rows.iter()
                .any(|row| row.contains("❯ Save draft and leave Plan mode"))
        );
        assert!(rows.last().unwrap().starts_with('╰'));
    }
}
