use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use ratatui::{
    Frame,
    layout::{Constraint, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{
        Cell, Clear, Row, Scrollbar, ScrollbarOrientation, ScrollbarState, Table, TableState,
    },
};

pub(super) const SEPARATOR: Color = Color::DarkGray;
pub(super) const SELECTED: Color = Color::Rgb(153, 194, 232);
const MAX_ROWS: usize = 8;
const MODEL_LABEL: Color = Color::Rgb(224, 230, 240);
const MODEL_DETAIL: Color = Color::Rgb(170, 182, 200);
const MODEL_SELECTED: Color = Color::Rgb(130, 200, 255);
const FAVORITE: Color = Color::Rgb(254, 188, 56);
const DEFAULT_MODEL: Color = Color::Rgb(187, 154, 247);
const CURRENT_MODEL: Color = Color::Rgb(158, 206, 106);

pub(super) fn model_icon_style(icon: &str) -> Style {
    let color = match icon {
        "" => FAVORITE,
        "" => DEFAULT_MODEL,
        "" => CURRENT_MODEL,
        _ => MODEL_DETAIL,
    };
    Style::default().fg(color).add_modifier(Modifier::BOLD)
}

struct MenuStyle {
    label: Color,
    detail: Color,
    selected: Modifier,
    current_suffix: &'static str,
}

impl Default for MenuStyle {
    fn default() -> Self {
        Self {
            label: Color::DarkGray,
            detail: Color::DarkGray,
            selected: Modifier::empty(),
            current_suffix: " (current)",
        }
    }
}

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
    hint: Option<Line<'static>>,
    accent: Color,
    style: MenuStyle,
    label_suffixes: Vec<Option<&'static str>>,
    empty_message: Option<&'static str>,
    max_rows: usize,
    scrollbar: bool,
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
            accent: SELECTED,
            style: MenuStyle::default(),
            label_suffixes: Vec::new(),
            empty_message: None,
            max_rows: MAX_ROWS,
            scrollbar: false,
        }
    }

    pub(super) fn with_accent(mut self, accent: Color) -> Self {
        self.accent = accent;
        self
    }

    pub(super) fn with_max_rows(mut self, rows: usize) -> Self {
        self.max_rows = rows.max(1);
        self
    }

    pub(super) fn with_scrollbar(mut self) -> Self {
        self.scrollbar = true;
        self
    }

    pub(super) fn wheel(&mut self, up: bool, lines: usize) {
        if let Some(selected) = self.selected_index() {
            let next = if up {
                selected.saturating_sub(lines)
            } else {
                selected.saturating_add(lines).min(self.items.len() - 1)
            };
            self.selection.select(Some(next));
        }
    }

    pub(super) fn with_model_style(mut self) -> Self {
        self.accent = MODEL_SELECTED;
        self.style = MenuStyle {
            label: MODEL_LABEL,
            detail: MODEL_DETAIL,
            selected: Modifier::BOLD,
            current_suffix: "",
        };
        self
    }

    pub(super) fn with_label_suffixes(mut self, suffixes: Vec<Option<&'static str>>) -> Self {
        self.label_suffixes = suffixes;
        self
    }

    pub(super) fn label_suffixes(&self) -> &[Option<&'static str>] {
        &self.label_suffixes
    }

    pub(super) fn with_empty_message(mut self, message: &'static str) -> Self {
        self.empty_message = Some(message);
        self
    }

    pub(super) fn with_hint(mut self, hint: impl Into<Line<'static>>) -> Self {
        self.set_hint(hint);
        self
    }

    pub(super) fn set_hint(&mut self, hint: impl Into<Line<'static>>) {
        self.hint = Some(hint.into());
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
        let rows = self
            .items
            .len()
            .min(self.max_rows)
            .max(usize::from(self.empty_message.is_some()));
        (rows as u16 + u16::from(self.hint.is_some())).min(available)
    }

    pub(super) fn handle_key(&mut self, key: KeyEvent) -> MenuEvent<T> {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return MenuEvent::Ignored;
        }
        if key.code == KeyCode::Esc {
            return MenuEvent::Cancel;
        }
        let Some(selected) = self.selected_index() else {
            return if self.empty_message.is_some()
                && matches!(key.code, KeyCode::Enter | KeyCode::Tab)
            {
                MenuEvent::Handled
            } else {
                MenuEvent::Ignored
            };
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
        let mut rows_area = Rect::new(area.x, area.y, area.width, area.height - hint_height);
        if hint_height > 0 {
            frame.render_widget(
                self.hint
                    .clone()
                    .unwrap_or_default()
                    .style(Style::default().fg(self.style.detail)),
                Rect::new(area.x, rows_area.bottom(), area.width, hint_height),
            );
        }
        if self.items.is_empty() {
            if let Some(message) = self.empty_message {
                frame.render_widget(
                    Line::styled(message, Style::default().fg(self.style.detail)),
                    rows_area,
                );
            }
            return;
        }
        self.page_size = usize::from(rows_area.height).max(1);
        let scrollbar_area =
            (self.scrollbar && self.items.len() > self.page_size && rows_area.width > 1)
                .then_some(rows_area);
        if scrollbar_area.is_some() {
            rows_area.width -= 1;
        }
        let selected = self.selected_index();
        // 按名称的实际显示宽度对齐说明，宽终端不额外拉大两列间距。
        let label_width = self
            .items
            .iter()
            .enumerate()
            .map(|(index, item)| {
                2 + item.icon.map_or(0, |icon| Line::from(icon).width() + 1)
                    + Line::from(item.label.as_str()).width()
                    + self
                        .label_suffixes
                        .get(index)
                        .and_then(|suffix| *suffix)
                        .map_or(0, |suffix| Line::from(suffix).width())
            })
            .max()
            .unwrap_or(0)
            .min(usize::from(area.width / 2)) as u16;
        let rows = self.items.iter().enumerate().map(|(index, item)| {
            let active = selected == Some(index);
            let emphasis = if active {
                self.style.selected
            } else {
                Modifier::empty()
            };
            let label_style = Style::default()
                .fg(if active {
                    self.accent
                } else {
                    self.style.label
                })
                .add_modifier(emphasis);
            let detail_style = Style::default()
                .fg(if active {
                    self.accent
                } else {
                    self.style.detail
                })
                .add_modifier(emphasis);
            let mut label = vec![Span::styled(
                format!("{} ", if active { "❯" } else { " " }),
                detail_style,
            )];
            if let Some(icon) = item.icon {
                label.push(Span::styled(format!("{icon} "), detail_style));
            }
            if let Some(suffix) = self.label_suffixes.get(index).and_then(|suffix| *suffix) {
                let prefix_width: usize = label.iter().map(Span::width).sum();
                let available = usize::from(label_width).saturating_sub(prefix_width);
                let name_width = available.saturating_sub(Line::from(suffix).width());
                label.push(Span::styled(
                    fit_label(&item.label, name_width),
                    label_style,
                ));
                label.extend(
                    suffix
                        .split_whitespace()
                        .map(|icon| Span::styled(format!(" {icon}"), model_icon_style(icon))),
                );
            } else {
                label.push(Span::styled(item.label.as_str(), label_style));
            }
            let current = if item.current {
                self.style.current_suffix
            } else {
                ""
            };
            Row::new([
                Cell::from(Line::from(label)),
                Cell::from(format!("{}{current}", item.description)).style(detail_style),
            ])
        });
        let table = Table::new(rows, [Constraint::Length(label_width), Constraint::Min(0)])
            .column_spacing(8.min(area.width.saturating_sub(label_width)));
        frame.render_stateful_widget(table, rows_area, &mut self.selection);
        if let Some(area) = scrollbar_area {
            let mut state =
                ScrollbarState::new(self.items.len().saturating_sub(self.page_size) + 1)
                    .position(self.selection.offset());
            frame.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .begin_symbol(None)
                    .end_symbol(None),
                area,
                &mut state,
            );
        }
    }

    /// ask 使用纵向文字布局，复用菜单焦点与滚动状态。
    pub(super) fn choices_height(&self, width: u16, editor_height: Option<u16>) -> u16 {
        let prefix_width = self.choice_prefix_width();
        self.items
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let height =
                    choice_lines(item, width.saturating_sub(prefix_width), Style::default()).len()
                        as u16;
                if index + 1 == self.items.len() {
                    height.max(editor_height.unwrap_or(0))
                } else {
                    height
                }
            })
            .fold(0, u16::saturating_add)
    }

    fn choice_prefix_width(&self) -> u16 {
        3 + self
            .items
            .iter()
            .map(|item| Span::raw(item.icon.unwrap_or_default()).width() as u16)
            .max()
            .unwrap_or(0)
    }

    /// 返回正在编辑的末项文字区域，供 ask 原位绘制输入框。
    pub(super) fn draw_choices(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        editor_height: Option<u16>,
    ) -> Option<Rect> {
        use super::transcript_style::{ACCENT, ADDED, DIM};

        if area.is_empty() {
            return None;
        }
        let compact = area.height < 3;
        self.page_size = usize::from(area.height).max(1);
        let prefix_width = self.choice_prefix_width();
        let focused = self.selected_index();
        let mut heights = Vec::with_capacity(self.items.len());
        let rows: Vec<_> = self
            .items
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let active = focused == Some(index);
                let label_style = if active {
                    Style::default().fg(ACCENT)
                } else {
                    Style::default()
                };
                let mut lines =
                    choice_lines(item, area.width.saturating_sub(prefix_width), label_style);
                if compact {
                    lines.truncate(1);
                }
                let editing = editor_height.filter(|_| index + 1 == self.items.len());
                let height = editing
                    .unwrap_or(0)
                    .max(lines.len() as u16)
                    .min(area.height);
                if editing.is_some() {
                    lines.clear();
                }
                heights.push(height);
                Row::new([
                    Cell::from(if active { "❯" } else { " " }).style(Style::default().fg(ACCENT)),
                    Cell::from(item.icon.unwrap_or_default()).style(
                        Style::default()
                            .fg(if item.current { ADDED } else { DIM })
                            .add_modifier(ratatui::style::Modifier::BOLD),
                    ),
                    Cell::from(Text::from(lines)),
                ])
                .height(height)
            })
            .collect();
        let table = Table::new(
            rows,
            [
                Constraint::Length(1),
                Constraint::Length(prefix_width - 3),
                Constraint::Min(0),
            ],
        )
        .column_spacing(1);
        frame.render_stateful_widget(table, area, &mut self.selection);
        editor_height?;
        let last = heights.len().checked_sub(1)?;
        let y = heights[self.selection.offset()..last]
            .iter()
            .fold(area.y, |y, height| y.saturating_add(*height));
        Some(Rect::new(
            area.x + prefix_width.min(area.width),
            y.min(area.bottom()),
            area.width.saturating_sub(prefix_width),
            heights[last].min(area.bottom().saturating_sub(y)),
        ))
    }
}

// 为后缀保留显示列，按 UTF-8 字符边界截断名称。
fn fit_label(text: &str, width: usize) -> &str {
    let mut used = 0;
    for (index, ch) in text.char_indices() {
        used += Span::raw(ch.to_string()).width();
        if used > width {
            return &text[..index];
        }
    }
    text
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
                width.saturating_sub(2),
                "",
            )
            .into_iter()
            .map(|mut line| {
                line.spans.insert(0, Span::raw("  "));
                line
            })
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
        assert_eq!(buffer[(2, 1)].fg, SELECTED);
        assert_eq!(buffer[(18, 1)].symbol(), "d");
        assert_eq!(buffer[(18, 1)].fg, SELECTED);
        assert_eq!(buffer[(18, 0)].fg, Color::DarkGray);
        let row: String = (0..100).map(|x| buffer[(x, 1)].symbol()).collect();
        assert!(row.replace(' ', "").contains("detail-1(current)"));
        assert_eq!(menu.height(30), 8);
        assert_eq!(menu.height(2), 2);
        let mut wide = Terminal::new(TestBackend::new(200, 4)).unwrap();
        wide.draw(|frame| menu.draw(frame, frame.area())).unwrap();
        assert_eq!(wide.backend().buffer()[(18, 1)].symbol(), "d");
        menu.handle_key(press(KeyCode::End));
        terminal
            .draw(|frame| menu.draw(frame, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let last: String = (0..100).map(|x| buffer[(x, 3)].symbol()).collect();
        assert!(last.contains("❯ item-11"));
    }

    #[test]
    fn model_style_renders_bright_labels_colored_icons_and_empty_results() {
        let mut menu = SelectionMenu::new(items(2), 1)
            .with_model_style()
            .with_label_suffixes(vec![Some("   "), Some("   ")])
            .with_hint("Search: type")
            .with_empty_message("No matching models");
        let mut terminal = Terminal::new(TestBackend::new(100, 3)).unwrap();
        terminal
            .draw(|frame| menu.draw(frame, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(4, 0)].fg, MODEL_LABEL);
        assert_eq!(buffer[(4, 1)].fg, MODEL_SELECTED);
        assert!(buffer[(4, 1)].modifier.contains(Modifier::BOLD));
        for y in 0..2 {
            for (icon, color) in [
                ("", Color::Rgb(254, 188, 56)),
                ("", Color::Rgb(187, 154, 247)),
                ("", Color::Rgb(158, 206, 106)),
            ] {
                let x = (0..100).find(|x| buffer[(*x, y)].symbol() == icon).unwrap();
                assert_eq!(buffer[(x, y)].fg, color);
                assert!(buffer[(x, y)].modifier.contains(Modifier::BOLD));
            }
        }
        let row: String = (0..100).map(|x| buffer[(x, 0)].symbol()).collect();
        assert!(row.contains("item-0   "));
        let row: String = (0..100).map(|x| buffer[(x, 1)].symbol()).collect();
        assert!(!row.contains("(current)"));
        assert_eq!(buffer[(0, 2)].fg, MODEL_DETAIL);
        menu.items_mut()[0].label = "模型名称很长-long-model-name".into();
        menu.items_mut()[0].icon = None;
        for width in [1, 2, 10, 20, 100] {
            let mut narrow = Terminal::new(TestBackend::new(width, 3)).unwrap();
            narrow.draw(|frame| menu.draw(frame, frame.area())).unwrap();
            if width >= 10 {
                assert!((0..width).any(|x| narrow.backend().buffer()[(x, 0)].symbol() == ""));
            }
        }
        let mut empty = SelectionMenu::<usize>::new(Vec::new(), 0)
            .with_model_style()
            .with_hint("Search: type")
            .with_empty_message("No matching models");
        assert_eq!(empty.height(30), 2);
        terminal
            .draw(|frame| empty.draw(frame, frame.area()))
            .unwrap();
        let row: String = (0..100)
            .map(|x| terminal.backend().buffer()[(x, 0)].symbol())
            .collect();
        assert!(row.contains("No matching models"));
        for code in [KeyCode::Enter, KeyCode::Tab] {
            assert!(matches!(empty.handle_key(press(code)), MenuEvent::Handled));
        }
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
