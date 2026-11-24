//! 权限审批卡片、权限规则后台 I/O worker 与模态 `/permissions` 命令浮层。

use std::{
    sync::mpsc::{self, Receiver, Sender},
    thread,
};

use anyhow::{Context, Result, bail};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph, Wrap},
};
use ratatui_textarea::{CursorMove, TextArea};

use crate::commands::PermissionCommand;
use crate::tui::{editor, statusline, transcript_style};

mod prompt;
pub(super) use prompt::Prompt;

pub(super) fn is_prompt(call_id: &str) -> bool {
    call_id == "permission-dialog"
        || call_id.starts_with("subagent-") && call_id.ends_with("-permission")
}

pub(super) enum RuleRequest {
    List(pp_permission::Gate),
    SaveBash {
        gate: pp_permission::Gate,
        rule: pp_permission::BashRule,
    },
    Revoke {
        gate: pp_permission::Gate,
        target: String,
    },
}

pub(super) struct Worker {
    requests: Sender<RuleRequest>,
    pub(super) replies: Receiver<Result<String>>,
}

impl Worker {
    /// 独立后台线程执行 `permission.toml` 读取与持锁撤销；故意 detach，
    /// 防止另一进程长期占用 `permission.lock` 时阻塞 TUI 退出或取消。
    pub(super) fn new() -> Result<Self> {
        let (requests, receiver) = mpsc::channel::<RuleRequest>();
        let (sender, replies) = mpsc::channel();
        thread::Builder::new()
            .name("pp-permissions".into())
            .spawn(move || {
                while let Ok(request) = receiver.recv() {
                    let reply = match request {
                        RuleRequest::List(gate) => gate.list_rules(),
                        RuleRequest::SaveBash { gate, rule } => {
                            let pattern = rule.display_pattern();
                            gate.save_bash_rule(rule)
                                .map(|()| format!("Saved bash permission rule: {pattern}"))
                        }
                        RuleRequest::Revoke { gate, target } => gate
                            .revoke_rule(&target)
                            .map(|()| "Saved permission revoked.".into()),
                    };
                    if sender.send(reply).is_err() {
                        break;
                    }
                }
            })
            .context("starting permission rules worker")?;
        Ok(Self { requests, replies })
    }

    pub(super) fn send(&self, request: RuleRequest) -> Result<()> {
        self.requests
            .send(request)
            .map_err(|_| anyhow::anyhow!("permission rules worker stopped"))
    }
}

pub(super) enum OverlayAction {
    None,
    Close,
    Submit(String),
}

/// 模态状态下的 `/permissions` 命令入口：不关闭底层面板、不改动主草稿，
/// 支持直接查看详情、切档或通过后台 worker 列出／撤销持久规则。
pub(super) struct Overlay {
    input: TextArea<'static>,
    output: String,
    error: Option<String>,
    pending: bool,
}

impl Overlay {
    pub(super) fn new(status: String) -> Self {
        let mut input = editor();
        input.insert_str("/permissions ");
        input.move_cursor(CursorMove::End);
        Self {
            input,
            output: status,
            error: None,
            pending: false,
        }
    }

    pub(super) fn parse_input(raw: &str) -> Result<PermissionCommand> {
        let trimmed = raw.trim();
        let argument = if let Some(rest) = trimmed.strip_prefix("/permissions") {
            if rest.is_empty() || rest.starts_with(char::is_whitespace) {
                rest.trim()
            } else {
                bail!("{}", PermissionCommand::USAGE);
            }
        } else if trimmed.starts_with('/') {
            bail!("{}", PermissionCommand::USAGE);
        } else {
            trimmed
        };
        PermissionCommand::parse(argument)
    }

    pub(super) fn set_output(&mut self, output: String) {
        self.output = output;
        self.error = None;
        self.pending = false;
    }

    pub(super) fn set_pending(&mut self, message: &str) {
        self.output = message.to_owned();
        self.error = None;
        self.pending = true;
    }

    pub(super) fn set_error(&mut self, error: String) {
        self.error = Some(error);
        self.pending = false;
    }

    pub(super) fn reset_input(&mut self) {
        let mut input = editor();
        input.insert_str("/permissions ");
        input.move_cursor(CursorMove::End);
        self.input = input;
    }

    pub(super) fn key(&mut self, key: KeyEvent) -> OverlayAction {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return OverlayAction::None;
        }
        match key.code {
            KeyCode::Esc => OverlayAction::Close,
            KeyCode::Enter if key.kind == KeyEventKind::Press && key.modifiers.is_empty() => {
                OverlayAction::Submit(self.input.lines().join(" "))
            }
            KeyCode::Enter => OverlayAction::None,
            _ => {
                self.error = None;
                self.input.input(key);
                OverlayAction::None
            }
        }
    }

    pub(super) fn paste(&mut self, text: &str) {
        self.error = None;
        let single = text.replace(['\r', '\n'], " ");
        self.input.insert_str(single);
    }

    pub(super) fn draw(&mut self, frame: &mut Frame, area: Rect) {
        if area.width < 12 || area.height < 7 {
            return;
        }
        let width = area.width.saturating_sub(4).clamp(12, 84);
        let body_lines = self.output.lines().count().clamp(2, 10) as u16;
        let error_lines = u16::from(self.error.is_some());
        let height = (body_lines + error_lines + 6).min(area.height.saturating_sub(2));
        let popup = Rect::new(
            area.x + (area.width.saturating_sub(width)) / 2,
            area.y + (area.height.saturating_sub(height)) / 2,
            width,
            height,
        );
        frame.render_widget(Clear, popup);
        let block = Block::default()
            .title(Line::from(vec![
                Span::styled(
                    " Permissions ",
                    Style::default()
                        .fg(statusline::accent())
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    "· F7 / Esc close ",
                    Style::default().fg(transcript_style::muted()),
                ),
            ]))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(statusline::accent()))
            .style(Style::default().bg(transcript_style::ask_bg()))
            .padding(Padding::horizontal(1));
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        if inner.width == 0 || inner.height < 3 {
            return;
        }

        let input_row = Rect::new(inner.x, inner.y, inner.width, 1);
        let prompt_width = inner.width.min(2);
        frame.render_widget(
            Line::styled("❯ ", Style::default().fg(statusline::accent())),
            Rect::new(input_row.x, input_row.y, prompt_width, 1),
        );
        let editor_area = Rect::new(
            input_row.x + prompt_width,
            input_row.y,
            input_row.width.saturating_sub(prompt_width),
            1,
        );
        if editor_area.width > 0 {
            self.input.set_style(
                Style::default()
                    .fg(transcript_style::text())
                    .bg(transcript_style::ask_bg()),
            );
            frame.render_widget(&self.input, editor_area);
            let cursor = self.input.screen_cursor();
            let (_, left) = self.input.scroll_offset();
            if let Some(column) = cursor.col.checked_sub(usize::from(left))
                && column < usize::from(editor_area.width)
            {
                frame.set_cursor_position((editor_area.x + column as u16, editor_area.y));
            }
        }

        let hint_y = inner.bottom() - 1;
        let body_top = inner.y + 2;
        let body_height = hint_y.saturating_sub(body_top);
        if body_height > 0 {
            let mut lines: Vec<Line<'static>> = Vec::new();
            if let Some(error) = &self.error {
                lines.push(Line::styled(
                    error.clone(),
                    Style::default().fg(transcript_style::removed()),
                ));
            }
            let body_style = if self.pending {
                Style::default().fg(transcript_style::activity())
            } else {
                Style::default().fg(transcript_style::text())
            };
            for line in self.output.lines() {
                lines.push(Line::styled(line.to_owned(), body_style));
            }
            frame.render_widget(
                Paragraph::new(lines).wrap(Wrap { trim: false }),
                Rect::new(inner.x, body_top, inner.width, body_height),
            );
        }

        let hint = "Enter run · F6 cycle · rules|allow|prompt|deny|revoke <index|rule> · Esc close";
        frame.render_widget(
            Line::styled(hint, Style::default().fg(transcript_style::muted())),
            Rect::new(inner.x, hint_y, inner.width, 1),
        );
    }
}
