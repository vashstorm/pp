use crate::skills::{Selection, SharedSkills, Skill, SkillSource, Skills};
use anyhow::{Result, anyhow};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Tabs, Wrap},
};
use std::{
    collections::BTreeMap,
    sync::mpsc::{self, Receiver, Sender},
    thread,
};

pub(super) enum Request {
    Open,
    Sources,
    Reload,
    Save(Selection),
    Toggle(String, bool),
    Source(SkillSource, bool),
    Preview(Selection),
}
pub(super) struct Reply {
    pub(super) result: Result<Skills>,
    pub(super) saved: bool,
    pub(super) preview: bool,
    pub(super) open_sources: bool,
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
                    let saved = matches!(
                        request,
                        Request::Save(_) | Request::Toggle(..) | Request::Source(..)
                    );
                    let preview = matches!(request, Request::Preview(_));
                    let open_sources = matches!(request, Request::Sources);
                    let result = (|| -> Result<Skills> {
                        let mut skills = shared
                            .lock()
                            .map_err(|_| anyhow!("skill registry unavailable"))?;
                        match request {
                            Request::Open | Request::Sources => {}
                            Request::Reload => skills.reload()?,
                            Request::Save(selection) => skills.save_management(&selection)?,
                            Request::Toggle(name, enabled) => {
                                skills.save_selection(&[(name, enabled)])?
                            }
                            Request::Source(source, enabled) => {
                                skills.save_source(source, enabled)?
                            }
                            Request::Preview(selection) => return skills.preview(&selection),
                        }
                        Ok(skills.clone())
                    })();
                    if sender
                        .send(Reply {
                            result,
                            saved,
                            preview,
                            open_sources,
                        })
                        .is_err()
                    {
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
    source_selection: ListState,
    sources: Vec<SkillSource>,
    sources_tab: bool,
    choices: BTreeMap<String, bool>,
    detail_scroll: u16,
    pub(super) busy: bool,
}
pub(super) enum Action {
    None,
    Cancel,
    Save(Selection),
    Preview(Selection),
    Reload,
}
impl Panel {
    pub(super) fn new(skills: Skills) -> Self {
        Self {
            selection: ListState::default()
                .with_selected((!skills.entries.is_empty()).then_some(0)),
            detail_scroll: 0,
            source_selection: ListState::default().with_selected(Some(0)),
            sources: skills.sources,
            sources_tab: true,
            choices: BTreeMap::new(),
            entries: skills.entries,
            diagnostics: skills.diagnostics,
            busy: false,
        }
    }
    fn draft(&self) -> Selection {
        let mut choices = self.choices.clone();
        choices.extend(
            self.entries
                .iter()
                .map(|skill| (skill.name.clone(), skill.enabled)),
        );
        Selection {
            sources: self.sources.clone(),
            skills: choices.into_iter().collect(),
        }
    }

    pub(super) fn show_sources(&mut self) {
        self.sources_tab = true;
    }

    pub(super) fn apply_preview(&mut self, snapshot: Skills) {
        self.choices = self.draft().skills.into_iter().collect();
        self.entries = snapshot.entries;
        self.diagnostics = snapshot.diagnostics;
        self.sources = snapshot.sources;
        self.selection.select(
            (!self.entries.is_empty()).then_some(
                self.selection
                    .selected()
                    .unwrap_or(0)
                    .min(self.entries.len().saturating_sub(1)),
            ),
        );
        self.busy = false;
    }

    pub(super) fn key(&mut self, key: KeyEvent) -> Action {
        if self.busy {
            return Action::None;
        }
        let (selection, count) = if self.sources_tab {
            (&mut self.source_selection, SkillSource::ALL.len())
        } else {
            (&mut self.selection, self.entries.len())
        };
        let selected = selection.selected().unwrap_or(0);
        match key.code {
            KeyCode::PageUp => self.detail_scroll = self.detail_scroll.saturating_sub(3),
            KeyCode::PageDown => self.detail_scroll = self.detail_scroll.saturating_add(3),
            KeyCode::Esc => return Action::Cancel,
            KeyCode::Enter if key.kind == KeyEventKind::Press => {
                return Action::Save(self.draft());
            }
            KeyCode::Tab | KeyCode::BackTab | KeyCode::Left | KeyCode::Right
                if key.kind == KeyEventKind::Press =>
            {
                self.sources_tab = !self.sources_tab;
                self.detail_scroll = 0;
            }
            KeyCode::Char('r') if key.kind == KeyEventKind::Press => return Action::Reload,
            KeyCode::Char(' ') if key.kind == KeyEventKind::Press => {
                if self.sources_tab {
                    let source = SkillSource::ALL[selected];
                    if self.sources.contains(&source) {
                        self.sources.retain(|value| *value != source);
                    } else {
                        self.sources.push(source);
                    }
                    return Action::Preview(self.draft());
                }
                if let Some(skill) = self.entries.get_mut(selected) {
                    skill.enabled = !skill.enabled;
                }
            }
            KeyCode::Up => selection.select((count > 0).then_some(selected.saturating_sub(1))),
            KeyCode::Down => {
                selection.select((count > 0).then_some((selected + 1).min(count.saturating_sub(1))))
            }
            KeyCode::Home => selection.select((count > 0).then_some(0)),
            KeyCode::End => selection.select(count.checked_sub(1)),
            _ => {}
        }
        Action::None
    }
    pub(super) fn draw(&mut self, frame: &mut Frame, area: Rect) {
        frame.render_widget(Clear, area);
        let title = if self.busy {
            " Skills · Working "
        } else {
            " Skills "
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
        let titles = if inner.width >= 34 {
            ["Sources", "Global management"]
        } else if inner.width >= 22 {
            ["Sources", "Global"]
        } else {
            ["S", "G"]
        };
        frame.render_widget(
            Tabs::new(titles)
                .select(usize::from(!self.sources_tab))
                .style(Style::default().fg(Color::DarkGray))
                .highlight_style(
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Magenta)
                        .add_modifier(Modifier::BOLD),
                )
                .divider(" │ "),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );
        if inner.height > 1 {
            frame.render_widget(
                Line::styled(
                    "─".repeat(inner.width as usize),
                    Style::default().fg(Color::DarkGray),
                ),
                Rect::new(inner.x, inner.y + 1, inner.width, 1),
            );
        }
        let inner = Rect::new(
            inner.x,
            inner.y + inner.height.min(2),
            inner.width,
            inner.height.saturating_sub(2),
        );
        if inner.height == 0 {
            return;
        }
        let detail_height = inner.height.saturating_sub(1).min(7);
        let list_area = Rect::new(
            inner.x,
            inner.y,
            inner.width,
            inner.height.saturating_sub(detail_height + 1),
        );
        if self.sources_tab {
            let rows = SkillSource::ALL.into_iter().map(|source| {
                ListItem::new(format!(
                    "[{}] {}  {}",
                    if self.sources.contains(&source) {
                        "x"
                    } else {
                        " "
                    },
                    source.name(),
                    source.roots().join(", ")
                ))
            });
            frame.render_stateful_widget(
                List::new(rows)
                    .highlight_style(Style::default().fg(Color::Yellow))
                    .highlight_symbol("❯ "),
                list_area,
                &mut self.source_selection,
            );
        } else if self.entries.is_empty() {
            frame.render_widget(
                Paragraph::new(
                    "No skills found. Press Tab to inherit Claude/Codex skills, or update skill_dirs and press r.",
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
        if self.sources_tab {
            details.push(Line::raw(
                "Inherit user skills from selected tools; pp skill_dirs stay active.",
            ));
            details.push(Line::raw(
                "Tab: adjust individual skills. Enter: save sources and skills. Esc: discard.",
            ));
        } else if let Some(skill) = self
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
                "Tab/←→ tabs · ↑↓ select · Space toggle · Enter save · Esc cancel · r rescan",
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
        panel.key(press(KeyCode::Tab));
        panel.key(press(KeyCode::Char(' ')));
        assert!(!panel.entries[0].enabled);
        assert!(
            matches!(panel.key(press(KeyCode::Enter)), Action::Save(selection) if selection.skills == [("rust".into(), false)])
        );
        let mut repeat = press(KeyCode::Char(' '));
        repeat.kind = KeyEventKind::Repeat;
        panel.key(repeat);
        assert!(!panel.entries[0].enabled);
        panel.busy = true;
        assert!(matches!(panel.key(press(KeyCode::Enter)), Action::None));
        assert!(matches!(panel.key(press(KeyCode::Esc)), Action::None));
        let mut empty = Panel::new(Skills::default());
        empty.key(press(KeyCode::Tab));
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
            matches!(empty.key(press(KeyCode::Enter)), Action::Save(selection) if selection.skills.is_empty())
        );
    }
}
