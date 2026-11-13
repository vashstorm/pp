//! 临时管道阶段浏览：只持有当前选中流的有界正文，读取与轮询在观察线程。
//!
//! 面板打开不进入 steering、不发送模型请求；Esc 只关闭面板，不改写输入草稿，
//! 也不触碰原生主屏历史。正文副本在关闭时随面板一起释放。

mod keys;
mod render;
mod worker;

#[cfg(test)]
use crossterm::event::{KeyCode, KeyEvent};
use pp_core::text::display_text;
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

#[cfg(test)]
mod fixtures {
    use super::*;
    use crossterm::event::KeyModifiers;
    use ratatui::{Terminal, backend::TestBackend, buffer::Buffer};

    pub(super) fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    pub(super) fn stream(id: u64, open: bool) -> StreamView {
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

    pub(super) fn run(id: char, streams: Vec<StreamView>) -> RunView {
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

    pub(super) fn panel() -> Panel {
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

    pub(super) fn draw(panel: &mut Panel, width: u16, height: u16) -> (Buffer, String) {
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
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

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
}
