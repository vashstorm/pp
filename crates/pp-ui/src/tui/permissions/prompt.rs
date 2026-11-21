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
    text::{Line, Span},
    widgets::{Block, Clear},
};

use crate::tui::{dialog::Action, themes, transcript::wrap_block};

const MAX_WIDTH: u16 = 88;
const TITLE: &str = "Permission required";

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
        let tool = question.header.as_deref().unwrap_or_default();
        let mut lines = wrap_block(
            Line::styled(tool.to_owned(), Style::new().fg(palette.text).bold()),
            width,
            "",
        );
        let reason_style = Style::new().fg(palette.activity);
        lines.extend(
            question.question.lines().flat_map(|line| {
                wrap_block(Line::styled(line.to_owned(), reason_style), width, "")
            }),
        );
        lines
    }

    pub(in crate::tui) fn height(&self, available: Rect, screen_height: u16) -> u16 {
        if available.is_empty() {
            return 0;
        }
        let width = available.width.min(MAX_WIDTH).saturating_sub(3);
        let needed = self.detail_lines(width).len() + self.options().len() + 3;
        let cap = (u32::from(screen_height) * 7 / 10).max(1) as u16;
        needed.min(usize::from(cap.min(available.height))) as u16
    }

    pub(in crate::tui) fn draw(&mut self, frame: &mut Frame, available: Rect) {
        let width = available.width.min(MAX_WIDTH);
        let area = Rect::new(
            available.x + (available.width - width) / 2,
            available.y,
            width,
            available.height,
        );
        self.area = area;
        self.details_area = Rect::default();
        if area.is_empty() {
            return;
        }

        let palette = themes::palette();
        let warning = Style::new().fg(palette.plan_mode);
        frame.render_widget(Clear, area);
        frame.render_widget(
            Block::new().style(Style::new().bg(palette.welcome_card_bg)),
            area,
        );

        let count = self.options().len() as u16;
        let header = u16::from(area.height > 1);
        if header > 0 {
            let row = Rect::new(area.x, area.y, width, 1);
            let style = warning.bg(palette.tool_bg).bold();
            frame.render_widget(Block::new().style(style), row);
            let title = if usize::from(width) >= TITLE.len() + 4 {
                " !  Permission required"
            } else {
                TITLE
            };
            frame.render_widget(Line::styled(title, style), row);
        }

        // 矮终端优先保留当前操作。宽度不足时省略装饰边线。
        let body = area.height - header;
        let rail = if width >= 6 { 3 } else { 0 };
        let details = self.detail_lines(width - rail);
        let footer = u16::from(body >= count + 2);
        let detail_budget = (body - footer).saturating_sub(count.max(1));
        let detail_rows = details.len().min(usize::from(detail_budget)) as u16;
        let gap = u16::from(detail_rows > 0 && detail_budget > detail_rows);
        self.details_area = Rect::new(area.x, area.y + header, width, detail_rows);
        self.details_limit = if detail_rows > 0 {
            details.len() - usize::from(detail_rows)
        } else {
            0
        };
        self.details_offset = self.details_offset.min(self.details_limit);
        if detail_rows > 0 {
            frame.render_widget(
                Block::new().style(Style::new().bg(palette.tool_bg)),
                self.details_area,
            );
        }
        for (index, line) in details
            .iter()
            .skip(self.details_offset)
            .take(detail_rows.into())
            .enumerate()
        {
            let y = self.details_area.y + index as u16;
            if rail > 0 {
                let edge = if themes::is_ascii() { " | " } else { " │ " };
                frame.render_widget(Line::styled(edge, warning), Rect::new(area.x, y, rail, 1));
            }
            frame.render_widget(line, Rect::new(area.x + rail, y, width - rail, 1));
        }

        let rows = (body - footer - detail_rows - gap).min(count);
        let first = self
            .focused
            .saturating_add(1)
            .saturating_sub(usize::from(rows));
        for (row, option) in self
            .options()
            .iter()
            .skip(first)
            .take(rows.into())
            .enumerate()
        {
            let y = self.details_area.bottom() + gap + row as u16;
            self.draw_option(
                frame,
                Rect::new(area.x, y, width, 1),
                option,
                first + row == self.focused,
            );
        }
        if footer > 0 {
            let hint = match (width, self.details_limit > 0) {
                (64.., true) => " Up/Down choose   Enter confirm   Esc cancel   PgUp/PgDn details",
                (45.., true) => " Enter confirm   Esc cancel   PgUp/PgDn details",
                (29.., true) => " Enter · Esc · PgUp/PgDn",
                (45.., false) => " Up/Down choose   Enter confirm   Esc cancel",
                _ => " Enter confirm   Esc cancel",
            };
            frame.render_widget(
                Line::styled(hint, Style::new().fg(palette.activity)),
                Rect::new(area.x, area.bottom() - 1, width, 1),
            );
        }
    }

    fn draw_option(&self, frame: &mut Frame, row: Rect, option: &AskOption, focused: bool) {
        let palette = themes::palette();
        let label_color = if option.label == "Deny" {
            palette.removed
        } else {
            palette.text
        };
        let (label, detail) = if focused {
            let style = Style::new().fg(palette.tool_bg).bg(palette.plan_mode);
            frame.render_widget(Block::new().style(style), row);
            (style.bold(), style)
        } else {
            (
                Style::new().fg(label_color),
                Style::new().fg(palette.activity),
            )
        };
        let mut spans = Vec::new();
        if row.width >= 6 {
            spans.push(Span::styled(if focused { " > " } else { "   " }, label));
        }
        spans.push(Span::styled(display_text(&option.label), label));
        frame.render_widget(Line::from(spans), row);

        const DESCRIPTION_COLUMN: u16 = 27;
        if let Some(description) = option.description.as_deref() {
            let span = Span::styled(display_text(description), detail);
            if span.width() + usize::from(DESCRIPTION_COLUMN) < usize::from(row.width) {
                frame.render_widget(
                    Line::from(span),
                    Rect::new(
                        row.x + DESCRIPTION_COLUMN,
                        row.y,
                        row.width - DESCRIPTION_COLUMN,
                        1,
                    ),
                );
            }
        }
    }
}
