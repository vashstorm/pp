//! Inline current-history selection; history I/O and navigation remain on the worker.
use super::transcript::{Transcript, TreeBlock};
use super::transcript_style::{ACCENT, ADDED, MUTED};
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

pub(super) struct Panel {
    tree: Tree,
    visible: Vec<usize>,
    selected: Option<usize>,
    filter: usize,
    preview: Transcript,
    blocks: Vec<TreeBlock>,
    reveal: bool,
    pub pending: bool,
    loading: bool,
    error: Option<String>,
}

impl Panel {
    pub fn new() -> Self {
        Self {
            tree: Tree::default(),
            visible: Vec::new(),
            selected: None,
            filter: 0,
            preview: Transcript::new(),
            blocks: Vec::new(),
            reveal: true,
            pending: false,
            loading: true,
            error: None,
        }
    }

    pub(super) fn set_error(&mut self, message: String) {
        self.error = Some(super::display_text(&message).into_owned());
    }

    pub fn is_loading(&self) -> bool {
        self.loading
    }

    pub fn set_tree(&mut self, view: View) {
        self.tree = view.tree;
        self.loading = false;
        self.preview = Transcript::new();
        self.blocks = view
            .records
            .into_iter()
            .enumerate()
            .map(|(index, records)| {
                let node = &self.tree.nodes[index];
                let entries = self.preview.append_tree_node(records, &node.text);
                TreeBlock {
                    entries,
                    label: Line::from(vec![
                        Span::raw(" "),
                        Span::styled("• ", Style::default().fg(ACCENT)),
                        Span::styled(
                            role(node.kind),
                            Style::default().fg(match node.kind {
                                Kind::User => ACCENT,
                                Kind::Assistant => ADDED,
                                Kind::Tools => MUTED,
                            }),
                        ),
                        Span::raw(" "),
                    ]),
                }
            })
            .collect();
        self.selected = self.tree.leaf;
        self.refilter();
    }

    fn shown(&self, index: usize) -> bool {
        match self.filter {
            1 => self.tree.nodes[index].kind != Kind::Tools,
            2 => self.tree.nodes[index].kind == Kind::User,
            _ => true,
        }
    }

    fn refilter(&mut self) {
        self.visible = (0..self.tree.nodes.len())
            .filter(|&index| self.shown(index))
            .collect();
        if !self
            .selected
            .is_some_and(|index| self.visible.contains(&index))
        {
            self.selected = self
                .selected
                .and_then(|selected| {
                    self.visible
                        .iter()
                        .copied()
                        .find(|&index| index >= selected)
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
        let status = if let Some(error) = &self.error {
            error.clone()
        } else if self.loading {
            "Loading session tree…".into()
        } else if self.pending {
            "Rewinding conversation…".into()
        } else {
            format!(
                "[current] [{}] {}/{}",
                ["all", "no-tools", "user"][self.filter],
                self.selected
                    .and_then(|selected| self.visible.iter().position(|&i| i == selected))
                    .map_or(0, |i| i + 1),
                self.visible.len()
            )
        };
        frame.render_widget(
            Line::styled(
                status,
                Style::default().fg(if self.error.is_some() {
                    super::transcript_style::ERROR_BORDER
                } else {
                    MUTED
                }),
            ),
            area,
        );
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect) {
        if area.is_empty() {
            return;
        }
        let height = area.height.saturating_sub(1);
        let body = Rect::new(area.x, area.y, area.width, height);
        if self.tree.nodes.is_empty() {
            frame.render_widget(
                Line::styled(
                    "No messages in current history.",
                    Style::default().fg(MUTED),
                ),
                body,
            );
        } else {
            self.preview.draw_tree(
                frame,
                body,
                &self.blocks,
                &self.visible,
                self.selected,
                self.reveal,
            );
        }
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
                (Some(1), Kind::User, "工具问题"),
                (Some(2), Kind::Tools, "read, bash"),
                (Some(3), Kind::User, "当前问题🙂"),
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
        panel.key(key(KeyCode::Home));
        panel.key(key(KeyCode::Up));
        assert_eq!(panel.selected, Some(0));
        panel.key(key(KeyCode::Right));
        assert_eq!(panel.selected, Some(2));
        panel.key(key(KeyCode::Right));
        panel.key(key(KeyCode::Right));
        assert_eq!(panel.selected, Some(4));
        panel.key(key(KeyCode::Left));
        assert_eq!(panel.selected, Some(2));
        panel.key(key(KeyCode::End));
        panel.key(key(KeyCode::Down));
        assert_eq!(panel.selected, Some(5));
        panel.key(key(KeyCode::Char('f')));
        assert_eq!(panel.visible, [0, 1, 2, 4, 5]);
        assert_eq!(panel.selected, Some(5));
        panel.key(key(KeyCode::Char('f')));
        assert_eq!(panel.visible, [0, 2, 4]);
        assert_eq!(panel.selected, Some(4));
        panel.key(key(KeyCode::Char('f')));
        panel.key(key(KeyCode::Home));
        panel.key(key(KeyCode::Down));
        panel.key(key(KeyCode::Char('f')));
        panel.key(key(KeyCode::Char('f')));
        assert_eq!(panel.selected, Some(2));
    }

    #[test]
    fn tree_empty_history_has_no_navigation_target() {
        let mut panel = Panel::new();
        panel.set_tree(view(&[], None));
        panel.key(key(KeyCode::Char('b')));
        assert_eq!(panel.selected, None);
        assert!(matches!(panel.key(key(KeyCode::Enter)), Action::None));
        assert!(contents(&draw(&mut panel, 80, 12)).contains("No messages in current history."));
    }

    #[test]
    fn tree_navigation_submission_repeat_cancel_and_input_consumption() {
        let mut panel = panel();
        assert!(matches!(panel.key(key(KeyCode::Enter)), Action::Cancel));
        panel.key(key(KeyCode::Home));
        let mut repeat = key(KeyCode::Enter);
        repeat.kind = KeyEventKind::Repeat;
        assert!(matches!(panel.key(repeat), Action::None));
        for c in ['b', 'x', 'o', '文'] {
            panel.key(key(KeyCode::Char(c)));
        }
        panel.key(KeyEvent::new(
            KeyCode::Char('o'),
            crate::shortcuts::FUNCTION_MODIFIER,
        ));
        let mut repeated_filter = key(KeyCode::Char('f'));
        repeated_filter.kind = KeyEventKind::Repeat;
        panel.key(repeated_filter);
        assert_eq!(panel.filter, 0);
        assert_eq!(panel.selected, Some(0));
        assert!(matches!(panel.key(key(KeyCode::Enter)), Action::Navigate(id) if id == "node-0"));
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
    fn tree_long_history_keeps_content_and_unicode_visible() {
        let mut fixture = vec![(None, Kind::User, "根节点")];
        for _ in 0..1000 {
            fixture.push((Some(fixture.len() - 1), Kind::User, "深层🙂中文"));
        }
        let mut panel = Panel::new();
        panel.set_tree(view(&fixture, Some(fixture.len() - 1)));
        let screen = contents(&draw(&mut panel, 40, 12));
        assert!(screen.contains("深层🙂中文"), "{screen}");
        panel.key(key(KeyCode::Home));
        panel.key(key(KeyCode::PageDown));
        panel.key(key(KeyCode::End));
        draw(&mut panel, 30, 10);
    }
}
