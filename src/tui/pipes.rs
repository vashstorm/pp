//! 临时管道阶段浏览：只持有当前选中流的有界正文，读取与轮询在观察线程。
//!
//! 面板打开不进入 steering、不发送模型请求；Esc 只关闭面板，不改写输入草稿，
//! 也不触碰原生主屏历史。正文副本在关闭时随面板一起释放。

mod worker;

use super::transcript::wrap_block;
use super::transcript_style::{ACCENT, ACTIVITY, ADDED, ASK_BG, MUTED, REMOVED, display_text};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Clear,
};
pub(super) use worker::{Reply, Request, Worker};

/// 面板正文上限；跟随时只保留最新 1 MiB，冻结快照同样有界。
const MAX_BODY_BYTES: usize = 1024 * 1024;

pub(super) const HINT: &str = "←/→ run  ↑/↓ stream  PgUp/PgDn/wheel scroll  Home top  End tail  f follow  r refresh  Esc close";

/// 观察线程与面板共用的有界 run 视图；不暴露 `tools::bash` 的私有类型。
#[derive(Clone, Debug, Default)]
pub(super) struct RunView {
    pub run_id: String,
    pub owner: String,
    pub state: String,
    pub exit_code: Option<i32>,
    pub revision: u64,
    pub pipelines: Vec<PipelineView>,
    pub streams: Vec<StreamView>,
    pub nonzero_stages: u64,
    pub omitted_pipelines: u64,
    pub omitted_stages: u64,
    pub untapped_edges: u64,
    pub evicted_streams: u64,
    pub incomplete: bool,
    pub capture_error: Option<String>,
}

#[derive(Clone, Debug)]
pub(super) struct PipelineView {
    pub id: u64,
    pub parent: Option<u64>,
    pub stage_count: usize,
    pub bang: bool,
    pub pipefail: bool,
    pub status: Option<u8>,
    pub stages: Vec<StageView>,
}

#[derive(Clone, Debug)]
pub(super) struct StageView {
    pub command: String,
    pub state: String,
    pub status: Option<u8>,
    pub signal: Option<i32>,
}

#[derive(Clone, Debug)]
pub(super) struct StreamView {
    pub id: u64,
    pub pipeline: u64,
    pub producer_stage: usize,
    pub consumer_stage: usize,
    pub producer: String,
    pub consumer: String,
    pub open: bool,
    pub total_bytes: u64,
    pub retained_start: u64,
    pub retained_end: u64,
    pub revision: u64,
    pub close_reason: Option<String>,
}

/// 一次受观察的正文更新；`reset` 表示必须丢弃旧正文重建。
#[derive(Clone, Debug)]
pub(super) struct BodyUpdate {
    pub run: String,
    pub stream: u64,
    pub data: String,
    pub start: u64,
    pub end: u64,
    pub reset: bool,
    pub retained_start: u64,
    pub retained_end: u64,
    pub total_bytes: u64,
    pub open: bool,
    pub revision: u64,
    pub prefix_evicted: bool,
}

pub(super) enum Action {
    None,
    Close,
    Refresh { full: bool },
    SelectRun { run: String },
    SelectStream { run: String, stream: u64 },
    Follow { run: String, stream: u64 },
}

#[derive(Clone, Copy, Default)]
struct LiveMeta {
    retained_start: u64,
    retained_end: u64,
    total_bytes: u64,
    open: bool,
}

#[derive(Default)]
struct Body {
    text: String,
    retained_start: u64,
    retained_end: u64,
    total_bytes: u64,
    open: bool,
    revision: u64,
    end: u64,
    trimmed: bool,
}

pub(super) struct Panel {
    explicit_run: Option<String>,
    runs: Vec<RunView>,
    selected_run: usize,
    selected_stream: usize,
    body: Body,
    live: LiveMeta,
    line_starts: Vec<usize>,
    scroll: usize,
    following: bool,
    awaiting: bool,
    reload_frozen: bool,
    new_bytes: u64,
    evicted: bool,
    body_height: usize,
    loading: bool,
    unavailable: bool,
    error: Option<String>,
}

impl Panel {
    pub(super) fn new(explicit: Option<String>) -> Self {
        Self {
            explicit_run: explicit,
            runs: Vec::new(),
            selected_run: 0,
            selected_stream: 0,
            body: Body::default(),
            live: LiveMeta::default(),
            line_starts: vec![0],
            scroll: 0,
            following: true,
            awaiting: true,
            reload_frozen: false,
            new_bytes: 0,
            evicted: false,
            body_height: 1,
            loading: true,
            unavailable: false,
            error: None,
        }
    }

    pub(super) fn set_error(&mut self, error: String) {
        self.error = Some(display_text(&error).into_owned());
        self.loading = false;
        self.awaiting = false;
    }

    pub(super) fn set_runs(
        &mut self,
        runs: Vec<RunView>,
        selected_run: Option<String>,
        selected_stream: Option<u64>,
    ) -> bool {
        let old_run = self.selected_run_id().map(str::to_owned);
        let old_stream = self.current_stream().map(|stream| stream.id);
        self.runs = runs;
        self.loading = false;
        let explicit_selection = selected_run.is_some();
        let target_run = selected_run
            .or_else(|| {
                old_run
                    .clone()
                    .filter(|id| self.runs.iter().any(|run| &run.run_id == id))
            })
            .or_else(|| {
                self.explicit_run
                    .is_none()
                    .then(|| self.runs.first().map(|run| run.run_id.clone()))
                    .flatten()
            });
        let target_index = target_run
            .as_deref()
            .and_then(|id| self.runs.iter().position(|run| run.run_id == id));
        if let Some(index) = target_index {
            self.selected_run = index;
        } else {
            self.selected_run = 0;
            self.selected_stream = 0;
        }
        let target_stream = selected_stream
            .filter(|id| self.streams().iter().any(|stream| stream.id == *id))
            .or_else(|| {
                old_stream.filter(|id| self.streams().iter().any(|stream| stream.id == *id))
            })
            .or_else(|| self.streams().first().map(|stream| stream.id));
        self.unavailable = self.explicit_run.is_some() && !explicit_selection;
        let stream_index =
            target_stream.and_then(|id| self.streams().iter().position(|stream| stream.id == id));
        let stream_changed = target_stream != old_stream;
        if let Some(index) = stream_index {
            self.selected_stream = index;
        } else {
            self.selected_stream = 0;
        }
        let run_changed = self.selected_run_id() != old_run.as_deref();
        if run_changed || stream_changed {
            self.reset_body();
        }
        if let Some(stream) = self.current_stream() {
            self.live = LiveMeta {
                retained_start: stream.retained_start,
                retained_end: stream.retained_end,
                total_bytes: stream.total_bytes,
                open: stream.open,
            };
        }
        // Runs 只在签名或选择变化时到达；即便两者未变，live 元数据也可能更新。
        let _ = (run_changed, stream_changed);
        true
    }

    pub(super) fn apply_body(&mut self, update: BodyUpdate) -> bool {
        if self.unavailable
            || self.selected_run_id() != Some(update.run.as_str())
            || self.current_stream().map(|stream| stream.id) != Some(update.stream)
        {
            return false;
        }
        let previous_end = self.live.retained_end.max(self.live.total_bytes);
        if self.reload_frozen && update.reset {
            self.live = LiveMeta {
                retained_start: update.retained_start,
                retained_end: update.retained_end,
                total_bytes: update.total_bytes,
                open: update.open,
            };
            self.body.text.clear();
            self.body.text.push_str(&update.data);
            self.body.end = update.end;
            self.body.retained_start = update.retained_start;
            self.body.retained_end = update.retained_end;
            self.body.total_bytes = update.total_bytes;
            self.body.open = update.open;
            self.body.revision = update.revision;
            self.body.trimmed = false;
            self.awaiting = false;
            self.reload_frozen = false;
            self.new_bytes = 0;
            self.evicted = update.prefix_evicted;
            self.scroll = 0;
            self.trim_body();
            self.reindex();
            return true;
        }
        self.live = LiveMeta {
            retained_start: update.retained_start,
            retained_end: update.retained_end,
            total_bytes: update.total_bytes,
            open: update.open,
        };
        if !self.following {
            if update.end > self.body.end {
                let from = self.body.end.max(previous_end).min(update.end);
                self.new_bytes = self.new_bytes.saturating_add(update.end - from);
            }
            if update.prefix_evicted
                || update.start > self.body.end
                || update.retained_start > self.body.retained_start
            {
                self.evicted = true;
            }
            return true;
        }
        if self.awaiting && !update.reset {
            return false;
        }
        if update.reset || update.start != self.body.end {
            self.body.text.clear();
            self.body.text.push_str(&update.data);
            self.body.trimmed = false;
        } else if !update.data.is_empty() {
            self.body.text.push_str(&update.data);
        }
        self.body.end = update.end;
        self.body.retained_start = update.retained_start;
        self.body.retained_end = update.retained_end;
        self.body.total_bytes = update.total_bytes;
        self.body.open = update.open;
        self.body.revision = update.revision;
        self.awaiting = false;
        self.new_bytes = 0;
        self.evicted = update.prefix_evicted;
        self.trim_body();
        self.reindex();
        true
    }

    pub(super) fn key(&mut self, key: KeyEvent) -> Action {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
            || !key.modifiers.is_empty()
        {
            return Action::None;
        }
        if key.code == KeyCode::Esc {
            return Action::Close;
        }
        let page = self.body_height.max(1);
        match key.code {
            KeyCode::Char('f') if key.kind == KeyEventKind::Press => {
                if self.following {
                    self.following = false;
                    return Action::None;
                }
                self.following = true;
                self.awaiting = true;
                self.new_bytes = 0;
                self.evicted = false;
                return self.follow_action();
            }
            KeyCode::Char('r') if key.kind == KeyEventKind::Press => {
                self.error = None;
                return Action::Refresh {
                    full: self.following,
                };
            }
            KeyCode::Left => {
                if self.unavailable {
                    if self.rebind_current_run() {
                        return self.select_run_action();
                    }
                } else if let Some(index) = self.selected_run.checked_sub(1) {
                    if self.select_run(index) {
                        return self.select_run_action();
                    }
                }
            }
            KeyCode::Right => {
                if self.unavailable {
                    if self.rebind_current_run() {
                        return self.select_run_action();
                    }
                } else if self.select_run(self.selected_run.saturating_add(1)) {
                    return self.select_run_action();
                }
            }
            KeyCode::Up if !self.unavailable => {
                if let Some(index) = self.selected_stream.checked_sub(1) {
                    if self.select_stream(index) {
                        return self.select_stream_action();
                    }
                }
            }
            KeyCode::Down if !self.unavailable => {
                if self.select_stream(self.selected_stream.saturating_add(1)) {
                    return self.select_stream_action();
                }
            }
            KeyCode::Home => {
                self.following = false;
                self.scroll = 0;
                if self.body.trimmed {
                    // 面板副本已裁掉前缀：重新取保留区间起点，但仍保持冻结。
                    self.awaiting = true;
                    self.reload_frozen = true;
                    return Action::Refresh { full: true };
                }
            }
            KeyCode::End => {
                self.following = true;
                self.awaiting = true;
                self.new_bytes = 0;
                self.evicted = false;
                return self.follow_action();
            }
            KeyCode::PageUp => {
                self.freeze();
                self.scroll = self.scroll.saturating_sub(page);
            }
            KeyCode::PageDown => {
                if !self.following {
                    self.scroll = self.scroll.saturating_add(page);
                }
            }
            _ => {}
        }
        Action::None
    }

    pub(super) fn wheel(&mut self, up: bool, lines: usize) {
        if self.body.text.is_empty() {
            return;
        }
        if up {
            self.freeze();
            self.scroll = self.scroll.saturating_sub(lines);
        } else if !self.following {
            self.scroll = self.scroll.saturating_add(lines);
        }
    }

    pub(super) fn draw(&mut self, frame: &mut Frame, mut area: Rect) {
        frame.render_widget(Clear, area);
        if area.is_empty() {
            return;
        }
        draw_line(
            frame,
            &mut area,
            Line::from(vec![
                Span::styled("Pipe streams", Style::default().fg(ACCENT)),
                Span::styled(
                    "  temporary stage streams · memory only",
                    Style::default().fg(MUTED),
                ),
            ]),
        );
        if let Some(error) = &self.error {
            draw_line(
                frame,
                &mut area,
                Line::styled(error.as_str(), Style::default().fg(REMOVED)),
            );
        }
        if self.loading {
            draw_line(
                frame,
                &mut area,
                Line::styled("Loading pipe streams…", Style::default().fg(ACCENT)),
            );
            return;
        }
        if self.runs.is_empty() || self.unavailable {
            let message = self.explicit_run.as_deref().map_or_else(
                || "No pipe runs in this session.".to_owned(),
                |run| {
                    format!(
                        "Pipe run {} is unavailable in this session.",
                        display_text(run)
                    )
                },
            );
            draw_line(
                frame,
                &mut area,
                Line::styled(message, Style::default().fg(MUTED)),
            );
            return;
        }
        self.draw_run(frame, &mut area);
    }

    fn draw_run(&mut self, frame: &mut Frame, area: &mut Rect) {
        let (summary, pipelines, stream_rows, list_height) = {
            let run = &self.runs[self.selected_run];
            let state = run_state(run);
            draw_line(
                frame,
                area,
                Line::from(vec![
                    Span::styled(
                        format!("run {}/{} ", self.selected_run + 1, self.runs.len()),
                        Style::default().fg(MUTED),
                    ),
                    Span::styled(
                        display_text(&run.run_id).into_owned(),
                        Style::default().fg(ACTIVITY),
                    ),
                    Span::styled(
                        format!("  {}", display_text(&run.owner)),
                        Style::default().fg(MUTED),
                    ),
                    Span::styled(
                        format!("  {state}"),
                        Style::default().fg(if run.exit_code.is_some_and(|code| code != 0) {
                            ACCENT
                        } else {
                            MUTED
                        }),
                    ),
                ]),
            );
            let mut spans = vec![Span::styled(
                format!(
                    "pipelines {} · streams {} · nonzero {}",
                    run.pipelines.len(),
                    run.streams.len(),
                    run.nonzero_stages
                ),
                Style::default().fg(if run.nonzero_stages > 0 {
                    ACCENT
                } else {
                    MUTED
                }),
            )];
            if run.incomplete {
                spans.push(Span::styled(" · incomplete", Style::default().fg(ACCENT)));
            }
            if run.omitted_pipelines > 0 || run.omitted_stages > 0 {
                spans.push(Span::styled(
                    format!(
                        " · omitted {}/{}",
                        run.omitted_pipelines, run.omitted_stages
                    ),
                    Style::default().fg(MUTED),
                ));
            }
            if run.untapped_edges > 0 {
                spans.push(Span::styled(
                    format!(" · untapped {}", run.untapped_edges),
                    Style::default().fg(ACCENT),
                ));
            }
            if run.evicted_streams > 0 {
                spans.push(Span::styled(
                    format!(" · evicted {}", run.evicted_streams),
                    Style::default().fg(ACCENT),
                ));
            }
            let stream_rows: Vec<Line<'static>> = run
                .streams
                .iter()
                .enumerate()
                .map(|(index, stream)| stream_row(stream, index == self.selected_stream))
                .collect();
            let list_height = run.streams.len().min(5).min(usize::from(area.height / 3));
            (
                Line::from(spans),
                pipeline_rows(run),
                stream_rows,
                list_height,
            )
        };
        draw_line(frame, area, summary);
        for line in pipelines {
            draw_line(frame, area, line);
        }
        if let Some(error) = self
            .runs
            .get(self.selected_run)
            .and_then(|run| run.capture_error.clone())
        {
            draw_line(
                frame,
                area,
                Line::styled(
                    format!("capture error: {}", display_text(&error)),
                    Style::default().fg(REMOVED),
                ),
            );
        }
        if stream_rows.is_empty() {
            draw_line(
                frame,
                area,
                Line::styled("No pipe streams in this run.", Style::default().fg(MUTED)),
            );
        } else if list_height > 0 {
            let start = self
                .selected_stream
                .saturating_add(1)
                .saturating_sub(list_height);
            for index in start..start + list_height {
                let Some(row) = stream_rows.get(index) else {
                    break;
                };
                let selected = index == self.selected_stream;
                let style = if selected {
                    Style::default().bg(ASK_BG).add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                let target = take_line(area);
                frame.render_widget(
                    Line::styled(format!("{}", if selected { "›" } else { " " }), style),
                    target,
                );
                frame.render_widget(
                    row,
                    Rect::new(
                        target.x.saturating_add(2).min(target.right()),
                        target.y,
                        target.width.saturating_sub(2),
                        target.height,
                    ),
                );
            }
        }
        self.draw_divider(frame, area);
        self.draw_body(frame, *area);
    }

    fn draw_divider(&mut self, frame: &mut Frame, area: &mut Rect) {
        let (retained_start, retained_end, total_bytes, open) = if self.following {
            (
                self.live.retained_start,
                self.live.retained_end,
                self.live.total_bytes,
                self.live.open,
            )
        } else {
            (
                self.body.retained_start,
                self.body.retained_end,
                self.body.total_bytes,
                self.body.open,
            )
        };
        let mut spans = vec![Span::styled(
            format!(
                "output · kept {retained_start}..{retained_end} / {total_bytes} bytes · {}",
                if self.following { "follow" } else { "frozen" }
            ),
            Style::default().fg(if self.following { ADDED } else { MUTED }),
        )];
        if !open {
            spans.push(Span::styled(" · closed", Style::default().fg(MUTED)));
        }
        if self.body.trimmed {
            spans.push(Span::styled(
                " · earlier display trimmed",
                Style::default().fg(MUTED),
            ));
        }
        if retained_start > 0 {
            spans.push(Span::styled(
                " · earlier output evicted",
                Style::default().fg(ACCENT),
            ));
        }
        if !self.following {
            if self.new_bytes > 0 {
                spans.push(Span::styled(
                    format!(" · new +{} bytes", self.new_bytes),
                    Style::default().fg(ACCENT),
                ));
            }
            if self.evicted {
                spans.push(Span::styled(
                    " · cached bytes overwritten",
                    Style::default().fg(ACCENT),
                ));
            }
        }
        draw_line(frame, area, Line::from(spans));
    }

    fn draw_body(&mut self, frame: &mut Frame, mut area: Rect) {
        self.body_height = usize::from(area.height).max(1);
        if area.is_empty() {
            return;
        }
        if self.body.text.is_empty() {
            let message = if self.awaiting {
                "Loading stream output…"
            } else if self.live.open || self.body.open {
                "Waiting for output…"
            } else {
                "No output captured."
            };
            draw_line(
                frame,
                &mut area,
                Line::styled(message, Style::default().fg(MUTED)),
            );
            return;
        }
        let height = usize::from(area.height);
        let tail = self.tail_start(area.width, height);
        let start = if self.following {
            tail
        } else {
            self.scroll.min(tail)
        };
        self.scroll = start;
        let mut index = start;
        let mut rows = 0;
        while index < self.line_starts.len() && rows < height {
            let text = self.line_at(index).to_owned();
            for row in wrap_block(Line::raw(text), area.width, "") {
                if rows >= height {
                    break;
                }
                frame.render_widget(row, take_line(&mut area));
                rows += 1;
            }
            index += 1;
        }
    }

    fn tail_start(&self, width: u16, height: usize) -> usize {
        if height == 0 || self.line_starts.is_empty() {
            return 0;
        }
        let mut rows = 0;
        for index in (0..self.line_starts.len()).rev() {
            rows += wrap_block(Line::raw(self.line_at(index).to_owned()), width, "")
                .len()
                .max(1);
            if rows >= height {
                return index;
            }
        }
        0
    }

    fn line_at(&self, index: usize) -> &str {
        let start = self.line_starts[index];
        let end = self
            .line_starts
            .get(index + 1)
            .map_or(self.body.text.len(), |next| next.saturating_sub(1));
        self.body.text.get(start..end).unwrap_or("")
    }

    fn reset_body(&mut self) {
        self.body = Body::default();
        self.line_starts.clear();
        self.line_starts.push(0);
        self.scroll = 0;
        self.following = true;
        self.awaiting = true;
        self.reload_frozen = false;
        self.new_bytes = 0;
        self.evicted = false;
    }

    fn freeze(&mut self) {
        self.following = false;
    }

    fn select_run(&mut self, index: usize) -> bool {
        if index == self.selected_run || index >= self.runs.len() {
            return false;
        }
        let id = self.runs[index].run_id.clone();
        self.selected_run = index;
        self.selected_stream = 0;
        self.explicit_run = Some(id);
        self.unavailable = false;
        self.error = None;
        self.reset_body();
        true
    }

    /// 从“指定 run 不可用”回到当前最近 run，仅在本地面板内切换。
    fn rebind_current_run(&mut self) -> bool {
        if self.runs.is_empty() {
            return false;
        }
        let id = self.runs[self.selected_run.min(self.runs.len() - 1)]
            .run_id
            .clone();
        self.selected_stream = 0;
        self.explicit_run = Some(id);
        self.unavailable = false;
        self.error = None;
        self.reset_body();
        true
    }

    fn select_stream(&mut self, index: usize) -> bool {
        if index == self.selected_stream || index >= self.streams().len() {
            return false;
        }
        self.selected_stream = index;
        self.error = None;
        self.reset_body();
        true
    }

    fn select_run_action(&self) -> Action {
        match self.selected_run_id() {
            Some(run) => Action::SelectRun {
                run: run.to_owned(),
            },
            None => Action::None,
        }
    }

    fn select_stream_action(&self) -> Action {
        match (self.selected_run_id(), self.current_stream()) {
            (Some(run), Some(stream)) => Action::SelectStream {
                run: run.to_owned(),
                stream: stream.id,
            },
            _ => Action::None,
        }
    }

    fn follow_action(&self) -> Action {
        match (self.selected_run_id(), self.current_stream()) {
            (Some(run), Some(stream)) => Action::Follow {
                run: run.to_owned(),
                stream: stream.id,
            },
            _ => Action::Refresh { full: true },
        }
    }

    fn reindex(&mut self) {
        self.line_starts.clear();
        self.line_starts.push(0);
        for (index, _) in self.body.text.match_indices('\n') {
            self.line_starts.push(index.saturating_add(1));
        }
    }

    fn trim_body(&mut self) {
        if self.body.text.len() <= MAX_BODY_BYTES {
            return;
        }
        let mut cut = self.body.text.len() - MAX_BODY_BYTES;
        while cut < self.body.text.len() && !self.body.text.is_char_boundary(cut) {
            cut += 1;
        }
        self.body.text.drain(..cut);
        self.body.trimmed = true;
    }

    fn streams(&self) -> &[StreamView] {
        self.current_run()
            .map(|run| run.streams.as_slice())
            .unwrap_or(&[])
    }

    fn current_run(&self) -> Option<&RunView> {
        self.runs.get(self.selected_run)
    }

    fn current_stream(&self) -> Option<&StreamView> {
        self.streams().get(self.selected_stream)
    }

    fn selected_run_id(&self) -> Option<&str> {
        self.current_run().map(|run| run.run_id.as_str())
    }

    #[cfg(test)]
    pub(super) fn runs(&self) -> &[RunView] {
        &self.runs
    }

    #[cfg(test)]
    pub(super) fn selected_run_id_for_test(&self) -> Option<&str> {
        self.selected_run_id()
    }

    #[cfg(test)]
    pub(super) fn body_text(&self) -> &str {
        &self.body.text
    }

    #[cfg(test)]
    pub(super) fn following(&self) -> bool {
        self.following
    }

    #[cfg(test)]
    pub(super) fn scroll(&self) -> usize {
        self.scroll
    }

    #[cfg(test)]
    pub(super) fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
}

fn run_state(run: &RunView) -> String {
    match run.state.as_str() {
        "running" => "running".to_owned(),
        "interrupted" => "interrupted".to_owned(),
        _ => run
            .exit_code
            .map_or_else(|| "exited".to_owned(), |code| format!("exited {code}")),
    }
}

fn pipeline_rows(run: &RunView) -> Vec<Line<'static>> {
    let mut rows = Vec::new();
    for pipeline in run.pipelines.iter().take(3) {
        let mut spans = vec![Span::styled(
            match pipeline.parent {
                Some(parent) => format!("p{}←{} {}s ", pipeline.id, parent, pipeline.stage_count),
                None => format!("p{} {}s ", pipeline.id, pipeline.stage_count),
            },
            Style::default().fg(MUTED),
        )];
        for (index, stage) in pipeline.stages.iter().take(4).enumerate() {
            if index > 0 {
                spans.push(Span::styled(" → ", Style::default().fg(MUTED)));
            }
            let detail = match (stage.status, stage.signal) {
                (_, Some(signal)) => format!("signal {signal}"),
                (Some(status), None) => status.to_string(),
                (None, None) => stage.state.clone(),
            };
            let nonzero = stage.status.is_some_and(|status| status != 0) || stage.signal.is_some();
            spans.push(Span::styled(
                format!("{}={detail}", display_text(&stage.command)),
                Style::default().fg(if nonzero { ACCENT } else { ACTIVITY }),
            ));
        }
        if pipeline.stages.len() > 4 {
            spans.push(Span::styled(
                format!(" +{}", pipeline.stages.len() - 4),
                Style::default().fg(MUTED),
            ));
        }
        let status = pipeline
            .status
            .map_or_else(|| "?".to_owned(), |status| status.to_string());
        spans.push(Span::styled(
            format!(
                "  status {status}{}{}",
                if pipeline.pipefail { " pipefail" } else { "" },
                if pipeline.bang { " !" } else { "" }
            ),
            Style::default().fg(if pipeline.status.is_some_and(|status| status != 0) {
                ACCENT
            } else {
                MUTED
            }),
        ));
        rows.push(Line::from(spans));
    }
    if run.pipelines.len() > 3 {
        rows.push(Line::styled(
            format!("+{} more pipelines", run.pipelines.len() - 3),
            Style::default().fg(MUTED),
        ));
    }
    rows
}

fn stream_row(stream: &StreamView, selected: bool) -> Line<'static> {
    let mut spans = vec![Span::styled(
        format!(
            "s{} p{} {}→{} ",
            stream.id, stream.pipeline, stream.producer_stage, stream.consumer_stage
        ),
        Style::default().fg(MUTED),
    )];
    spans.push(Span::styled(
        format!(
            "{} → {}",
            display_text(&stream.producer),
            display_text(&stream.consumer)
        ),
        Style::default().fg(if selected { ACTIVITY } else { MUTED }),
    ));
    spans.push(Span::styled(
        if stream.open { "  open" } else { "  closed" },
        Style::default().fg(if stream.open { ADDED } else { MUTED }),
    ));
    spans.push(Span::styled(
        format!(
            "  kept {}..{} / {}  rev {}",
            stream.retained_start, stream.retained_end, stream.total_bytes, stream.revision
        ),
        Style::default().fg(MUTED),
    ));
    if let Some(reason) = &stream.close_reason {
        spans.push(Span::styled(
            format!("  {}", display_text(reason)),
            Style::default().fg(MUTED),
        ));
    }
    Line::from(spans)
}

fn take_line(area: &mut Rect) -> Rect {
    let row = Rect::new(area.x, area.y, area.width, area.height.min(1));
    area.y = area.y.saturating_add(row.height);
    area.height = area.height.saturating_sub(row.height);
    row
}

fn draw_line(frame: &mut Frame, area: &mut Rect, line: Line<'_>) {
    if !area.is_empty() {
        frame.render_widget(line, take_line(area));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    use ratatui::{Terminal, backend::TestBackend, buffer::Buffer};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn stream(id: u64, open: bool) -> StreamView {
        StreamView {
            id,
            pipeline: 1,
            producer_stage: 1,
            consumer_stage: 2,
            producer: "printf".into(),
            consumer: "tail".into(),
            open,
            total_bytes: 21,
            retained_start: 0,
            retained_end: 21,
            revision: 3,
            close_reason: None,
        }
    }

    fn run(id: char, streams: Vec<StreamView>) -> RunView {
        RunView {
            run_id: id.to_string().repeat(32),
            owner: "root".into(),
            state: "exited".into(),
            exit_code: Some(0),
            revision: 2,
            pipelines: vec![PipelineView {
                id: 1,
                parent: None,
                stage_count: 2,
                bang: false,
                pipefail: false,
                status: Some(0),
                stages: vec![
                    StageView {
                        command: "printf".into(),
                        state: "exited".into(),
                        status: Some(0),
                        signal: None,
                    },
                    StageView {
                        command: "tail".into(),
                        state: "exited".into(),
                        status: Some(0),
                        signal: None,
                    },
                ],
            }],
            streams,
            nonzero_stages: 0,
            omitted_pipelines: 0,
            omitted_stages: 0,
            untapped_edges: 0,
            evicted_streams: 0,
            incomplete: false,
            capture_error: None,
        }
    }

    fn panel() -> Panel {
        let mut panel = Panel::new(None);
        panel.set_runs(
            vec![
                run('a', vec![stream(1, true)]),
                run('b', vec![stream(2, false), stream(3, false)]),
            ],
            Some("a".repeat(32)),
            Some(1),
        );
        panel.apply_body(BodyUpdate {
            run: "a".repeat(32),
            stream: 1,
            data: (1..=20).map(|line| format!("line-{line}\n")).collect(),
            start: 0,
            end: 21,
            reset: true,
            retained_start: 0,
            retained_end: 21,
            total_bytes: 21,
            open: true,
            revision: 3,
            prefix_evicted: false,
        });
        panel
    }

    fn draw(panel: &mut Panel, width: u16, height: u16) -> (Buffer, String) {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| panel.draw(frame, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let text = (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        (buffer, text)
    }

    #[test]
    fn draws_runs_pipelines_streams_and_body() {
        let mut panel = panel();
        let (_, text) = draw(&mut panel, 120, 30);
        assert!(text.contains("Pipe streams"));
        assert!(text.contains(&"a".repeat(32)));
        assert!(text.contains("printf=0"));
        assert!(text.contains("tail=0"));
        assert!(text.contains("s1 p1 1→2"));
        assert!(text.contains("open"));
        assert!(text.contains("line-20"), "{text}");
        assert!(panel.following());
    }

    #[test]
    fn arrows_switch_run_and_stream_with_commands() {
        let mut panel = panel();
        assert!(matches!(panel.key(key(KeyCode::Left)), Action::None));
        let Action::SelectRun { run } = panel.key(key(KeyCode::Right)) else {
            panic!("right arrow selects the next run");
        };
        let expected = "b".repeat(32);
        assert_eq!(run, expected);
        assert_eq!(panel.selected_run_id_for_test(), Some(expected.as_str()));
        assert!(panel.body_text().is_empty());
        let Action::SelectStream { stream, .. } = panel.key(key(KeyCode::Down)) else {
            panic!("down arrow selects the next stream");
        };
        assert_eq!(stream, 3);
        assert!(matches!(panel.key(key(KeyCode::Right)), Action::None));
        assert!(matches!(panel.key(key(KeyCode::Down)), Action::None));
    }

    #[test]
    fn scrolling_freezes_and_end_or_follow_resumes_tail() {
        let mut panel = panel();
        draw(&mut panel, 80, 24);
        assert!(panel.following());
        panel.key(key(KeyCode::PageUp));
        assert!(!panel.following());
        let frozen = panel.scroll();
        assert!(frozen < 19);
        assert!(matches!(
            panel.key(key(KeyCode::End)),
            Action::Follow { .. }
        ));
        assert!(panel.following());
        draw(&mut panel, 80, 24);
        assert!(panel.scroll() > frozen);
        panel.key(key(KeyCode::Char('f')));
        assert!(!panel.following());
        assert!(matches!(
            panel.key(key(KeyCode::Char('f'))),
            Action::Follow { .. }
        ));
        assert!(panel.following());
        panel.wheel(true, 3);
        assert!(!panel.following());
        panel.key(key(KeyCode::Home));
        assert_eq!(panel.scroll(), 0);
    }

    #[test]
    fn frozen_snapshot_reports_new_bytes_and_eviction_without_rewriting_body() {
        let mut panel = panel();
        let before = panel.body_text().to_owned();
        draw(&mut panel, 80, 24);
        panel.key(key(KeyCode::PageUp));
        assert!(!panel.following());
        panel.apply_body(BodyUpdate {
            run: "a".repeat(32),
            stream: 1,
            data: "tail-21\n".into(),
            start: 21,
            end: 29,
            reset: false,
            retained_start: 0,
            retained_end: 29,
            total_bytes: 29,
            open: true,
            revision: 4,
            prefix_evicted: false,
        });
        assert_eq!(panel.body_text(), before);
        assert_eq!(panel.new_bytes, 8);
        panel.apply_body(BodyUpdate {
            run: "a".repeat(32),
            stream: 1,
            data: "fresh\n".into(),
            start: 30,
            end: 36,
            reset: true,
            retained_start: 30,
            retained_end: 36,
            total_bytes: 36,
            open: true,
            revision: 5,
            prefix_evicted: true,
        });
        assert_eq!(panel.body_text(), before);
        assert!(panel.evicted);
        let (_, text) = draw(&mut panel, 100, 30);
        assert!(text.contains("new +15 bytes"));
        assert!(text.contains("cached bytes overwritten"));
    }

    #[test]
    fn following_applies_reset_and_incremental_deltas() {
        let mut panel = panel();
        panel.apply_body(BodyUpdate {
            run: "a".repeat(32),
            stream: 1,
            data: "more\n".into(),
            start: 21,
            end: 26,
            reset: false,
            retained_start: 0,
            retained_end: 26,
            total_bytes: 26,
            open: true,
            revision: 4,
            prefix_evicted: false,
        });
        assert!(panel.body_text().ends_with("line-20\nmore\n"));
        panel.apply_body(BodyUpdate {
            run: "a".repeat(32),
            stream: 1,
            data: "rebuilt\n".into(),
            start: 10,
            end: 18,
            reset: true,
            retained_start: 10,
            retained_end: 18,
            total_bytes: 18,
            open: false,
            revision: 5,
            prefix_evicted: true,
        });
        assert_eq!(panel.body_text(), "rebuilt\n");
        let (_, text) = draw(&mut panel, 100, 30);
        assert!(text.contains("earlier output evicted"));
    }

    #[test]
    fn stale_run_or_stream_body_updates_are_ignored() {
        let mut panel = panel();
        let before = panel.body_text().to_owned();
        assert!(!panel.apply_body(BodyUpdate {
            run: "b".repeat(32),
            stream: 1,
            data: "wrong\n".into(),
            start: 21,
            end: 27,
            reset: true,
            retained_start: 0,
            retained_end: 27,
            total_bytes: 27,
            open: true,
            revision: 9,
            prefix_evicted: false,
        }));
        assert!(!panel.apply_body(BodyUpdate {
            run: "a".repeat(32),
            stream: 2,
            data: "wrong\n".into(),
            start: 21,
            end: 27,
            reset: true,
            retained_start: 0,
            retained_end: 27,
            total_bytes: 27,
            open: true,
            revision: 9,
            prefix_evicted: false,
        }));
        assert_eq!(panel.body_text(), before);
    }

    #[test]
    fn body_is_bounded_and_resize_preserves_anchor() {
        let mut panel = Panel::new(None);
        panel.set_runs(
            vec![run('a', vec![stream(1, true)])],
            Some("a".repeat(32)),
            Some(1),
        );
        let data = vec![b'x'; MAX_BODY_BYTES + 4096];
        panel.apply_body(BodyUpdate {
            run: "a".repeat(32),
            stream: 1,
            data: String::from_utf8(data).expect("ascii"),
            start: 0,
            end: (MAX_BODY_BYTES + 4096) as u64,
            reset: true,
            retained_start: 0,
            retained_end: (MAX_BODY_BYTES + 4096) as u64,
            total_bytes: (MAX_BODY_BYTES + 4096) as u64,
            open: true,
            revision: 1,
            prefix_evicted: false,
        });
        assert!(panel.body.text.len() <= MAX_BODY_BYTES);
        assert!(panel.body.trimmed);
        panel.key(key(KeyCode::PageUp));
        draw(&mut panel, 80, 24);
        let anchor = panel.scroll();
        draw(&mut panel, 60, 18);
        assert_eq!(panel.scroll, anchor);
        assert!(!panel.following());
    }

    #[test]
    fn home_on_trimmed_snapshot_reloads_retained_prefix_while_staying_frozen() {
        let mut panel = panel();
        draw(&mut panel, 80, 24);
        panel.body.trimmed = true;
        assert!(matches!(
            panel.key(key(KeyCode::Home)),
            Action::Refresh { full: true }
        ));
        assert!(!panel.following());
        panel.apply_body(BodyUpdate {
            run: "a".repeat(32),
            stream: 1,
            data: "retained-start\n".into(),
            start: 0,
            end: 15,
            reset: true,
            retained_start: 0,
            retained_end: 15,
            total_bytes: 15,
            open: false,
            revision: 6,
            prefix_evicted: false,
        });
        assert_eq!(panel.body_text(), "retained-start\n");
        assert!(!panel.body.trimmed);
        assert!(!panel.following());
        assert_eq!(panel.scroll, 0);
    }

    #[test]
    fn explicit_unavailable_and_errors_are_visible() {
        let mut panel = Panel::new(Some("c".repeat(32)));
        panel.set_runs(Vec::new(), None, None);
        panel.set_error(
            "Temporary pipe output is unavailable (evicted or from another session).".into(),
        );
        let (_, text) = draw(&mut panel, 100, 20);
        assert!(text.contains("Temporary pipe output is unavailable"));
        assert!(text.contains(&"c".repeat(32)));
        assert!(panel.error().is_some());
    }

    #[test]
    fn escape_only_closes() {
        let mut panel = panel();
        assert!(matches!(panel.key(key(KeyCode::Esc)), Action::Close));
        assert!(matches!(
            panel.key(key(KeyCode::Char('r'))),
            Action::Refresh { full: true }
        ));
        panel.key(key(KeyCode::PageUp));
        assert!(matches!(
            panel.key(key(KeyCode::Char('r'))),
            Action::Refresh { full: false }
        ));
    }
}
