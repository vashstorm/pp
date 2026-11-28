//! Prefix geometry of the alternate screen: content slot heights, monotonic starts and the total sync incrementally by layout identity.
//!
//! The geometry is only used on the alternate-screen path (regular `draw`, the browse anchor and `reveal_entry`); main-screen
//! native history (`draw_native`) follows the commit cursor and frozen lines, and the tree view (`draw_tree`) uses its own
//! `TreeLayout`, so neither reads the state here.

use super::{Activity, EntryLayout};
use std::sync::Arc;

/// Geometry mapping one-to-one onto the content slots: every entry plus one virtual slot when a final summary exists.
///
/// `starts[i]` is the number of accumulated screen lines before the i-th slot: a zero-height slot occupies no line and
/// produces no gap; a positive-height slot additionally takes one gap line after it. `total` is the accumulated line count
/// of all slots minus the trailing gap, matching the existing rendering semantics, and overflow always uses saturating arithmetic.
#[derive(Default)]
pub(super) struct Geometry {
    heights: Vec<usize>,
    starts: Vec<usize>,
    total: usize,
    /// Layout snapshot per slot from the previous frame. It holds the `Arc` itself rather than a raw address and compares with
    /// `Arc::ptr_eq`, so a reused address after the old layout is freed is not mistaken for an unchanged layout.
    layouts: Vec<Option<Arc<EntryLayout>>>,
}

/// Whether two slot layouts are the same laid-out result; `None` means that slot currently has no layout.
fn same_layout(previous: &Option<Arc<EntryLayout>>, current: &Option<Arc<EntryLayout>>) -> bool {
    match (previous, current) {
        (Some(previous), Some(current)) => Arc::ptr_eq(previous, current),
        (None, None) => true,
        _ => false,
    }
}

impl Geometry {
    /// Line contribution of a slot: a positive-height slot's body plus one gap line; a zero-height slot takes no line.
    fn contribution(height: usize) -> usize {
        height + usize::from(height > 0)
    }

    /// Accumulated line count before slot `index`; `index` may equal the total number of slots.
    fn cumulative(&self, index: usize) -> usize {
        match index.checked_sub(1) {
            None => 0,
            Some(prev) => self.starts[prev].saturating_add(Self::contribution(self.heights[prev])),
        }
    }

    /// Sync to the current content slots; only the suffix is recomputed from the first slot that changed (count, identity or height).
    pub(super) fn sync<'a, I>(&mut self, slots: I)
    where
        I: Iterator<Item = (&'a Option<Arc<EntryLayout>>, usize)>,
    {
        let mut changed = false;
        let mut count = 0usize;
        let mut cum = 0usize;
        for (index, (layout, height)) in slots.enumerate() {
            count = index + 1;
            if !changed {
                let unchanged = index < self.layouts.len()
                    && self.heights[index] == height
                    && same_layout(&self.layouts[index], layout);
                if unchanged {
                    continue;
                }
                changed = true;
                // The verified-identical prefix is kept as is; heights and starts are recomputed from the current slot onwards.
                self.heights.truncate(index);
                self.starts.truncate(index);
                self.layouts.truncate(index);
                cum = self.cumulative(index);
            }
            self.starts.push(cum);
            self.heights.push(height);
            self.layouts.push(layout.clone());
            cum = cum.saturating_add(Self::contribution(height));
        }
        if !changed {
            // All slots match: only the surplus tail left by removed entries has to be truncated.
            self.heights.truncate(count);
            self.starts.truncate(count);
            self.layouts.truncate(count);
        }
        self.total = self.cumulative(count).saturating_sub(1);
    }

    /// Locate the slot owning line `offset` and the residual line inside it; the semantics match the renderer's skip scan:
    /// return the first positive-height slot that covers `offset` (including its gap line).
    pub(super) fn locate(&self, offset: usize) -> Option<(usize, usize)> {
        let end = self.starts.partition_point(|&start| start <= offset);
        let index = end.checked_sub(1)?;
        let height = *self.heights.get(index)?;
        if height == 0 {
            // Only when every slot has zero height is the last slot selected; there is then no renderable line.
            return None;
        }
        let skip = offset.saturating_sub(self.starts[index]);
        (skip <= height).then_some((index, skip))
    }

    /// Accumulated line count before slot `index`; `index` may equal the total number of slots.
    pub(super) fn start(&self, index: usize) -> usize {
        let index = index.min(self.heights.len());
        match self.starts.get(index) {
            Some(&start) => start,
            None => self.cumulative(index),
        }
    }

    /// Physical height of slot `index`; out-of-range indexes are treated as 0.
    pub(super) fn height(&self, index: usize) -> usize {
        self.heights.get(index).copied().unwrap_or(0)
    }

    pub(super) fn total(&self) -> usize {
        self.total
    }

    /// Cleared when the tree view takes over the display; a forced full sync follows once the regular view returns.
    pub(super) fn clear(&mut self) {
        self.heights.clear();
        self.starts.clear();
        self.layouts.clear();
        self.total = 0;
    }

    /// Test-only: verify that a new session carries no slot snapshot from the previous frame.
    #[cfg(test)]
    pub(super) fn is_empty(&self) -> bool {
        self.starts.is_empty()
    }
}

impl super::Transcript {
    /// Sync the current content slots into the geometry and refresh the total; every layout must already be generated.
    pub(super) fn sync_geometry(&mut self) {
        let slots = self
            .entries
            .iter()
            .chain(self.activity.as_ref().and_then(Activity::summary))
            .map(|entry| {
                (
                    &entry.layout,
                    entry.layout.as_ref().map_or(0, |layout| layout.height),
                )
            });
        self.geometry.sync(slots);
        self.total = self.geometry.total();
    }

    pub(in crate::tui) fn measure_height(&mut self, width: u16) -> usize {
        self.prepare_layout(width);
        self.sync_geometry();
        self.total
    }
}
