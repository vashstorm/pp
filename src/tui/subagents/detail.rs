//! 独立全屏 child 详情：fresh Transcript、按 Run 导入历史、锚点定位与输入框。
//!
//! 详情只在 Hub 内显示，不把主聊天界面切换成 child 会话；主会话继续运行并
//! 保留草稿、viewport 与用量归属。输入的提交通过观察线程调用既有
//! `Binding::control`，不在 UI 线程做 I/O。

use super::super::transcript_style::{ACCENT, ACTIVITY, ADDED, DIM, MUTED, REMOVED, display_text};
use super::Drafts;
use super::projection::AgentView;
use crate::events::AgentEvent;
use crate::subagent::{EventPage, HubEvent, ObservedEntry, Run};
use crate::tui::Transcript;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use ratatui_textarea::{CursorMove, TextArea};
use std::collections::{BTreeMap, BTreeSet};

/// Activity 行到详情正文的锚点。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::tui) struct Anchor {
    pub run: Option<String>,
    pub offset: u64,
    pub tool: Option<String>,
}

pub(super) struct Detail {
    pub agent: String,
    pub view: u64,
    transcript: Transcript,
    /// run -> 首个 user 卡片条目索引。
    by_run: BTreeMap<String, usize>,
    /// 源行起始偏移 -> 条目索引。
    by_offset: BTreeMap<u64, usize>,
    /// (run, 工具调用 id) -> 卡片条目索引。
    by_tool: BTreeMap<(String, String), usize>,
    /// 已追加过 user 卡片的 Run。
    started: BTreeSet<String>,
    /// 已应用终态的 Run。
    terminal: BTreeMap<String, Terminal>,
    active_run: Option<String>,
    accepted: BTreeSet<(String, u64)>,
    last_offset: Option<u64>,
    caught_up: bool,
    pending: Option<Anchor>,
    pub notice: Option<String>,
    pub focus: bool,
}

struct Terminal {
    status: String,
    timestamp_ms: Option<u64>,
}

impl Detail {
    pub(super) fn new(agent: &str, view: u64, transcript: Transcript) -> Self {
        Self {
            agent: agent.to_owned(),
            view,
            transcript,
            by_run: BTreeMap::new(),
            by_offset: BTreeMap::new(),
            by_tool: BTreeMap::new(),
            started: BTreeSet::new(),
            terminal: BTreeMap::new(),
            active_run: None,
            accepted: BTreeSet::new(),
            last_offset: None,
            caught_up: false,
            pending: None,
            notice: None,
            focus: false,
        }
    }

    pub(super) fn rebuild(&mut self) {
        let focus = self.focus;
        let following = self.transcript.following();
        let mut fresh = Self::new(&self.agent, self.view, self.transcript.fresh());
        fresh.focus = focus;
        if !following {
            fresh.transcript.scroll_up(0);
        }
        fresh.notice = Some("Event log was replaced; transcript rebuilt.".into());
        *self = fresh;
    }

    pub(super) fn tick(&mut self, now: std::time::Instant) {
        self.transcript.tick(now);
    }

    /// settled 是 worker 读页前冻结的终态；只有读完本页历史后才可用它补齐。
    pub(super) fn ingest(
        &mut self,
        page: &EventPage,
        runs: &[Run],
        settled: &BTreeSet<String>,
        now_ms: u64,
    ) {
        let now = std::time::Instant::now();
        self.caught_up = false;
        for entry in &page.entries {
            if self
                .last_offset
                .is_some_and(|offset| entry.start_offset <= offset)
            {
                continue;
            }
            self.consume(entry, runs, now_ms, now);
            self.last_offset = Some(entry.start_offset);
        }
        if !page.has_more {
            self.caught_up = true;
            if let Some(run) = runs.iter().find(|run| {
                self.active_run.as_deref() == Some(&run.id) && settled.contains(&run.id)
            }) {
                self.apply_terminal(&run.id, &run.status, None);
            }
        }
        self.refresh_terminal(runs);
        self.resolve_pending();
    }

    fn consume(
        &mut self,
        entry: &ObservedEntry,
        runs: &[Run],
        now_ms: u64,
        now: std::time::Instant,
    ) {
        let record = &entry.record;
        let run = record.run_id.clone().unwrap_or_default();
        if !run.is_empty()
            && self.active_run.as_deref() != Some(&run)
            && !self.terminal.contains_key(&run)
        {
            // 旧日志可能缺少终态标记；新 Run 的记录证明上一轮已读完。
            if let Some(previous) = runs.iter().find(|candidate| {
                self.active_run.as_deref() == Some(&candidate.id)
                    && !matches!(candidate.status.as_str(), "queued" | "running")
            }) {
                self.apply_terminal(&previous.id, &previous.status, None);
            }
            self.active_run = Some(run.clone());
            self.transcript
                .begin_recorded_run("Requesting model", None, now_ms, now);
        }
        if !run.is_empty()
            && !self.started.contains(&run)
            && let Some(task) = runs
                .iter()
                .find(|candidate| candidate.id == run && candidate.agent_id == self.agent)
                .map(|candidate| &candidate.task)
        {
            let index = self.transcript.entry_count();
            self.transcript.append("You", task);
            self.by_run.insert(run.clone(), index);
            self.by_offset.insert(entry.start_offset, index);
            self.started.insert(run.clone());
        }
        match (&record.event, &record.hub) {
            // 已接受的输入单独生成一次 user 卡片；progress 明确忽略该事件。
            (Some(AgentEvent::UserInputAccepted { id, text }), _) => {
                if !self.accepted.insert((run.clone(), *id)) {
                    return;
                }
                let index = self.transcript.entry_count();
                self.transcript.append("You", text);
                self.by_offset.insert(entry.start_offset, index);
            }
            (Some(event), _) => {
                let before = self.transcript.entry_count();
                self.transcript
                    .progress_recorded(event.clone(), record.timestamp_ms, now_ms, now);
                let after = self.transcript.entry_count();
                if after > before {
                    self.by_offset.insert(entry.start_offset, before);
                }
                if let AgentEvent::ToolStarted { id, .. } = event {
                    // ToolStarted 总是追加一张卡片；原位更新沿用同一索引。
                    self.by_tool.insert((run.clone(), id.clone()), before);
                }
            }
            (None, Some(HubEvent::RunState { status })) if !run.is_empty() => {
                match status.as_str() {
                    "queued" | "running" if !self.terminal.contains_key(&run) => {
                        let (label, start) = if status == "queued" {
                            ("Queued", None)
                        } else {
                            ("Requesting model", record.timestamp_ms)
                        };
                        self.transcript
                            .begin_recorded_run(label, start, now_ms, now);
                    }
                    "queued" | "running" => {}
                    _ => self.apply_terminal(&run, status, record.timestamp_ms),
                }
            }
            _ => {}
        }
    }

    /// 只有对应 Run 已读完（或读到终态 lifecycle）才应用终态。
    fn apply_terminal(&mut self, run: &str, status: &str, timestamp_ms: Option<u64>) {
        if let Some(terminal) = self.terminal.get_mut(run) {
            // 快照补齐后，迟到的 lifecycle 仍能补回真实完成时间。
            terminal.timestamp_ms = terminal.timestamp_ms.or(timestamp_ms);
            return;
        }
        self.terminal.insert(
            run.to_owned(),
            Terminal {
                status: status.into(),
                timestamp_ms,
            },
        );
        if self.active_run.as_deref() == Some(run) {
            match status {
                "cancelled" => self.transcript.stop_recorded_run(true),
                "failed" | "interrupted" => self.transcript.stop_recorded_run(false),
                _ => {}
            }
        }
    }

    fn refresh_terminal(&mut self, runs: &[Run]) {
        let Some(id) = &self.active_run else { return };
        let Some(terminal) = self.terminal.get(id) else {
            return;
        };
        let run = runs
            .iter()
            .find(|run| run.id == *id && run.status == terminal.status);
        let label = match terminal.status.as_str() {
            "completed" => "Completed",
            "cancelled" => "Cancelled",
            "interrupted" => "Interrupted",
            _ => "Failed",
        };
        let finished_at = terminal
            .timestamp_ms
            .and_then(|ms| i64::try_from(ms).ok())
            .and_then(chrono::DateTime::from_timestamp_millis)
            .map(|time| time.with_timezone(&chrono::Local));
        self.transcript.finish_run_with_timing(
            label,
            run.map(|run| std::time::Duration::from_millis(run.duration_ms)),
            finished_at,
        );
        if matches!(terminal.status.as_str(), "failed" | "interrupted")
            && let Some(error) = run.and_then(|run| run.result.as_ref()).and_then(|result| {
                result["error"]
                    .as_str()
                    .or_else(|| result["output"].as_str())
            })
        {
            self.notice = Some(error.to_owned());
        }
    }

    pub(super) fn anchor(&mut self, anchor: Anchor) {
        self.pending = Some(anchor);
        self.resolve_pending();
    }

    fn resolve_pending(&mut self) {
        let Some(anchor) = self.pending.clone() else {
            return;
        };
        if !self.caught_up && self.last_offset.is_none_or(|offset| offset < anchor.offset) {
            return;
        }
        let index = anchor
            .tool
            .as_ref()
            .and_then(|tool| {
                self.by_tool
                    .get(&(anchor.run.clone().unwrap_or_default(), tool.clone()))
                    .copied()
            })
            .or_else(|| {
                self.by_offset
                    .range(..=anchor.offset)
                    .next_back()
                    .map(|(_, index)| *index)
            })
            .or_else(|| {
                anchor
                    .run
                    .as_ref()
                    .and_then(|run| self.by_run.get(run).copied())
            });
        if let Some(index) = index {
            if self.transcript.reveal_entry(index) {
                self.pending = None;
            }
        } else if self.caught_up {
            self.pending = None;
            self.notice = Some("Source entry is no longer available in this transcript.".into());
        }
    }

    /// 返回 Some(原文) 表示需要提交；返回 None 表示只处理了本地编辑。
    pub(super) fn key(&mut self, key: KeyEvent, drafts: &mut Drafts) -> Option<String> {
        if key.kind != KeyEventKind::Press && key.kind != KeyEventKind::Repeat {
            return None;
        }
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        if control {
            match key.code {
                KeyCode::Char('j') => {
                    drafts.draft(&self.agent).editor.insert_newline();
                    drafts.touch(&self.agent);
                    return None;
                }
                KeyCode::Char('a') => {
                    drafts
                        .draft(&self.agent)
                        .editor
                        .move_cursor(CursorMove::Head);
                    return None;
                }
                KeyCode::Char('e') => {
                    drafts
                        .draft(&self.agent)
                        .editor
                        .move_cursor(CursorMove::End);
                    return None;
                }
                KeyCode::Char('k') => {
                    let draft = drafts.draft(&self.agent);
                    draft.editor.cancel_selection();
                    draft.editor.delete_line_by_end();
                    drafts.touch(&self.agent);
                    return None;
                }
                KeyCode::Char('o') => {
                    self.transcript.toggle_tool_output();
                    return None;
                }
                KeyCode::Home => {
                    self.transcript.scroll_up(usize::MAX);
                    return None;
                }
                KeyCode::End => {
                    self.transcript.follow();
                    return None;
                }
                _ => return None,
            }
        }
        match key.code {
            KeyCode::Esc => None,
            KeyCode::Enter => {
                let text = drafts.draft(&self.agent).editor.lines().join("\n");
                if text.trim().is_empty() {
                    return None;
                }
                // 只有真正提交的原文返回给 App；全空白拒绝。
                Some(text)
            }
            KeyCode::Char(_) | KeyCode::Backspace | KeyCode::Delete | KeyCode::Tab => {
                drafts.draft(&self.agent).editor.input(key);
                drafts.touch(&self.agent);
                None
            }
            KeyCode::Left
            | KeyCode::Right
            | KeyCode::Up
            | KeyCode::Down
            | KeyCode::Home
            | KeyCode::End => {
                drafts.draft(&self.agent).editor.input(key);
                None
            }
            KeyCode::PageUp => {
                self.transcript.page_up();
                None
            }
            KeyCode::PageDown => {
                self.transcript.page_down();
                None
            }
            KeyCode::Insert | KeyCode::Null | KeyCode::CapsLock => None,
            _ => None,
        }
    }

    pub(super) fn paste(&mut self, text: &str, drafts: &mut Drafts) {
        drafts
            .draft(&self.agent)
            .editor
            .insert_str(text.replace("\r\n", "\n").replace('\r', "\n"));
        drafts.touch(&self.agent);
    }

    pub(super) fn wheel(&mut self, up: bool, lines: usize) {
        if up {
            self.transcript.scroll_up(lines);
        } else {
            self.transcript.scroll_down(lines);
        }
    }

    pub(super) fn draw(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        view: &AgentView,
        drafts: &mut Drafts,
        now_ms: u64,
    ) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let hint_height = 1;
        let header_height = 2.min(area.height);
        let body = Rect::new(
            area.x,
            area.y + header_height,
            area.width,
            area.height.saturating_sub(header_height + hint_height),
        );
        let editor_lines = drafts.draft(&self.agent).editor.lines().len().clamp(1, 3) as u16;
        let editor_height = (editor_lines + 2).min(body.height);
        let transcript_area = Rect::new(
            body.x,
            body.y,
            body.width,
            body.height.saturating_sub(editor_height),
        );
        let editor_area = Rect::new(
            body.x,
            transcript_area.bottom(),
            body.width,
            body.height.saturating_sub(transcript_area.height),
        );
        // 标题两行：身份与真实状态。
        let status_color = match view.status {
            super::projection::AgentStatus::Running => ACCENT,
            super::projection::AgentStatus::Idle => ADDED,
            super::projection::AgentStatus::Parked => MUTED,
            super::projection::AgentStatus::Aborted => REMOVED,
        };
        let model_change = view
            .model_change
            .as_deref()
            .map(|status| format!(" · {status}"))
            .unwrap_or_default();
        let header = vec![
            Line::from(vec![
                Span::styled("Agent Hub", Style::default().fg(ACCENT)),
                Span::styled(
                    format!(" · {} / {}", display_text(&view.name), view.id),
                    Style::default().fg(ACTIVITY),
                ),
            ]),
            Line::from(vec![
                Span::styled(
                    format!(
                        "{} · parent {} · {} · effort {}{}",
                        view.status.label(),
                        display_text(&view.parent),
                        display_text(&view.model),
                        view.thinking,
                        model_change
                    ),
                    Style::default().fg(status_color),
                ),
                Span::styled(
                    format!(" · {}", usage_summary(view, now_ms)),
                    Style::default().fg(DIM),
                ),
            ]),
        ];
        frame.render_widget(
            Paragraph::new(header).wrap(Wrap { trim: false }),
            Rect::new(area.x, area.y, area.width, header_height),
        );
        self.transcript.set_terminal_rows(area.height);
        self.transcript
            .resize_preserving_anchor(transcript_area.width);
        // 首批历史可能在首帧获得宽度前读完，定位不能等待下一批事件。
        self.resolve_pending();
        self.transcript.draw(frame, transcript_area);
        let block = Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(if self.focus { ACCENT } else { DIM }));
        let inner = block.inner(editor_area);
        frame.render_widget(block, editor_area);
        let prompt_width = inner.width.saturating_sub(1).min(2);
        if prompt_width > 0 && inner.height > 0 {
            frame.render_widget(
                Line::from(Span::styled("❯", Style::default().fg(ACCENT))),
                Rect::new(inner.x, inner.y, prompt_width, 1),
            );
        }
        let editor_rect = Rect::new(
            inner.x + prompt_width,
            inner.y,
            inner.width.saturating_sub(prompt_width),
            inner.height,
        );
        let draft = drafts.draft(&self.agent);
        draft.editor.set_block(Block::default());
        if editor_rect.width > 0 && editor_rect.height > 0 {
            frame.render_widget(&draft.editor, editor_rect);
            if self.focus {
                let cursor = draft.editor.screen_cursor();
                let (top, left) = draft.editor.scroll_offset();
                if let (Some(row), Some(column)) = (
                    cursor.row.checked_sub(usize::from(top)),
                    cursor.col.checked_sub(usize::from(left)),
                ) && row < usize::from(editor_rect.height)
                    && column < usize::from(editor_rect.width)
                {
                    frame.set_cursor_position((
                        editor_rect.x + column as u16,
                        editor_rect.y + row as u16,
                    ));
                }
            }
        }
        let footer = self.notice.clone().unwrap_or_else(|| {
            "Enter send · Ctrl+J newline · PgUp/PgDn scroll · Ctrl+O tools · Esc back".into()
        });
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                display_text(&footer).into_owned(),
                Style::default().fg(if self.notice.is_some() { REMOVED } else { DIM }),
            ))),
            Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1),
        );
    }
}

/// 详情底部的真实 child 用量摘要；未知值显示 —。
pub(super) fn usage_summary(view: &AgentView, now_ms: u64) -> String {
    let age = view
        .last_activity
        .map(|timestamp| format_age(now_ms.saturating_sub(timestamp)))
        .unwrap_or_else(|| "—".into());
    let tokens = view
        .tokens
        .map(|tokens| tokens.to_string())
        .unwrap_or_else(|| "—".into());
    let cost = match (view.cost, view.cost_measured, view.runs) {
        (Some(cost), measured, runs) if measured == runs || runs == 0 => format!("${cost:.4}"),
        (Some(cost), measured, _) => format!("${cost:.4} ({measured} measured)"),
        (None, _, _) => "cost —".into(),
    };
    let time = view
        .time_ms
        .map(format_duration)
        .unwrap_or_else(|| "—".into());
    let context = view
        .context
        .and_then(|usage| usage.percent())
        .map(|percent| format!("ctx {percent}%"))
        .unwrap_or_default();
    format!(
        "{tokens} tokens · {cost} · agent time {time} · active {age}{}{}",
        if context.is_empty() {
            String::new()
        } else {
            format!(" · {context}")
        },
        if view.loading { " · loading" } else { "" }
    )
}

pub(super) fn format_age(ms: u64) -> String {
    let seconds = ms / 1000;
    if seconds < 60 {
        format!("{seconds}s ago")
    } else if seconds < 3600 {
        format!("{}m ago", seconds / 60)
    } else {
        format!("{}h ago", seconds / 3600)
    }
}

pub(super) fn format_clock(timestamp: Option<u64>) -> String {
    let Some(timestamp) = timestamp else {
        return "—".into();
    };
    let seconds = (timestamp / 1000) % 86_400;
    format!(
        "{:02}:{:02}:{:02}",
        seconds / 3600,
        (seconds % 3600) / 60,
        seconds % 60
    )
}

fn format_duration(ms: u64) -> String {
    let seconds = ms / 1000;
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m{:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h{:02}m", seconds / 3600, (seconds % 3600) / 60)
    }
}

/// child 编辑器不执行 slash、shell、@ 展开或主会话历史。
pub(super) fn editor() -> TextArea<'static> {
    let mut editor = TextArea::default();
    editor.set_cursor_line_style(Style::default());
    editor.set_cursor_style(Style::default().add_modifier(Modifier::empty()));
    editor
}

#[cfg(test)]
mod tests;
