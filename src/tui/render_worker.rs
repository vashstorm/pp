//! 正文排版服务。队列按消息合并，UI 只读取已经完成的不可变结果。
use super::transcript::{EntryLayout, LayoutContent, LayoutLine};
use ratatui::{style::Style, text::Line};
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
    gate: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
}
struct Job {
    id: u64,
    epoch: u64,
    version: u64,
    text: Arc<str>,
    width: u16,
    muted: bool,
    slot: Weak<Mutex<Option<Rendered>>>,
    latest: Weak<AtomicU64>,
}
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
                    if let Some(gate) = state.gate.lock().unwrap_or_else(|e| e.into_inner()).take()
                    {
                        let _ = gate.recv();
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
                    if job
                        .latest
                        .upgrade()
                        .is_none_or(|latest| latest.load(Ordering::Acquire) != job.version)
                    {
                        continue;
                    }
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
    width: u16,
    slot: Arc<Mutex<Option<Rendered>>>,
    latest: Arc<AtomicU64>,
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
            width: 0,
            slot: Arc::new(Mutex::new(None)),
            latest: Arc::new(AtomicU64::new(0)),
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
        if replaced {
            self.epoch += 1;
        }
    }
    pub fn layout(&mut self, text: &str, width: u16, muted: bool) -> Arc<EntryLayout> {
        if self.revision != self.snapshot_revision || width != self.width || self.version == 0 {
            self.version += 1;
            self.latest.store(self.version, Ordering::Release);
            if self.revision != self.snapshot_revision {
                self.source = Arc::from(text);
                self.snapshot_revision = self.revision;
            }
            self.fallback = None;
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
                        latest: Arc::downgrade(&self.latest),
                    });
                    shared.wake.notify_one();
                }
            });
        }
        let result = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(result) = result
            .as_ref()
            .filter(|r| r.epoch == self.epoch && r.width == width)
        {
            self.exact = result.version == self.version;
            if self.exact {
                return result.layout.clone();
            }
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
        Line::styled(super::display_text(text).into_owned(), style),
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
    fn layout_worker_blocked_render_does_not_block_input_or_commit_preview() {
        let worker = Worker::start().unwrap();
        let (release, gate) = std::sync::mpsc::channel();
        *worker.shared.gate.lock().unwrap() = Some(gate);
        let mut app = crate::tui::App::new(Err(anyhow::anyhow!("fixture"))).unwrap();
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
        release.send(()).unwrap();
    }

    #[test]
    fn layout_worker_unchanged_revision_reuses_snapshot_fallback_and_job() {
        let worker = Worker::start().unwrap();
        let (release, gate) = std::sync::mpsc::channel();
        *worker.shared.gate.lock().unwrap() = Some(gate);
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
        release.send(()).unwrap();
    }

    #[test]
    fn layout_worker_rejects_replacement_and_width_mismatch() {
        let worker = Worker::start().unwrap();
        let (release, gate) = std::sync::mpsc::channel();
        *worker.shared.gate.lock().unwrap() = Some(gate);
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
        release.send(()).unwrap();
    }
}
