use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use ratatui::{
    Frame,
    layout::{Constraint, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Cell, Clear, Row, Table, TableState},
};

pub(super) const SEPARATOR: Color = Color::Rgb(190, 133, 222);
const MAX_ROWS: usize = 8;

#[derive(PartialEq, Eq)]
pub(super) struct MenuItem<T> {
    pub(super) value: T,
    pub(super) label: String,
    pub(super) description: String,
    pub(super) icon: Option<&'static str>,
    pub(super) current: bool,
}

pub(super) struct SelectionMenu<T> {
    items: Vec<MenuItem<T>>,
    selection: TableState,
    page_size: usize,
}

pub(super) enum MenuEvent<T> {
    Ignored,
    Handled,
    Cancel,
    Confirm(T),
    Complete(T),
}

impl<T: Copy> SelectionMenu<T> {
    pub(super) fn new(items: Vec<MenuItem<T>>, selected: usize) -> Self {
        let selected = (!items.is_empty()).then(|| selected.min(items.len() - 1));
        Self {
            items,
            selection: TableState::default().with_selected(selected),
            page_size: 1,
        }
    }

    pub(super) fn items(&self) -> &[MenuItem<T>] {
        &self.items
    }

    pub(super) fn selected_index(&self) -> Option<usize> {
        self.selection.selected()
    }

    pub(super) fn height(&self, available: u16) -> u16 {
        (self.items.len().min(MAX_ROWS) as u16).min(available)
    }

    pub(super) fn handle_key(&mut self, key: KeyEvent) -> MenuEvent<T> {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return MenuEvent::Ignored;
        }
        if key.code == KeyCode::Esc {
            return MenuEvent::Cancel;
        }
        let Some(selected) = self.selected_index() else {
            return MenuEvent::Ignored;
        };
        let last = self.items.len() - 1;
        let next = match key.code {
            KeyCode::Up => selected.saturating_sub(1),
            KeyCode::Down => (selected + 1).min(last),
            KeyCode::Home => 0,
            KeyCode::End => last,
            KeyCode::PageUp => selected.saturating_sub(self.page_size),
            KeyCode::PageDown => selected.saturating_add(self.page_size).min(last),
            KeyCode::Enter | KeyCode::Tab => {
                // 按住确认键只处理首次按下，避免重复执行。
                if key.kind != KeyEventKind::Press {
                    return MenuEvent::Handled;
                }
                return if key.code == KeyCode::Enter {
                    MenuEvent::Confirm(self.items[selected].value)
                } else {
                    MenuEvent::Complete(self.items[selected].value)
                };
            }
            _ => return MenuEvent::Ignored,
        };
        self.selection.select(Some(next));
        MenuEvent::Handled
    }

    pub(super) fn draw(&mut self, frame: &mut Frame, area: Rect) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        self.page_size = usize::from(area.height).max(1);
        let selected = self.selected_index();
        let rows = self.items.iter().enumerate().map(|(index, item)| {
            let active = selected == Some(index);
            let label_style =
                Style::default().fg(if active { Color::Yellow } else { Color::Reset });
            let detail_style = Style::default().fg(if active {
                Color::Yellow
            } else {
                Color::DarkGray
            });
            let mut label = vec![Span::styled(
                format!("{} ", if active { "❯" } else { " " }),
                detail_style,
            )];
            if let Some(icon) = item.icon {
                label.push(Span::styled(format!("{icon} "), detail_style));
            }
            label.push(Span::styled(item.label.as_str(), label_style));
            let current = if item.current { "（当前）" } else { "" };
            Row::new([
                Cell::from(Line::from(label)),
                Cell::from(format!("{}{current}", item.description)).style(detail_style),
            ])
        });
        let table = Table::new(
            rows,
            [Constraint::Percentage(46), Constraint::Percentage(54)],
        )
        .column_spacing(1);
        frame.render_widget(Clear, area);
        frame.render_stateful_widget(table, area, &mut self.selection);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    use ratatui::{Terminal, backend::TestBackend};

    fn items(count: usize) -> Vec<MenuItem<usize>> {
        (0..count)
            .map(|index| MenuItem {
                value: index,
                label: format!("item-{index}"),
                description: format!("detail-{index}"),
                icon: (index == 0).then_some("◆"),
                current: index == 1,
            })
            .collect()
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn navigation_clamps_pages_and_confirms_only_on_first_press() {
        let mut menu = SelectionMenu::new(items(12), 0);
        let mut terminal = Terminal::new(TestBackend::new(100, 4)).unwrap();
        terminal
            .draw(|frame| menu.draw(frame, frame.area()))
            .unwrap();
        menu.handle_key(press(KeyCode::Up));
        assert_eq!(menu.selected_index(), Some(0));
        menu.handle_key(press(KeyCode::PageDown));
        assert_eq!(menu.selected_index(), Some(4));
        menu.handle_key(press(KeyCode::End));
        menu.handle_key(press(KeyCode::Down));
        assert_eq!(menu.selected_index(), Some(11));
        menu.handle_key(press(KeyCode::PageUp));
        assert_eq!(menu.selected_index(), Some(7));
        assert!(matches!(
            menu.handle_key(press(KeyCode::Tab)),
            MenuEvent::Complete(7)
        ));
        assert!(matches!(
            menu.handle_key(press(KeyCode::Enter)),
            MenuEvent::Confirm(7)
        ));
        for code in [KeyCode::Tab, KeyCode::Enter] {
            let mut key = press(code);
            key.kind = KeyEventKind::Repeat;
            assert!(matches!(menu.handle_key(key), MenuEvent::Handled));
        }
        assert!(matches!(
            menu.handle_key(press(KeyCode::Esc)),
            MenuEvent::Cancel
        ));
        assert!(matches!(
            menu.handle_key(press(KeyCode::Char('x'))),
            MenuEvent::Ignored
        ));
    }

    #[test]
    fn rendering_shares_columns_colors_current_marker_and_scroll_position() {
        let mut menu = SelectionMenu::new(items(12), 1);
        let mut terminal = Terminal::new(TestBackend::new(100, 4)).unwrap();
        terminal
            .draw(|frame| menu.draw(frame, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(2, 0)].symbol(), "◆");
        assert_eq!(buffer[(4, 0)].symbol(), "i");
        assert_eq!(buffer[(0, 1)].symbol(), "❯");
        assert_eq!(buffer[(2, 1)].fg, Color::Yellow);
        assert_eq!(buffer[(47, 1)].fg, Color::Yellow);
        assert_eq!(buffer[(47, 0)].fg, Color::DarkGray);
        let row: String = (0..100).map(|x| buffer[(x, 1)].symbol()).collect();
        assert!(row.replace(' ', "").contains("detail-1（当前）"));
        assert_eq!(menu.height(30), 8);
        assert_eq!(menu.height(2), 2);
        menu.handle_key(press(KeyCode::End));
        terminal
            .draw(|frame| menu.draw(frame, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let last: String = (0..100).map(|x| buffer[(x, 3)].symbol()).collect();
        assert!(last.contains("❯ item-11"));
    }

    #[test]
    fn empty_menu_and_out_of_range_initial_selection_are_safe() {
        let mut empty = SelectionMenu::<usize>::new(Vec::new(), 42);
        assert_eq!(empty.height(30), 0);
        assert_eq!(empty.selected_index(), None);
        for code in [KeyCode::Enter, KeyCode::Tab, KeyCode::Up, KeyCode::Down] {
            assert!(matches!(empty.handle_key(press(code)), MenuEvent::Ignored));
        }
        let menu = SelectionMenu::new(items(2), 42);
        assert_eq!(menu.selected_index(), Some(1));
    }
}
