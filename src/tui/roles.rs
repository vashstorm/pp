use crate::config::{ModelChoice, ModelRole, ModelRoles, RoleAssignment, ThinkLevel};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};

const BG: Color = Color::Rgb(40, 42, 54);
const BORDER: Color = Color::Rgb(36, 126, 155);
const GOLD: Color = Color::Rgb(255, 191, 51);
const TEXT: Color = Color::Rgb(205, 207, 214);
const MUTED: Color = Color::Rgb(98, 104, 118);
const GREEN: Color = Color::Rgb(131, 207, 126);

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Roles,
    Models,
    Effort,
}

pub(super) enum Action {
    None,
    Close,
    Save(ModelRole, Option<RoleAssignment>),
}

pub(super) struct Panel {
    choices: Vec<ModelChoice>,
    roles: ModelRoles,
    providers: Vec<String>,
    fallback: String,
    fallback_effort: ThinkLevel,
    view: View,
    sidebar: usize,
    sidebar_focus: bool,
    role_index: usize,
    selected: usize,
    query: String,
    pending: Option<RoleAssignment>,
    area: Rect,
    body: Rect,
    sidebar_area: Rect,
    sidebar_scroll: usize,
    scroll: usize,
    pub(super) busy: bool,
    message: String,
}

impl Panel {
    pub(super) fn new(
        choices: Vec<ModelChoice>,
        roles: ModelRoles,
        fallback: String,
        effort: ThinkLevel,
    ) -> Self {
        let mut panel = Self {
            choices: vec![],
            roles,
            providers: vec![],
            fallback,
            fallback_effort: effort,
            view: View::Roles,
            sidebar: 0,
            sidebar_focus: true,
            role_index: 0,
            selected: 0,
            query: String::new(),
            pending: None,
            area: Rect::default(),
            body: Rect::default(),
            sidebar_area: Rect::default(),
            sidebar_scroll: 0,
            scroll: 0,
            busy: false,
            message: String::new(),
        };
        panel.update_choices(choices);
        panel
    }

    pub(super) fn update_choices(&mut self, choices: Vec<ModelChoice>) {
        self.providers = choices
            .iter()
            .filter_map(|choice| choice.reference.split_once('/').map(|(p, _)| p.to_owned()))
            .collect();
        self.providers.sort();
        self.providers.dedup();
        self.choices = choices;
        self.sidebar = self.sidebar.min(self.providers.len() + 1);
        self.selected = self
            .selected
            .min(self.model_indices().len().saturating_sub(1));
    }

    pub(super) fn update_roles(&mut self, roles: ModelRoles, fallback: String, effort: ThinkLevel) {
        self.roles = roles;
        self.fallback = fallback;
        self.fallback_effort = effort;
    }

    pub(super) fn saved(&mut self) {
        self.busy = false;
        self.view = View::Roles;
        self.sidebar = 0;
        self.sidebar_focus = false;
        self.pending = None;
        self.message.clear();
        self.scroll = 0;
    }

    fn role(&self) -> ModelRole {
        ModelRole::ALL[self.role_index.min(ModelRole::ALL.len() - 1)]
    }

    fn content_offset(&self) -> u16 {
        if self.view == View::Roles { 1 } else { 2 }
    }

    fn effective(&self) -> RoleAssignment {
        self.roles
            .get(&self.role())
            .cloned()
            .unwrap_or_else(|| RoleAssignment {
                enabled: false,
                model: self.fallback.clone(),
                effort: self.fallback_effort,
            })
    }

    fn toggle_role(&self) -> Action {
        let mut assignment = self.effective();
        assignment.enabled = !assignment.enabled;
        Action::Save(self.role(), Some(assignment))
    }

    fn model_indices(&self) -> Vec<usize> {
        let provider = self
            .sidebar
            .checked_sub(2)
            .and_then(|index| self.providers.get(index));
        let query = self.query.to_lowercase();
        self.choices
            .iter()
            .enumerate()
            .filter(|(_, choice)| {
                provider.is_none_or(|p| {
                    choice
                        .reference
                        .split_once('/')
                        .is_some_and(|(id, _)| id == p)
                }) && (choice.reference.to_lowercase().contains(&query)
                    || choice.model.to_lowercase().contains(&query))
            })
            .map(|(index, _)| index)
            .collect()
    }

    fn begin_models(&mut self) {
        let selected = self.effective().model;
        self.sidebar = 1;
        self.sidebar_focus = false;
        self.view = View::Models;
        self.query.clear();
        self.selected = self
            .model_indices()
            .iter()
            .position(|&index| self.choices[index].reference == selected)
            .unwrap_or(0);
        self.scroll = 0;
        self.message.clear();
    }

    fn begin_effort(&mut self, model: String) {
        let Some(choice) = self.choices.iter().find(|choice| choice.reference == model) else {
            self.message = "Model is no longer available; select a model again".into();
            return;
        };
        let old = self.effective().effort;
        let effort = if choice.thinking.supports(old) {
            old
        } else {
            choice.thinking.effective_default(self.fallback_effort)
        };
        self.selected = choice
            .thinking
            .levels()
            .position(|level| level == effort)
            .unwrap_or(0);
        self.pending = Some(RoleAssignment {
            enabled: self
                .roles
                .get(&self.role())
                .is_none_or(|assignment| assignment.enabled),
            model,
            effort,
        });
        self.view = View::Effort;
        self.sidebar_focus = false;
        self.scroll = 0;
        self.message.clear();
    }

    fn efforts(&self) -> Vec<ThinkLevel> {
        self.pending
            .as_ref()
            .and_then(|assignment| {
                self.choices
                    .iter()
                    .find(|choice| choice.reference == assignment.model)
            })
            .map(|choice| choice.thinking.levels().collect())
            .unwrap_or_default()
    }

    fn activate_sidebar(&mut self) {
        self.view = if self.sidebar == 0 {
            View::Roles
        } else {
            View::Models
        };
        self.query.clear();
        self.selected = 0;
        self.scroll = 0;
        self.pending = None;
    }

    pub(super) fn handle(&mut self, event: Event) -> Action {
        if self.busy {
            return Action::None;
        }
        match event {
            Event::Paste(text) if self.view == View::Models && !self.sidebar_focus => {
                self.query.push_str(&text.replace(['\r', '\n'], " "));
                self.selected = 0;
            }
            Event::Mouse(mouse) => {
                if !self.area.contains((mouse.column, mouse.row).into()) {
                    return Action::None;
                }
                let sidebar = self.sidebar_area.contains((mouse.column, mouse.row).into());
                if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
                    if sidebar {
                        let row = usize::from(mouse.row.saturating_sub(self.sidebar_area.y))
                            + self.sidebar_scroll;
                        let index = match row {
                            0..=1 => Some(row),
                            3.. => Some(row - 1),
                            _ => None,
                        };
                        if let Some(index) = index.filter(|index| *index < self.providers.len() + 2)
                        {
                            self.sidebar = index;
                            self.sidebar_focus = true;
                            self.activate_sidebar();
                        }
                    } else if self.body.contains((mouse.column, mouse.row).into()) {
                        self.sidebar_focus = false;
                        let content_y = self.body.y + self.content_offset();
                        let row = usize::from(mouse.row.saturating_sub(content_y)) + self.scroll;
                        if self.view == View::Roles {
                            if mouse.row >= content_y && row < ModelRole::ALL.len() {
                                self.role_index = row;
                                if mouse.column == self.body.x + 2 {
                                    return self.toggle_role();
                                }
                            }
                        } else if self.view == View::Models {
                            self.selected = row.min(self.model_indices().len().saturating_sub(1));
                        }
                    }
                } else if matches!(
                    mouse.kind,
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                ) {
                    self.move_selection(mouse.kind == MouseEventKind::ScrollDown, 3);
                }
            }
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                if key.code == KeyCode::Esc {
                    if self.view == View::Effort {
                        self.view = View::Models;
                        self.pending = None;
                        self.selected = 0;
                    } else if self.view == View::Models {
                        self.saved();
                    } else {
                        return Action::Close;
                    }
                    return Action::None;
                }
                if !key.modifiers.is_empty() && key.modifiers != KeyModifiers::SHIFT {
                    return Action::None;
                }
                match key.code {
                    KeyCode::Char(' ')
                        if self.view == View::Roles && key.kind == KeyEventKind::Press =>
                    {
                        self.sidebar_focus = false;
                        return self.toggle_role();
                    }
                    KeyCode::Up | KeyCode::Down => {
                        self.move_selection(key.code == KeyCode::Down, 1)
                    }
                    KeyCode::PageUp | KeyCode::PageDown => self.move_selection(
                        key.code == KeyCode::PageDown,
                        usize::from(self.body.height.saturating_sub(3)).max(1),
                    ),
                    KeyCode::Left if self.view == View::Effort => self.move_selection(false, 1),
                    KeyCode::Right if self.view == View::Effort => self.move_selection(true, 1),
                    KeyCode::Left | KeyCode::BackTab => self.sidebar_focus = true,
                    KeyCode::Right if self.sidebar_focus => self.sidebar_focus = false,
                    KeyCode::Tab if self.sidebar_focus => self.sidebar_focus = false,
                    KeyCode::Enter if self.sidebar_focus => self.sidebar_focus = false,
                    KeyCode::Enter if key.kind == KeyEventKind::Press => match self.view {
                        View::Roles => self.begin_models(),
                        View::Models => {
                            if let Some(&index) = self.model_indices().get(self.selected) {
                                self.begin_effort(self.choices[index].reference.clone());
                            }
                        }
                        View::Effort => {
                            if let Some(&effort) = self.efforts().get(self.selected)
                                && let Some(mut assignment) = self.pending.clone()
                            {
                                assignment.effort = effort;
                                return Action::Save(self.role(), Some(assignment));
                            }
                        }
                    },
                    KeyCode::Char('e') if self.view == View::Roles && !self.sidebar_focus => {
                        self.begin_effort(self.effective().model)
                    }
                    KeyCode::Delete | KeyCode::Backspace
                        if self.view == View::Roles
                            && !self.sidebar_focus
                            && key.kind == KeyEventKind::Press =>
                    {
                        return Action::Save(self.role(), None);
                    }
                    KeyCode::Backspace if self.view == View::Models && !self.sidebar_focus => {
                        self.query.pop();
                        self.selected = 0;
                    }
                    KeyCode::Char(c) if self.view == View::Models && !self.sidebar_focus => {
                        self.query.push(c);
                        self.selected = 0;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        Action::None
    }

    fn move_selection(&mut self, down: bool, amount: usize) {
        let len = if self.sidebar_focus {
            self.providers.len() + 2
        } else {
            match self.view {
                View::Roles => ModelRole::ALL.len(),
                View::Models => self.model_indices().len(),
                View::Effort => self.efforts().len(),
            }
        };
        let selected = if self.sidebar_focus {
            &mut self.sidebar
        } else if self.view == View::Roles {
            &mut self.role_index
        } else {
            &mut self.selected
        };
        *selected = if down {
            selected.saturating_add(amount).min(len.saturating_sub(1))
        } else {
            selected.saturating_sub(amount)
        };
        if self.sidebar_focus {
            self.activate_sidebar();
        }
    }

    pub(super) fn draw(&mut self, frame: &mut Frame, area: Rect) {
        self.area = area;
        frame.render_widget(
            Block::default().style(Style::default().bg(BG).fg(TEXT)),
            area,
        );
        if area.width < 4 || area.height < 4 {
            return;
        }
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(BORDER))
            .title(Line::styled(" Models ", Style::default().fg(GOLD)));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let sidebar_width = if inner.width >= 54 {
            (inner.width / 5).clamp(22, 32)
        } else {
            0
        };
        self.sidebar_area = Rect::new(
            inner.x,
            inner.y,
            sidebar_width.saturating_sub(1),
            inner.height.saturating_sub(1),
        );
        if sidebar_width > 0 {
            let mut lines = vec![];
            for index in 0..self.providers.len() + 2 {
                if index == 2 {
                    lines.push(Line::styled(
                        "─".repeat(usize::from(sidebar_width.saturating_sub(2))),
                        Style::default().fg(BORDER),
                    ));
                }
                let active = self.sidebar == index;
                let (label, count) = match index {
                    0 => (
                        "♧ Roles".to_owned(),
                        format!(
                            "{}/6",
                            self.roles
                                .values()
                                .filter(|assignment| assignment.enabled)
                                .count()
                        ),
                    ),
                    1 => ("☷ All models".to_owned(), self.choices.len().to_string()),
                    _ => {
                        let provider = &self.providers[index - 2];
                        (
                            format!("● {provider}"),
                            self.choices
                                .iter()
                                .filter(|choice| {
                                    choice
                                        .reference
                                        .split_once('/')
                                        .is_some_and(|(id, _)| id == provider)
                                })
                                .count()
                                .to_string(),
                        )
                    }
                };
                let prefix = if active && self.sidebar_focus {
                    "❯ "
                } else {
                    "  "
                };
                let used = Line::from(format!("{prefix}{label}{count}")).width();
                let gap = usize::from(sidebar_width.saturating_sub(2))
                    .saturating_sub(used)
                    .max(1);
                let mut spans = vec![Span::styled(prefix, Style::default().fg(GOLD))];
                if index >= 2 {
                    spans.push(Span::styled("● ", Style::default().fg(GREEN)));
                    spans.push(Span::styled(
                        super::display_text(&self.providers[index - 2]).into_owned(),
                        Style::default().fg(if active { GOLD } else { TEXT }),
                    ));
                } else {
                    spans.push(Span::styled(
                        label,
                        Style::default().fg(if active { GOLD } else { TEXT }),
                    ));
                }
                spans.push(Span::styled(
                    format!("{}{count}", " ".repeat(gap)),
                    Style::default().fg(MUTED),
                ));
                lines.push(Line::from(spans));
            }
            let selected_row = self.sidebar + usize::from(self.sidebar >= 2);
            let offset = selected_row
                .saturating_sub(usize::from(self.sidebar_area.height.saturating_sub(1)));
            self.sidebar_scroll = offset;
            frame.render_widget(
                Paragraph::new(lines).scroll((offset.min(u16::MAX as usize) as u16, 0)),
                self.sidebar_area,
            );
            frame.render_widget(
                Paragraph::new(vec![Line::from("│"); usize::from(inner.height)])
                    .style(Style::default().fg(BORDER)),
                Rect::new(inner.x + sidebar_width - 1, inner.y, 1, inner.height),
            );
        }
        self.body = Rect::new(
            inner.x + sidebar_width + u16::from(sidebar_width > 0),
            inner.y,
            inner
                .width
                .saturating_sub(sidebar_width + u16::from(sidebar_width > 0)),
            inner.height.saturating_sub(2),
        );
        let header = match self.view {
            View::Roles => {
                "Model roles — Space or click the dot to toggle; only selected roles are offered to the model".to_owned()
            }
            View::Models => format!(
                "Select a model for {} · type to search · Enter to choose effort",
                self.role().name()
            ),
            View::Effort => format!(
                "{} · {}",
                self.role().name(),
                self.pending
                    .as_ref()
                    .map_or("", |assignment| assignment.model.as_str())
            ),
        };
        frame.render_widget(
            Line::styled(
                super::display_text(&header).into_owned(),
                Style::default().fg(MUTED),
            ),
            Rect::new(self.body.x, self.body.y, self.body.width, 1),
        );
        let content_offset = self.content_offset();
        if self.body.height <= content_offset {
            return;
        }
        let content = Rect::new(
            self.body.x,
            self.body.y + content_offset,
            self.body.width,
            self.body.height - content_offset,
        );
        match self.view {
            View::Roles => {
                let roles = ModelRole::ALL;
                self.scroll = self
                    .role_index
                    .saturating_sub(usize::from(content.height.saturating_sub(1)));
                for (row, role) in roles
                    .into_iter()
                    .skip(self.scroll)
                    .take(usize::from(content.height))
                    .enumerate()
                {
                    let configured = self.roles.get(&role);
                    let enabled = configured.is_some_and(|assignment| assignment.enabled);
                    let assignment = configured.cloned().unwrap_or_else(|| RoleAssignment {
                        enabled: false,
                        model: self.fallback.clone(),
                        effort: self.fallback_effort,
                    });
                    let selected = !self.sidebar_focus && self.role_index == row + self.scroll;
                    let color = if !enabled {
                        MUTED
                    } else {
                        match role {
                            ModelRole::Default => GREEN,
                            ModelRole::Fast => Color::Rgb(222, 200, 26),
                            ModelRole::Slow => GOLD,
                            ModelRole::Vision => Color::Rgb(255, 64, 85),
                            ModelRole::Plan => Color::Rgb(153, 194, 232),
                            ModelRole::Review => Color::Rgb(187, 154, 247),
                        }
                    };
                    let mut spans = vec![
                        Span::styled(
                            if selected { "❯ " } else { "  " },
                            Style::default().fg(if enabled { GOLD } else { MUTED }),
                        ),
                        Span::styled(
                            format!("{} {:<8} ", if enabled { "●" } else { "○" }, role.name()),
                            Style::default().fg(color),
                        ),
                    ];
                    if configured.is_none() {
                        spans.push(Span::styled("auto → ", Style::default().fg(MUTED)));
                    }
                    spans.extend(model_spans(
                        &assignment.model,
                        if enabled && selected { GOLD } else { color },
                    ));
                    let y = content.y + row as u16;
                    let effort_width =
                        (assignment.effort.as_str().len() as u16 + 3).min(content.width);
                    let model_width = content.width.saturating_sub(effort_width + 1);
                    frame.render_widget(Line::from(spans), Rect::new(content.x, y, model_width, 1));
                    frame.render_widget(
                        Line::styled(
                            format!("◴ {}", assignment.effort.as_str()),
                            Style::default().fg(color),
                        ),
                        Rect::new(content.right() - effort_width, y, effort_width, 1),
                    );
                }
            }
            View::Models => {
                frame.render_widget(
                    Line::styled(format!(" {}", self.query), Style::default().fg(GOLD)),
                    Rect::new(self.body.x, self.body.y + 1, self.body.width, 1),
                );
                let indices = self.model_indices();
                self.selected = self.selected.min(indices.len().saturating_sub(1));
                self.scroll = self
                    .selected
                    .saturating_sub(usize::from(content.height.saturating_sub(1)));
                if indices.is_empty() {
                    frame.render_widget("No matching models", content);
                }
                for (row, index) in indices
                    .into_iter()
                    .skip(self.scroll)
                    .take(usize::from(content.height))
                    .enumerate()
                {
                    let selected = !self.sidebar_focus && row + self.scroll == self.selected;
                    let mut spans = vec![Span::styled(
                        if selected { "❯ " } else { "  " },
                        Style::default().fg(GOLD),
                    )];
                    spans.extend(model_spans(
                        &self.choices[index].reference,
                        if selected { GOLD } else { TEXT },
                    ));
                    frame.render_widget(
                        Line::from(spans),
                        Rect::new(content.x, content.y + row as u16, content.width, 1),
                    );
                }
            }
            View::Effort => {
                frame.render_widget(
                    "Choose an effort for this role (only levels supported by the model)",
                    Rect::new(self.body.x, self.body.y + 1, self.body.width, 1),
                );
                let chips = self
                    .efforts()
                    .iter()
                    .enumerate()
                    .map(|(index, level)| {
                        Span::styled(
                            format!(" {} ", level.as_str()),
                            if index == self.selected {
                                Style::default()
                                    .fg(BG)
                                    .bg(GOLD)
                                    .add_modifier(Modifier::BOLD)
                            } else {
                                Style::default().fg(TEXT)
                            },
                        )
                    })
                    .collect::<Vec<_>>();
                frame.render_widget(
                    Paragraph::new(Line::from(chips)).wrap(ratatui::widgets::Wrap { trim: false }),
                    content,
                );
            }
        }
        let hint = if self.busy {
            "Saving…"
        } else if !self.message.is_empty() {
            &self.message
        } else {
            "↑↓ Move · Space Toggle · Enter Configure/Save · e Effort · Delete Clear · Esc Back"
        };
        frame.render_widget(
            Line::styled(hint, Style::default().fg(MUTED)),
            Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
        );
    }
}

fn model_spans(reference: &str, color: Color) -> Vec<Span<'static>> {
    let safe = super::display_text(reference).into_owned();
    match safe.split_once('/') {
        Some((provider, model)) => vec![
            Span::styled(format!("{provider}/"), Style::default().fg(MUTED)),
            Span::styled(model.to_owned(), Style::default().fg(color)),
        ],
        None => vec![Span::styled(safe, Style::default().fg(MUTED))],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;
    use ratatui::{Terminal, backend::TestBackend};

    fn panel() -> Panel {
        let choices = ["one", "two"]
            .into_iter()
            .map(|name| ModelChoice {
                reference: format!("fixture/{name}"),
                model: name.into(),
                protocol: crate::models::Protocol::Completion,
                thinking: if name == "two" {
                    crate::models::ModelThinking::without_reasoning()
                } else {
                    Default::default()
                },
                context_window: None,
            })
            .collect();
        Panel::new(
            choices,
            ModelRoles::new(),
            "fixture/one".into(),
            ThinkLevel::High,
        )
    }

    fn key(panel: &mut Panel, code: KeyCode) -> Action {
        panel.handle(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)))
    }

    fn draw(panel: &mut Panel, width: u16, height: u16) -> ratatui::buffer::Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| panel.draw(frame, frame.area()))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    #[test]
    fn roles_layout_has_six_roles_sidebar_colors_and_independent_efforts() {
        let mut panel = panel();
        panel.roles.insert(
            ModelRole::Fast,
            RoleAssignment {
                enabled: true,
                model: "fixture/two".into(),
                effort: ThinkLevel::None,
            },
        );
        let buffer = draw(&mut panel, 140, 24);
        let text = buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        for label in [
            "Models",
            "Roles",
            "All models",
            "DEFAULT",
            "FAST",
            "SLOW",
            "VISION",
            "PLAN",
            "REVIEW",
            "high",
            "none",
            "1/6",
        ] {
            assert!(text.contains(label), "{label}: {text}");
        }
        assert!(!text.contains("SMOL"));
        assert_eq!(buffer[(50, 20)].bg, BG);
        assert_eq!(buffer[(0, 5)].fg, BORDER);
        for (width, height) in [(80, 12), (42, 8), (12, 4), (1, 1), (140, 24)] {
            draw(&mut panel, width, height);
        }
    }

    #[test]
    fn roles_choose_model_then_only_supported_effort_before_atomic_save() {
        let mut panel = panel();
        key(&mut panel, KeyCode::Right);
        key(&mut panel, KeyCode::Down);
        assert_eq!(panel.role(), ModelRole::Fast);
        key(&mut panel, KeyCode::Enter);
        panel.handle(Event::Paste("two".into()));
        assert!(matches!(key(&mut panel, KeyCode::Enter), Action::None));
        assert_eq!(panel.efforts(), [ThinkLevel::None]);
        let Action::Save(role, Some(assignment)) = key(&mut panel, KeyCode::Enter) else {
            panic!("应一次保存 model 与 effort");
        };
        assert_eq!(role, ModelRole::Fast);
        assert_eq!(assignment.model, "fixture/two");
        assert_eq!(assignment.effort, ThinkLevel::None);
        assert!(panel.roles.is_empty());
    }

    #[test]
    fn roles_cancel_and_clear_and_busy_never_leak_partial_configuration() {
        let mut panel = panel();
        key(&mut panel, KeyCode::Right);
        key(&mut panel, KeyCode::Enter);
        key(&mut panel, KeyCode::Enter);
        key(&mut panel, KeyCode::Esc);
        key(&mut panel, KeyCode::Esc);
        assert!(panel.roles.is_empty());
        assert!(matches!(
            key(&mut panel, KeyCode::Delete),
            Action::Save(ModelRole::Default, None)
        ));
        panel.busy = true;
        assert!(matches!(key(&mut panel, KeyCode::Enter), Action::None));
        assert!(matches!(key(&mut panel, KeyCode::Esc), Action::None));
        panel.saved();
        assert!(matches!(key(&mut panel, KeyCode::Esc), Action::Close));
    }

    #[test]
    fn roles_only_enabled_rows_are_colored_and_space_or_dot_toggles_preserving_assignment() {
        let mut panel = panel();
        let mut value = RoleAssignment {
            enabled: true,
            model: "fixture/two".into(),
            effort: ThinkLevel::None,
        };
        panel.roles.insert(ModelRole::Fast, value.clone());
        panel.roles.insert(
            ModelRole::Review,
            RoleAssignment {
                enabled: false,
                ..value.clone()
            },
        );
        key(&mut panel, KeyCode::Right);
        key(&mut panel, KeyCode::Down);
        key(&mut panel, KeyCode::Down);
        let buffer = draw(&mut panel, 140, 24);
        let x = panel.body.x;
        let y = panel.body.y + panel.content_offset();
        for row in [0, 2, 3, 4, 5] {
            for col in x..panel.body.right() {
                let cell = &buffer[(col, y + row)];
                if !cell.symbol().trim().is_empty() {
                    assert_eq!(cell.fg, MUTED, "未选中的行不能因光标高亮变彩色");
                }
            }
        }
        assert_ne!(buffer[(x + 2, y + 1)].fg, MUTED);
        key(&mut panel, KeyCode::Up);
        let Action::Save(ModelRole::Fast, Some(off)) = key(&mut panel, KeyCode::Char(' ')) else {
            panic!("空格应切换启用状态");
        };
        value.enabled = false;
        assert_eq!(off, value);
        panel.roles.insert(ModelRole::Fast, off);
        draw(&mut panel, 140, 24);
        let Action::Save(ModelRole::Fast, Some(on)) =
            panel.handle(Event::Mouse(crossterm::event::MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: x + 2,
                row: y + 1,
                modifiers: KeyModifiers::NONE,
            }))
        else {
            panic!("点击圆点应切换启用状态");
        };
        assert!(on.enabled);
        assert_eq!(on.model, value.model);
        assert_eq!(on.effort, value.effort);
    }
}
