use super::{MOUSE_SCROLL_LINES, statusline::accent};
use crate::{
    commands::{
        BASH_DISPLAY_INTRO, BASH_DISPLAY_ROWS, BASH_DISPLAY_TITLE, CommandCategory, FileCommands,
        HELP_USAGE_COLUMN, SHELL_COMMAND_DESCRIPTION, SHELL_COMMAND_USAGE, category_specs,
    },
    shortcuts::platform_text,
};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};

pub(in crate::tui) enum Action {
    None,
    Close,
}

pub(in crate::tui) struct Panel {
    block: HelpBlock,
    scroll: usize,
    visible: usize,
    total: usize,
    cached_width: u16,
    cached_theme: Option<pp_config::config::Theme>,
    wrapped: Vec<Line<'static>>,
}

impl Panel {
    pub(in crate::tui) fn new(files: &FileCommands) -> Self {
        Self {
            block: HelpBlock::new(files),
            scroll: 0,
            visible: 0,
            total: 0,
            cached_width: 0,
            cached_theme: None,
            wrapped: Vec::new(),
        }
    }

    pub(in crate::tui) fn handle(&mut self, event: Event) -> Action {
        match event {
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                self.handle_key(key)
            }
            Event::Mouse(mouse) => {
                match mouse.kind {
                    MouseEventKind::ScrollUp => {
                        self.scroll = self.scroll.saturating_sub(MOUSE_SCROLL_LINES);
                    }
                    MouseEventKind::ScrollDown => {
                        self.scroll = self.scroll.saturating_add(MOUSE_SCROLL_LINES);
                    }
                    _ => {}
                }
                self.clamp_scroll();
                Action::None
            }
            _ => Action::None,
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> Action {
        if key.modifiers.is_empty() && key.code == KeyCode::Esc {
            return Action::Close;
        }
        if key.kind == KeyEventKind::Press && key.modifiers.is_empty() && key.code == KeyCode::F(1)
        {
            return Action::Close;
        }
        let page = self.visible.max(1);
        let half_page = (page / 2).max(1);
        match key.code {
            KeyCode::Up | KeyCode::Char('k') if key.modifiers.is_empty() => {
                self.scroll = self.scroll.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') if key.modifiers.is_empty() => {
                self.scroll = self.scroll.saturating_add(1);
            }
            KeyCode::PageUp if key.modifiers.is_empty() => {
                self.scroll = self.scroll.saturating_sub(page);
            }
            KeyCode::PageDown | KeyCode::Char(' ') if key.modifiers.is_empty() => {
                self.scroll = self.scroll.saturating_add(page);
            }
            KeyCode::Char('u') if key.modifiers == KeyModifiers::CONTROL => {
                self.scroll = self.scroll.saturating_sub(half_page);
            }
            KeyCode::Char('d') if key.modifiers == KeyModifiers::CONTROL => {
                self.scroll = self.scroll.saturating_add(half_page);
            }
            KeyCode::Home | KeyCode::Char('g') if key.modifiers.is_empty() => {
                self.scroll = 0;
            }
            KeyCode::End if key.modifiers.is_empty() => {
                self.scroll = self.end_scroll();
            }
            KeyCode::Char('G')
                if matches!(key.modifiers, KeyModifiers::NONE | KeyModifiers::SHIFT) =>
            {
                self.scroll = self.end_scroll();
            }
            _ => return Action::None,
        }
        self.clamp_scroll();
        Action::None
    }

    fn max_scroll(&self) -> usize {
        self.total.saturating_sub(self.visible)
    }

    fn end_scroll(&self) -> usize {
        if self.total == 0 {
            usize::MAX
        } else {
            self.max_scroll()
        }
    }

    fn clamp_scroll(&mut self) {
        if self.total > 0 {
            self.scroll = self.scroll.min(self.max_scroll());
        }
    }

    pub(in crate::tui) fn draw(&mut self, frame: &mut Frame, area: Rect) {
        if area.is_empty() {
            return;
        }
        let p = super::themes::palette();
        frame.render_widget(Clear, area);
        frame.render_widget(
            Block::default().style(Style::default().bg(p.panel_bg).fg(p.text)),
            area,
        );
        if area.width < 6 || area.height < 4 {
            return;
        }
        let outer = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(p.panel_border))
            .title(Line::styled(
                " Help ",
                Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
            ));
        let box_inner = outer.inner(area);
        frame.render_widget(outer, area);
        if box_inner.width == 0 || box_inner.height == 0 {
            return;
        }
        let inner = if box_inner.width >= 16 {
            Rect::new(
                box_inner.x + 1,
                box_inner.y,
                box_inner.width - 2,
                box_inner.height,
            )
        } else {
            box_inner
        };

        let (body, footer) = if inner.height >= 4 {
            let sections = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(1),
                    Constraint::Length(1),
                    Constraint::Min(1),
                    Constraint::Length(1),
                ])
                .split(inner);
            let subtitle = truncate_chars(
                "Commands, keybindings & safety notes · /tips for compact grid",
                usize::from(sections[0].width),
            );
            frame.render_widget(
                Line::styled(subtitle, Style::default().fg(p.muted)),
                sections[0],
            );
            frame.render_widget(
                Line::styled(
                    "─".repeat(usize::from(sections[1].width)),
                    Style::default().fg(p.panel_border),
                ),
                sections[1],
            );
            (sections[2], Some(sections[3]))
        } else if inner.height >= 2 {
            let sections = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(1), Constraint::Length(1)])
                .split(inner);
            (sections[0], Some(sections[1]))
        } else {
            (inner, None)
        };

        let theme = super::themes::current_theme();
        if self.cached_width != body.width || self.cached_theme != Some(theme) {
            self.wrapped = body_lines(body.width, &self.block)
                .into_iter()
                .flat_map(|line| super::transcript::wrap_block(line, body.width, ""))
                .collect();
            self.cached_width = body.width;
            self.cached_theme = Some(theme);
        }
        self.total = self.wrapped.len();
        self.visible = usize::from(body.height);
        self.scroll = self.scroll.min(self.max_scroll());

        frame.render_widget(
            Paragraph::new(
                self.wrapped
                    .iter()
                    .skip(self.scroll)
                    .take(self.visible)
                    .cloned()
                    .collect::<Vec<_>>(),
            ),
            body,
        );

        if let Some(footer_area) = footer {
            self.draw_footer(frame, footer_area, p);
        }
    }

    fn draw_footer(&self, frame: &mut Frame, area: Rect, p: &super::themes::Palette) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let width = usize::from(area.width);
        let position = (self.total > self.visible && self.visible > 0).then(|| {
            let start = self.scroll + 1;
            let end = (self.scroll + self.visible).min(self.total);
            format!("{start}-{end}/{}", self.total)
        });
        let pos_width = position.as_ref().map_or(0, |text| text.chars().count());
        let show_position = pos_width > 0 && width >= pos_width + 12;
        let hint_budget = if show_position {
            width.saturating_sub(pos_width + 2)
        } else {
            width
        };
        let hint = [
            "↑↓ / j/k / PgUp/PgDn scroll · Home/End · Esc close",
            "↑↓ / PgUp/PgDn scroll · Esc close",
            "Esc close",
        ]
        .into_iter()
        .find(|candidate| candidate.chars().count() <= hint_budget)
        .map_or_else(|| truncate_chars("Esc close", hint_budget), str::to_owned);
        frame.render_widget(
            Line::styled(hint, Style::default().fg(p.muted)),
            Rect::new(area.x, area.y, hint_budget as u16, 1),
        );
        if show_position && let Some(pos) = position {
            let pos_u16 = pos_width as u16;
            frame.render_widget(
                Line::styled(pos, Style::default().fg(p.muted)),
                Rect::new(area.right() - pos_u16, area.y, pos_u16, 1),
            );
        }
    }
}

pub(super) struct HelpBlock {
    pub(super) entries: Vec<(String, String)>,
    pub(super) diagnostics: Vec<String>,
}

impl HelpBlock {
    pub(super) fn new(files: &FileCommands) -> Self {
        Self {
            entries: files
                .entries
                .iter()
                .map(|file| {
                    (
                        format!("{} [arguments]", file.name),
                        file.description.clone(),
                    )
                })
                .collect(),
            diagnostics: files.diagnostics.clone(),
        }
    }
}

fn first() -> Color {
    super::themes::palette().welcome_first
}

fn second() -> Color {
    super::themes::palette().welcome_second
}

fn separator() -> Color {
    super::themes::palette().welcome_separator
}

fn muted() -> Color {
    super::themes::palette().welcome_muted
}

fn body_color() -> Color {
    super::themes::palette().text
}

fn error_color() -> Color {
    super::themes::palette().removed
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let mut out = String::new();
    for ch in text.chars().take(max_chars.saturating_sub(1)) {
        out.push(ch);
    }
    if max_chars > 0 {
        out.push('…');
    }
    out
}

const KEYBINDING_GROUPS: &[(&str, &[(&str, &str)])] = &[
    (
        "Queue",
        &[
            ("Enter", "steer"),
            (platform_text("⌥ + Enter", "Alt + Enter"), "queue"),
            (platform_text("⌥ + Q", "Ctrl + Q"), "pending messages"),
            (platform_text("⌥ + R", "Ctrl + R"), "resume queue"),
        ],
    ),
    (
        "Models",
        &[
            (platform_text("⌥ + P", "Ctrl + P"), "choose model"),
            ("Shift + Tab", "cycle thinking"),
            ("F6 / Alt + Shift + A", "cycle permissions"),
            ("F7", "open /permissions command prompt"),
            ("Ctrl + F", "favorite model"),
            ("Ctrl + S", "save model default"),
            (platform_text("⌥ + S", "Ctrl + S"), "save effort default"),
        ],
    ),
    (
        "Panels",
        &[
            ("F1", "help"),
            ("Esc Esc", "session tree"),
            ("Ctrl + O", "tool output"),
            (platform_text("⌥ + A", "Alt + A"), "Agent Hub"),
            (platform_text("⌥ + T", "Ctrl + T"), "TODO"),
            (
                platform_text("⌥ + PgUp/PgDn", "Ctrl + PgUp/PgDn"),
                "scroll TODO",
            ),
        ],
    ),
    (
        "Editing",
        &[
            ("Shift + Enter / Ctrl + J", "newline"),
            ("Tab", "complete"),
            ("@", "local resources"),
            ("Ctrl + V", "paste"),
            ("Ctrl + U", "undo"),
            ("Ctrl + A / Ctrl + E", "line start/end"),
            ("Ctrl + K", "delete to end"),
            ("↑ / ↓", "move/history/menu"),
            ("Wheel / PgUp/PgDn", "scroll"),
            ("Esc", "cancel/dismiss"),
            ("Ctrl + C", "quit"),
        ],
    ),
];

const SAFETY_NOTES: &[&str] = &[
    "write/edit only modify files inside the startup directory; bash has no directory isolation, so run in a trusted or temporary directory.",
    "Foreground bash has an execution timeout; managed background tasks survive tool calls and stop at this user task's end or controlled exit.",
    "Ctrl + C stops managed background groups, but cleanup of other foreground children is not guaranteed. No shell/file side-effect rollback; deliberately detached descendants are not covered.",
];

fn push_section_header(
    lines: &mut Vec<Line<'static>>,
    title: &'static str,
    color: Color,
    width: u16,
) {
    if !lines.is_empty() {
        lines.push(Line::default());
    }
    lines.push(Line::styled(
        title,
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    ));
    lines.push(Line::styled(
        "─".repeat(usize::from(width)),
        Style::default().fg(separator()),
    ));
}

fn usage_spans(usage: &str) -> Vec<Span<'static>> {
    let key_style = Style::default().fg(accent()).add_modifier(Modifier::BOLD);
    let arg_style = Style::default().fg(second());
    match usage.split_once(' ') {
        Some((name, args)) => vec![
            Span::styled(name.to_owned(), key_style),
            Span::styled(format!(" {args}"), arg_style),
        ],
        None => vec![Span::styled(usage.to_owned(), key_style)],
    }
}

fn description_spans(description: &str) -> Vec<Span<'static>> {
    let body_style = Style::default().fg(body_color());
    if let Some(prefix) = description.strip_suffix(" (TUI)") {
        vec![
            Span::styled(prefix.to_owned(), body_style),
            Span::styled(" (TUI)", Style::default().fg(muted())),
        ]
    } else {
        vec![Span::styled(description.to_owned(), body_style)]
    }
}

fn push_command_row(lines: &mut Vec<Line<'static>>, usage: &str, description: &str, width: u16) {
    let usage_len = usage.chars().count();
    let desc_len = description.chars().count();
    let mut spans = usage_spans(usage);
    if width >= 56 && usage_len < HELP_USAGE_COLUMN {
        spans.push(Span::raw(" ".repeat(HELP_USAGE_COLUMN - usage_len)));
        spans.extend(description_spans(description));
        lines.push(Line::from(spans));
    } else if width < 56 || usage_len + 3 + desc_len <= usize::from(width) {
        spans.push(Span::styled(" — ", Style::default().fg(separator())));
        spans.extend(description_spans(description));
        lines.push(Line::from(spans));
    } else {
        lines.push(Line::from(spans));
        let mut indented = vec![Span::raw("    ")];
        indented.extend(description_spans(description));
        lines.push(Line::from(indented));
    }
}

fn push_keybinding_group(
    lines: &mut Vec<Line<'static>>,
    group: &'static str,
    bindings: &'static [(&'static str, &'static str)],
    width: u16,
) {
    const LABEL_WIDTH: usize = 10;
    let label_style = Style::default().fg(first()).add_modifier(Modifier::BOLD);
    let key_style = Style::default().fg(second()).add_modifier(Modifier::BOLD);
    let desc_style = Style::default().fg(body_color());
    let sep_style = Style::default().fg(separator());
    let budget = usize::from(width).max(LABEL_WIDTH + 16);

    let mut current = vec![Span::styled(format!("{group:<LABEL_WIDTH$}"), label_style)];
    let mut current_width = LABEL_WIDTH;
    let mut first_in_row = true;

    for &(key, action) in bindings {
        let item_width = key.chars().count() + 1 + action.chars().count();
        let sep_width = if first_in_row { 0 } else { 3 };
        if !first_in_row && width >= 48 && current_width + sep_width + item_width > budget {
            lines.push(Line::from(current));
            current = vec![Span::raw(" ".repeat(LABEL_WIDTH))];
            current_width = LABEL_WIDTH;
            first_in_row = true;
        }
        if !first_in_row {
            current.push(Span::styled(" · ", sep_style));
            current_width += 3;
        }
        current.push(Span::styled(key, key_style));
        current.push(Span::styled(format!(" {action}"), desc_style));
        current_width += item_width;
        first_in_row = false;
    }
    lines.push(Line::from(current));
}

fn push_body_lines(lines: &mut Vec<Line<'static>>, width: u16, block: &HelpBlock) {
    for &category in CommandCategory::ALL {
        let specs = category_specs(category);
        if specs.is_empty() {
            continue;
        }
        push_section_header(lines, category.title(), first(), width);
        for spec in specs {
            push_command_row(lines, spec.usage, spec.description, width);
        }
        if category == CommandCategory::Workflow {
            push_command_row(lines, SHELL_COMMAND_USAGE, SHELL_COMMAND_DESCRIPTION, width);
        }
    }

    push_section_header(lines, BASH_DISPLAY_TITLE, first(), width);
    lines.push(Line::styled(
        BASH_DISPLAY_INTRO,
        Style::default().fg(muted()),
    ));
    for &(display, meaning) in BASH_DISPLAY_ROWS {
        push_command_row(lines, display, meaning, width);
    }

    if !block.entries.is_empty() {
        push_section_header(lines, "Custom Commands", first(), width);
        for (usage, description) in &block.entries {
            push_command_row(lines, usage, description, width);
        }
    }

    if !block.diagnostics.is_empty() {
        push_section_header(lines, "Command load issues:", error_color(), width);
        for diagnostic in &block.diagnostics {
            lines.push(Line::from(vec![
                Span::styled("• ", Style::default().fg(error_color())),
                Span::styled(diagnostic.clone(), Style::default().fg(body_color())),
            ]));
        }
    }

    push_section_header(lines, "Keybindings", first(), width);
    for &(group, bindings) in KEYBINDING_GROUPS {
        push_keybinding_group(lines, group, bindings, width);
    }

    push_section_header(lines, "Safety & Execution", first(), width);
    for &note in SAFETY_NOTES {
        lines.push(Line::from(vec![
            Span::styled("• ", Style::default().fg(first())),
            Span::styled(note, Style::default().fg(body_color())),
        ]));
    }
}

fn body_lines(width: u16, block: &HelpBlock) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    push_body_lines(&mut lines, width, block);
    lines
}

#[cfg(test)]
fn lines(width: u16, block: &HelpBlock) -> Vec<Line<'static>> {
    let header = if width >= 56 {
        Line::from(vec![
            Span::styled("Help:", Style::default().fg(accent())),
            Span::styled(
                "  Commands, keybindings & safety notes · /tips for compact grid",
                Style::default().fg(muted()),
            ),
        ])
    } else {
        Line::styled("Help:", Style::default().fg(accent()))
    };
    let mut lines = vec![header];
    push_body_lines(&mut lines, width, block);
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::FileCommand;

    #[test]
    fn help_lines_group_commands_keybindings_and_diagnostics() {
        let files = FileCommands {
            entries: vec![FileCommand {
                name: "/review".into(),
                description: "Review source".into(),
                content: "Review $1".into(),
            }],
            diagnostics: vec!["commands/broken.md: Invalid command metadata".into()],
        };
        let block = HelpBlock::new(&files);
        let wide = lines(100, &block);
        let wide_text: Vec<String> = wide
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        assert!(wide_text[0].starts_with("Help:  Commands, keybindings & safety notes"));
        for section in [
            "Session & History",
            "Models & Thinking",
            "Workflow & Execution",
            "Settings & Extensions",
            "Bash Command Display",
            "Custom Commands",
            "Command load issues:",
            "Keybindings",
            "Safety & Execution",
        ] {
            assert!(
                wide_text.iter().any(|line| line == section),
                "missing section {section}"
            );
        }
        // Short commands align to a fixed column; long ones fold into an indented description row at 100 columns.
        assert!(wide_text.iter().any(|line| {
            line.starts_with("/model [provider/model]")
                && line.contains("Select a model at the next safe boundary")
        }));
        let mcp_row = wide_text
            .iter()
            .position(|line| line.starts_with("/mcp [list|"))
            .expect("mcp usage line");
        assert_eq!(
            wide_text[mcp_row + 1],
            "    Manage MCP servers and connection state"
        );
        assert!(
            wide_text
                .iter()
                .any(|line| line.starts_with("/review [arguments]")
                    && line.contains("Review source"))
        );
        assert!(
            wide_text
                .iter()
                .any(|line| line == "• commands/broken.md: Invalid command metadata")
        );

        // Narrow screens drop the top-bar subtitle and let the single-line connector wrap naturally.
        let narrow = lines(40, &block);
        let narrow_text: Vec<String> = narrow
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        assert_eq!(narrow_text[0], "Help:");
        assert!(
            narrow_text
                .iter()
                .any(|line| line.starts_with("/new — Reset the session"))
        );
        assert!(
            narrow_text
                .iter()
                .any(|line| line.starts_with("⇡ first 10 — | head -n 10 "))
        );
    }

    #[test]
    fn help_panel_scrolls_wraps_and_closes_on_esc_and_f1() {
        let files = FileCommands {
            entries: vec![FileCommand {
                name: "/review".into(),
                description: "Review source".into(),
                content: "Review $1".into(),
            }],
            diagnostics: vec!["commands/broken.md: Invalid command metadata".into()],
        };
        let mut panel = Panel::new(&files);
        let render = |panel: &mut Panel, width: u16, height: u16| {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| panel.draw(frame, frame.area()))
                .unwrap();
            let buf = terminal.backend().buffer();
            (0..height)
                .map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect::<String>())
                .collect::<Vec<_>>()
                .join("\n")
        };

        // Tiny sizes never panic.
        let _ = render(&mut panel, 0, 0);
        let _ = render(&mut panel, 5, 3);
        let _ = render(&mut panel, 12, 5);

        let top = render(&mut panel, 100, 16);
        assert!(top.contains("Help"));
        assert!(top.contains("Session & History"));
        assert!(top.contains("Esc close"));
        assert!(panel.total > panel.visible);

        // Scroll down with j, PageDown and mouse wheel, then jump to End.
        let key_event = |code, modifiers| Event::Key(KeyEvent::new(code, modifiers));
        assert!(matches!(
            panel.handle(key_event(KeyCode::Char('j'), KeyModifiers::NONE)),
            Action::None
        ));
        assert_eq!(panel.scroll, 1);
        assert!(matches!(
            panel.handle(key_event(KeyCode::PageDown, KeyModifiers::NONE)),
            Action::None
        ));
        assert!(panel.scroll > 1);
        assert!(matches!(
            panel.handle(Event::Mouse(crossterm::event::MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 10,
                row: 5,
                modifiers: KeyModifiers::NONE,
            })),
            Action::None
        ));
        assert!(matches!(
            panel.handle(key_event(KeyCode::End, KeyModifiers::NONE)),
            Action::None
        ));
        let bottom = render(&mut panel, 100, 16);
        assert!(bottom.contains("Safety & Execution"), "{bottom}");
        assert!(
            bottom.contains("bash has no directory isolation"),
            "{bottom}"
        );

        // Home returns to the top.
        assert!(matches!(
            panel.handle(key_event(KeyCode::Home, KeyModifiers::NONE)),
            Action::None
        ));
        assert_eq!(panel.scroll, 0);

        // Repeat F1 is ignored; Press F1 and Esc close the panel.
        let mut repeat_f1 = KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE);
        repeat_f1.kind = KeyEventKind::Repeat;
        assert!(matches!(panel.handle(Event::Key(repeat_f1)), Action::None));
        assert!(matches!(
            panel.handle(key_event(KeyCode::F(1), KeyModifiers::NONE)),
            Action::Close
        ));
        assert!(matches!(
            panel.handle(key_event(KeyCode::Esc, KeyModifiers::NONE)),
            Action::Close
        ));
    }
}
