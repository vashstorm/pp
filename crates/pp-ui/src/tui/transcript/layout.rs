//! Physical line layout container: supports a shared immutable prefix `Arc` and zero-copy concatenation of a bounded preview tail.

use super::{Line, Range, Source};
use std::sync::Arc;

pub(in crate::tui) enum LayoutContent {
    Source {
        source: Source,
        range: Range<usize>,
        title_end: bool,
    },
    Styled(Line<'static>),
}

pub(in crate::tui) struct LayoutLine {
    pub(in crate::tui) content: LayoutContent,
    pub(in crate::tui) height: usize,
}

/// Real physical line layout: the prefix comes from a finished layout result and the preview appends only a bounded tail.
///
/// Prefix lines are never rewritten by the preview, so the scroll anchor and the lines frozen into native history stay stable; only the tail that
/// needs re-layout or continuation is regenerated, and the remaining prefix lines share one `Arc`.
pub(in crate::tui) struct EntryLayout {
    pub(super) width: u16,
    /// Sum of the physical heights of all lines (one source line may wrap into several screen rows).
    pub(in crate::tui) height: usize,
    prefix: Option<Prefix>,
    tail: Vec<LayoutLine>,
}

/// Reused laid-out prefix; `kept` is the number of lines retained from the prefix by zero-copy.
struct Prefix {
    layout: Arc<EntryLayout>,
    kept: usize,
}

impl Prefix {
    fn kept(&self) -> usize {
        self.kept
    }
}

impl EntryLayout {
    /// Fully owned layout: background layout results, non-streaming cards and native history re-layout.
    pub(in crate::tui) fn owned(width: u16, lines: Vec<LayoutLine>) -> Self {
        let height = lines.iter().map(|line| line.height).sum();
        Self {
            width,
            height,
            prefix: None,
            tail: lines,
        }
    }

    /// Reuse the first `kept` lines of the prefix and append the tail; the retained prefix lines share an `Arc` across frames and are neither copied nor rewritten.
    pub(in crate::tui) fn with_prefix(
        prefix: Arc<EntryLayout>,
        kept: usize,
        tail: Vec<LayoutLine>,
    ) -> Self {
        let total = prefix.len();
        let kept = kept.min(total);
        let prefix_height = if kept == total {
            prefix.height
        } else if kept == 0 {
            0
        } else if total - kept <= kept {
            let dropped: usize = (kept..total)
                .filter_map(|index| prefix.get(index))
                .map(|line| line.height)
                .sum();
            prefix.height - dropped
        } else {
            (0..kept)
                .filter_map(|index| prefix.get(index))
                .map(|line| line.height)
                .sum()
        };
        let tail_height: usize = tail.iter().map(|line| line.height).sum();
        Self {
            width: prefix.width,
            height: prefix_height + tail_height,
            prefix: (kept > 0).then_some(Prefix {
                layout: prefix,
                kept,
            }),
            tail,
        }
    }

    /// Append the preview tail after the prefix; `continuation` means the first tail line continues the last prefix line.
    pub(in crate::tui) fn appended(
        prefix: Arc<EntryLayout>,
        continuation: bool,
        tail: Vec<LayoutLine>,
    ) -> Self {
        let kept = if continuation {
            prefix.len().saturating_sub(1)
        } else {
            prefix.len()
        };
        Self::with_prefix(prefix, kept, tail)
    }

    pub(in crate::tui) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Number of lines; unlike the physical `height`, one source line may wrap into several rows.
    pub(in crate::tui) fn len(&self) -> usize {
        self.prefix.as_ref().map_or(0, Prefix::kept) + self.tail.len()
    }

    pub(in crate::tui) fn iter(&self) -> impl Iterator<Item = &LayoutLine> {
        (0..self.len()).map(move |index| self.get(index).expect("line index within layout bounds"))
    }

    pub(in crate::tui) fn get(&self, index: usize) -> Option<&LayoutLine> {
        let kept = self.prefix.as_ref().map_or(0, Prefix::kept);
        if index < kept {
            return self
                .prefix
                .as_ref()
                .and_then(|prefix| prefix.layout.get(index));
        }
        self.tail.get(index - kept)
    }
}
