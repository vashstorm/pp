//! Inline session-tree selection; history I/O and navigation remain on the worker.
use super::transcript::{Transcript, TreeBlock};
use super::transcript_style::{ACCENT, ADDED, DIM, MUTED};
use crate::agent::persistence::tree::{Kind, Tree, View};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    text::{Line, Span},
};

pub(super) const HINT: &str = "↑/↓ step  ←/→ user turns  f filter  󰌑 rewind  󱊷 cancel";

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
    selected: Option<usize>,
    filter: usize,
    preview: Transcript,
    blocks: Vec<TreeBlock>,
    reveal: bool,
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
            selected: None,
            filter: 0,
            preview: Transcript::new(),
            blocks: Vec::new(),
            reveal: true,
            pending: false,
            loading: true,
        }
    }

    pub fn is_loading(&self) -> bool {
        self.loading
    }

    pub fn set_tree(&mut self, view: View) {
        self.tree = view.tree;
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
        self.preview = Transcript::new();
        self.blocks = view
            .records
            .into_iter()
            .enumerate()
            .map(|(index, records)| {
                let entries = self
                    .preview
                    .append_tree_node(records, &self.tree.nodes[index].text);
                TreeBlock {
                    entries,
                    label: Line::default(),
                }
            })
            .collect();
        for row in &self.rows {
            self.blocks[row.index].label = self.label(*row);
        }
        self.selected = self
            .tree
            .leaf
            .or_else(|| self.rows.first().map(|row| row.index));
        self.refilter();
    }

    fn label(&self, row: Row) -> Line<'static> {
        // Compress only the branch label. Message bodies always retain their full width.
        let shift = row.depth.saturating_sub(6);
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
        let kind = self.tree.nodes[row.index].kind;
        Line::from(vec![
            Span::styled(
                prefix.into_iter().collect::<String>(),
                Style::default().fg(DIM),
            ),
            Span::styled(
                if self.active[row.index] { "• " } else { "" },
                Style::default().fg(ACCENT),
            ),
            Span::styled(
                role(kind),
                Style::default().fg(match kind {
                    Kind::User => ACCENT,
                    Kind::Assistant => ADDED,
                    Kind::Tools => MUTED,
                }),
            ),
        ])
    }

    fn shown(&self, index: usize) -> bool {
        match self.filter {
            1 => self.tree.nodes[index].kind != Kind::Tools,
            2 => self.tree.nodes[index].kind == Kind::User,
            _ => true,
        }
    }

    fn refilter(&mut self) {
        let old_position = self
            .selected
            .and_then(|selected| self.rows.iter().position(|row| row.index == selected));
        self.visible = self
            .rows
            .iter()
            .filter_map(|row| self.shown(row.index).then_some(row.index))
            .collect();
        if !self
            .selected
            .is_some_and(|index| self.visible.contains(&index))
        {
            self.selected = old_position
                .and_then(|position| {
                    self.rows[position..]
                        .iter()
                        .find(|row| self.shown(row.index))
                        .map(|row| row.index)
                })
                .or_else(|| self.visible.last().copied());
        }
        self.reveal = true;
    }

    pub fn key(&mut self, key: KeyEvent) -> Action {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
            || !key.modifiers.is_empty()
        {
            return Action::None;
        }
        if key.code == KeyCode::Esc {
            return if self.pending {
                Action::None
            } else {
                Action::Cancel
            };
        }
        if self.pending || self.loading {
            return Action::None;
        }
        let position = self
            .selected
            .and_then(|index| self.visible.iter().position(|&i| i == index));
        let next = match key.code {
            KeyCode::Enter if key.kind == KeyEventKind::Press => {
                if let Some(index) = self.selected {
                    if Some(index) == self.tree.leaf {
                        return Action::Cancel;
                    }
                    self.pending = true;
                    return Action::Navigate(self.tree.nodes[index].id.clone());
                }
                return Action::None;
            }
            KeyCode::Char('f') if key.kind == KeyEventKind::Press => {
                self.filter = (self.filter + 1) % 3;
                self.refilter();
                return Action::None;
            }
            KeyCode::Up => position.map(|i| i.saturating_sub(1)),
            KeyCode::Down => position.map(|i| (i + 1).min(self.visible.len().saturating_sub(1))),
            KeyCode::Left => position.and_then(|i| {
                (0..i)
                    .rev()
                    .find(|&p| self.tree.nodes[self.visible[p]].kind == Kind::User)
            }),
            KeyCode::Right => position.and_then(|i| {
                (i + 1..self.visible.len())
                    .find(|&p| self.tree.nodes[self.visible[p]].kind == Kind::User)
            }),
            KeyCode::Home => (!self.visible.is_empty()).then_some(0),
            KeyCode::End => self.visible.len().checked_sub(1),
            KeyCode::PageUp => {
                self.preview.page_up();
                self.reveal = false;
                return Action::None;
            }
            KeyCode::PageDown => {
                self.preview.page_down();
                self.reveal = false;
                return Action::None;
            }
            _ => return Action::None,
        };
        if let Some(next) = next {
            self.selected = Some(self.visible[next]);
            self.reveal = true;
        }
        Action::None
    }

    pub fn wheel(&mut self, up: bool, lines: usize) {
        if self.pending || self.loading {
            return;
        }
        if up {
            self.preview.scroll_up(lines);
        } else {
            self.preview.scroll_down(lines);
        }
        self.reveal = false;
    }

    pub fn draw_status(&self, frame: &mut Frame, area: Rect) {
        let status = if self.loading {
            "Loading session tree…".into()
        } else if self.pending {
            "Switching branch…".into()
        } else {
            format!(
                "[{}] {}/{}",
                ["all", "no-tools", "user"][self.filter],
                self.selected
                    .and_then(|selected| self.visible.iter().position(|&i| i == selected))
                    .map_or(0, |i| i + 1),
                self.visible.len()
            )
        };
        frame.render_widget(Line::styled(status, Style::default().fg(MUTED)), area);
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect) {
        if area.is_empty() {
            return;
        }
        let height = area.height.saturating_sub(1);
        self.preview.draw_tree(
            frame,
            Rect::new(area.x, area.y, area.width, height),
            &self.blocks,
            &self.visible,
            self.selected,
            self.reveal,
        );
        if height > 0 {
            self.reveal = false;
        }
        self.draw_status(frame, Rect::new(area.x, area.y + height, area.width, 1));
    }
}

fn role(kind: Kind) -> &'static str {
    match kind {
        Kind::User => "user",
        Kind::Assistant => "assistant",
        Kind::Tools => "tools",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{agent::persistence::ReplayRecord, events::AgentEvent};
    use crossterm::event::KeyModifiers;
    use ratatui::{Terminal, backend::TestBackend, buffer::Buffer};

    fn view(nodes: &[(Option<usize>, Kind, &str)], leaf: Option<usize>) -> View {
        View {
            tree: Tree::fixture(nodes, leaf),
            records: nodes
                .iter()
                .map(|(_, kind, text)| match kind {
                    Kind::User => vec![ReplayRecord::User((*text).into())],
                    _ => vec![
                        ReplayRecord::Event(AgentEvent::AssistantStarted),
                        ReplayRecord::Event(AgentEvent::AssistantFinished {
                            text: (*text).into(),
                        }),
                    ],
                })
                .collect(),
        }
    }
    fn panel() -> Panel {
        let mut panel = Panel::new();
        panel.set_tree(view(
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
            .draw(|frame| panel.draw(frame, frame.area()))
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
                    x += Line::raw(symbol).width().max(1) as u16;
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
    fn tree_navigation_clamps_steps_jumps_users_and_preserves_filter_identity() {
        let mut panel = panel();
        assert_eq!(panel.selected, Some(5));
        assert_eq!(panel.visible, vec![0, 1, 4, 5, 2, 3]);
        panel.key(key(KeyCode::Home));
        panel.key(key(KeyCode::Up));
        assert_eq!(panel.selected, Some(0));
        panel.key(key(KeyCode::Right));
        assert_eq!(panel.selected, Some(4));
        panel.key(key(KeyCode::Right));
        assert_eq!(panel.selected, Some(2));
        panel.key(key(KeyCode::Right));
        assert_eq!(panel.selected, Some(2));
        panel.key(key(KeyCode::Left));
        assert_eq!(panel.selected, Some(4));
        panel.key(key(KeyCode::End));
        panel.key(key(KeyCode::Down));
        assert_eq!(panel.selected, Some(3));
        panel.key(key(KeyCode::Char('f')));
        assert_eq!(panel.selected, Some(2));
        assert_eq!(panel.visible, vec![0, 1, 4, 5, 2]);
        panel.key(key(KeyCode::Char('f')));
        assert_eq!(panel.visible, vec![0, 4, 2]);
        assert_eq!(panel.selected, Some(2));
        panel.key(key(KeyCode::Char('f')));
        assert_eq!(panel.selected, Some(2));
        panel.key(key(KeyCode::Home));
        panel.key(key(KeyCode::Down));
        panel.key(key(KeyCode::Char('f')));
        panel.key(key(KeyCode::Char('f')));
        assert_eq!(panel.selected, Some(4));
    }
    #[test]
    fn tree_root_rewind_still_opens_with_a_selectable_first_node() {
        let mut panel = Panel::new();
        panel.set_tree(view(
            &[
                (None, Kind::User, "question"),
                (Some(0), Kind::Assistant, "answer"),
            ],
            None,
        ));
        assert_eq!(panel.selected, Some(0));
        assert!(matches!(panel.key(key(KeyCode::Enter)), Action::Navigate(id) if id == "node-0"));
    }

    #[test]
    fn tree_navigation_submission_repeat_cancel_and_input_consumption() {
        let mut panel = panel();
        assert!(matches!(panel.key(key(KeyCode::Enter)), Action::Cancel));
        panel.key(key(KeyCode::Home));
        let mut repeat = key(KeyCode::Enter);
        repeat.kind = KeyEventKind::Repeat;
        assert!(matches!(panel.key(repeat), Action::None));
        for c in ['x', 'o', '文'] {
            panel.key(key(KeyCode::Char(c)));
        }
        panel.key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
        let mut repeated_filter = key(KeyCode::Char('f'));
        repeated_filter.kind = KeyEventKind::Repeat;
        panel.key(repeated_filter);
        assert_eq!(panel.filter, 0);
        assert_eq!(panel.selected, Some(0));
        assert!(matches!(panel.key(key(KeyCode::Enter)),Action::Navigate(id) if id=="node-0"));
        assert!(matches!(panel.key(key(KeyCode::Enter)), Action::None));
        assert!(matches!(panel.key(key(KeyCode::Esc)), Action::None));
        panel.pending = false;
        assert!(matches!(panel.key(key(KeyCode::Esc)), Action::Cancel));
        assert!(matches!(
            Panel::new().key(key(KeyCode::Esc)),
            Action::Cancel
        ));
    }
    #[test]
    fn tree_all_tools_filter_has_no_candidate_and_small_screens_remain_cancellable() {
        let mut panel = Panel::new();
        panel.set_tree(view(&[(None, Kind::Tools, "tools")], Some(0)));
        panel.key(key(KeyCode::Char('f')));
        panel.key(key(KeyCode::Char('f')));
        assert!(panel.visible.is_empty());
        assert_eq!(panel.selected, None);
        assert!(matches!(panel.key(key(KeyCode::Enter)), Action::None));
        assert!(
            contents(&draw(&mut panel, 60, 12)).contains("Entries hidden by the current filter")
        );
        for (w, h) in [(1, 1), (2, 2), (5, 3), (40, 12)] {
            draw(&mut panel, w, h);
            assert!(matches!(panel.key(key(KeyCode::Esc)), Action::Cancel));
        }
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
        panel.set_tree(view(&fixture, Some(fixture.len() - 1)));
        let screen = contents(&draw(&mut panel, 40, 12));
        assert!(screen.contains('…'));
        assert!(screen.contains("深层🙂中文"), "{screen}");
        panel.key(key(KeyCode::Home));
        panel.key(key(KeyCode::PageDown));
        panel.key(key(KeyCode::End));
        draw(&mut panel, 30, 10);
    }
}
