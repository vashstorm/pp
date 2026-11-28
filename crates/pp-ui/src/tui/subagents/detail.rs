//! Standalone full-screen child detail: a fresh Transcript, history imported per Run, anchor positioning and an input box.
//!
//! The detail is only shown inside the Hub and never switches the main chat surface to the child session; the main session keeps running and
//! keeps its draft, viewport and usage attribution. Input submission calls the existing
//! `Binding::control` through the observer thread and performs no I/O on the UI thread.

use super::super::statusline;
use super::super::transcript_style::{accent, activity, dim, removed};
use super::Drafts;
use super::projection::AgentView;
use crate::tui::Transcript;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use pp_core::events::AgentEvent;
use pp_core::text::display_text;
use pp_runtime::subagent::Binding;
use pp_runtime::subagent::EventPage;
use pp_runtime::subagent::HubEvent;
use pp_runtime::subagent::ObservedEntry;
use pp_runtime::subagent::Run;
use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};
use ratatui_textarea::{CursorMove, TextArea};
use std::collections::{BTreeMap, BTreeSet};

/// Fixed editing hint shown when there is no notice; matches the newline semantics of the main input box.
const HINT: &str = "Enter send · Shift+Enter newline · Ctrl+J newline · PgUp/PgDn scroll · Ctrl+O tools · Esc back";

/// Anchor from an Activity row to a detail body line.
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
    token_speed: statusline::TokenSpeed,
    /// Worker generation and starting event sequence number of the current model request.
    speed_request: Option<(u64, u64)>,
    /// run -> index of the first user card entry.
    by_run: BTreeMap<String, usize>,
    /// Starting source line offset -> entry index.
    by_offset: BTreeMap<u64, usize>,
    /// (run, tool call id) -> card entry index.
    by_tool: BTreeMap<(String, String), usize>,
    /// Runs whose user card was already appended.
    started: BTreeSet<String>,
    /// Runs whose terminal state was already applied.
    terminal: BTreeMap<String, Terminal>,
    active_run: Option<String>,
    accepted: BTreeSet<(String, u64)>,
    last_offset: Option<u64>,
    caught_up: bool,
    history_error: Option<String>,
    pending: Option<Anchor>,
    pub notice: Option<String>,
    pub focus: bool,
}

struct Terminal {
    status: String,
    timestamp_ms: Option<u64>,
}

impl Detail {
    pub(super) fn output_preview(&mut self, record: &pp_runtime::subagent::ObservedRecord) {
        if self.caught_up
            && self.active_run.as_ref() == record.run_id.as_ref()
            && record
                .run_id
                .as_ref()
                .is_none_or(|id| !self.terminal.contains_key(id))
            && let Some(event @ AgentEvent::ToolOutputSnapshot { .. }) = &record.event
        {
            self.transcript.progress(event.clone());
        }
    }

    /// The pipeline stage preview and the main output snapshot arrive separately and must not replace each other.
    pub(super) fn pipeline_preview(&mut self, record: &pp_runtime::subagent::ObservedRecord) {
        if self.caught_up
            && self.active_run.as_ref() == record.run_id.as_ref()
            && record
                .run_id
                .as_ref()
                .is_none_or(|id| !self.terminal.contains_key(id))
            && let Some(
                event @ (AgentEvent::ToolPipelineSnapshot { .. }
                | AgentEvent::ToolShellSnapshot { .. }),
            ) = &record.event
        {
            self.transcript.progress(event.clone());
        }
    }
    pub(super) fn new(agent: &str, view: u64, transcript: Transcript) -> Self {
        Self {
            agent: agent.to_owned(),
            view,
            transcript,
            token_speed: Default::default(),
            speed_request: None,
            by_run: BTreeMap::new(),
            by_offset: BTreeMap::new(),
            by_tool: BTreeMap::new(),
            started: BTreeSet::new(),
            terminal: BTreeMap::new(),
            active_run: None,
            accepted: BTreeSet::new(),
            last_offset: None,
            caught_up: false,
            history_error: None,
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

    pub(super) fn tick(&mut self, now: std::time::Instant) -> bool {
        let speed_changed = self.token_speed.tick(now);
        self.transcript.tick(now) || speed_changed
    }

    pub(super) fn history_failed(&mut self, message: String) {
        self.history_error = Some(message);
    }

    pub(super) fn sample_model_progress(&mut self, binding: &Binding) {
        if let Some(run_id) = &self.active_run
            && let Some((generation, sequence)) = self.speed_request
            && let Some((bytes, first_output)) =
                binding.model_output_progress(&self.agent, run_id, generation, sequence)
        {
            self.token_speed.progress(bytes, first_output);
        }
    }

    /// settled is the terminal state frozen before the worker read the page; it may only be used to complete after the whole page was read.
    pub(super) fn ingest(
        &mut self,
        page: &EventPage,
        runs: &[Run],
        settled: &BTreeSet<String>,
        now_ms: u64,
    ) {
        let now = std::time::Instant::now();
        self.history_error = None;
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
            // Old logs may lack the terminal marker; a record of a new Run proves the previous turn was fully read.
            if let Some(previous) = runs.iter().find(|candidate| {
                self.active_run.as_deref() == Some(&candidate.id)
                    && !matches!(candidate.status.as_str(), "queued" | "running")
            }) {
                self.apply_terminal(&previous.id, &previous.status, None);
            }
            self.active_run = Some(run.clone());
            self.speed_request = None;
            self.token_speed.clear();
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
            // Accepted input produces one user card of its own; progress explicitly ignores that event.
            (Some(AgentEvent::UserInputAccepted { id, text }), _) => {
                if !self.accepted.insert((run.clone(), *id)) {
                    return;
                }
                let index = self.transcript.entry_count();
                self.transcript.append("You", text);
                self.by_offset.insert(entry.start_offset, index);
            }
            (Some(event), _) => {
                if (record.run_id.is_none() || record.run_id == self.active_run)
                    && !self.terminal.contains_key(&run)
                {
                    self.observe_speed(event, record, now_ms, now);
                }
                let before = self.transcript.entry_count();
                self.transcript
                    .progress_recorded(event.clone(), record.timestamp_ms, now_ms, now);
                let after = self.transcript.entry_count();
                if after > before {
                    self.by_offset.insert(entry.start_offset, before);
                }
                if let AgentEvent::ToolStarted { id, .. } = event
                    && let Some(index) = self.transcript.tool_index(id)
                {
                    // A background query can reuse the existing card, and the anchor follows the real tool index.
                    self.by_offset.insert(entry.start_offset, index);
                    self.by_tool.insert((run.clone(), id.clone()), index);
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

    fn observe_speed(
        &mut self,
        event: &AgentEvent,
        record: &pp_runtime::subagent::ObservedRecord,
        now_ms: u64,
        now: std::time::Instant,
    ) {
        // Old logs are replayed by event time for sampling so a slow paged read is not mistaken for model speed.
        let recorded_at = || {
            record
                .timestamp_ms
                .and_then(|timestamp| {
                    now.checked_sub(std::time::Duration::from_millis(
                        now_ms.saturating_sub(timestamp),
                    ))
                })
                .unwrap_or(now)
        };
        match event {
            AgentEvent::ModelRequestStarted => {
                self.speed_request = record
                    .run_id
                    .as_ref()
                    .zip(record.sequence)
                    .map(|(_, sequence)| (record.generation, sequence));
                self.token_speed.start(recorded_at());
            }
            AgentEvent::ModelOutputProgress { output_bytes } => {
                let at = recorded_at();
                self.token_speed.progress(*output_bytes, at);
                self.token_speed.tick(at);
            }
            AgentEvent::ModelRequestFinished {
                elapsed_ms,
                usage,
                success,
            } => {
                self.speed_request = None;
                self.token_speed.finish(
                    *elapsed_ms,
                    usage.map(|usage| usage.output_tokens),
                    *success,
                );
            }
            AgentEvent::CompactionFinished { .. } => {
                self.speed_request = None;
                self.token_speed.clear();
            }
            _ => {}
        }
    }

    /// Apply the terminal state only once the corresponding Run was fully read (or reached a terminal lifecycle).
    fn apply_terminal(&mut self, run: &str, status: &str, timestamp_ms: Option<u64>) {
        if let Some(terminal) = self.terminal.get_mut(run) {
            // After the snapshot is completed, a late lifecycle can still restore the real completion time.
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
            self.speed_request = None;
            if self.token_speed.is_active() {
                self.token_speed.clear();
            }
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

    /// Returning Some(raw text) means it must be submitted; None means only local editing was handled.
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
        // Shift + Enter matches the main input box: newline only, no submit (including Alt + Shift).
        if key.code == KeyCode::Enter && key.modifiers.contains(KeyModifiers::SHIFT) {
            drafts.draft(&self.agent).editor.insert_newline();
            drafts.touch(&self.agent);
            return None;
        }
        match key.code {
            KeyCode::Esc => None,
            KeyCode::Enter => {
                let text = drafts.draft(&self.agent).editor.lines().join("\n");
                if text.trim().is_empty() {
                    return None;
                }
                // Only the raw text that is really submitted is returned to App; all-whitespace input is rejected.
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
        cost: &pp_config::config::CostDisplay,
        now_ms: u64,
    ) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        // Every line budget uses saturating / checked arithmetic: 0, 1 and 2 rows and 1 column never go out of bounds.
        let status_height = u16::from(area.height >= 2);
        let body_height = area.height - status_height;
        let header_height = u16::from(body_height > 0);
        let hint_height = u16::from(body_height > header_height + 1);
        let input_space = body_height - header_height - hint_height;
        let bordered = input_space >= 3 && area.width >= 3;
        let border_height = if bordered { 2 } else { 0 };
        let prompt_width = area.width.saturating_sub(1).min(2);
        let editor_rows = crate::tui::input_rows(
            &drafts.draft(&self.agent).editor,
            area.width.saturating_sub(prompt_width),
            input_space.saturating_sub(border_height).clamp(1, 6),
        );
        let input_height = (editor_rows + border_height).min(input_space);
        let activity_height =
            u16::from(input_space > input_height && self.transcript.has_live_activity());
        let activity_spacing =
            u16::from(activity_height > 0 && input_space > input_height + activity_height + 1);
        let transcript_height =
            input_space.saturating_sub(input_height + activity_height + activity_spacing);
        // Single-line identity and real status; model, effort and usage are carried by the input metric row and the bottom status bar.
        let status_color = super::status_color(view.status);
        let status_icon = super::status_icon(view.status, now_ms);
        let parked = if view.parked && view.status == super::AgentStatus::Completed {
            " · parked"
        } else {
            ""
        };
        let model_change = view
            .model_change
            .as_deref()
            .map(|status| format!(" · {status}"))
            .unwrap_or_default();
        let age = view
            .last_activity
            .map(|timestamp| format_age(now_ms.saturating_sub(timestamp)))
            .unwrap_or_else(|| "—".into());
        let history_loading = !self.caught_up && self.history_error.is_none();
        let header = super::fit_line(
            Line::from(vec![
                Span::styled("Agent Hub", Style::default().fg(accent())),
                Span::styled(
                    format!(" · {} / {} · ", display_text(&view.name), view.id),
                    Style::default().fg(activity()),
                ),
                Span::styled(
                    format!("{status_icon} {}", view.status.label()),
                    Style::default().fg(status_color),
                ),
                Span::styled(
                    format!(
                        "{parked} · parent {}{}",
                        display_text(&view.parent),
                        model_change
                    ),
                    Style::default().fg(activity()),
                ),
                Span::styled(
                    format!(
                        " · active {age}{}",
                        if view.loading || history_loading {
                            " · loading"
                        } else {
                            ""
                        }
                    ),
                    Style::default().fg(dim()),
                ),
            ]),
            usize::from(area.width),
        );
        if header_height > 0 {
            frame.render_widget(
                Paragraph::new(header),
                Rect::new(area.x, area.y, area.width, header_height),
            );
        }
        let body = Rect::new(area.x, area.y + header_height, area.width, input_space);
        let transcript_area = Rect::new(body.x, body.y, body.width, transcript_height);
        let activity_area = Rect::new(
            body.x,
            transcript_area.bottom() + activity_spacing,
            body.width,
            activity_height,
        );
        let hint_area = Rect::new(body.x, activity_area.bottom(), body.width, hint_height);
        let input_area = Rect::new(body.x, hint_area.bottom(), body.width, input_height);
        self.transcript.set_terminal_rows(area.height);
        self.transcript
            .resize_preserving_anchor(transcript_area.width);
        // The first batch of history may finish loading before the first frame knows the width, so positioning cannot wait for the next batch.
        self.resolve_pending();
        self.transcript.draw(frame, transcript_area);
        if transcript_height > 0 && self.transcript.entry_count() == 0 {
            // The first frame uses the existing task snapshot and the body is taken over by the ordered log. The Activity anchor may point at an old task.
            let mut lines = Vec::new();
            if history_loading {
                lines.push(Line::styled(
                    "Loading task history…",
                    Style::default().fg(dim()),
                ));
            }
            if self.pending.is_none()
                && let Some(task) = view.task.as_deref()
            {
                let task = super::projection::truncate(task);
                lines.push(Line::raw(display_text(&task).into_owned()));
            }
            frame.render_widget(Paragraph::new(lines), transcript_area);
        }
        self.transcript.draw_live_activity(frame, activity_area);
        if hint_height > 0 {
            let notice = self.history_error.as_ref().or(self.notice.as_ref());
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    display_text(notice.map_or(HINT, String::as_str)).into_owned(),
                    Style::default().fg(if notice.is_some() { removed() } else { dim() }),
                ))),
                hint_area,
            );
        }
        let editor_area = if bordered {
            let block = Block::default()
                .borders(Borders::TOP | Borders::BOTTOM)
                .border_style(Style::default().fg(statusline::accent()));
            let inner = block.inner(input_area);
            frame.render_widget(block, input_area);
            // The metric row is written to the right of the top border row and must be covered after the border, keeping the left border glyph.
            let metrics = statusline::subagent_metrics(
                statusline::SubagentUsage {
                    speed: self.token_speed.value().map(|(rate, _)| rate),
                    usage: view.context.as_ref(),
                    tokens: view.tokens,
                    cost: view.cost,
                    measured: view.cost_measured,
                    runs: view.runs,
                    time_ms: view.time_ms,
                },
                cost,
                area.width,
            )
            .style(Style::default().bg(statusline::input_status_bg()));
            let metrics_width = u16::try_from(metrics.width())
                .unwrap_or(u16::MAX)
                .min(area.width);
            frame.render_widget(
                metrics,
                Rect::new(
                    input_area.right() - metrics_width,
                    input_area.y,
                    metrics_width,
                    1,
                ),
            );
            inner
        } else {
            input_area
        };
        let prompt_width = editor_area.width.saturating_sub(1).min(2);
        if prompt_width > 0 && editor_area.height > 0 {
            frame.render_widget(
                Line::from("❯"),
                Rect::new(editor_area.x, editor_area.y, prompt_width, 1),
            );
        }
        let editor_rect = Rect::new(
            editor_area.x + prompt_width,
            editor_area.y,
            editor_area.width.saturating_sub(prompt_width),
            editor_area.height,
        );
        let draft = drafts.draft(&self.agent);
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
        if status_height > 0 {
            frame.render_widget(
                statusline::subagent_status(
                    &view.model,
                    view.thinking
                        .parse::<pp_core::model::ThinkLevel>()
                        .unwrap_or_default(),
                    &statusline::workdir_label(&view.cwd),
                    area.width,
                ),
                Rect::new(
                    area.x,
                    area.bottom() - status_height,
                    area.width,
                    status_height,
                ),
            );
        }
    }
}

pub(super) fn format_age(ms: u64) -> String {
    let seconds = ms / 1000;
    if seconds < 5 {
        "just now".into()
    } else if seconds < 60 {
        format!("{seconds}s ago")
    } else if seconds < 3600 {
        format!("{}m ago", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h ago", seconds / 3600)
    } else if seconds < 604_800 {
        format!("{}d ago", seconds / 86_400)
    } else {
        format!("{}w ago", seconds / 604_800)
    }
}

pub(super) fn format_clock(timestamp: Option<u64>) -> String {
    timestamp
        .and_then(|ms| i64::try_from(ms).ok())
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map_or_else(
            || "—".into(),
            |time| {
                time.with_timezone(&chrono::Local)
                    .format("%H:%M:%S")
                    .to_string()
            },
        )
}

/// The child editor reuses the main editor configuration: TEXT for the body, word-level wrapping and 1000 undo steps.
pub(super) fn editor() -> TextArea<'static> {
    super::super::editor()
}

#[cfg(test)]
mod tests;
