//! 备用屏幕的前缀几何：内容槽位高度、单调起点与总量按布局身份增量同步。
//!
//! 几何只在备用屏幕路径（普通 `draw`、浏览锚点与 `reveal_entry`）使用；主屏幕
//! 原生历史（`draw_native`）沿用提交游标与冻结行，树视图（`draw_tree`）使用
//! 独立的 `TreeLayout`，两者都不读取这里的状态。

use super::{Activity, EntryLayout};
use std::sync::Arc;

/// 与内容槽位一一对应的几何：全部条目，外加存在结束摘要时的一个虚拟槽。
///
/// `starts[i]` 是第 i 个槽位之前的累计屏幕行数：高度为 0 的槽位既不占行也
/// 不产生 gap；正高度槽位之后额外占一行 gap。`total` 是全部槽位的累计行数
/// 去掉末尾 gap，与现有渲染语义一致，溢出一律使用饱和运算。
#[derive(Default)]
pub(super) struct Geometry {
    heights: Vec<usize>,
    starts: Vec<usize>,
    total: usize,
    /// 上一帧每个槽位的布局快照。持有 `Arc` 本体而不是裸地址，用 `Arc::ptr_eq`
    /// 比较，避免旧布局释放后地址复用被误判成未变化。
    layouts: Vec<Option<Arc<EntryLayout>>>,
}

/// 两个槽位布局是否为同一份排版结果；`None` 表示该槽位当前没有布局。
fn same_layout(previous: &Option<Arc<EntryLayout>>, current: &Option<Arc<EntryLayout>>) -> bool {
    match (previous, current) {
        (Some(previous), Some(current)) => Arc::ptr_eq(previous, current),
        (None, None) => true,
        _ => false,
    }
}

impl Geometry {
    /// 槽位对行数的贡献：正高度槽位正文加一行 gap，零高度槽位不占行。
    fn contribution(height: usize) -> usize {
        height + usize::from(height > 0)
    }

    /// 第 index 个槽位之前的累计行数；index 允许等于槽位总数。
    fn cumulative(&self, index: usize) -> usize {
        match index.checked_sub(1) {
            None => 0,
            Some(prev) => self.starts[prev].saturating_add(Self::contribution(self.heights[prev])),
        }
    }

    /// 同步到当前内容槽位；只从第一个发生变化（数量、身份或高度）的槽位重算后缀。
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
                // 已验证一致的前缀原样保留，从当前槽位起重算高度与起点。
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
            // 全部槽位一致：只需裁掉条目删除后多余的尾部。
            self.heights.truncate(count);
            self.starts.truncate(count);
            self.layouts.truncate(count);
        }
        self.total = self.cumulative(count).saturating_sub(1);
    }

    /// 定位 offset 行所属的槽位与槽内残余行数；语义与渲染的 skip 扫描一致：
    /// 返回第一个能覆盖 offset（含其 gap 行）的正高度槽位。
    pub(super) fn locate(&self, offset: usize) -> Option<(usize, usize)> {
        let end = self.starts.partition_point(|&start| start <= offset);
        let index = end.checked_sub(1)?;
        let height = *self.heights.get(index)?;
        if height == 0 {
            // 只有全部槽位都是零高度时末槽才会被选中；此时没有可渲染行。
            return None;
        }
        let skip = offset.saturating_sub(self.starts[index]);
        (skip <= height).then_some((index, skip))
    }

    /// 第 index 个槽位之前的累计行数；index 允许等于槽位总数。
    pub(super) fn start(&self, index: usize) -> usize {
        let index = index.min(self.heights.len());
        match self.starts.get(index) {
            Some(&start) => start,
            None => self.cumulative(index),
        }
    }

    /// 第 index 个槽位的物理高度；越界按 0 处理。
    pub(super) fn height(&self, index: usize) -> usize {
        self.heights.get(index).copied().unwrap_or(0)
    }

    pub(super) fn total(&self) -> usize {
        self.total
    }

    /// 树视图接管展示时清空；返回普通视图后强制一次全量同步。
    pub(super) fn clear(&mut self) {
        self.heights.clear();
        self.starts.clear();
        self.layouts.clear();
        self.total = 0;
    }

    /// 仅测试使用：验证新会话没有携带上一帧的槽位快照。
    #[cfg(test)]
    pub(super) fn is_empty(&self) -> bool {
        self.starts.is_empty()
    }
}

impl super::Transcript {
    /// 把当前内容槽位同步进几何并刷新总量；布局必须已经全部生成。
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
