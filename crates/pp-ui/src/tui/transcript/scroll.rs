use super::{BrowseAnchor, EntryKind, Transcript};

impl Transcript {
    pub(super) fn capture_browse_anchor(&mut self) {
        self.browse_anchor = None;
        if self.follow_bottom {
            return;
        }
        // The geometry comes from the previous frame's content layout, so wheel events still work after streaming invalidates the layout.
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

    /// Number of content slots: every entry plus one virtual slot when a final summary exists.
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
        // The anchor slot may be gone (for example the final summary slot is removed once a new turn starts): fall back to the last slot.
        anchor.entry = anchor.entry.min(slots - 1);
        let height = self.geometry.height(anchor.entry);
        let exact = self.entries.get(anchor.entry).is_none_or(|entry| {
            !matches!(entry.kind, EntryKind::Assistant | EntryKind::Think)
                || !crate::tui::render_worker::enabled()
                || entry.render.exact
        });
        // A transient preview is no proof that the body shrank, so lines the user wants to browse are not permanently trimmed.
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
