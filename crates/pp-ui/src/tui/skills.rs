use anyhow::{Result, anyhow};
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use pp_config::skills::Selection;
use pp_config::skills::SharedSkills;
use pp_config::skills::Skill;
use pp_config::skills::SkillSource;
use pp_config::skills::Skills;
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, ListState, Paragraph, Wrap},
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
    list_scroll: usize,
    pub(super) area: Rect,
    sidebar_area: Rect,
    list_area: Rect,
    pub(super) busy: bool,
    pub(super) loading: bool,
    pub(super) saving: bool,
    error: Option<String>,
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
            list_scroll: 0,
            source_selection: ListState::default().with_selected(Some(0)),
            sources: skills.sources,
            sources_tab: true,
            choices: BTreeMap::new(),
            entries: skills.entries,
            diagnostics: skills.diagnostics,
            area: Rect::default(),
            sidebar_area: Rect::default(),
            list_area: Rect::default(),
            busy: false,
            loading: false,
            saving: false,
            error: None,
        }
    }

    pub(super) fn loading(sources_tab: bool) -> Self {
        Self {
            selection: ListState::default(),
            detail_scroll: 0,
            list_scroll: 0,
            source_selection: ListState::default().with_selected(Some(0)),
            sources: Vec::new(),
            sources_tab,
            choices: BTreeMap::new(),
            entries: Vec::new(),
            diagnostics: Vec::new(),
            area: Rect::default(),
            sidebar_area: Rect::default(),
            list_area: Rect::default(),
            busy: true,
            loading: true,
            saving: false,
            error: None,
        }
    }

    pub(super) fn is_saving(&self) -> bool {
        self.saving
    }

    pub(super) fn set_error(&mut self, message: String) {
        self.error = Some(pp_core::text::display_text(&message).into_owned());
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
        self.set_tab(true);
    }

    fn set_tab(&mut self, sources_tab: bool) {
        self.sources_tab = sources_tab;
        self.detail_scroll = 0;
        self.list_scroll = 0;
    }

    pub(super) fn apply_snapshot(&mut self, snapshot: Skills, open_sources: bool) {
        self.error = None;
        self.entries = snapshot.entries;
        self.diagnostics = snapshot.diagnostics;
        self.sources = snapshot.sources;
        self.choices.clear();
        if open_sources {
            self.set_tab(true);
        }
        self.selection.select(
            (!self.entries.is_empty()).then_some(
                self.selection
                    .selected()
                    .unwrap_or(0)
                    .min(self.entries.len().saturating_sub(1)),
            ),
        );
        self.loading = false;
        self.busy = false;
        self.saving = false;
    }

    pub(super) fn apply_preview(&mut self, snapshot: Skills) {
        self.error = None;
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
        self.loading = false;
        self.busy = false;
        self.saving = false;
    }

    pub(super) fn handle(&mut self, event: Event) -> Action {
        if self.busy {
            return Action::None;
        }
        match event {
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                self.key(key)
            }
            Event::Mouse(mouse) if self.area.contains((mouse.column, mouse.row).into()) => {
                match mouse.kind {
                    MouseEventKind::ScrollUp => {
                        self.move_selection(-1);
                        Action::None
                    }
                    MouseEventKind::ScrollDown => {
                        self.move_selection(1);
                        Action::None
                    }
                    MouseEventKind::Down(MouseButton::Left) => {
                        let pos = (mouse.column, mouse.row).into();
                        if self.sidebar_area.contains(pos) {
                            let row = usize::from(mouse.row - self.sidebar_area.y);
                            match row {
                                0 => self.set_tab(true),
                                1 => self.set_tab(false),
                                _ => {}
                            }
                            return Action::None;
                        }
                        if self.list_area.contains(pos) {
                            let row = usize::from(mouse.row - self.list_area.y) + self.list_scroll;
                            let switch_col = mouse.column == self.list_area.x + 2;
                            if self.sources_tab {
                                if row < SkillSource::ALL.len() {
                                    self.source_selection.select(Some(row));
                                    if switch_col {
                                        return self.toggle_source(row);
                                    }
                                }
                            } else if row < self.entries.len() {
                                self.selection.select(Some(row));
                                if switch_col {
                                    self.entries[row].enabled = !self.entries[row].enabled;
                                }
                            }
                        }
                        Action::None
                    }
                    _ => Action::None,
                }
            }
            _ => Action::None,
        }
    }

    pub(super) fn key(&mut self, key: KeyEvent) -> Action {
        if self.busy {
            return Action::None;
        }
        if !key.modifiers.is_empty() && key.modifiers != KeyModifiers::SHIFT {
            return Action::None;
        }
        match key.code {
            KeyCode::PageUp => self.detail_scroll = self.detail_scroll.saturating_sub(3),
            KeyCode::PageDown => self.detail_scroll = self.detail_scroll.saturating_add(3),
            KeyCode::Esc => return Action::Cancel,
            KeyCode::Enter if key.kind == KeyEventKind::Press => {
                return Action::Save(self.draft());
            }
            KeyCode::Tab | KeyCode::BackTab if key.kind == KeyEventKind::Press => {
                self.set_tab(!self.sources_tab);
            }
            KeyCode::Char('r') if key.kind == KeyEventKind::Press => return Action::Reload,
            KeyCode::Char(' ') if key.kind == KeyEventKind::Press => {
                if self.sources_tab {
                    let selected = self.source_selection.selected().unwrap_or(0);
                    return self.toggle_source(selected);
                }
                let selected = self.selection.selected().unwrap_or(0);
                if let Some(skill) = self.entries.get_mut(selected) {
                    skill.enabled = !skill.enabled;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Home => self.jump_selection(0),
            KeyCode::End => self.jump_selection(self.current_len().saturating_sub(1)),
            _ => {}
        }
        Action::None
    }

    fn current_len(&self) -> usize {
        if self.sources_tab {
            SkillSource::ALL.len()
        } else {
            self.entries.len()
        }
    }

    fn move_selection(&mut self, delta: isize) {
        let count = self.current_len();
        if count == 0 {
            return;
        }
        let selected = if self.sources_tab {
            self.source_selection.selected().unwrap_or(0)
        } else {
            self.selection.selected().unwrap_or(0)
        };
        let next = if delta < 0 {
            selected.saturating_sub(1)
        } else {
            (selected + 1).min(count.saturating_sub(1))
        };
        self.jump_selection(next);
    }

    fn jump_selection(&mut self, index: usize) {
        let count = self.current_len();
        let target = (count > 0).then(|| index.min(count.saturating_sub(1)));
        if self.sources_tab {
            self.source_selection.select(target);
        } else {
            self.selection.select(target);
        }
    }

    fn toggle_source(&mut self, index: usize) -> Action {
        let Some(&source) = SkillSource::ALL.get(index) else {
            return Action::None;
        };
        if self.sources.contains(&source) {
            self.sources.retain(|value| *value != source);
        } else {
            self.sources.push(source);
        }
        Action::Preview(self.draft())
    }

    pub(super) fn draw(&mut self, frame: &mut Frame, area: Rect) {
        self.area = area;
        self.sidebar_area = Rect::default();
        self.list_area = Rect::default();
        if area.is_empty() {
            return;
        }
        let p = super::themes::palette();
        let ascii = super::themes::is_ascii();
        frame.render_widget(Clear, area);
        frame.render_widget(
            Block::default().style(Style::default().bg(p.panel_bg).fg(p.text)),
            area,
        );
        if area.width < 4 || area.height < 4 {
            return;
        }
        let block = Block::default()
            .borders(Borders::ALL)
            .title(super::settings::render::tabs_title(
                super::settings::Tab::Skills,
                None,
                area,
                p.accent,
                p.text,
                p.muted,
            ))
            .border_style(Style::default().fg(p.panel_border));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.height == 0 || inner.width == 0 {
            return;
        }

        let (body, detail_area, footer_area) = if inner.height >= 8 {
            let detail_height = if inner.height >= 14 { 4 } else { 2 };
            let sections = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(2),
                    Constraint::Min(2),
                    Constraint::Length(1),
                    Constraint::Length(detail_height),
                    Constraint::Length(1),
                ])
                .split(inner);
            let section_name = if self.sources_tab {
                "Sources"
            } else {
                "Global management"
            };
            frame.render_widget(
                Line::styled(
                    format!(
                        "Skills — {section_name} · Space or click the switch icon to toggle; Enter saves"
                    ),
                    Style::default().fg(p.muted),
                ),
                Rect::new(sections[0].x, sections[0].y, sections[0].width, 1),
            );
            frame.render_widget(
                Line::styled(
                    "─".repeat(usize::from(sections[0].width)),
                    Style::default().fg(p.panel_border),
                ),
                Rect::new(sections[0].x, sections[0].y + 1, sections[0].width, 1),
            );
            frame.render_widget(
                Line::styled(
                    "─".repeat(usize::from(sections[2].width)),
                    Style::default().fg(p.panel_border),
                ),
                sections[2],
            );
            (sections[1], sections[3], sections[4])
        } else if inner.height >= 4 {
            let sections = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Min(1),
                    Constraint::Length(1),
                    Constraint::Length(1),
                ])
                .split(inner);
            (sections[0], sections[1], sections[2])
        } else {
            (inner, Rect::default(), Rect::default())
        };

        let sidebar_width = super::settings::render::sidebar_width(body.width);
        if sidebar_width > 0 {
            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Length(sidebar_width),
                    Constraint::Length(1),
                    Constraint::Min(10),
                ])
                .split(body);
            self.sidebar_area = cols[0];
            self.list_area = cols[2];
            self.draw_sidebar(frame, cols[0], p, ascii);
            frame.render_widget(
                Paragraph::new(vec![Line::from("│"); usize::from(cols[1].height)])
                    .style(Style::default().fg(p.panel_border)),
                cols[1],
            );
            self.draw_list(frame, cols[2], p, ascii);
        } else {
            let narrow = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(1), Constraint::Min(1)])
                .split(body);
            let category_text = if body.width >= 30 {
                format!(
                    " Category: {} (Tab switch)",
                    if self.sources_tab {
                        "Sources"
                    } else {
                        "Global management"
                    }
                )
            } else if body.width >= 14 {
                format!(
                    " {} (Tab)",
                    if self.sources_tab {
                        "Sources"
                    } else {
                        "Global"
                    }
                )
            } else if self.sources_tab {
                "S".to_owned()
            } else {
                "G".to_owned()
            };
            frame.render_widget(
                Line::styled(
                    category_text,
                    Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
                ),
                narrow[0],
            );
            self.list_area = narrow[1];
            self.draw_list(frame, narrow[1], p, ascii);
        }

        self.draw_detail(frame, detail_area, p);
        self.draw_footer(frame, footer_area, p);
    }

    fn draw_sidebar(&self, frame: &mut Frame, area: Rect, p: &super::themes::Palette, ascii: bool) {
        if area.is_empty() {
            return;
        }
        let sources_enabled = SkillSource::ALL
            .iter()
            .filter(|source| self.sources.contains(source))
            .count();
        let sources_count = format!("{sources_enabled}/{}", SkillSource::ALL.len());
        let skills_enabled = self.entries.iter().filter(|skill| skill.enabled).count();
        let skills_count = format!("{skills_enabled}/{}", self.entries.len());
        let global_label = if 2 + 2 + "Global management".len() + 1 + skills_count.len()
            <= usize::from(area.width.saturating_sub(1))
        {
            "Global management"
        } else {
            "Global"
        };
        let items = [
            (
                self.sources_tab,
                if ascii { "S" } else { "\u{f048d}" },
                "Sources",
                sources_count,
            ),
            (
                !self.sources_tab,
                if ascii { "G" } else { "\u{f000e}" },
                global_label,
                skills_count,
            ),
        ];
        let mut lines = Vec::with_capacity(items.len());
        for (active, icon, name, count) in items {
            let label_style = if active {
                Style::default().fg(p.accent).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(p.text)
            };
            let used = Line::from(format!("  {icon} {name}{count}")).width();
            let gap = usize::from(area.width.saturating_sub(1))
                .saturating_sub(used)
                .max(1);
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("{icon} "), label_style),
                Span::styled(name, label_style),
                Span::styled(
                    format!("{}{count}", " ".repeat(gap)),
                    Style::default().fg(p.muted),
                ),
            ]));
        }
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn draw_list(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        p: &super::themes::Palette,
        ascii: bool,
    ) {
        if area.is_empty() {
            return;
        }
        if self.loading {
            frame.render_widget(
                Paragraph::new(Line::styled(
                    "Loading skills...",
                    Style::default().fg(p.muted),
                )),
                area,
            );
            return;
        }
        let enabled_dot = if ascii { "*" } else { "\u{f0765}" };
        let disabled_dot = if ascii { "o" } else { "\u{f0766}" };
        let visible = usize::from(area.height).max(1);

        if self.sources_tab {
            let selected_idx = self.source_selection.selected().unwrap_or(0);
            if selected_idx < self.list_scroll {
                self.list_scroll = selected_idx;
            } else if selected_idx >= self.list_scroll + visible {
                self.list_scroll = selected_idx + 1 - visible;
            }
            for (row, (idx, source)) in SkillSource::ALL
                .into_iter()
                .enumerate()
                .skip(self.list_scroll)
                .take(visible)
                .enumerate()
            {
                let selected = idx == selected_idx;
                let enabled = self.sources.contains(&source);
                let prefix = if selected { "❯ " } else { "  " };
                let dot = if enabled { enabled_dot } else { disabled_dot };
                let dot_color = if enabled { p.added } else { p.muted };
                let name_style = if selected {
                    Style::default().fg(p.accent).add_modifier(Modifier::BOLD)
                } else if enabled {
                    Style::default().fg(p.text)
                } else {
                    Style::default().fg(p.muted)
                };
                let line = Line::from(vec![
                    Span::styled(
                        prefix,
                        Style::default().fg(if selected { p.accent } else { p.muted }),
                    ),
                    Span::styled(format!("{dot} "), Style::default().fg(dot_color)),
                    Span::styled(format!("{:<8} ", source.name()), name_style),
                    Span::styled(source.roots().join(", "), Style::default().fg(p.muted)),
                ]);
                frame.render_widget(line, Rect::new(area.x, area.y + row as u16, area.width, 1));
            }
            return;
        }

        if self.entries.is_empty() {
            frame.render_widget(
                Paragraph::new(
                    "No skills found. Press Tab to inherit Claude/Codex skills, or update skill_dirs and press r.",
                )
                .style(Style::default().fg(p.muted))
                .wrap(Wrap { trim: false }),
                area,
            );
            return;
        }

        let selected_idx = self.selection.selected().unwrap_or(0);
        if selected_idx < self.list_scroll {
            self.list_scroll = selected_idx;
        } else if selected_idx >= self.list_scroll + visible {
            self.list_scroll = selected_idx + 1 - visible;
        }
        let name_width = self
            .entries
            .iter()
            .map(|skill| skill.name.chars().count())
            .max()
            .unwrap_or(12)
            .min(usize::from(area.width).saturating_sub(10).max(4));
        for (row, (idx, skill)) in self
            .entries
            .iter()
            .enumerate()
            .skip(self.list_scroll)
            .take(visible)
            .enumerate()
        {
            let selected = idx == selected_idx;
            let prefix = if selected { "❯ " } else { "  " };
            let dot = if skill.enabled {
                enabled_dot
            } else {
                disabled_dot
            };
            let dot_color = if skill.enabled { p.added } else { p.muted };
            let name_style = if selected {
                Style::default().fg(p.accent).add_modifier(Modifier::BOLD)
            } else if skill.enabled {
                Style::default().fg(p.text)
            } else {
                Style::default().fg(p.muted)
            };
            let line = Line::from(vec![
                Span::styled(
                    prefix,
                    Style::default().fg(if selected { p.accent } else { p.muted }),
                ),
                Span::styled(format!("{dot} "), Style::default().fg(dot_color)),
                Span::styled(
                    format!("{:<width$}  ", skill.name, width = name_width),
                    name_style,
                ),
                Span::styled(
                    skill.description.replace(['\n', '\r'], " "),
                    Style::default().fg(p.muted),
                ),
            ]);
            frame.render_widget(line, Rect::new(area.x, area.y + row as u16, area.width, 1));
        }
    }

    fn draw_detail(&self, frame: &mut Frame, area: Rect, p: &super::themes::Palette) {
        if area.is_empty() {
            return;
        }
        let mut details = Vec::new();
        if self.sources_tab {
            details.push(Line::styled(
                "Inherit user skills from selected tools; pp skill_dirs stay active.",
                Style::default().fg(p.text),
            ));
            details.push(Line::styled(
                "Tab: adjust individual skills. Enter: save sources and skills. Esc: discard.",
                Style::default().fg(p.muted),
            ));
        } else if let Some(skill) = self
            .selection
            .selected()
            .and_then(|index| self.entries.get(index))
        {
            details.push(Line::styled(
                format!("Source: {}", skill.path.display()),
                Style::default().fg(p.muted),
            ));
            details.push(Line::styled(
                skill.description.clone(),
                Style::default().fg(p.text),
            ));
        }
        for diagnostic in &self.diagnostics {
            details.push(Line::styled(diagnostic, Style::default().fg(p.removed)));
        }
        frame.render_widget(
            Paragraph::new(details)
                .wrap(Wrap { trim: false })
                .scroll((self.detail_scroll, 0)),
            area,
        );
    }

    fn draw_footer(&self, frame: &mut Frame, area: Rect, p: &super::themes::Palette) {
        if area.is_empty() {
            return;
        }
        let line = if let Some(error) = &self.error {
            Line::styled(error.as_str(), Style::default().fg(p.removed))
        } else if self.saving {
            Line::styled("Saving skills...", Style::default().fg(p.accent))
        } else if self.busy {
            Line::styled("Loading skills...", Style::default().fg(p.accent))
        } else {
            Line::styled(
                "←/→ tabs · Tab switch · ↑↓ select · Space toggle · Enter save · r rescan · Esc cancel",
                Style::default().fg(p.muted),
            )
        };
        frame.render_widget(line, area);
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
