//! 物理行布局容器：支持共享不可变前缀 `Arc` 与有界预览尾部零拷贝拼接。

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

/// 真实物理行布局：前缀来自已完成的排版结果，预览只追加有界尾部。
///
/// 前缀行不被预览改写，因此滚动锚点和冻结进原生历史的行保持稳定；只有需要
/// 重排或续接的尾部会重新生成，其余前缀行共享同一个 `Arc`。
pub(in crate::tui) struct EntryLayout {
    pub(super) width: u16,
    /// 所有行的物理高度之和（一行源码文本可能折成多个屏幕行）。
    pub(in crate::tui) height: usize,
    prefix: Option<Prefix>,
    tail: Vec<LayoutLine>,
}

/// 复用的已排版前缀；`kept` 表示从前缀中零拷贝保留的行数。
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
    /// 完整自有的布局：后台排版结果、非流式卡片与原生历史重排。
    pub(in crate::tui) fn owned(width: u16, lines: Vec<LayoutLine>) -> Self {
        let height = lines.iter().map(|line| line.height).sum();
        Self {
            width,
            height,
            prefix: None,
            tail: lines,
        }
    }

    /// 复用前缀的前 `kept` 行并追加尾部；前缀保留行跨帧共享 `Arc`，不被复制或改写。
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

    /// 在前缀之后追加预览尾部；`continuation` 表示尾部首行承接前缀末行。
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

    /// 行数；与物理高度 `height` 不同，一行源码文本可能折成多行。
    pub(in crate::tui) fn len(&self) -> usize {
        self.prefix.as_ref().map_or(0, Prefix::kept) + self.tail.len()
    }

    pub(in crate::tui) fn iter(&self) -> impl Iterator<Item = &LayoutLine> {
        (0..self.len()).map(move |index| self.get(index).expect("行索引在布局范围内"))
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
