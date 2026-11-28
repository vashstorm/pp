use super::{
    Activity, Arc, Buffer, EntryKind, EntryLayout, Frame, LayoutContent, LayoutLine, Line,
    NativeBatch, NativeState, Rect, Span, Transcript, Widget, wrap_block,
};

impl Transcript {
    pub(in crate::tui) fn render_pending(&self) -> bool {
        crate::tui::render_worker::enabled()
            && self.entries[self.native.entry..]
                .iter()
                .enumerate()
                .any(|(offset, entry)| {
                    matches!(entry.kind, EntryKind::Assistant | EntryKind::Think)
                        && !entry.render.exact
                        && !(offset == 0 && self.native.commit_layout.is_some())
                })
    }

    fn prepare_native_layout(&mut self, width: u16) {
        self.reflow_native(width);
        self.prepare_read_groups();
        for entry in self.entries[self.native.entry..]
            .iter_mut()
            .chain(self.activity.as_mut().and_then(Activity::summary_mut))
        {
            Self::layout_entry(entry, self.display, width, false);
        }
    }

    fn reflow_native(&mut self, width: u16) {
        let old_width = self.native.width;
        if old_width == width {
            return;
        }
        if old_width > 0 {
            let mut retained = std::collections::VecDeque::new();
            for cells in &self.native.retained {
                for line in reflow_row(cells, width) {
                    let mut buffer = Buffer::empty(Rect::new(0, 0, width, 1));
                    line.render(buffer.area, &mut buffer);
                    retained.push_back(buffer.content);
                }
            }
            self.native.retained = retained;
            if let Some(layout) = self.native.commit_layout.clone() {
                let mut lines = Vec::new();
                if let Some(entry) = self.entries.get(self.native.entry) {
                    let mut offset = self.native.row;
                    while offset < layout.height {
                        let count = (layout.height - offset).min(64) as u16;
                        let area = Rect::new(0, 0, old_width, count);
                        let mut buffer = Buffer::empty(area);
                        Self::render_entry_layouts(
                            std::iter::once((
                                entry,
                                Some(layout.as_ref()),
                                self.native.commit_full_rows,
                            )),
                            &mut buffer,
                            area,
                            offset,
                            false,
                        );
                        for cells in buffer.content.chunks(usize::from(old_width)) {
                            lines.extend(reflow_row(cells, width).into_iter().map(|line| {
                                LayoutLine {
                                    content: LayoutContent::Styled(line),
                                    height: 1,
                                }
                            }));
                        }
                        offset += usize::from(count);
                    }
                }
                if lines.is_empty() {
                    if self.native.row == layout.height && layout.height > 0 {
                        self.native
                            .retained
                            .push_back(Buffer::empty(Rect::new(0, 0, width, 1)).content);
                    }
                    self.native.entry += 1;
                    self.native.commit_layout = None;
                    self.native.commit_full_rows = false;
                } else {
                    self.native.commit_layout = Some(Arc::new(EntryLayout::owned(width, lines)));
                    self.native.commit_full_rows = true;
                }
                self.native.row = 0;
            }
            self.native.revision += 1;
            self.native_preview = None;
        }
        self.native.width = width;
    }

    fn native_height(&self, state: &NativeState) -> usize {
        let mut height = 0;
        for entry in self.entries[state.entry..]
            .iter()
            .chain(self.activity.as_ref().and_then(Activity::summary))
            .enumerate()
        {
            let (offset, entry) = entry;
            let rows = if offset == 0 {
                state.commit_layout.as_ref().or(entry.layout.as_ref())
            } else {
                entry.layout.as_ref()
            }
            .expect("layout initialized")
            .height;
            if rows > 0 {
                height += rows + 1;
            }
        }
        height.saturating_sub(1).saturating_sub(state.row)
    }

    fn native_frontier(&self, exiting: bool) -> usize {
        // Temporary argument previews must not be frozen into native history on exit either.
        let mut frontier = self
            .argument_previews
            .values()
            .copied()
            .min()
            .unwrap_or(self.entries.len());
        if crate::tui::render_worker::enabled()
            && let Some(index) = self
                .entries
                .iter()
                .enumerate()
                .skip(self.native.entry)
                .find_map(|(index, entry)| {
                    (matches!(entry.kind, EntryKind::Assistant | EntryKind::Think)
                        && !entry.render.exact
                        && !(index == self.native.entry && self.native.commit_layout.is_some()))
                    .then_some(index)
                })
        {
            frontier = frontier.min(index);
        }
        if exiting {
            return frontier;
        }
        for index in self
            .assistant
            .iter()
            .chain(self.thinking.iter())
            .chain(self.running_tools.iter().map(|(_, index)| index))
            // A background process may still be queried after it finishes; the original card is not frozen before the current turn wraps up.
            .chain(self.background_bash.values())
        {
            frontier = frontier.min(*index);
        }
        // Consecutive reads may also merge into one summary, so the group head cannot be frozen before the whole group closes.
        if self.has_live_activity() {
            if let Some(offset) =
                self.entries
                    .get(self.native.entry..frontier)
                    .and_then(|entries| {
                        entries
                            .iter()
                            .position(super::TranscriptEntry::awaits_subagent_injection)
                    })
            {
                frontier = self.native.entry + offset;
            }
            let mut index = self.entries.len();
            while index > 0 && self.entries[index - 1].tool.as_deref() == Some("read") {
                index -= 1;
            }
            frontier = frontier.min(index);
        }
        frontier
    }

    /// Prepare at most 64 lines at a time; the caller must acknowledge after the terminal output succeeds before the source cursor advances.
    pub(in crate::tui) fn next_native_batch(
        &mut self,
        width: u16,
        height: u16,
        exiting: bool,
    ) -> Option<NativeBatch> {
        if width == 0 || height == 0 {
            return None;
        }
        self.prepare_native_layout(width);
        let mut next = self.native.clone();
        let capacity = usize::from(height);
        let mut overflow =
            (next.retained.len() + self.native_height(&next)).saturating_sub(capacity);
        let mut rows = Vec::new();
        if overflow > 0 && !next.retained.is_empty() {
            for _ in 0..overflow.min(64).min(next.retained.len()) {
                rows.push(next.retained.pop_front().expect("retained row"));
            }
        } else {
            if (overflow == 0 && next.row == 0) || next.entry >= self.native_frontier(exiting) {
                return None;
            }
            let entry = &self.entries[next.entry];
            if next.commit_layout.is_none() {
                next.commit_layout = entry.layout.clone();
            }
            let entry_height = next
                .commit_layout
                .as_ref()
                .expect("layout initialized")
                .height;
            // Group members without a body produce no separator line.
            let span = if entry_height == 0 {
                0
            } else {
                entry_height
                    + usize::from(next.entry + 1 < self.entries.len() || self.activity.is_some())
            };
            let count = span.saturating_sub(next.row).min(64);
            if count > 0 {
                let area = Rect::new(0, 0, width, count as u16);
                let mut buffer = Buffer::empty(area);
                Self::render_entry_layouts(
                    std::iter::once((entry, next.commit_layout.as_deref(), next.commit_full_rows)),
                    &mut buffer,
                    area,
                    next.row,
                    false,
                );
                for row in buffer.content.chunks(usize::from(width)) {
                    next.retained.push_back(row.to_vec());
                }
                next.row += count;
            }
            if next.row >= span {
                next.entry += 1;
                next.row = 0;
                next.commit_layout = None;
                next.commit_full_rows = false;
            }
            overflow = (next.retained.len() + self.native_height(&next)).saturating_sub(capacity);
            for _ in 0..overflow.min(next.retained.len()) {
                rows.push(next.retained.pop_front().expect("retained row"));
            }
        }
        Some(NativeBatch {
            generation: self.native_generation,
            revision: self.native.revision,
            next,
            rows,
        })
    }

    pub(in crate::tui) fn preview_native_batch(&mut self, batch: &NativeBatch) {
        self.native_preview = Some(batch.next.clone());
    }

    pub(in crate::tui) fn acknowledge_native_batch(&mut self, batch: NativeBatch) {
        assert_eq!(
            batch.generation, self.native_generation,
            "history batch belongs to another session"
        );
        assert_eq!(
            batch.revision, self.native.revision,
            "history batch is stale"
        );
        self.native = batch.next;
        self.native.revision += 1;
        self.native_preview = None;
    }

    pub(in crate::tui) fn draw_native(&mut self, frame: &mut Frame, area: Rect) {
        self.viewport_width = area.width;
        self.viewport_bottom = area.bottom();
        self.viewport_height = usize::from(area.height);
        if area.width == 0 || area.height == 0 {
            return;
        }
        self.prepare_native_layout(area.width);
        let state = self.native_preview.as_ref().unwrap_or(&self.native);
        let live = self.native_height(state);
        let retained = state
            .retained
            .len()
            .min(self.viewport_height.saturating_sub(live));
        for (row, cells) in state
            .retained
            .iter()
            .skip(state.retained.len() - retained)
            .enumerate()
        {
            crate::tui::surface::copy_row(
                cells,
                frame.buffer_mut(),
                area.x,
                area.y + row as u16,
                area.width,
            );
        }
        let tail = Rect::new(
            area.x,
            area.y + retained as u16,
            area.width,
            area.height - retained as u16,
        );
        Self::render_entry_layouts(
            self.entries[state.entry..]
                .iter()
                .enumerate()
                .map(|(offset, entry)| {
                    (
                        entry,
                        if offset == 0 {
                            state.commit_layout.as_deref()
                        } else {
                            None
                        },
                        offset == 0 && state.commit_full_rows,
                    )
                })
                .chain(
                    self.activity
                        .as_ref()
                        .and_then(Activity::summary)
                        .map(|entry| (entry, None, false)),
                ),
            frame.buffer_mut(),
            tail,
            state.row + live.saturating_sub(usize::from(tail.height)),
            false,
        );
    }
}

fn cell_line(cells: &[ratatui::buffer::Cell]) -> Line<'static> {
    let end = cells
        .iter()
        .rposition(|cell| !cell.symbol().trim().is_empty())
        .map_or(0, |index| index + 1);
    let mut spans = Vec::new();
    let mut index = 0;
    while index < end {
        let cell = &cells[index];
        let text = cell.symbol();
        let columns = Span::raw(text).width().max(1);
        spans.push(Span::styled(text.to_owned(), cell.style()));
        index += columns;
    }
    Line::from(spans).style(
        cells
            .first()
            .map_or_else(Default::default, |cell| cell.style()),
    )
}

fn reflow_row(cells: &[ratatui::buffer::Cell], width: u16) -> Vec<Line<'static>> {
    let line = cell_line(cells);
    let style = line.style;
    wrap_block(line, width, "")
        .into_iter()
        .map(|line| line.style(style))
        .collect()
}
