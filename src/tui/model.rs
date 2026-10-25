use super::{
    MenuAction, ModelChoice, editor,
    menu::{MenuEvent, MenuItem, SelectionMenu},
};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    text::Line,
    widgets::{Block, Borders, Clear},
};
use ratatui_textarea::{CursorMove, TextArea, WrapMode};

const MAX_HEIGHT: u16 = 15;
const LIST_ROWS: usize = 11;
const HINT: &str = "Enter select · Esc cancel · ↑↓/PgUp/PgDn/Wheel · Tab complete · Ctrl + F favorite · Ctrl + S default";

pub(super) struct Panel {
    pub(super) search: TextArea<'static>,
    pub(super) menu: SelectionMenu<MenuAction>,
    area: Rect,
    initial_height: u16,
}

pub(super) enum Action {
    None,
    Cancel,
    Select(usize),
    Favorite(usize),
    Default(usize),
}

impl Panel {
    pub(super) fn new(query: &str, model_count: usize) -> Self {
        let mut panel = Self {
            search: editor(),
            menu: SelectionMenu::new(Vec::new(), 0),
            area: Rect::default(),
            // 打开时按完整模型列表确定高度，之后筛选不移动搜索框与提示行。
            initial_height: (4 + model_count.clamp(1, LIST_ROWS) as u16).min(MAX_HEIGHT),
        };
        panel.search.set_wrap_mode(WrapMode::None);
        panel.paste(query);
        panel
    }

    pub(super) fn refresh(
        &mut self,
        choices: &[ModelChoice],
        current: Option<&str>,
        default: Option<&str>,
        favorites: &[String],
        edited: bool,
    ) {
        let query = self.search.lines()[0].trim();
        let folded = query.to_lowercase();
        let mut items: Vec<_> = choices
            .iter()
            .enumerate()
            .filter(|(_, choice)| {
                choice.reference.to_lowercase().contains(&folded)
                    || choice.model.to_lowercase().contains(&folded)
            })
            .map(|(index, choice)| MenuItem {
                value: MenuAction::Model(index),
                label: if default == Some(choice.reference.as_str()) {
                    format!("{} (default)", choice.reference)
                } else {
                    choice.reference.clone()
                },
                description: String::new(),
                icon: None,
                current: current == Some(choice.reference.as_str()),
            })
            .collect();
        items.sort_by_key(|item| match item.value {
            MenuAction::Model(index) => !favorites.contains(&choices[index].reference),
            _ => true,
        });
        let suffixes: Vec<_> = items
            .iter()
            .map(|item| match item.value {
                MenuAction::Model(index) if favorites.contains(&choices[index].reference) => {
                    Some(" ")
                }
                _ => None,
            })
            .collect();
        if !edited && self.menu.items() == items && self.menu.label_suffixes() == suffixes {
            return;
        }
        let selected = (!edited).then(|| self.selected()).flatten();
        let preferred = items
            .iter()
            .position(|item| Some(item.value) == selected.map(MenuAction::Model))
            .or_else(|| {
                items.iter().position(|item| match item.value {
                    MenuAction::Model(index) => choices[index].reference == query,
                    _ => false,
                })
            })
            .or_else(|| items.iter().position(|item| item.current))
            .unwrap_or(0);
        self.menu = SelectionMenu::new(items, preferred)
            .with_model_style()
            .with_label_suffixes(suffixes)
            .with_empty_message("No matching models")
            .with_max_rows(LIST_ROWS)
            .with_scrollbar()
            .with_hint(HINT);
    }

    fn selected(&self) -> Option<usize> {
        match self
            .menu
            .selected_index()
            .map(|index| self.menu.items()[index].value)
        {
            Some(MenuAction::Model(index)) => Some(index),
            _ => None,
        }
    }

    pub(super) fn key(&mut self, key: KeyEvent, choices: &[ModelChoice]) -> (Action, bool) {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return (Action::None, false);
        }
        if key.modifiers == KeyModifiers::CONTROL {
            match key.code {
                KeyCode::Char('f' | 's') => {
                    let action = if key.kind == KeyEventKind::Press {
                        self.selected()
                            .map(|index| {
                                if key.code == KeyCode::Char('f') {
                                    Action::Favorite(index)
                                } else {
                                    Action::Default(index)
                                }
                            })
                            .unwrap_or(Action::None)
                    } else {
                        Action::None
                    };
                    return (action, false);
                }
                KeyCode::Char('j' | 'm') => return (Action::None, false),
                KeyCode::Char('a') => {
                    self.search.move_cursor(CursorMove::Head);
                    return (Action::None, false);
                }
                KeyCode::Char('e') => {
                    self.search.move_cursor(CursorMove::End);
                    return (Action::None, false);
                }
                _ => {}
            }
        }
        if key.modifiers.is_empty() {
            match self.menu.handle_key(key) {
                MenuEvent::Cancel => return (Action::Cancel, false),
                MenuEvent::Confirm(MenuAction::Model(index)) => {
                    return (Action::Select(index), false);
                }
                MenuEvent::Complete(MenuAction::Model(index)) => {
                    self.search = editor();
                    self.search.set_wrap_mode(WrapMode::None);
                    self.search.insert_str(&choices[index].reference);
                    return (Action::None, true);
                }
                MenuEvent::Handled => return (Action::None, false),
                _ => {}
            }
        }
        // 面板只允许单行搜索；换行、提交修饰键与功能键不穿透主输入框。
        if matches!(
            key.code,
            KeyCode::Enter | KeyCode::Tab | KeyCode::BackTab | KeyCode::Esc
        ) || key.modifiers.contains(KeyModifiers::ALT)
        {
            return (Action::None, false);
        }
        (Action::None, self.search.input(key))
    }

    pub(super) fn paste(&mut self, text: &str) {
        self.search
            .insert_str(text.replace("\r\n", " ").replace(['\r', '\n'], " "));
    }

    pub(super) fn wheel(&mut self, mouse: MouseEvent, lines: usize) -> bool {
        if !self.area.contains((mouse.column, mouse.row).into()) {
            return false;
        }
        match mouse.kind {
            MouseEventKind::ScrollUp => self.menu.wheel(true, lines),
            MouseEventKind::ScrollDown => self.menu.wheel(false, lines),
            _ => return false,
        }
        true
    }

    pub(super) fn height(&self, available: u16) -> u16 {
        self.initial_height.min(available)
    }

    pub(super) fn draw(&mut self, frame: &mut Frame, area: Rect) {
        let expanded = area.width > self.area.width;
        self.area = area;
        if area.is_empty() {
            return;
        }
        frame.render_widget(Clear, area);
        let search_height = if area.height >= 5 { 3 } else { 1 };
        let search = Rect::new(area.x, area.y, area.width, search_height);
        let inner = if search_height == 3 {
            let block = Block::default()
                .borders(Borders::TOP | Borders::BOTTOM)
                .border_style(Style::default().fg(super::menu::SELECTED));
            let inner = block.inner(search);
            frame.render_widget(block, search);
            inner
        } else {
            search
        };
        let search_icon = Line::from(" ");
        let prefix = (search_icon.width() as u16).min(inner.width.saturating_sub(1));
        frame.render_widget(search_icon, Rect::new(inner.x, inner.y, prefix, 1));
        let input = Rect::new(inner.x + prefix, inner.y, inner.width - prefix, 1);
        frame.render_widget(&self.search, input);
        if input.width > 0 {
            // 扩宽后收回多余的横向偏移；先渲染以更新编辑器的 viewport 尺寸，
            // 再在光标仍可见的范围内左滚，不改变搜索词、光标或撤销历史。
            let (_, left) = self.search.scroll_offset();
            let target = self
                .search
                .screen_cursor()
                .col
                .saturating_sub(usize::from(input.width - 1));
            let mut remaining = usize::from(left).saturating_sub(target);
            if expanded && remaining > 0 {
                while remaining > 0 {
                    let step = remaining.min(i16::MAX as usize) as i16;
                    self.search.scroll((0, -step));
                    remaining -= step as usize;
                }
                frame.render_widget(&self.search, input);
            }
            let cursor = self.search.screen_cursor();
            let (_, left) = self.search.scroll_offset();
            if let Some(column) = cursor.col.checked_sub(usize::from(left))
                && column < usize::from(input.width)
            {
                frame.set_cursor_position((input.x + column as u16, input.y));
            }
        }
        let rows = Rect::new(
            area.x,
            search.bottom(),
            area.width,
            area.height - search_height,
        );
        self.menu.draw(frame, rows);
    }
}
