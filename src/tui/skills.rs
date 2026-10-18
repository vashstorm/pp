use crate::skills::{SharedSkills, Skill, Skills};
use anyhow::{Result, anyhow};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};
use std::{
    sync::mpsc::{self, Receiver, Sender},
    thread,
};

pub(super) enum Request {
    Open,
    Reload,
    Save(Vec<(String, bool)>),
}
pub(super) struct Reply {
    pub(super) result: Result<Skills>,
    pub(super) saved: bool,
}
pub(super) struct Worker {
    pub(super) requests: Sender<Request>,
    pub(super) replies: Receiver<Reply>,
}
impl Worker {
    pub(super) fn new(shared: SharedSkills) -> Result<Self> {
        let (requests, receiver) = mpsc::channel();
        let (sender, replies) = mpsc::channel();
        thread::Builder::new()
            .name("pp-skills".into())
            .spawn(move || {
                while let Ok(request) = receiver.recv() {
                    let saved = matches!(request, Request::Save(_));
                    let result = (|| -> Result<Skills> {
                        let mut skills = shared
                            .lock()
                            .map_err(|_| anyhow!("skill registry unavailable"))?;
                        match request {
                            Request::Open => {}
                            Request::Reload => skills.reload()?,
                            Request::Save(selection) => skills.save_selection(&selection)?,
                        }
                        Ok(skills.clone())
                    })();
                    if sender.send(Reply { result, saved }).is_err() {
                        break;
                    }
                }
            })?;
        Ok(Self { requests, replies })
    }
}

pub(super) struct Panel {
    pub(super) entries: Vec<Skill>,
    pub(super) diagnostics: Vec<String>,
    selection: ListState,
    detail_scroll: u16,
    pub(super) busy: bool,
}
pub(super) enum Action {
    None,
    Cancel,
    Save(Vec<(String, bool)>),
    Reload,
}
impl Panel {
    pub(super) fn new(skills: Skills) -> Self {
        Self {
            selection: ListState::default()
                .with_selected((!skills.entries.is_empty()).then_some(0)),
            detail_scroll: 0,
            entries: skills.entries,
            diagnostics: skills.diagnostics,
            busy: false,
        }
    }
    pub(super) fn key(&mut self, key: KeyEvent) -> Action {
        if self.busy {
            return Action::None;
        }
        let selected = self.selection.selected().unwrap_or(0);
        match key.code {
            KeyCode::PageUp => self.detail_scroll = self.detail_scroll.saturating_sub(3),
            KeyCode::PageDown => self.detail_scroll = self.detail_scroll.saturating_add(3),
            KeyCode::Esc => return Action::Cancel,
            KeyCode::Enter if key.kind == KeyEventKind::Press => {
                return Action::Save(
                    self.entries
                        .iter()
                        .map(|skill| (skill.name.clone(), skill.enabled))
                        .collect(),
                );
            }
            KeyCode::Char('r') if key.kind == KeyEventKind::Press => return Action::Reload,
            KeyCode::Char(' ') if key.kind == KeyEventKind::Press => {
                if let Some(skill) = self.entries.get_mut(selected) {
                    skill.enabled = !skill.enabled;
                }
            }
            KeyCode::Up => self
                .selection
                .select((!self.entries.is_empty()).then_some(selected.saturating_sub(1))),
            KeyCode::Down => self.selection.select(
                (!self.entries.is_empty())
                    .then_some((selected + 1).min(self.entries.len().saturating_sub(1))),
            ),
            KeyCode::Home => self
                .selection
                .select((!self.entries.is_empty()).then_some(0)),
            KeyCode::End => self.selection.select(self.entries.len().checked_sub(1)),
            _ => {}
        }
        Action::None
    }
    pub(super) fn draw(&mut self, frame: &mut Frame, area: Rect) {
        frame.render_widget(Clear, area);
        let title = if self.busy {
            " Skills · Working "
        } else {
            " Skills · Global management "
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(Style::default().fg(Color::Magenta));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.height == 0 || inner.width == 0 {
            return;
        }
        let detail_height = inner.height.saturating_sub(1).min(7);
        let list_area = Rect::new(
            inner.x,
            inner.y,
            inner.width,
            inner.height.saturating_sub(detail_height + 1),
        );
        if self.entries.is_empty() {
            frame.render_widget(
                Paragraph::new(
                    "No skills found. Update skill_dirs in global settings.jsonc, then press r to rescan.",
                ),
                list_area,
            );
        } else {
            let rows = self.entries.iter().map(|skill| {
                ListItem::new(Line::from(vec![
                    Span::raw(format!(
                        "[{}] {}  ",
                        if skill.enabled { "x" } else { " " },
                        skill.name
                    )),
                    Span::styled(
                        skill.description.replace(['\n', '\r'], " "),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]))
            });
            frame.render_stateful_widget(
                List::new(rows)
                    .highlight_style(Style::default().fg(Color::Yellow))
                    .highlight_symbol("❯ "),
                list_area,
                &mut self.selection,
            );
        }
        let mut details = Vec::new();
        if let Some(skill) = self
            .selection
            .selected()
            .and_then(|index| self.entries.get(index))
        {
            details.push(Line::raw(format!("Source: {}", skill.path.display())));
            details.push(Line::raw(skill.description.clone()));
        }
        for diagnostic in &self.diagnostics {
            details.push(Line::styled(diagnostic, Style::default().fg(Color::Red)));
        }
        frame.render_widget(
            Paragraph::new(details)
                .wrap(Wrap { trim: false })
                .scroll((self.detail_scroll, 0)),
            Rect::new(inner.x, list_area.bottom(), inner.width, detail_height),
        );
        frame.render_widget(
            Line::raw(
                "↑↓ select · Space toggle · Enter save · Esc cancel · r rescan · PgUp/PgDn details",
            ),
            Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    #[test]
    fn panel_toggles_saves_empty_lists_and_ignores_repeat_or_pending_actions() {
        let mut snapshot = Skills::default();
        snapshot.entries.push(Skill {
            name: "rust".into(),
            description: "测试".into(),
            path: "SKILL.md".into(),
            enabled: true,
        });
        let mut panel = Panel::new(snapshot);
        let press = |code| KeyEvent::new(code, KeyModifiers::NONE);
        panel.key(press(KeyCode::Char(' ')));
        assert!(!panel.entries[0].enabled);
        assert!(
            matches!(panel.key(press(KeyCode::Enter)), Action::Save(selection) if selection == [("rust".into(), false)])
        );
        let mut repeat = press(KeyCode::Char(' '));
        repeat.kind = KeyEventKind::Repeat;
        panel.key(repeat);
        assert!(!panel.entries[0].enabled);
        panel.busy = true;
        assert!(matches!(panel.key(press(KeyCode::Enter)), Action::None));
        assert!(matches!(panel.key(press(KeyCode::Esc)), Action::None));
        let mut empty = Panel::new(Skills::default());
        for code in [
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::Home,
            KeyCode::End,
            KeyCode::Char(' '),
        ] {
            empty.key(press(code));
        }
        assert!(
            matches!(empty.key(press(KeyCode::Enter)), Action::Save(selection) if selection.is_empty())
        );
    }
}
