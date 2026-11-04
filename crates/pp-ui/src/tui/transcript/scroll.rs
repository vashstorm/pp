use super::{BrowseAnchor, EntryKind, Transcript};

impl Transcript {
    pub(super) fn remember_heights(&mut self) {
        self.displayed_heights.clear();
        self.displayed_heights.extend(
            self.entries
                .iter()
                .chain(self.activity.iter().map(|activity| &activity.entry))
                .map(|entry| entry.layout.as_ref().map_or(0, |layout| layout.height)),
        );
    }

    pub(super) fn capture_browse_anchor(&mut self) {
        self.browse_anchor = None;
        if self.follow_bottom {
            return;
        }
        let mut row = self.offset;
        for (entry, &height) in self.displayed_heights.iter().enumerate() {
            if height == 0 {
                continue;
            }
            if row <= height {
                self.browse_anchor = Some(BrowseAnchor { entry, row });
                break;
            }
            row -= height + 1;
        }
    }

    pub(super) fn ensure_browse_anchor(&mut self) {
        if !self.follow_bottom && self.browse_anchor.is_none() {
            self.capture_browse_anchor();
        }
    }

    pub(super) fn resolve_browse_anchor(&mut self) {
        let Some(mut anchor) = self.browse_anchor else {
            return;
        };
        let mut preceding = 0;
        let mut last = None;
        for (index, entry) in self
            .entries
            .iter()
            .chain(self.activity.iter().map(|a| &a.entry))
            .enumerate()
        {
            let height = entry.layout.as_ref().map_or(0, |layout| layout.height);
            let exact = !matches!(entry.kind, EntryKind::Assistant | EntryKind::Think)
                || !crate::tui::render_worker::enabled()
                || entry.render.exact;
            last = Some((index, preceding, height, exact));
            if index == anchor.entry {
                break;
            }
            if height > 0 {
                preceding += height + 1;
            }
        }
        if let Some((index, preceding, height, exact)) = last {
            anchor.entry = index;
            // 临时预览不足以证明正文缩短，不能永久裁掉用户希望浏览的行。
            if exact {
                anchor.row = anchor.row.min(height);
            }
            self.offset = preceding + anchor.row.min(height);
            self.browse_anchor = Some(anchor);
        } else {
            self.offset = 0;
            self.browse_anchor = None;
        }
    }
}
