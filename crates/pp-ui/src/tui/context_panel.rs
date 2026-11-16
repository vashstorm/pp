//! `/context` 上下文诊断独立 TUI 面板：
//! 顶部三行容量摘要 + 左侧 20×10 网格 + 右侧 12 行分类/容量列表，支持原地展开与收起内部条目。

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, MouseButton, MouseEvent, MouseEventKind,
};
use pp_core::text::display_text;
use pp_runtime::agent::context_diagnosis::{CategoryKey, ContextReport};
use ratatui::layout::Rect;

mod render;
#[cfg(test)]
mod tests;

pub(in crate::tui) enum Action {
    None,
    Close,
    Refresh,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RowKind {
    Category(usize),
    Child(usize, usize),
    AutoCompactBuffer,
    FreeSpace,
}

pub(in crate::tui) struct Panel {
    report: Option<ContextReport>,
    expanded: [bool; CategoryKey::ALL.len()],
    cursor: usize,
    scroll: usize,
    visible_height: usize,
    loading: bool,
    error: Option<String>,
    pub(super) area: Rect,
    list_area: Rect,
}

impl Panel {
    pub(in crate::tui) fn new() -> Self {
        Self {
            report: None,
            expanded: [false; CategoryKey::ALL.len()],
            cursor: 0,
            scroll: 0,
            visible_height: 1,
            loading: true,
            error: None,
            area: Rect::default(),
            list_area: Rect::default(),
        }
    }

    pub(in crate::tui) fn set_loading(&mut self) {
        self.loading = true;
        self.error = None;
    }

    pub(in crate::tui) fn set_error(&mut self, message: String) {
        self.loading = false;
        self.error = Some(display_text(&message).into_owned());
    }

    pub(in crate::tui) fn set_report(&mut self, report: ContextReport) {
        for (idx, expanded) in self.expanded.iter_mut().enumerate() {
            let has_items = report
                .categories
                .get(idx)
                .is_some_and(|cat| !cat.items.is_empty());
            if !has_items {
                *expanded = false;
            }
        }
        self.report = Some(report);
        self.loading = false;
        self.error = None;
        self.clamp_cursor();
    }

    pub(in crate::tui) fn handle(&mut self, event: Event) -> Action {
        match event {
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                self.handle_key(key)
            }
            Event::Mouse(mouse) => {
                self.handle_mouse(mouse);
                Action::None
            }
            _ => Action::None,
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> Action {
        if key.code == KeyCode::Esc && key.modifiers.is_empty() {
            return Action::Close;
        }
        if key.kind == KeyEventKind::Press
            && key.modifiers.is_empty()
            && key.code == KeyCode::Char('r')
        {
            self.set_loading();
            return Action::Refresh;
        }
        if !key.modifiers.is_empty() || self.report.is_none() {
            return Action::None;
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.move_cursor(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_cursor(1),
            KeyCode::PageUp => self.move_cursor(-(self.visible_height.max(1) as isize)),
            KeyCode::PageDown => self.move_cursor(self.visible_height.max(1) as isize),
            KeyCode::Home => {
                self.cursor = 0;
                self.scroll = 0;
            }
            KeyCode::End => {
                let len = self.visible_rows().len();
                if len > 0 {
                    self.cursor = len - 1;
                }
            }
            KeyCode::Enter | KeyCode::Char(' ') if key.kind == KeyEventKind::Press => {
                self.toggle_or_collapse_at_cursor();
            }
            KeyCode::Right => self.expand_at_cursor(),
            KeyCode::Left | KeyCode::Backspace => self.collapse_at_cursor(),
            _ => {}
        }
        Action::None
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) {
        if self.report.is_none() {
            return;
        }
        let pos = ratatui::layout::Position::new(mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) if self.list_area.contains(pos) => {
                let visual_row = usize::from(mouse.row.saturating_sub(self.list_area.y));
                let target_idx = self.scroll + visual_row;
                let rows = self.visible_rows();
                if let Some(&kind) = rows.get(target_idx) {
                    self.cursor = target_idx;
                    if let RowKind::Category(cat_idx) = kind {
                        self.toggle_category(cat_idx);
                    }
                }
            }
            MouseEventKind::ScrollUp => self.move_cursor(-1),
            MouseEventKind::ScrollDown => self.move_cursor(1),
            _ => {}
        }
    }

    pub(super) fn visible_rows(&self) -> Vec<RowKind> {
        let Some(report) = &self.report else {
            return Vec::new();
        };
        let mut rows = Vec::with_capacity(report.categories.len() + 2);
        for (cat_idx, cat) in report.categories.iter().enumerate() {
            rows.push(RowKind::Category(cat_idx));
            if self.expanded.get(cat_idx).copied().unwrap_or(false) {
                for item_idx in 0..cat.items.len() {
                    rows.push(RowKind::Child(cat_idx, item_idx));
                }
            }
        }
        rows.push(RowKind::AutoCompactBuffer);
        rows.push(RowKind::FreeSpace);
        rows
    }

    fn move_cursor(&mut self, delta: isize) {
        let len = self.visible_rows().len();
        if len == 0 {
            self.cursor = 0;
            self.scroll = 0;
            return;
        }
        self.cursor = (self.cursor as isize + delta).clamp(0, (len - 1) as isize) as usize;
    }

    fn toggle_or_collapse_at_cursor(&mut self) {
        let rows = self.visible_rows();
        match rows.get(self.cursor).copied() {
            Some(RowKind::Category(cat_idx)) => self.toggle_category(cat_idx),
            Some(RowKind::Child(cat_idx, _)) => self.collapse_category(cat_idx),
            _ => {}
        }
    }

    fn expand_at_cursor(&mut self) {
        let rows = self.visible_rows();
        if let Some(RowKind::Category(cat_idx)) = rows.get(self.cursor).copied()
            && self
                .report
                .as_ref()
                .and_then(|r| r.categories.get(cat_idx))
                .is_some_and(|c| !c.items.is_empty())
        {
            self.expanded[cat_idx] = true;
            self.clamp_cursor();
        }
    }

    fn collapse_at_cursor(&mut self) {
        let rows = self.visible_rows();
        match rows.get(self.cursor).copied() {
            Some(RowKind::Category(cat_idx)) => {
                if self.expanded.get(cat_idx).copied().unwrap_or(false) {
                    self.expanded[cat_idx] = false;
                    self.clamp_cursor();
                }
            }
            Some(RowKind::Child(cat_idx, _)) => self.collapse_category(cat_idx),
            _ => {}
        }
    }

    fn toggle_category(&mut self, cat_idx: usize) {
        let has_items = self
            .report
            .as_ref()
            .and_then(|r| r.categories.get(cat_idx))
            .is_some_and(|c| !c.items.is_empty());
        if !has_items {
            return;
        }
        if let Some(flag) = self.expanded.get_mut(cat_idx) {
            *flag = !*flag;
        }
        self.clamp_cursor();
    }

    fn collapse_category(&mut self, cat_idx: usize) {
        if let Some(flag) = self.expanded.get_mut(cat_idx) {
            *flag = false;
        }
        let rows = self.visible_rows();
        if let Some(pos) = rows
            .iter()
            .position(|kind| *kind == RowKind::Category(cat_idx))
        {
            self.cursor = pos;
        }
        self.clamp_cursor();
    }

    fn clamp_cursor(&mut self) {
        let len = self.visible_rows().len();
        if len == 0 {
            self.cursor = 0;
            self.scroll = 0;
        } else if self.cursor >= len {
            self.cursor = len - 1;
        }
    }
}
