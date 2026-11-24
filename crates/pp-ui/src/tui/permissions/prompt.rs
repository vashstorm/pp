//! 工具权限审批卡片：展示申请原因与可用授权范围。

use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use pp_core::{
    interaction::{AskAnswer, AskArgs, AskOption, AskQuestion},
    text::display_text,
};
use ratatui::{
    Frame,
    layout::{Position, Rect},
    style::Style,
    text::Line,
    widgets::{Block, BorderType, Borders, Clear, Padding},
};

use crate::tui::{dialog::Action, themes, transcript::wrap_block};

const TITLE: &str = "Permission required";
const MIN_BORDER_WIDTH: u16 = 28;
const CARD_PADDING: u16 = 1;
// 两行外框、一行分隔线、一行快捷键。
const CARD_CHROME: u16 = 4;

pub(in crate::tui) struct Prompt {
    args: Arc<AskArgs>,
    focused: usize,
    details_offset: usize,
    details_limit: usize,
    area: Rect,
    details_area: Rect,
}

impl Prompt {
    pub(in crate::tui) fn new(args: Arc<AskArgs>) -> Self {
        Self {
            args,
            focused: 0,
            details_offset: 0,
            details_limit: 0,
            area: Rect::default(),
            details_area: Rect::default(),
        }
    }

    fn question(&self) -> Option<&AskQuestion> {
        self.args.questions.first()
    }

    fn options(&self) -> &[AskOption] {
        self.question().map_or(&[], |question| &question.options)
    }

    pub(in crate::tui) fn key(&mut self, key: KeyEvent) -> Action {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return Action::None;
        }
        let Some(question) = self.question() else {
            return Action::Cancel;
        };
        let last = question.options.len().saturating_sub(1);
        match key.code {
            KeyCode::Esc => return Action::Cancel,
            KeyCode::Enter if key.kind == KeyEventKind::Press => {
                return question
                    .options
                    .get(self.focused)
                    .map_or(Action::Cancel, |option| {
                        Action::Submit(vec![AskAnswer {
                            id: question.id.clone(),
                            selected_options: vec![option.label.clone()],
                            custom_input: None,
                        }])
                    });
            }
            KeyCode::Up if key.modifiers != KeyModifiers::CONTROL => self.move_focus(true),
            KeyCode::Down if key.modifiers != KeyModifiers::CONTROL => self.move_focus(false),
            KeyCode::Home => self.focused = 0,
            KeyCode::End => self.focused = last,
            KeyCode::PageUp => self.scroll_details(true),
            KeyCode::PageDown => self.scroll_details(false),
            _ => {}
        }
        Action::None
    }

    fn move_focus(&mut self, up: bool) {
        if up {
            self.focused = self.focused.saturating_sub(1);
        } else {
            self.focused = (self.focused + 1).min(self.options().len().saturating_sub(1));
        }
    }

    fn scroll_details(&mut self, up: bool) {
        if up {
            self.details_offset = self.details_offset.saturating_sub(1);
        } else {
            self.details_offset = (self.details_offset + 1).min(self.details_limit);
        }
    }

    pub(in crate::tui) fn wheel(&mut self, up: bool, column: u16, row: u16) -> bool {
        let position = Position::new(column, row);
        if !self.area.contains(position) {
            return false;
        }
        if self.details_area.contains(position) {
            self.scroll_details(up);
        } else {
            self.move_focus(up);
        }
        true
    }

    fn detail_lines(&self, width: u16) -> Vec<Line<'static>> {
        let palette = themes::palette();
        let Some(question) = self.question() else {
            return Vec::new();
        };
        let mut lines = Vec::new();
        if let Some(tool) = question.header.as_ref().filter(|tool| !tool.is_empty()) {
            lines.extend(wrap_block(
                Line::styled(tool.clone(), Style::new().fg(palette.text).bold()),
                width,
                "",
            ));
        }
        let reason = Style::new().fg(palette.activity);
        lines.extend(
            question
                .question
                .lines()
                .flat_map(|line| wrap_block(Line::styled(line.to_owned(), reason), width, "")),
        );
        lines
    }

    pub(in crate::tui) fn height(&self, available: Rect, screen_height: u16) -> u16 {
        if available.is_empty() {
            return 0;
        }
        let bordered = available.width >= MIN_BORDER_WIDTH;
        let width = available.width - u16::from(bordered) * (2 + 2 * CARD_PADDING);
        let details = self.detail_lines(width).len();
        let chrome = if bordered { CARD_CHROME } else { 2 };
        let needed = details + self.options().len().max(1) + usize::from(chrome);
        let cap = (u32::from(screen_height) * 7 / 10).max(1) as u16;
        needed.min(usize::from(cap.min(available.height))) as u16
    }

    pub(in crate::tui) fn draw(&mut self, frame: &mut Frame, area: Rect) {
        self.area = area;
        self.details_area = Rect::default();
        if area.is_empty() {
            return;
        }
        let palette = themes::palette();
        let title = Style::new().fg(palette.plan_mode).bold();
        let background = Style::new().bg(palette.panel_bg);
        let count = self.options().len();
        let bordered = area.width >= MIN_BORDER_WIDTH
            && usize::from(area.height) > count.max(1) + usize::from(CARD_CHROME);
        frame.render_widget(Clear, area);

        let (inner, footer) = if bordered {
            let block = Block::new()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::new().fg(palette.panel_border))
                .padding(Padding::horizontal(CARD_PADDING))
                .style(background)
                .title(Line::styled(format!(" {TITLE} "), title));
            let inner = block.inner(area);
            frame.render_widget(block, area);
            (inner, 2)
        } else {
            let header = u16::from(area.height > 1);
            frame.render_widget(Block::new().style(background), area);
            if header > 0 {
                frame.render_widget(Line::styled(TITLE, title), Rect { height: 1, ..area });
            }
            let inner = Rect::new(area.x, area.y + header, area.width, area.height - header);
            let footer = u16::from(usize::from(inner.height) >= count.max(1) + 2);
            (inner, footer)
        };

        let details = self.detail_lines(inner.width);
        let body = usize::from(inner.height - footer);
        let detail_budget = body.saturating_sub(count.max(1));
        let detail_rows = details.len().min(detail_budget);
        self.details_area = Rect {
            height: detail_rows as u16,
            ..inner
        };
        self.details_limit = if detail_rows == 0 {
            0
        } else {
            details.len() - detail_rows
        };
        self.details_offset = self.details_offset.min(self.details_limit);
        for (row, line) in details
            .iter()
            .skip(self.details_offset)
            .take(detail_rows)
            .enumerate()
        {
            frame.render_widget(
                line,
                Rect::new(inner.x, inner.y + row as u16, inner.width, 1),
            );
        }

        let visible = (body - detail_rows).min(count);
        let first = (self.focused + 1).saturating_sub(visible);
        for (y, (index, option)) in (self.details_area.bottom()..)
            .zip(self.options().iter().enumerate().skip(first).take(visible))
        {
            self.draw_option(
                frame,
                Rect::new(inner.x, y, inner.width, 1),
                option,
                index == self.focused,
            );
        }
        if footer > 0 {
            let y = inner.bottom() - 1;
            if bordered {
                frame.render_widget(
                    Line::styled(
                        "─".repeat(usize::from(inner.width)),
                        Style::new().fg(palette.panel_border),
                    ),
                    Rect::new(inner.x, y - 1, inner.width, 1),
                );
            }
            let hint = self.hint(inner.width);
            frame.render_widget(
                Line::styled(hint, Style::new().fg(palette.muted)),
                Rect::new(inner.x, y, inner.width, 1),
            );
        }
    }

    fn hint(&self, width: u16) -> &'static str {
        match (width, self.details_limit > 0) {
            (61.., true) => "↑/↓ choose   Enter confirm   Esc cancel   PgUp/PgDn details",
            (36.., true) => "↑/↓   Enter   Esc   PgUp/PgDn",
            (_, true) => "Enter · Esc · PgUp/PgDn",
            (40.., false) => "↑/↓ choose   Enter confirm   Esc cancel",
            _ => "↑/↓ · Enter · Esc",
        }
    }

    fn draw_option(&self, frame: &mut Frame, row: Rect, option: &AskOption, focused: bool) {
        let palette = themes::palette();
        let color = if option.label == "Deny" {
            palette.removed
        } else if focused {
            palette.accent
        } else {
            palette.text
        };
        let style = Style::new().fg(color);
        let (prefix, style) = if focused {
            (if themes::is_ascii() { "> " } else { "❯ " }, style.bold())
        } else {
            ("  ", style)
        };
        let prefix = if row.width > 2 { prefix } else { "" };
        frame.render_widget(
            Line::styled(format!("{prefix}{}", display_text(&option.label)), style),
            row,
        );
    }
}
