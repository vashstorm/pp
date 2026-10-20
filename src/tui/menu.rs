use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use ratatui::{
    Frame,
    layout::{Constraint, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
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
    hint: Option<&'static str>,
    accent: Color,
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
            hint: None,
            accent: Color::Yellow,
        }
    }

    pub(super) fn with_accent(mut self, accent: Color) -> Self {
        self.accent = accent;
        self
    }

    pub(super) fn with_hint(mut self, hint: &'static str) -> Self {
        self.hint = Some(hint);
        self
    }

    pub(super) fn items_mut(&mut self) -> &mut [MenuItem<T>] {
        &mut self.items
    }

    pub(super) fn items(&self) -> &[MenuItem<T>] {
        &self.items
    }

    pub(super) fn selected_index(&self) -> Option<usize> {
        self.selection.selected()
    }

    pub(super) fn height(&self, available: u16) -> u16 {
        (self.items.len().min(MAX_ROWS) as u16 + u16::from(self.hint.is_some())).min(available)
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
        frame.render_widget(Clear, area);
        let hint_height = u16::from(self.hint.is_some() && area.height > 1);
        let rows_area = Rect::new(area.x, area.y, area.width, area.height - hint_height);
        if hint_height > 0 {
            frame.render_widget(
                Line::styled(
                    self.hint.unwrap_or_default(),
                    Style::default().fg(Color::DarkGray),
                ),
                Rect::new(area.x, rows_area.bottom(), area.width, hint_height),
            );
        }
        self.page_size = usize::from(rows_area.height).max(1);
        let selected = self.selected_index();
        let rows = self.items.iter().enumerate().map(|(index, item)| {
            let active = selected == Some(index);
            let label_style = Style::default().fg(if active { self.accent } else { Color::Reset });
            let detail_style =
                Style::default().fg(if active { self.accent } else { Color::DarkGray });
            let mut label = vec![Span::styled(
                format!("{} ", if active { "❯" } else { " " }),
                detail_style,
            )];
            if let Some(icon) = item.icon {
                label.push(Span::styled(format!("{icon} "), detail_style));
            }
            label.push(Span::styled(item.label.as_str(), label_style));
            let current = if item.current { " (current)" } else { "" };
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
        frame.render_stateful_widget(table, rows_area, &mut self.selection);
    }

    /// ask 使用纵向文字布局，复用菜单焦点与滚动状态。
    pub(super) fn choices_height(&self, width: u16) -> u16 {
        self.items
            .iter()
            .take(MAX_ROWS)
            .map(|item| {
                choice_lines(item, width.saturating_sub(6), Style::default()).len() as u16 + 1
            })
            .sum::<u16>()
            .saturating_sub(1)
    }

    pub(super) fn draw_choices(&mut self, frame: &mut Frame, area: Rect) {
        use super::transcript_style::{ACCENT, DIM, TOOL_PENDING_BG};

        if area.is_empty() {
            return;
        }
        let compact = area.height < 5;
        self.page_size = usize::from(area.height / if compact { 1 } else { 3 }).max(1);
        let focused = self.selected_index();
        let rows = self.items.iter().enumerate().map(|(index, item)| {
            let active = focused == Some(index);
            let label_style = if active {
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let mut lines = choice_lines(item, area.width.saturating_sub(6), label_style);
            if compact {
                lines.truncate(1);
            }
            let height = (lines.len() as u16).min(area.height);
            Row::new([
                Cell::from(if active { "▸" } else { " " }).style(Style::default().fg(ACCENT)),
                Cell::from(item.icon.unwrap_or_default())
                    .style(Style::default().fg(if item.current { ACCENT } else { DIM })),
                Cell::from(Text::from(lines)),
            ])
            .height(height)
            .bottom_margin(u16::from(!compact && index + 1 < self.items.len()))
            .style(if active {
                Style::default().bg(TOOL_PENDING_BG)
            } else {
                Style::default()
            })
        });
        let table = Table::new(
            rows,
            [
                Constraint::Length(1),
                Constraint::Length(3),
                Constraint::Min(0),
            ],
        )
        .column_spacing(1);
        frame.render_stateful_widget(table, area, &mut self.selection);
    }
}

fn choice_lines<T>(item: &MenuItem<T>, width: u16, label_style: Style) -> Vec<Line<'static>> {
    use super::transcript::{block_line, wrap_block};
    use super::transcript_style::MUTED;

    let mut lines: Vec<_> = wrap_block(block_line(item.label.clone(), label_style), width, "")
        .into_iter()
        .take(2)
        .collect();
    if lines.is_empty() {
        lines.push(Line::default());
    }
    if !item.description.is_empty() {
        lines.extend(
            wrap_block(
                block_line(item.description.clone(), Style::default().fg(MUTED)),
                width,
                "",
            )
            .into_iter()
            .take(2),
        );
    }
    lines
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
        assert!(row.replace(' ', "").contains("detail-1(current)"));
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
