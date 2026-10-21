//! OMP 终端圆角树面板；历史读取和跳转由主 worker 处理。
use super::transcript_style::{ACCENT, ADDED, DIM, MUTED, display_text};
use crate::agent::persistence::tree::{Kind, Tree};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear},
};

const BORDER: Color = Color::Rgb(23, 143, 185);
const SELECTED: Color = Color::Rgb(49, 54, 63);

pub(super) enum Action {
    None,
    Cancel,
    Navigate(String),
}

#[derive(Clone, Copy)]
struct Row {
    index: usize,
    branch_parent: Option<usize>,
    depth: usize,
    last: bool,
    connector: bool,
}

pub(super) struct Panel {
    tree: Tree,
    rows: Vec<Row>,
    visible: Vec<usize>,
    active: Vec<bool>,
    selected: usize,
    query: String,
    filter: usize,
    page_size: usize,
    pub pending: bool,
    loading: bool,
}

impl Panel {
    pub fn new() -> Self {
        Self {
            tree: Tree::default(),
            rows: Vec::new(),
            visible: Vec::new(),
            active: Vec::new(),
            selected: 0,
            query: String::new(),
            filter: 0,
            page_size: 5,
            pending: false,
            loading: true,
        }
    }

    pub fn set_tree(&mut self, tree: Tree) {
        self.tree = tree;
        self.loading = false;
        self.active = vec![false; self.tree.nodes.len()];
        for index in self.tree.path(self.tree.leaf) {
            self.active[index] = true;
        }
        let mut children = vec![Vec::new(); self.tree.nodes.len()];
        let mut roots = Vec::new();
        for (index, node) in self.tree.nodes.iter().enumerate() {
            if let Some(parent) = node.parent {
                children[parent].push(index);
            } else {
                roots.push(index);
            }
        }
        for siblings in children.iter_mut().chain(std::iter::once(&mut roots)) {
            siblings.sort_by_key(|i| !self.active[*i]);
        }
        self.rows.clear();
        let multiple_roots = roots.len() > 1;
        let mut stack: Vec<_> = roots
            .into_iter()
            .rev()
            .map(|i| (i, None, 0, true, false))
            .collect();
        while let Some((index, branch_parent, depth, last, connector)) = stack.pop() {
            let row = self.rows.len();
            self.rows.push(Row {
                index,
                branch_parent,
                depth,
                last,
                connector,
            });
            let split = children[index].len() > 1;
            for (position, child) in children[index].iter().enumerate().rev() {
                let child_depth = depth
                    + usize::from(
                        split || (multiple_roots && self.tree.nodes[index].parent.is_none()),
                    );
                stack.push((
                    *child,
                    if split { Some(row) } else { branch_parent },
                    child_depth,
                    if split {
                        position + 1 == children[index].len()
                    } else {
                        last
                    },
                    split,
                ));
            }
        }
        self.refilter();
        if let Some(leaf) = self.tree.leaf
            && let Some(index) = self
                .visible
                .iter()
                .position(|row| self.rows[*row].index == leaf)
        {
            self.selected = index;
        }
    }

    fn refilter(&mut self) {
        let selected = self
            .visible
            .get(self.selected)
            .map(|row| self.rows[*row].index);
        self.visible = self
            .rows
            .iter()
            .enumerate()
            .filter_map(|(i, row)| {
                let node = &self.tree.nodes[row.index];
                let shown = match self.filter {
                    1 => node.kind != Kind::Tools,
                    2 => node.kind == Kind::User,
                    _ => true,
                };
                (shown && matches_query(&format!("{} {}", role(node.kind), node.text), &self.query))
                    .then_some(i)
            })
            .collect();
        self.selected = selected
            .and_then(|index| {
                self.visible
                    .iter()
                    .position(|row| self.rows[*row].index == index)
            })
            .unwrap_or(self.selected.min(self.visible.len().saturating_sub(1)));
    }

    pub fn key(&mut self, key: KeyEvent) -> Action {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return Action::None;
        }
        if key.code == KeyCode::Esc && key.modifiers.is_empty() {
            if self.pending {
                return Action::None;
            }
            if self.query.is_empty() {
                return Action::Cancel;
            }
            self.query.clear();
            self.refilter();
            return Action::None;
        }
        if self.pending || self.loading {
            return Action::None;
        }
        let len = self.visible.len();
        match key.code {
            KeyCode::Enter
                if key.kind == KeyEventKind::Press && key.modifiers.is_empty() && len > 0 =>
            {
                let index = self.rows[self.visible[self.selected]].index;
                if Some(index) == self.tree.leaf {
                    return Action::Cancel;
                }
                self.pending = true;
                return Action::Navigate(self.tree.nodes[index].id.clone());
            }
            KeyCode::Up if len > 0 => self.selected = (self.selected + len - 1) % len,
            KeyCode::Down if len > 0 => self.selected = (self.selected + 1) % len,
            KeyCode::Left | KeyCode::PageUp => {
                self.selected = self.selected.saturating_sub(self.page_size)
            }
            KeyCode::Right | KeyCode::PageDown => {
                self.selected = self
                    .selected
                    .saturating_add(self.page_size)
                    .min(len.saturating_sub(1))
            }
            KeyCode::Home => self.selected = 0,
            KeyCode::End => self.selected = len.saturating_sub(1),
            KeyCode::Char('o') if key.modifiers == KeyModifiers::CONTROL => {
                self.filter = (self.filter + 1) % 3;
                self.refilter();
            }
            KeyCode::Backspace => {
                self.query.pop();
                self.refilter();
            }
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.query.push(c);
                self.refilter();
            }
            _ => {}
        }
        Action::None
    }

    pub fn paste(&mut self, text: &str) {
        if !self.pending && !self.loading {
            self.query.extend(text.chars().filter(|c| !c.is_control()));
            self.refilter();
        }
    }

    pub fn wheel(&mut self, up: bool) {
        if !self.pending && !self.loading {
            self.selected = if up {
                self.selected.saturating_sub(1)
            } else {
                (self.selected + 1).min(self.visible.len().saturating_sub(1))
            };
        }
    }

    pub fn height(&self, available: u16) -> u16 {
        let rows = usize::from(
            (available / 2)
                .max(5)
                .min(available.saturating_sub(8).max(1)),
        );
        (rows.min(self.visible.len().max(1)) as u16 + 8).min(available)
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect) {
        if area.is_empty() {
            return;
        }
        frame.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(BORDER))
            .title(Line::from(vec![
                Span::styled("─", Style::default().fg(BORDER)),
                Span::styled(
                    " Session Tree ",
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
            ]));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.width < 2 || inner.height == 0 {
            return;
        }
        let body = Rect::new(
            inner.x + 1,
            inner.y,
            inner.width.saturating_sub(2),
            inner.height,
        );
        if body.width == 0 {
            return;
        }
        let hint_y = body.y.saturating_add(1);
        let search_y = body.y.saturating_add(2);
        if hint_y < inner.bottom() {
            frame.render_widget(Line::styled("enter: switch. ↑/↓: select. pgup/pgdn (←/→): page. home/end: first/last. ctrl+o: filter. esc: cancel. Type to search", Style::default().fg(MUTED)), Rect::new(body.x, hint_y, body.width, 1));
        }
        if search_y < inner.bottom() {
            let mut query = self.query.as_str();
            let width = usize::from(body.width.saturating_sub(9));
            while Line::from(query).width() > width {
                query = &query[query.chars().next().map_or(0, char::len_utf8)..];
            }
            frame.render_widget(
                Line::from(vec![
                    Span::styled("Search: ", Style::default().fg(MUTED)),
                    Span::styled(
                        if query.is_empty() {
                            "Type to search"
                        } else {
                            query
                        },
                        Style::default().fg(if query.is_empty() { DIM } else { Color::Reset }),
                    ),
                ]),
                Rect::new(body.x, search_y, body.width, 1),
            );
            if !self.pending && !self.loading && body.width > 8 {
                frame
                    .set_cursor_position((body.x + 8 + Line::from(query).width() as u16, search_y));
            }
        }
        let divider_y = body.y.saturating_add(3);
        if divider_y < inner.bottom() {
            frame.render_widget(
                Line::styled(
                    format!(
                        "├{}┤",
                        "─".repeat(usize::from(area.width.saturating_sub(2)))
                    ),
                    Style::default().fg(BORDER),
                ),
                Rect::new(area.x, divider_y, area.width, 1),
            );
        }
        let top = body.y.saturating_add(5).min(inner.bottom());
        let footer = inner.bottom().saturating_sub(1);
        let height = footer.saturating_sub(top);
        self.page_size = usize::from(height).max(1);
        if height == 0 {
            return;
        }
        let status = if self.loading {
            Some("Loading session tree…")
        } else if self.pending {
            Some("Switching branch…")
        } else if self.visible.is_empty() {
            Some(if self.tree.nodes.is_empty() {
                "No entries found"
            } else if !self.query.is_empty() {
                "No entries match search"
            } else {
                "Entries hidden by the current filter"
            })
        } else {
            None
        };
        if let Some(status) = status {
            frame.render_widget(
                Line::styled(status, Style::default().fg(MUTED)),
                Rect::new(body.x, top, body.width, 1),
            );
        } else {
            let start = self
                .selected
                .saturating_sub(self.page_size / 2)
                .min(self.visible.len().saturating_sub(self.page_size));
            let window = &self.visible[start..self.visible.len().min(start + self.page_size)];
            let max_depth = window
                .iter()
                .map(|r| self.rows[*r].depth)
                .max()
                .unwrap_or(0);
            let indent = usize::from(body.width.saturating_sub(18) / 3).min(max_depth);
            let shift = max_depth.saturating_sub(indent);
            for (position, &row_index) in window.iter().enumerate() {
                let row = self.rows[row_index];
                let node = &self.tree.nodes[row.index];
                let selected = start + position == self.selected;
                let width = body
                    .width
                    .saturating_sub(u16::from(self.visible.len() > self.page_size));
                let y = top + position as u16;
                let style = if selected {
                    Style::default().bg(SELECTED).add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                frame.render_widget(
                    Block::default().style(style),
                    Rect::new(body.x, y, width, 1),
                );
                let mut prefix = vec![' '; row.depth.saturating_sub(shift) * 3];
                if row.connector && row.depth > shift {
                    let at = (row.depth - shift - 1) * 3;
                    prefix[at] = if row.last { '└' } else { '├' };
                    prefix[at + 1] = '─';
                } else if row.depth > shift && !row.last {
                    prefix[(row.depth - shift - 1) * 3] = '│';
                }
                let mut ancestor = row.branch_parent;
                while let Some(i) = ancestor {
                    let a = self.rows[i];
                    if a.depth > shift && a.depth < row.depth && !a.last {
                        prefix[(a.depth - shift - 1) * 3] = '│';
                    }
                    ancestor = a.branch_parent;
                }
                if shift > 0 && !prefix.is_empty() {
                    prefix[0] = '…';
                }
                let text = display_text(&node.text).replace(['\n', '\t', '\r'], " ");
                let spans = vec![
                    Span::styled(if selected { "› " } else { "  " }, style.fg(ACCENT)),
                    Span::styled(prefix.into_iter().collect::<String>(), style.fg(DIM)),
                    Span::styled(
                        if self.active[row.index] { "• " } else { "" },
                        style.fg(ACCENT),
                    ),
                    Span::styled(
                        format!("{}: ", role(node.kind)),
                        style.fg(match node.kind {
                            Kind::User => ACCENT,
                            Kind::Assistant => ADDED,
                            Kind::Tools => MUTED,
                        }),
                    ),
                    Span::styled(
                        text,
                        style.fg(if node.kind == Kind::Tools {
                            MUTED
                        } else {
                            Color::Reset
                        }),
                    ),
                ];
                frame.render_widget(Line::from(spans), Rect::new(body.x, y, width, 1));
            }
            if self.visible.len() > self.page_size && body.width > 0 {
                let thumb = self.selected * usize::from(height.saturating_sub(1))
                    / self.visible.len().saturating_sub(1).max(1);
                for row in 0..height {
                    frame.render_widget(
                        Line::styled(
                            if usize::from(row) == thumb {
                                "┃"
                            } else {
                                "│"
                            },
                            Style::default().fg(if usize::from(row) == thumb {
                                ACCENT
                            } else {
                                MUTED
                            }),
                        ),
                        Rect::new(body.right() - 1, top + row, 1, 1),
                    );
                }
            }
        }
        if self.filter != 0 || self.visible.is_empty() {
            frame.render_widget(
                Line::styled(
                    format!(
                        "({}/{}){}",
                        self.visible.len(),
                        self.tree.nodes.len(),
                        match self.filter {
                            1 => " [no-tools]",
                            2 => " [user]",
                            _ => "",
                        }
                    ),
                    Style::default().fg(MUTED),
                ),
                Rect::new(body.x, footer, body.width, 1),
            );
        }
    }
}

fn role(kind: Kind) -> &'static str {
    match kind {
        Kind::User => "user",
        Kind::Assistant => "assistant",
        Kind::Tools => "tools",
    }
}

fn matches_query(text: &str, query: &str) -> bool {
    let text = text.to_lowercase();
    query.split_whitespace().all(|token| {
        let mut chars = text.chars();
        token
            .to_lowercase()
            .chars()
            .all(|c| chars.by_ref().any(|t| t == c))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend, buffer::Buffer};

    fn panel() -> Panel {
        let mut panel = Panel::new();
        panel.set_tree(Tree::fixture(
            &[
                (None, Kind::User, "最初的问题"),
                (Some(0), Kind::Assistant, "共同回复"),
                (Some(1), Kind::User, "旧分支"),
                (Some(2), Kind::Tools, "read, bash"),
                (Some(1), Kind::User, "当前分支🙂"),
                (Some(4), Kind::Assistant, "当前回答"),
            ],
            Some(5),
        ));
        panel
    }

    fn draw(panel: &mut Panel, width: u16, height: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                panel.draw(frame, area);
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn contents(buffer: &Buffer) -> String {
        (0..buffer.area.height)
            .map(|y| {
                let mut text = String::new();
                let mut x = 0;
                while x < buffer.area.width {
                    let symbol = buffer[(x, y)].symbol();
                    text.push_str(symbol);
                    x += Line::from(symbol).width().max(1) as u16;
                }
                text
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn tree_panel_matches_omp_frame_connectors_selection_and_colors() {
        let mut panel = panel();
        let buffer = draw(&mut panel, 100, 16);
        let screen = contents(&buffer);
        assert!(screen.contains("╭─ Session Tree"));
        assert!(screen.contains("Search: Type to search"));
        assert!(screen.contains("├─ • user: 当前分支🙂"), "{screen}");
        assert!(screen.contains("│  • assistant: 当前回答"));
        assert!(screen.contains("└─ user: 旧分支"));
        assert!(screen.contains("tools: read, bash"));
        assert!(screen.find("当前分支").unwrap() < screen.find("旧分支").unwrap());
        assert_eq!(buffer[(0, 0)].fg, BORDER);
        let selected_y = screen.lines().position(|line| line.contains("› ")).unwrap() as u16;
        assert_eq!(buffer[(2, selected_y)].fg, ACCENT);
        assert_eq!(buffer[(2, selected_y)].bg, SELECTED);
        assert_eq!(buffer[(97, selected_y)].bg, SELECTED);
        assert!(buffer[(7, selected_y)].modifier.contains(Modifier::BOLD));
        assert_eq!(buffer[(99, selected_y)].fg, BORDER);
    }

    #[test]
    fn tree_panel_search_filter_escape_repeat_and_small_screens() {
        let mut panel = panel();
        panel.paste("USER 当前🙂");
        assert_eq!(panel.visible.len(), 1);
        let mut repeat = key(KeyCode::Enter);
        repeat.kind = KeyEventKind::Repeat;
        assert!(matches!(panel.key(repeat), Action::None));
        assert!(matches!(
            panel.key(key(KeyCode::Enter)),
            Action::Navigate(_)
        ));
        assert!(matches!(panel.key(key(KeyCode::Esc)), Action::None));
        panel.pending = false;
        assert!(matches!(panel.key(key(KeyCode::Esc)), Action::None));
        assert_eq!(panel.visible.len(), 6);
        panel.key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
        assert_eq!(panel.visible.len(), 5);
        panel.key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
        assert_eq!(panel.visible.len(), 3);
        panel.paste("不存在");
        assert!(contents(&draw(&mut panel, 100, 16)).contains("No entries match search"));
        for (width, height) in [(1, 1), (2, 2), (5, 3), (30, 10), (40, 12)] {
            draw(&mut panel, width, height);
        }
        panel.key(key(KeyCode::Esc));
        assert!(matches!(panel.key(key(KeyCode::Esc)), Action::Cancel));
    }

    #[test]
    fn tree_panel_deep_branching_keeps_content_and_unicode_visible() {
        let mut fixture = vec![(None, Kind::User, "根节点")];
        for _ in 0..1000 {
            let parent = fixture.len() - 1;
            fixture.push((Some(parent), Kind::Assistant, "旁支"));
            fixture.push((Some(parent), Kind::User, "深层🙂中文"));
        }
        let mut panel = Panel::new();
        panel.set_tree(Tree::fixture(&fixture, Some(fixture.len() - 1)));
        let screen = contents(&draw(&mut panel, 40, 12));
        assert!(screen.contains('…'));
        assert!(screen.contains("深层🙂中文"), "{screen}");
        panel.key(key(KeyCode::Home));
        panel.key(key(KeyCode::PageDown));
        panel.key(key(KeyCode::End));
        draw(&mut panel, 30, 10);
    }
}
