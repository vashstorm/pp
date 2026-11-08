//! 正文排版服务。队列按消息合并，UI 只读取已经完成的不可变结果。
use super::transcript::{EntryLayout, LayoutContent, LayoutLine};
use ratatui::{
    style::Style,
    text::{Line, Span},
};
use std::{
    cell::RefCell,
    collections::{BTreeMap, VecDeque},
    sync::{
        Arc, Condvar, Mutex, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
};

thread_local! { static CURRENT: RefCell<Option<Arc<Shared>>> = const { RefCell::new(None) }; }
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Default)]
struct Pending {
    order: VecDeque<u64>,
    jobs: BTreeMap<u64, Job>,
}
impl Pending {
    fn is_empty(&self) -> bool {
        self.jobs.is_empty()
    }
    fn pop_front(&mut self) -> Option<Job> {
        while let Some(id) = self.order.pop_front() {
            if let Some(job) = self.jobs.remove(&id) {
                return Some(job);
            }
        }
        None
    }
    fn push(&mut self, job: Job) {
        if self.jobs.is_empty() {
            self.order.clear();
        }
        if !self.jobs.contains_key(&job.id) {
            self.order.push_front(job.id);
        }
        self.jobs.insert(job.id, job);
    }
}
struct Shared {
    jobs: Mutex<Pending>,
    wake: Condvar,
    stopped: AtomicBool,
    changed: AtomicBool,
    #[cfg(test)]
    gate: Mutex<Option<TestGate>>,
}
#[cfg(test)]
struct TestGate {
    entered: std::sync::mpsc::Sender<()>,
    release: std::sync::mpsc::Receiver<()>,
}

#[cfg(test)]
pub(super) struct TestPause {
    entered: std::sync::mpsc::Receiver<()>,
    release: std::sync::mpsc::Sender<()>,
}

#[cfg(test)]
impl TestPause {
    pub fn wait(&self) {
        self.entered
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("后台排版应进入暂停点");
    }
}

#[cfg(test)]
impl Drop for TestPause {
    fn drop(&mut self) {
        let _ = self.release.send(());
    }
}
struct Job {
    id: u64,
    epoch: u64,
    version: u64,
    text: Arc<str>,
    width: u16,
    muted: bool,
    streaming: bool,
    slot: Weak<Mutex<Option<Rendered>>>,
}
#[derive(Clone)]
struct Rendered {
    epoch: u64,
    version: u64,
    width: u16,
    source: Arc<str>,
    layout: Arc<EntryLayout>,
    ends_with_table: bool,
    table_tail_start: Option<usize>,
}

struct CachedDocument {
    id: u64,
    epoch: u64,
    source: Arc<str>,
    streaming: bool,
    markdown: super::markdown::MarkdownDocument,
}

pub(super) struct Worker {
    shared: Arc<Shared>,
    join: Option<JoinHandle<()>>,
}
impl Worker {
    pub fn start() -> std::io::Result<Self> {
        let shared = Arc::new(Shared {
            jobs: Mutex::new(Pending::default()),
            wake: Condvar::new(),
            stopped: AtomicBool::new(false),
            changed: AtomicBool::new(false),
            #[cfg(test)]
            gate: Mutex::new(None),
        });
        let state = shared.clone();
        let join = thread::Builder::new()
            .name("pp-layout".into())
            .spawn(move || {
                let mut documents: VecDeque<CachedDocument> = VecDeque::new();
                loop {
                    let job = {
                        let mut jobs = state.jobs.lock().unwrap_or_else(|e| e.into_inner());
                        while jobs.is_empty() && !state.stopped.load(Ordering::Acquire) {
                            jobs = state.wake.wait(jobs).unwrap_or_else(|e| e.into_inner());
                        }
                        if state.stopped.load(Ordering::Acquire) {
                            break;
                        }
                        jobs.pop_front().expect("排版任务已经就绪")
                    };
                    let Some(slot) = job.slot.upgrade() else {
                        continue;
                    };
                    #[cfg(test)]
                    {
                        let gate = state.gate.lock().unwrap_or_else(|e| e.into_inner()).take();
                        if let Some(gate) = gate {
                            let _ = gate.entered.send(());
                            // 暂停点不能永久等待：测试漏 drop(TestPause) 时让后台继续，
                            // 由断言报错，而不是把 drop(Worker) 的 join 拖死。
                            let _ = gate.release.recv_timeout(std::time::Duration::from_secs(5));
                        }
                    }
                    super::markdown::highlight_context(job.id, job.epoch);
                    let render = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let index = documents.iter().position(|document| document.id == job.id);
                        let mut document = index
                            .and_then(|i| documents.remove(i))
                            .filter(|document| document.epoch == job.epoch)
                            .unwrap_or_else(|| CachedDocument {
                                id: job.id,
                                epoch: job.epoch,
                                source: job.text.clone(),
                                streaming: job.streaming,
                                markdown: super::markdown::parse_streaming(
                                    &job.text,
                                    job.streaming,
                                ),
                            });
                        if !Arc::ptr_eq(&document.source, &job.text)
                            || document.streaming != job.streaming
                        {
                            document.markdown.update(&job.text, job.streaming);
                            document.source = job.text.clone();
                            document.streaming = job.streaming;
                        }
                        let mut lines = document.markdown.layout_cached(job.width);
                        if job.muted {
                            for line in &mut lines {
                                *line = std::mem::take(line).style(
                                    Style::default().fg(ratatui::style::Color::Rgb(110, 117, 129)),
                                );
                            }
                        }
                        let ends_with_table = document.markdown.ends_with_table();
                        let table_tail_start = document.markdown.table_tail_start;
                        documents.push_back(document);
                        while documents.len() > 64
                            || documents.iter().map(|d| d.source.len()).sum::<usize>()
                                > 8 * 1024 * 1024
                        {
                            documents.pop_front();
                        }
                        (lines, ends_with_table, table_tail_start)
                    }));
                    let (lines, ends_with_table, table_tail_start) = render
                        .unwrap_or_else(|_| (plain(&job.text, job.width, job.muted), false, None));
                    // 已完成的追加版本仍可用于展示；最新版本资格由 Ticket 单独判断。
                    let mut result = slot.lock().unwrap_or_else(|e| e.into_inner());
                    if result
                        .as_ref()
                        .is_none_or(|r| (job.epoch, job.version) >= (r.epoch, r.version))
                    {
                        *result = Some(Rendered {
                            epoch: job.epoch,
                            version: job.version,
                            width: job.width,
                            source: job.text,
                            layout: Arc::new(prepared(lines, job.width)),
                            ends_with_table,
                            table_tail_start,
                        });
                    }
                    if job.slot.strong_count() == 1 {
                        super::markdown::release_highlight_context(job.id);
                    }
                    state.changed.store(true, Ordering::Release);
                }
            })?;
        CURRENT.with(|current| *current.borrow_mut() = Some(shared.clone()));
        Ok(Self {
            shared,
            join: Some(join),
        })
    }
    pub fn changed(&self) -> bool {
        self.shared.changed.swap(false, Ordering::AcqRel)
    }
    #[cfg(test)]
    pub fn pause(&self) -> TestPause {
        let (entered, waiting) = std::sync::mpsc::channel();
        let (release, gate) = std::sync::mpsc::channel();
        *self.shared.gate.lock().unwrap() = Some(TestGate {
            entered,
            release: gate,
        });
        TestPause {
            entered: waiting,
            release,
        }
    }
    pub fn pending(&self) -> bool {
        !self
            .shared
            .jobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        CURRENT.with(|current| *current.borrow_mut() = None);
        // Condvar::wait 注册 waiter 与释放锁是原子的：只有持锁发布 stopped，
        // 才能保证唤醒不会落在 worker 通过 predicate 检查与注册之间而丢失。
        // 无锁 notify 会让 join 永久阻塞。
        {
            let jobs = self.shared.jobs.lock().unwrap_or_else(|e| e.into_inner());
            self.shared.stopped.store(true, Ordering::Release);
            drop(jobs);
        }
        self.shared.wake.notify_all();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

pub(super) fn enabled() -> bool {
    CURRENT.with(|c| c.borrow().is_some())
}

pub(super) struct Ticket {
    id: u64,
    epoch: u64,
    version: u64,
    source: Arc<str>,
    revision: u64,
    snapshot_revision: u64,
    fallback: Option<Arc<EntryLayout>>,
    displayed: Option<Rendered>,
    width: u16,
    streaming: bool,
    slot: Arc<Mutex<Option<Rendered>>>,
    pub exact: bool,
}
impl Default for Ticket {
    fn default() -> Self {
        Self {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            epoch: 0,
            version: 0,
            source: Arc::from(""),
            revision: 1,
            snapshot_revision: 0,
            fallback: None,
            displayed: None,
            width: 0,
            streaming: false,
            slot: Arc::new(Mutex::new(None)),
            exact: false,
        }
    }
}
impl Drop for Ticket {
    fn drop(&mut self) {
        CURRENT.with(|current| {
            if let Some(shared) = current.borrow().as_ref() {
                shared
                    .jobs
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .jobs
                    .remove(&self.id);
            }
        });
        super::markdown::release_highlight_context(self.id);
    }
}
impl Ticket {
    pub fn invalidate(&mut self, replaced: bool) {
        self.revision += 1;
        self.exact = false;
        if replaced {
            self.epoch += 1;
            self.displayed = None;
        }
    }
    pub fn layout(
        &mut self,
        text: &str,
        width: u16,
        muted: bool,
        streaming: bool,
    ) -> Arc<EntryLayout> {
        let mode_changed = self.streaming != streaming;
        self.streaming = streaming;
        // 结束时把仍在预览的末行排入表格。正文已经完整排版时直接复用。
        let needs_layout_for_mode = mode_changed
            && (streaming
                || !self.exact
                || self
                    .displayed
                    .as_ref()
                    .is_some_and(|result| result.table_tail_start.is_some()));
        if self.revision != self.snapshot_revision
            || width != self.width
            || self.version == 0
            || needs_layout_for_mode
        {
            self.version += 1;
            if self.revision != self.snapshot_revision {
                self.source = Arc::from(text);
                self.snapshot_revision = self.revision;
            }
            self.fallback = None;
            if self.width != width {
                self.displayed = None;
            }
            self.width = width;
            self.exact = false;
            CURRENT.with(|current| {
                if let Some(shared) = current.borrow().as_ref() {
                    let mut jobs = shared.jobs.lock().unwrap_or_else(|e| e.into_inner());
                    jobs.push(Job {
                        id: self.id,
                        epoch: self.epoch,
                        version: self.version,
                        text: self.source.clone(),
                        width,
                        muted,
                        streaming,
                        slot: Arc::downgrade(&self.slot),
                    });
                    shared.wake.notify_one();
                }
            });
        }
        let result = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(result) = result
            .as_ref()
            .filter(|r| r.epoch == self.epoch && r.width == width)
            && self
                .displayed
                .as_ref()
                .is_none_or(|old| result.version >= old.version)
        {
            if self
                .displayed
                .as_ref()
                .is_none_or(|old| old.version != result.version)
            {
                self.fallback = None;
            }
            self.displayed = Some(result.clone());
        }
        drop(result);
        if let Some(result) = &self.displayed {
            self.exact = result.version == self.version;
            if self.exact && result.table_tail_start.is_none() {
                return result.layout.clone();
            }
        }
        if let Some(layout) = &self.fallback {
            return layout.clone();
        }
        // 已排版前缀保持样式，新增尾部立即预览，不等待后台排版。
        // 预览只处理有限尾部，不能用于历史提交。
        let layout =
            match &self.displayed {
                Some(result)
                    if (result.ends_with_table || result.table_tail_start.is_some())
                        && text.starts_with(result.source.as_ref()) =>
                {
                    // 表格布局只包含已收齐的行，预览始终另外占一行。
                    let start = result.table_tail_start.unwrap_or(result.source.len());
                    match table_tail_preview(&text[start..], width, muted) {
                        Some(line) => {
                            let mut lines = rendered_lines(result);
                            lines.push(line);
                            Arc::new(prepared(lines, width))
                        }
                        None => result.layout.clone(),
                    }
                }
                Some(result) if text.starts_with(result.source.as_ref()) => Arc::new(
                    append_preview(result, &text[result.source.len()..], width, muted),
                ),
                _ => Arc::new(prepared(plain(preview_tail(text), width, muted), width)),
            };
        self.fallback = Some(layout.clone());
        layout
    }
}

fn preview_tail(text: &str) -> &str {
    let start = text.len().saturating_sub(8192);
    let start = (start..=text.len())
        .find(|&i| text.is_char_boundary(i))
        .unwrap_or(text.len());
    &text[start..]
}

pub(super) fn table_tail_preview(text: &str, width: u16, muted: bool) -> Option<Line<'static>> {
    let last = text.trim_end_matches(['\n', '\r']).rsplit('\n').next()?;
    (!last.is_empty())
        .then(|| super::markdown::table_tail_line(preview_tail(last), width, text_style(muted)))
}

fn rendered_lines(result: &Rendered) -> Vec<Line<'static>> {
    result
        .layout
        .lines
        .iter()
        .map(|line| match &line.content {
            LayoutContent::Styled(line) => line.clone(),
            _ => unreachable!("后台正文排版仅包含样式行"),
        })
        .collect()
}

fn append_preview(result: &Rendered, suffix: &str, width: u16, muted: bool) -> EntryLayout {
    let mut lines = rendered_lines(result);
    if suffix.is_empty() {
        return prepared(lines, width);
    }
    let tail = preview_tail(suffix);
    let mut line = if tail.len() == suffix.len() && !result.source.ends_with('\n') {
        lines.pop().unwrap_or_default()
    } else {
        Line::default()
    };
    if tail.len() != suffix.len() {
        line.spans.push(Span::raw("…\n"));
    }
    line.spans
        .push(Span::styled(tail.to_owned(), text_style(muted)));
    lines.extend(super::transcript::wrap_block(line, width, ""));
    prepared(lines, width)
}
fn prepared(lines: Vec<Line<'static>>, width: u16) -> EntryLayout {
    EntryLayout {
        width,
        height: lines.len(),
        lines: lines
            .into_iter()
            .map(|line| LayoutLine {
                content: LayoutContent::Styled(line),
                height: 1,
            })
            .collect(),
    }
}
fn text_style(muted: bool) -> Style {
    if muted {
        Style::default().fg(ratatui::style::Color::Rgb(110, 117, 129))
    } else {
        Style::default()
    }
}

fn plain(text: &str, width: u16, muted: bool) -> Vec<Line<'static>> {
    super::transcript::wrap_block(
        Line::from(Span::styled(
            pp_core::text::display_text(text).into_owned(),
            text_style(muted),
        )),
        width,
        "",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::Event;
    use pp_core::events::AgentEvent;
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn layout_worker_plain_preserves_lines_tabs_and_unicode() {
        let rows = plain("first\n\n中\t🙂\n", 80, false);
        assert_eq!(
            rows.iter().map(Line::to_string).collect::<Vec<_>>(),
            ["first", "", "中  🙂", ""]
        );
    }

    #[test]
    fn layout_worker_initial_tail_keeps_utf8_boundaries_and_empty_lines() {
        let worker = Worker::start().unwrap();
        let pause = worker.pause();
        let mut ticket = Ticket::default();
        let text = format!("{}🙂\n\nend", "中".repeat(3000));
        let preview = ticket.layout(&text, 8, false, false);
        pause.wait();
        let rows = preview
            .lines
            .iter()
            .map(|line| {
                let LayoutContent::Styled(line) = &line.content else {
                    panic!("应为预览行")
                };
                line.to_string()
            })
            .collect::<Vec<_>>();
        assert!(rows[0].starts_with('中'));
        assert!(!rows.join("").contains('\u{fffd}'));
        assert!(rows.join("").contains('🙂'));
        assert_eq!(&rows[rows.len() - 2..], &["", "end"]);
        assert!(!ticket.exact);
        drop(pause);
    }

    #[test]
    fn layout_worker_previews_latest_append_while_preserving_rendered_styles() {
        let worker = Worker::start().unwrap();
        let first = worker.pause();
        let mut ticket = Ticket::default();
        ticket.layout("**first**", 80, false, false);
        first.wait();
        let second = worker.pause();
        ticket.invalidate(false);
        ticket.layout("**first**\n\nsecond", 80, false, false);
        drop(first);
        second.wait();
        let intermediate = ticket.layout("**first**\n\nsecond", 80, false, false);
        assert!(!ticket.exact);
        let LayoutContent::Styled(line) = &intermediate.lines[0].content else {
            panic!("应为 Markdown 行");
        };
        assert_eq!(line.to_string(), "first");
        assert!(line.spans.iter().any(|span| {
            span.style
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD)
        }));
        assert_eq!(layout_text(&intermediate), "first\n\nsecond");
        drop(second);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            ticket.layout("**first**\n\nsecond", 80, false, false);
            if ticket.exact {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "应最终完成最新排版");
            std::thread::yield_now();
        }
        let third = worker.pause();
        ticket.invalidate(false);
        assert!(!ticket.exact, "追加后立即撤销最新版本资格");
        let preview = ticket.layout("**first**\n\nsecond\n\nthird", 80, false, false);
        third.wait();
        assert_eq!(layout_text(&preview), "first\n\nsecond\n\nthird");
        assert!(!ticket.exact);
        ticket.invalidate(false);
        let preview = ticket.layout("**first**\n\nsecond\n\nthird 中🙂", 80, false, false);
        assert_eq!(layout_text(&preview), "first\n\nsecond\n\nthird 中🙂");
        assert!(!ticket.exact);
        drop(third);
    }

    fn layout_text(layout: &EntryLayout) -> String {
        layout
            .lines
            .iter()
            .map(|line| match &line.content {
                LayoutContent::Styled(line) => line.to_string(),
                _ => panic!("应为样式行"),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn layout_worker_append_preview_wraps_unicode_tabs_and_newlines() {
        for (source, suffix, expected) in [
            ("**ab**", "中🙂", "ab中\n🙂"),
            ("**ab**", "\t中", "ab  \n中"),
            ("**ab**", "\n\n中\n", "ab\n\n中\n"),
            ("**ab**\n", "中", "ab\n中"),
        ] {
            let worker = Worker::start().unwrap();
            let pause = worker.pause();
            let mut ticket = Ticket::default();
            ticket.layout(source, 4, true, false);
            pause.wait();
            *ticket.slot.lock().unwrap() = Some(Rendered {
                epoch: ticket.epoch,
                version: ticket.version,
                width: 4,
                source: Arc::from(source),
                layout: Arc::new(prepared(vec![Line::raw("ab")], 4)),
                ends_with_table: false,
                table_tail_start: None,
            });
            ticket.layout(source, 4, true, false);
            ticket.invalidate(false);
            let preview = ticket.layout(&format!("{source}{suffix}"), 4, true, false);
            assert_eq!(layout_text(&preview), expected);
            assert!(!ticket.exact);
            assert!(Arc::ptr_eq(
                &preview,
                &ticket.layout(&format!("{source}{suffix}"), 4, true, false)
            ));
            drop(pause);
        }
    }

    #[test]
    fn layout_worker_append_preview_bounds_tail_without_losing_prefix() {
        let result = Rendered {
            epoch: 0,
            version: 1,
            width: 80,
            source: Arc::from("**prefix**"),
            layout: Arc::new(prepared(vec![Line::raw("prefix")], 80)),
            ends_with_table: false,
            table_tail_start: None,
        };
        let suffix = format!("{}🙂\n\nlatest", "中".repeat(4000));
        let preview = append_preview(&result, &suffix, 80, true);
        let text = layout_text(&preview);
        assert!(text.starts_with("prefix\n…\n"));
        assert!(text.ends_with("🙂\n\nlatest"));
        assert!(!text.contains('\u{fffd}'));
        assert!(text.len() < 9000);
        let LayoutContent::Styled(last) = &preview.lines.last().unwrap().content else {
            panic!("应为样式行");
        };
        assert_eq!(
            last.spans[0].style.fg,
            Some(ratatui::style::Color::Rgb(110, 117, 129))
        );
        assert_eq!(
            layout_text(&append_preview(&result, "", 80, true)),
            "prefix"
        );
    }

    #[test]
    fn layout_worker_blocked_render_does_not_block_input_or_commit_preview() {
        let worker = Worker::start().unwrap();
        let pause = worker.pause();
        let mut app = crate::tui::App::new(Err(anyhow::anyhow!("fixture"))).unwrap();
        app.conversation = app.conversation.fresh();
        app.state = crate::tui::State::Running;
        app.show_welcome = false;
        app.native_render = true;
        app.conversation.progress(AgentEvent::AssistantStarted);
        app.conversation.progress(AgentEvent::AssistantDelta {
            text: "```rust\nlet value = 1;\n```".into(),
            refusal: false,
        });
        app.conversation.progress(AgentEvent::AssistantFinished {
            text: "```rust\nlet value = 1;\n```".into(),
        });
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|frame| app.draw(frame, &Default::default()))
            .unwrap();
        assert!(app.conversation.render_pending());
        assert!(app.conversation.next_native_batch(100, 1, true).is_none());
        app.handle(Event::Paste("可继续输入🙂".into()), &mut false);
        assert_eq!(app.input.lines(), &["可继续输入🙂"]);
        drop(pause);
    }

    #[test]
    fn layout_worker_unchanged_revision_reuses_snapshot_fallback_and_job() {
        let worker = Worker::start().unwrap();
        let pause = worker.pause();
        let mut ticket = Ticket::default();
        let first = ticket.layout("正文🙂", 80, false, false);
        let source = ticket.source.clone();
        let version = ticket.version;
        let second = ticket.layout("正文🙂", 80, false, false);
        assert!(Arc::ptr_eq(&first, &second));
        assert!(Arc::ptr_eq(&source, &ticket.source));
        assert_eq!(version, ticket.version);
        ticket.invalidate(false);
        ticket.layout("正文🙂新增", 80, false, false);
        assert_eq!(ticket.version, version + 1);
        drop(pause);
    }

    #[test]
    fn layout_worker_rejects_replacement_and_width_mismatch() {
        let worker = Worker::start().unwrap();
        let pause = worker.pause();
        let mut ticket = Ticket::default();
        ticket.layout("old", 80, false, false);
        // 先确认后台排版已停在暂停点，再直接写 slot；否则下面的手动结果会与
        // 后台结果竞争，断言变成依赖调度顺序。
        pause.wait();
        assert!(
            ticket.slot.lock().unwrap().is_none(),
            "暂停点之后不应出现后台结果"
        );
        let old_epoch = ticket.epoch;
        ticket.invalidate(true);
        ticket.layout("new", 80, false, false);
        *ticket.slot.lock().unwrap() = Some(Rendered {
            epoch: old_epoch,
            version: u64::MAX,
            width: 80,
            source: Arc::from("old"),
            layout: Arc::new(prepared(vec![Line::raw("stale")], 80)),
            ends_with_table: false,
            table_tail_start: None,
        });
        let rows = ticket.layout("new", 80, false, false);
        let LayoutContent::Styled(line) = &rows.lines[0].content else {
            panic!("应为已排版行")
        };
        assert_eq!(line.to_string(), "new");
        assert!(!ticket.exact);
        *ticket.slot.lock().unwrap() = Some(Rendered {
            epoch: ticket.epoch,
            version: ticket.version,
            width: 80,
            source: Arc::from("new"),
            layout: Arc::new(prepared(vec![Line::raw("new")], 80)),
            ends_with_table: false,
            table_tail_start: None,
        });
        ticket.layout("new", 80, false, false);
        assert!(ticket.exact);
        ticket.layout("new", 40, false, false);
        assert!(!ticket.exact);
        drop(pause);
    }
}
