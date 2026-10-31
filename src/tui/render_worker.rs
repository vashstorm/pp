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
    slot: Weak<Mutex<Option<Rendered>>>,
}
#[derive(Clone)]
struct Rendered {
    epoch: u64,
    version: u64,
    width: u16,
    layout: Arc<EntryLayout>,
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
                let mut documents = VecDeque::new();
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
                            let _ = gate.release.recv();
                        }
                    }
                    super::markdown::highlight_context(job.id, job.epoch);
                    let lines = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let index = documents.iter().position(
                            |(id, _epoch, _text, _): &(
                                u64,
                                u64,
                                Arc<str>,
                                super::markdown::MarkdownDocument,
                            )| { *id == job.id },
                        );
                        let mut document =
                            index.and_then(|i| documents.remove(i)).unwrap_or_else(|| {
                                (
                                    job.id,
                                    job.epoch,
                                    job.text.clone(),
                                    super::markdown::parse(&job.text),
                                )
                            });
                        if document.1 != job.epoch {
                            document.3 = super::markdown::parse(&job.text);
                            document.1 = job.epoch;
                            document.2 = job.text.clone();
                        } else if !Arc::ptr_eq(&document.2, &job.text) {
                            document.3.update(&job.text);
                            document.2 = job.text.clone();
                        }
                        let mut lines = document.3.layout_cached(job.width);
                        if job.muted {
                            for line in &mut lines {
                                *line = std::mem::take(line).style(
                                    Style::default().fg(ratatui::style::Color::Rgb(110, 117, 129)),
                                );
                            }
                        }
                        documents.push_back(document);
                        while documents.len() > 64
                            || documents.iter().map(|d| d.2.len()).sum::<usize>() > 8 * 1024 * 1024
                        {
                            documents.pop_front();
                        }
                        lines
                    }))
                    .unwrap_or_else(|_| plain(&job.text, job.width, job.muted));
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
                            layout: Arc::new(prepared(lines, job.width)),
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
        self.shared.stopped.store(true, Ordering::Release);
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
    pub fn layout(&mut self, text: &str, width: u16, muted: bool) -> Arc<EntryLayout> {
        if self.revision != self.snapshot_revision || width != self.width || self.version == 0 {
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
            self.displayed = Some(result.clone());
        }
        if let Some(result) = &self.displayed {
            self.exact = result.version == self.version;
            return result.layout.clone();
        }
        // 回退只排版有限尾部；完整正文仍由后台处理，不能用于历史提交。
        if let Some(layout) = &self.fallback {
            return layout.clone();
        }
        let start = text.len().saturating_sub(8192);
        let start = (start..=text.len())
            .find(|&i| text.is_char_boundary(i))
            .unwrap_or(text.len());
        let layout = Arc::new(prepared(plain(&text[start..], width, muted), width));
        self.fallback = Some(layout.clone());
        layout
    }
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
fn plain(text: &str, width: u16, muted: bool) -> Vec<Line<'static>> {
    let style = if muted {
        Style::default().fg(ratatui::style::Color::Rgb(110, 117, 129))
    } else {
        Style::default()
    };
    super::transcript::wrap_block(
        Line::from(Span::styled(super::display_text(text).into_owned(), style)),
        width,
        "",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::AgentEvent;
    use crossterm::event::Event;
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
        let preview = ticket.layout(&text, 8, false);
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
    fn layout_worker_publishes_intermediate_append_without_marking_it_exact() {
        let worker = Worker::start().unwrap();
        let first = worker.pause();
        let mut ticket = Ticket::default();
        ticket.layout("**first**", 80, false);
        first.wait();
        let second = worker.pause();
        ticket.invalidate(false);
        ticket.layout("**first**\n\nsecond", 80, false);
        drop(first);
        second.wait();
        let intermediate = ticket.layout("**first**\n\nsecond", 80, false);
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
        drop(second);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            ticket.layout("**first**\n\nsecond", 80, false);
            if ticket.exact {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "应最终完成最新排版");
            std::thread::yield_now();
        }
        let third = worker.pause();
        ticket.invalidate(false);
        assert!(!ticket.exact, "追加后立即撤销最新版本资格");
        let retained = ticket.layout("**first**\n\nsecond\n\nthird", 80, false);
        assert_eq!(retained.height, 3);
        assert!(!ticket.exact);
        drop(third);
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
        let first = ticket.layout("正文🙂", 80, false);
        let source = ticket.source.clone();
        let version = ticket.version;
        let second = ticket.layout("正文🙂", 80, false);
        assert!(Arc::ptr_eq(&first, &second));
        assert!(Arc::ptr_eq(&source, &ticket.source));
        assert_eq!(version, ticket.version);
        ticket.invalidate(false);
        ticket.layout("正文🙂新增", 80, false);
        assert_eq!(ticket.version, version + 1);
        drop(pause);
    }

    #[test]
    fn layout_worker_rejects_replacement_and_width_mismatch() {
        let worker = Worker::start().unwrap();
        let pause = worker.pause();
        let mut ticket = Ticket::default();
        ticket.layout("old", 80, false);
        let old_epoch = ticket.epoch;
        ticket.invalidate(true);
        ticket.layout("new", 80, false);
        *ticket.slot.lock().unwrap() = Some(Rendered {
            epoch: old_epoch,
            version: u64::MAX,
            width: 80,
            layout: Arc::new(prepared(vec![Line::raw("stale")], 80)),
        });
        let rows = ticket.layout("new", 80, false);
        let LayoutContent::Styled(line) = &rows.lines[0].content else {
            panic!("应为已排版行")
        };
        assert_eq!(line.to_string(), "new");
        assert!(!ticket.exact);
        *ticket.slot.lock().unwrap() = Some(Rendered {
            epoch: ticket.epoch,
            version: ticket.version,
            width: 80,
            layout: Arc::new(prepared(vec![Line::raw("new")], 80)),
        });
        ticket.layout("new", 80, false);
        assert!(ticket.exact);
        ticket.layout("new", 40, false);
        assert!(!ticket.exact);
        drop(pause);
    }
}
