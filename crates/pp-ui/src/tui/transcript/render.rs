use super::{
    Activity, Buffer, DisplayContext, EntryKind, EntryStatus, LayoutContent, Modifier, Paragraph,
    Rect, Source, Span, Style, Transcript, TranscriptEntry, Widget, Wrap, bash_background,
    bash_border, eval_background, eval_border, palette, tool_display,
};

impl Transcript {
    pub(super) fn prepare_layout(&mut self, width: u16) {
        self.layout_entries(width, false);
    }

    pub(super) fn layout_entries(&mut self, width: u16, compact: bool) {
        self.prepare_read_groups();
        for entry in self
            .entries
            .iter_mut()
            .chain(self.activity.as_mut().and_then(Activity::summary_mut))
        {
            Self::layout_entry(entry, self.display, width, compact);
        }
    }

    pub(super) fn layout_entry(
        entry: &mut TranscriptEntry,
        context: DisplayContext,
        width: u16,
        compact: bool,
    ) -> usize {
        let display = DisplayContext {
            full: context.full || entry.display.full,
            ..context
        };
        if entry.display != display {
            entry.display = display;
            entry.layout = None;
        }
        let padding = if compact
            || entry.is_file_read()
            || matches!(entry.kind, EntryKind::Error | EntryKind::Notice)
        {
            0
        } else if matches!(entry.kind, EntryKind::Bash | EntryKind::Eval) || entry.is_search() {
            4
        } else {
            2
        };
        entry.ensure_layout(width.saturating_sub(padding).max(1));
        entry.layout.as_ref().expect("layout initialized").height
    }

    pub(super) fn prepare_read_groups(&mut self) {
        let key = (
            self.entries.len(),
            self.display.full,
            self.display.read_preview,
        );
        if self.read_groups_key == Some(key) {
            return;
        }
        let mut index = 0;
        while index < self.entries.len() {
            if self.entries[index].tool.as_deref() != Some("read") {
                index += 1;
                continue;
            }
            let mut end = index + 1;
            let grouped = !self.display.full
                && !self.entries[index].display.full
                && self.entries[index].status != EntryStatus::Failed;
            if grouped {
                while end < self.entries.len()
                    && self.entries[end].tool.as_deref() == Some("read")
                    && self.entries[end].status != EntryStatus::Failed
                    && !self.entries[end].display.full
                {
                    end += 1;
                }
            }
            let title = TranscriptEntry::read_summary(&self.entries[index..end]);
            for (offset, entry) in self.entries[index..end].iter_mut().enumerate() {
                let hidden = offset > 0;
                let next_title = if hidden { None } else { Some(title.clone()) };
                if entry.hidden != hidden || entry.display_title != next_title {
                    entry.hidden = hidden;
                    entry.display_title = next_title;
                    entry.layout = None;
                }
            }
            index = end;
        }
        self.read_groups_key = Some(key);
    }

    /// 完整查看器回放使用：从历史开头整段排版，不经过几何定位。
    #[cfg(test)]
    pub(super) fn render(&self, buffer: &mut Buffer, area: Rect, skip: usize) {
        Self::render_entries(
            self.entries
                .iter()
                .chain(self.activity.as_ref().and_then(Activity::summary)),
            buffer,
            area,
            skip,
            false,
        );
    }

    pub(super) fn render_entries<'a>(
        entries: impl Iterator<Item = &'a TranscriptEntry>,
        buffer: &mut Buffer,
        area: Rect,
        skip: usize,
        compact: bool,
    ) {
        Self::render_entry_layouts(
            entries.map(|entry| (entry, None, false)),
            buffer,
            area,
            skip,
            compact,
        );
    }

    pub(super) fn render_entry_layouts<'a>(
        entries: impl Iterator<Item = (&'a TranscriptEntry, Option<&'a super::EntryLayout>, bool)>,
        buffer: &mut Buffer,
        area: Rect,
        mut skip: usize,
        compact: bool,
    ) {
        let inset = if compact { 0 } else { area.width.min(2) };
        let mut y = area.y;
        let end = area.bottom();
        for (entry, frozen, full_rows) in entries {
            let layout = frozen
                .or(entry.layout.as_deref())
                .expect("layout initialized");
            if layout.height == 0 {
                continue;
            }
            if skip > layout.height {
                skip -= layout.height + 1;
                continue;
            }
            for line in layout.iter() {
                if skip >= line.height {
                    skip -= line.height;
                    continue;
                }
                if y == end {
                    return;
                }
                let height = (line.height - skip).min(usize::from(end - y)) as u16;
                if full_rows {
                    let line = entry.line(line);
                    buffer.set_style(Rect::new(area.x, y, area.width, height), line.style);
                    Paragraph::new(line)
                        .scroll((skip as u16, 0))
                        .render(Rect::new(area.x, y, area.width, height), buffer);
                    y += height;
                    skip = 0;
                    continue;
                }
                let bash = entry.kind == EntryKind::Bash;
                let eval = entry.kind == EntryKind::Eval;
                let edit = entry.is_edit();
                let file_read = entry.is_file_read();
                // 自带完整外框的条目：不再叠加左侧竖线，也不缩进正文。
                let framed = matches!(entry.kind, EntryKind::Error | EntryKind::Notice);
                let search = entry.is_search();
                let think = entry.kind == EntryKind::Think;
                let background = if bash {
                    Some(bash_background())
                } else if eval {
                    Some(eval_background())
                } else if edit {
                    Some(palette::edit_bg())
                } else if search {
                    Some(palette::rg_bg())
                } else if file_read {
                    None
                } else if matches!(entry.kind, EntryKind::Diff | EntryKind::Ask) {
                    Some(match entry.status {
                        EntryStatus::Running => palette::tool_pending_bg(),
                        EntryStatus::Complete => palette::tool_success_bg(),
                        EntryStatus::Failed => palette::tool_error_bg(),
                    })
                } else {
                    entry.kind.conversation_background()
                };
                for row in y..y + height {
                    if let Some(background) = background {
                        buffer.set_style(
                            Rect::new(area.x, row, area.width, 1),
                            Style::default().bg(background),
                        );
                    }
                    if compact {
                        continue;
                    }
                    if bash || eval || search {
                        let (left, right, label) = match line.source() {
                            Some(Source::Top) => ("╭", "╮", ""),
                            Some(Source::Divider) => ("├", "┤", " Output "),
                            Some(Source::Bottom) => ("╰", "╯", ""),
                            _ => ("│", "│", ""),
                        };
                        let card_bg = if bash {
                            bash_background()
                        } else if eval {
                            eval_background()
                        } else {
                            palette::rg_bg()
                        };
                        let border_color = if entry.status == EntryStatus::Failed {
                            palette::removed()
                        } else if bash {
                            bash_border()
                        } else if eval {
                            eval_border()
                        } else {
                            palette::rg_border()
                        };
                        let style = Style::default().fg(border_color).bg(card_bg);
                        let horizontal = matches!(
                            line.source(),
                            Some(Source::Top | Source::Divider | Source::Bottom)
                        );
                        if horizontal {
                            Span::styled("─".repeat(usize::from(area.width)), style)
                                .render(Rect::new(area.x, row, area.width, 1), buffer);
                        }
                        Span::styled(left, style).render(Rect::new(area.x, row, 1, 1), buffer);
                        if area.width > 1 {
                            Span::styled(right, style)
                                .render(Rect::new(area.right() - 1, row, 1, 1), buffer);
                        }
                        if !label.is_empty() && area.width > 4 {
                            let label_color = if bash || eval {
                                palette::text()
                            } else {
                                palette::muted()
                            };
                            Span::styled(label, Style::default().fg(label_color).bg(card_bg))
                                .render(Rect::new(area.x + 2, row, area.width - 4, 1), buffer);
                        }
                        if (bash || eval)
                            && matches!(line.source(), Some(Source::Bottom))
                            && area.width > 4
                        {
                            let available = usize::from(area.width.saturating_sub(6));
                            let warning_needed = !entry.footer_warning.is_empty();
                            let warning_width =
                                ratatui::text::Line::raw(entry.footer_warning.as_str()).width();
                            let footer_budget = if warning_needed {
                                available.saturating_sub(
                                    warning_width + if entry.footer.is_empty() { 0 } else { 3 },
                                )
                            } else {
                                available
                            };
                            let footer =
                                tool_display::clip(&entry.footer, footer_budget, warning_needed);
                            let footer_width = ratatui::text::Line::raw(footer.as_str()).width();
                            let separator = if footer.is_empty() { "" } else { " · " };
                            let remaining = available.saturating_sub(
                                footer_width + ratatui::text::Line::raw(separator).width(),
                            );
                            let warning = if warning_needed {
                                tool_display::clip(&entry.footer_warning, remaining, false)
                            } else {
                                String::new()
                            };
                            let mut spans = vec![Span::styled(
                                format!(" {footer}"),
                                Style::default().fg(palette::muted()).bg(card_bg),
                            )];
                            if !warning.is_empty() {
                                if !separator.is_empty() {
                                    spans.push(Span::styled(
                                        separator.to_owned(),
                                        Style::default().fg(palette::muted()).bg(card_bg),
                                    ));
                                }
                                spans.push(Span::styled(
                                    format!("{warning} "),
                                    Style::default().fg(palette::accent()).bg(card_bg),
                                ));
                            } else if !footer.is_empty() {
                                spans.push(Span::styled(
                                    " ",
                                    Style::default().fg(palette::muted()).bg(card_bg),
                                ));
                            }
                            ratatui::text::Line::from(spans)
                                .render(Rect::new(area.x + 2, row, area.width - 4, 1), buffer);
                        }
                        if bash && matches!(line.source(), Some(Source::Top)) && area.width > 4 {
                            let (status, color) = match entry.status {
                                EntryStatus::Running => ("running", palette::accent()),
                                EntryStatus::Complete => ("done", palette::added()),
                                EntryStatus::Failed => ("failed", palette::removed()),
                            };
                            let title = if entry.tool.as_deref() == Some("bash_task") {
                                "Bash task"
                            } else {
                                "Bash"
                            };
                            ratatui::text::Line::from(vec![
                                Span::styled(
                                    format!(" {title} "),
                                    Style::default().fg(palette::bash_title()).bg(card_bg),
                                ),
                                Span::styled(
                                    format!("· {status} "),
                                    Style::default().fg(color).bg(card_bg),
                                ),
                            ])
                            .render(Rect::new(area.x + 2, row, area.width - 4, 1), buffer);
                        } else if eval
                            && matches!(line.source(), Some(Source::Top))
                            && area.width > 4
                        {
                            let lang = tool_display::EvalLang::from_entry(
                                &entry.arguments,
                                entry.result.as_ref(),
                            );
                            let icons = super::super::themes::current_icons();
                            let (status, color) = match entry.status {
                                EntryStatus::Running => ("running", palette::accent()),
                                EntryStatus::Complete => ("done", palette::added()),
                                EntryStatus::Failed => ("failed", palette::removed()),
                            };
                            ratatui::text::Line::from(vec![
                                Span::styled(
                                    format!(" {}", lang.icon(icons)),
                                    Style::default()
                                        .fg(lang.icon_color())
                                        .bg(card_bg)
                                        .add_modifier(Modifier::BOLD),
                                ),
                                Span::styled(
                                    format!(" {} ", lang.label()),
                                    Style::default()
                                        .fg(lang.accent())
                                        .bg(card_bg)
                                        .add_modifier(Modifier::BOLD),
                                ),
                                Span::styled(
                                    format!("· {status} "),
                                    Style::default().fg(color).bg(card_bg),
                                ),
                            ])
                            .render(Rect::new(area.x + 2, row, area.width - 4, 1), buffer);
                        }
                    } else if edit {
                        let (left, right, horizontal, label) = match line.source() {
                            Some(Source::Title) => ("╭", "╮", true, ""),
                            Some(Source::Divider) => ("├", "┤", true, " Output "),
                            Some(Source::Bottom) => ("╰", "╯", true, ""),
                            _ => ("│", "│", false, ""),
                        };
                        let style = Style::default()
                            .fg(palette::edit_border())
                            .bg(palette::edit_bg());
                        if horizontal {
                            Span::styled("─".repeat(usize::from(area.width)), style)
                                .render(Rect::new(area.x, row, area.width, 1), buffer);
                        }
                        Span::styled(left, style).render(Rect::new(area.x, row, 1, 1), buffer);
                        if area.width > 1 {
                            Span::styled(right, style)
                                .render(Rect::new(area.right() - 1, row, 1, 1), buffer);
                        }
                        if !label.is_empty() && area.width > 4 {
                            Span::styled(
                                label,
                                Style::default()
                                    .fg(palette::read_text())
                                    .bg(palette::read_bg()),
                            )
                            .render(Rect::new(area.x + 2, row, area.width - 4, 1), buffer);
                        }
                    } else if !think
                        && !file_read
                        && !framed
                        && entry.kind != EntryKind::Activity
                        && background.is_none()
                        && !entry.is_wait_card()
                    {
                        let color = if entry.tool.is_some() {
                            palette::tool_border()
                        } else if matches!(
                            entry.body,
                            super::EntryBody::Tips | super::EntryBody::Help(_)
                        ) {
                            super::super::statusline::accent()
                        } else {
                            entry.kind.color()
                        };
                        Span::styled("│", Style::default().fg(color))
                            .render(Rect::new(area.x, row, 1, 1), buffer);
                    }
                }
                let padding = if compact || file_read || framed {
                    0
                } else if bash
                    || eval
                    || search
                    || (edit && matches!(line.source(), Some(Source::Title)))
                {
                    4
                } else {
                    inset
                };
                if area.width > padding
                    && !matches!(
                        line.source(),
                        Some(Source::Top | Source::Divider | Source::Bottom)
                    )
                {
                    let target = Rect::new(
                        area.x
                            + if file_read || framed {
                                0
                            } else if edit
                                && !compact
                                && !matches!(line.source(), Some(Source::Title))
                            {
                                1
                            } else {
                                inset
                            },
                        y,
                        area.width - padding,
                        height,
                    );
                    let text_color = if entry.kind == EntryKind::Assistant {
                        palette::md_text()
                    } else {
                        palette::text()
                    };
                    let paragraph =
                        Paragraph::new(entry.line(line)).style(Style::default().fg(text_color));
                    let paragraph = if matches!(line.content, LayoutContent::Source { .. }) {
                        paragraph.wrap(Wrap { trim: false })
                    } else {
                        paragraph
                    };
                    paragraph
                        .scroll((u16::try_from(skip).expect("bounded logical window"), 0))
                        .render(target, buffer);
                }
                y += height;
                skip = 0;
            }
            if y == end {
                return;
            }
            y += 1;
            skip = 0;
        }
    }
}
