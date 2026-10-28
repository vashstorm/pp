use super::{Buffer, EntryKind, Frame, NativeBatch, NativeState, Rect, Transcript};

impl Transcript {
    fn prepare_native_layout(&mut self, width: u16) {
        self.prepare_read_groups();
        for entry in self.entries[self.native.entry..]
            .iter_mut()
            .chain(self.activity.iter_mut().map(|activity| &mut activity.entry))
        {
            Self::layout_entry(entry, self.display, width, false);
        }
    }

    fn native_height(&self, state: &NativeState) -> usize {
        let mut height = 0;
        for entry in self.entries[state.entry..]
            .iter()
            .chain(self.activity.iter().map(|activity| &activity.entry))
        {
            let rows = entry.layout.as_ref().expect("layout initialized").height;
            if rows > 0 {
                height += rows + 1;
            }
        }
        height.saturating_sub(1).saturating_sub(state.row)
    }

    fn native_frontier(&self, exiting: bool) -> usize {
        if exiting {
            return self.entries.len();
        }
        let mut frontier = self.entries.len();
        for index in self
            .assistant
            .iter()
            .chain(self.thinking.iter())
            .chain(self.running_tools.iter().map(|(_, index)| index))
        {
            frontier = frontier.min(*index);
        }
        // 相邻 read 还可能合并成同一摘要，整组关闭前不能冻结组首。
        if self
            .activity
            .as_ref()
            .is_some_and(|activity| activity.started.is_some())
        {
            let mut index = self.entries.len();
            while index > 0
                && self.entries[index - 1].tool.as_deref() == Some("read")
                // 指令卡片独立成卡，不会并入 read 摘要组。
                && self.entries[index - 1].kind != EntryKind::Read
            {
                index -= 1;
            }
            frontier = frontier.min(index);
        }
        frontier
    }

    /// 每次最多准备 64 行；调用者必须在终端输出成功后确认，才推进源游标。
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
            let entry_height = entry.layout.as_ref().expect("layout initialized").height;
            // 无正文的分组成员不产生分隔行。
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
                Self::render_entries(std::iter::once(entry), &mut buffer, area, next.row, false);
                for row in buffer.content.chunks(usize::from(width)) {
                    next.retained.push_back(row.to_vec());
                }
                next.row += count;
            }
            if next.row >= span {
                next.entry += 1;
                next.row = 0;
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
            "历史批次来自其他会话"
        );
        assert_eq!(batch.revision, self.native.revision, "历史批次已过期");
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
        Self::render_entries(
            self.entries[state.entry..]
                .iter()
                .chain(self.activity.iter().map(|activity| &activity.entry)),
            frame.buffer_mut(),
            tail,
            state.row + live.saturating_sub(usize::from(tail.height)),
            false,
        );
    }
}
