use super::{BrowseAnchor, EntryKind, Transcript};

impl Transcript {
    pub(super) fn capture_browse_anchor(&mut self) {
        self.browse_anchor = None;
        if self.follow_bottom {
            return;
        }
        // 几何来自上一帧的内容布局，流式事件使 layout 失效后仍能处理滚轮。
        let Some((entry, row)) = self.geometry.locate(self.offset) else {
            return;
        };
        self.browse_anchor = Some(BrowseAnchor { entry, row });
    }

    pub(super) fn ensure_browse_anchor(&mut self) {
        if !self.follow_bottom && self.browse_anchor.is_none() {
            self.capture_browse_anchor();
        }
    }

    /// 内容槽位数：全部条目，外加存在结束摘要时的一个虚拟槽。
    fn slot_count(&self) -> usize {
        self.entries.len()
            + usize::from(
                self.activity
                    .as_ref()
                    .is_some_and(|activity| !activity.running),
            )
    }

    pub(super) fn resolve_browse_anchor(&mut self) {
        let Some(mut anchor) = self.browse_anchor else {
            return;
        };
        let slots = self.slot_count();
        if slots == 0 {
            self.offset = 0;
            self.browse_anchor = None;
            return;
        }
        // 锚点槽位可能已消失（如新一轮开始后结束摘要槽撤下）：回落到最后一个槽位。
        anchor.entry = anchor.entry.min(slots - 1);
        let height = self.geometry.height(anchor.entry);
        let exact = self.entries.get(anchor.entry).is_none_or(|entry| {
            !matches!(entry.kind, EntryKind::Assistant | EntryKind::Think)
                || !crate::tui::render_worker::enabled()
                || entry.render.exact
        });
        // 临时预览不足以证明正文缩短，不能永久裁掉用户希望浏览的行。
        if exact {
            anchor.row = anchor.row.min(height);
        }
        self.offset = self
            .geometry
            .start(anchor.entry)
            .saturating_add(anchor.row.min(height));
        self.browse_anchor = Some(anchor);
    }
}
