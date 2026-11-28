use super::{
    Activity, Buffer, DisplayContext, EntryKind, EntryStatus, LayoutContent, Paragraph, Rect,
    Source, Style, Transcript, TranscriptEntry, Widget, Wrap, card, palette,
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
        let padding = card::entry_padding(entry, compact, width);
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
            let read_group: Vec<card::ReadItem> = self.entries[index..end]
                .iter()
                .map(|entry| {
                    let (path, meta) = entry.read_path_parts();
                    card::ReadItem { path, meta }
                })
                .collect();
            for (offset, entry) in self.entries[index..end].iter_mut().enumerate() {
                let hidden = offset > 0;
                let next_title = if hidden { None } else { Some(title.clone()) };
                let next_group = if hidden {
                    Vec::new()
                } else {
                    read_group.clone()
                };
                if entry.hidden != hidden || entry.display_title != next_title {
                    entry.hidden = hidden;
                    entry.display_title = next_title;
                    entry.read_group = next_group;
                    entry.layout = None;
                }
            }
            index = end;
        }
        self.read_groups_key = Some(key);
    }

    /// Used by full viewer replay: lay out the whole history from the beginning without geometry positioning.
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
            let total = layout.len();
            let mut seen_body = false;
            for (line_idx, line) in layout.iter().enumerate() {
                let is_chrome = matches!(
                    line.source(),
                    Some(Source::Top | Source::Divider | Source::Bottom)
                );
                let is_first_body = !is_chrome && !seen_body;
                if !is_chrome {
                    seen_body = true;
                }
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
                let background = card::entry_background(entry);
                let row_ctx = card::RowCtx {
                    index: line_idx,
                    total,
                    is_first_body,
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
                    card::paint_chrome(entry, line, row_ctx, row, area, buffer);
                }
                let target_dims = if compact {
                    (!matches!(
                        line.source(),
                        Some(Source::Top | Source::Divider | Source::Bottom)
                    ))
                    .then_some((0, area.width))
                } else {
                    card::text_target(entry, line, inset, area.width)
                };
                if let Some((x_offset, target_width)) = target_dims {
                    let target = Rect::new(area.x + x_offset, y, target_width, height);
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
