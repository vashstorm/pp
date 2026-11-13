//! 列表、搜索行与确认框的绘制，以及路径／分类／时间格式化。

use super::*;

impl Panel {
    pub(in crate::tui) fn draw(&mut self, frame: &mut Frame, area: Rect) {
        frame.render_widget(Clear, area);
        if area.is_empty() {
            return;
        }
        frame.render_widget(
            Block::default()
                .borders(Borders::TOP)
                .border_style(Style::default().fg(MUTED))
                .title(Line::styled(
                    " Plan list ",
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                )),
            area,
        );
        if area.height < 2 {
            return;
        }
        let mut y = area.y + 1;
        let bottom = area.bottom();
        // 分类标签行：All / Drafts / Plans / Done。
        if y < bottom {
            let mut spans = Vec::with_capacity(8);
            for tab in [Tab::All, Tab::Drafts, Tab::Plans, Tab::Done] {
                spans.push(Span::styled(
                    format!(" {} ", tab.label()),
                    if tab == self.tab {
                        Style::default()
                            .fg(if tab == Tab::All { CODE } else { tab.color() })
                            .add_modifier(Modifier::BOLD | Modifier::REVERSED)
                    } else {
                        Style::default().fg(tab.color())
                    },
                ));
                spans.push(Span::raw(" "));
            }
            frame.render_widget(Line::from(spans), Rect::new(area.x, y, area.width, 1));
            y += 1;
        }
        // 输入行：重命名编辑器优先，其余状态始终展示搜索入口。
        let mut cursor = None;
        if y < bottom.saturating_sub(1) {
            if let Some(name) = &self.rename {
                let line = format!("rename> {name}");
                frame.render_widget(
                    Line::styled(line.clone(), Style::default().fg(ACCENT)),
                    Rect::new(area.x, y, area.width, 1),
                );
                cursor = Some((area.x + 8 + Line::from(name.as_str()).width() as u16, y));
            } else {
                cursor = self.draw_search(frame, Rect::new(area.x, y, area.width, 1));
            }
            y += 1;
        }
        // 错误行：保留旧列表与输入，仅展示可见提示。
        if y < bottom
            && let Some(error) = &self.error
        {
            frame.render_widget(
                Line::styled(error.clone(), Style::default().fg(REMOVED)),
                Rect::new(area.x, y, area.width, 1),
            );
            y += 1;
        }
        // 底部提示与诊断；诊断最多占两行，超出截断。
        let diagnostics = self.listing.diagnostics.len().min(2) as u16;
        let hint_y = bottom.saturating_sub(1 + diagnostics);
        let list_bottom = hint_y;
        if y < list_bottom {
            let list_area = Rect::new(area.x, y, area.width, list_bottom - y);
            self.draw_entries(frame, list_area);
        }
        for (index, diagnostic) in self.listing.diagnostics.iter().take(2).enumerate() {
            let y = hint_y.saturating_sub(diagnostics) + index as u16;
            frame.render_widget(
                Line::styled(
                    display_text(diagnostic).into_owned(),
                    Style::default().fg(DIM),
                ),
                Rect::new(area.x, y, area.width, 1),
            );
        }
        let hint = if self.rename.is_some() {
            RENAME_HINT
        } else if self.confirm.is_some() {
            CONFIRM_HINT
        } else if self.searching {
            SEARCH_HINT
        } else {
            HINT
        };
        frame.render_widget(
            Line::styled(format!(" {hint}"), Style::default().fg(DIM)),
            Rect::new(area.x, bottom - 1, area.width, 1),
        );
        if self.confirm.is_some() {
            self.draw_confirmation(frame, area);
        }
        if let Some((x, y)) = cursor {
            frame.set_cursor_position((x, y));
        }
    }

    fn draw_search(&self, frame: &mut Frame, area: Rect) -> Option<(u16, u16)> {
        let prefix = "Search: ";
        // 编辑时至少为光标留一列；窄窗口优先截短前缀。
        let prefix_width =
            (prefix.len() as u16).min(area.width.saturating_sub(u16::from(self.searching)));
        frame.render_widget(
            Line::styled(prefix, Style::default().fg(MUTED)),
            Rect::new(area.x, area.y, prefix_width, 1),
        );
        let input = Rect::new(area.x + prefix_width, area.y, area.width - prefix_width, 1);
        if !self.searching && self.query.is_empty() {
            frame.render_widget(
                Line::styled("Press / to search", Style::default().fg(DIM)),
                input,
            );
            return None;
        }
        let available = usize::from(input.width.saturating_sub(u16::from(self.searching)));
        let mut start = self.query.len();
        let mut width = 0;
        // 按完整字素和屏幕列截取尾部，避免拆开中文、组合字符或 emoji。
        for (index, grapheme) in self.query.grapheme_indices(true).rev() {
            let next_width = width + Span::raw(grapheme).width();
            if next_width > available {
                break;
            }
            start = index;
            width = next_width;
        }
        frame.render_widget(
            Line::styled(&self.query[start..], Style::default().fg(ACCENT)),
            input,
        );
        self.searching.then_some((input.x + width as u16, input.y))
    }

    /// 居中确认删除框：先清空框内区域再画边框，避免背后的列表行透出；
    /// 内容为完整相对路径、固定提示与默认 Cancel 按钮行。
    fn draw_confirmation(&mut self, frame: &mut Frame, area: Rect) {
        if area.width < 28 || area.height < 9 {
            return;
        }
        let width = area.width.saturating_sub(4).min(72);
        let height = 7.min(area.height.saturating_sub(2));
        let area = Rect {
            x: area.x + (area.width - width) / 2,
            y: area.y + (area.height - height) / 2,
            width,
            height,
        };
        // 列表行在同一帧中先绘制，Block 只画边框、正文只占实际列宽，
        // 不先清理会让行内容透到框内空白行与正文尾部。
        frame.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(REMOVED))
            .title(Span::styled(
                " Delete plan? ",
                Style::default().fg(REMOVED).add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let Some(confirm) = &mut self.confirm else {
            return;
        };
        if confirm.width != Some(inner.width) {
            let mut lines = wrap_block(
                Line::raw(format!("Path: {}", confirm.path)),
                inner.width,
                "",
            );
            lines.extend(wrap_block(Line::raw(DELETE_NOTICE), inner.width, ""));
            confirm.lines = lines;
            confirm.width = Some(inner.width);
        }
        if inner.height > 1 {
            for (row, line) in confirm
                .lines
                .iter()
                .take(usize::from(inner.height - 1))
                .enumerate()
            {
                frame.render_widget(
                    line.clone(),
                    Rect::new(inner.x, inner.y + row as u16, inner.width, 1),
                );
            }
        }
        // 说明可能折行；按钮行固定在框底，空间不足时优先保证可见。
        let button = |label: &str, selected: bool| {
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
        };
        frame.render_widget(
            Line::from(vec![
                button("Cancel", !confirm.delete_selected),
                Span::raw("  "),
                button("Delete", confirm.delete_selected),
            ]),
            Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
        );
    }

    fn draw_entries(&mut self, frame: &mut Frame, area: Rect) {
        let phase = self.phase;
        if let Some(label) = phase.busy_label() {
            frame.render_widget(
                Paragraph::new(label).style(Style::default().fg(ACCENT)),
                area,
            );
            return;
        }
        if self.filtered.is_empty() {
            let message = if !self.query.is_empty() {
                "No matching plans."
            } else if self.tab != Tab::All {
                "No plans in this category."
            } else {
                EMPTY_NEW_PLAN
            };
            frame.render_widget(
                Paragraph::new(message).style(Style::default().fg(MUTED)),
                area,
            );
            return;
        }
        self.page_size = usize::from(area.height.max(1));
        if self.selected < self.offset {
            self.offset = self.selected;
        }
        if self.selected >= self.offset + self.page_size {
            self.offset = self.selected + 1 - self.page_size;
        }
        self.offset = self
            .offset
            .min(self.filtered.len().saturating_sub(self.page_size));
        let now = SystemTime::now();
        for (row, &index) in self
            .filtered
            .iter()
            .enumerate()
            .skip(self.offset)
            .take(self.page_size)
        {
            let y = area.y + (row - self.offset) as u16;
            if y >= area.bottom() {
                break;
            }
            let entry = &self.listing.entries[index];
            let selected = row == self.selected;
            let marker = if selected { "❯ " } else { "  " };
            let row_style = if selected {
                Style::default().fg(SELECTED_FG).bg(SELECTED_BG)
            } else {
                Style::default().bg(if row % 2 == 0 {
                    ROW_BG
                } else {
                    ALTERNATE_ROW_BG
                })
            };
            // 选中行统一使用深色文字，保证时间、标题与状态在浅蓝底上清晰可读。
            let text_style = |color| {
                if selected {
                    Style::default()
                } else {
                    Style::default().fg(color)
                }
            };
            let age = format!("{:<10}  ", format_age(entry.modified, now));
            let prefix_width = 2 + age.len();
            let mut spans = vec![Span::raw(marker), Span::styled(age, text_style(AGE_FG))];
            if let Some(error) = &entry.error {
                spans.push(Span::styled(
                    display_text(&entry.file_name).into_owned(),
                    text_style(REMOVED),
                ));
                spans.push(Span::styled(
                    format!("  {}", display_text(error)),
                    text_style(REMOVED),
                ));
            } else {
                let badge = if self.tab == Tab::All {
                    format!(" [{}]", category_label(entry.location))
                } else {
                    String::new()
                };
                let title_width =
                    usize::from(area.width).saturating_sub(prefix_width + badge.len());
                let color_index = entry.id.bytes().fold(0usize, |hash, byte| {
                    hash.wrapping_mul(31).wrapping_add(usize::from(byte))
                }) % TITLE_COLORS.len();
                spans.push(Span::styled(
                    truncate_title(&display_text(&entry.title), title_width),
                    text_style(TITLE_COLORS[color_index]),
                ));
                spans.push(Span::styled(
                    badge,
                    text_style(category_color(entry.location)),
                ));
            }
            frame.render_widget(
                Paragraph::new(Line::from(spans)).style(row_style),
                Rect::new(area.x, y, area.width, 1),
            );
        }
    }
}

pub(super) fn relative_path(entry: &PlanListEntry) -> String {
    format!(".pp/{}/{}", entry.location.dir_name(), entry.file_name)
}

pub(super) fn category_label(location: PlanLocation) -> &'static str {
    match location {
        PlanLocation::Drafts => "Draft",
        PlanLocation::Plans => "Plan",
        PlanLocation::Done => "Done",
    }
}

pub(super) fn category_color(location: PlanLocation) -> Color {
    match location {
        PlanLocation::Drafts => ACCENT,
        PlanLocation::Plans => LINK,
        PlanLocation::Done => ADDED,
    }
}

fn truncate_title(title: &str, width: usize) -> String {
    if Line::from(title).width() <= width {
        return title.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let mut result = String::new();
    let mut used = 0;
    for grapheme in title.graphemes(true) {
        let next = Span::raw(grapheme).width();
        if used + next > width - 1 {
            break;
        }
        result.push_str(grapheme);
        used += next;
    }
    result.push('…');
    result
}

fn format_age(time: SystemTime, now: SystemTime) -> String {
    let seconds = now.duration_since(time).unwrap_or_default().as_secs();
    let (value, unit) = match seconds {
        0..60 => return "just now".into(),
        60..3600 => (seconds / 60, "m"),
        3600..86_400 => (seconds / 3600, "h"),
        _ => (seconds / 86_400, "d"),
    };
    format!("{value}{unit} ago")
}
