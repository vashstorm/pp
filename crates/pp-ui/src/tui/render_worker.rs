//! Body layout service. The queue merges by message; the UI only reads completed immutable results.
use super::markdown::TailMeta;
use super::transcript::{EntryLayout, LayoutContent, LayoutLine};
use pp_config::config::{IconMode, Theme};
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

/// Byte cap for the main thread's synchronous Markdown preview of the active tail block; beyond it, fall back to bounded tail-row continuation.
const PREVIEW_MARKDOWN_LIMIT: usize = 8192;

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
            if job.streaming {
                self.order.push_front(job.id);
            } else {
                self.order.push_back(job.id);
            }
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
            .expect("background layout should reach the pause point");
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
    theme: Theme,
    icons: IconMode,
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
    theme: Theme,
    icons: IconMode,
    width: u16,
    source: Arc<str>,
    layout: Arc<EntryLayout>,
    ends_with_table: bool,
    table_tail_start: Option<usize>,
    tail: TailMeta,
}

struct CachedDocument {
    id: u64,
    epoch: u64,
    theme: Theme,
    icons: IconMode,
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
                super::markdown::prewarm_highlight();
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
                        jobs.pop_front().expect("the layout job is ready")
                    };
                    let Some(slot) = job.slot.upgrade() else {
                        continue;
                    };
                    #[cfg(test)]
                    {
                        let gate = state.gate.lock().unwrap_or_else(|e| e.into_inner()).take();
                        if let Some(gate) = gate {
                            let _ = gate.entered.send(());
                            // A pause point must not wait forever: when a test leaks drop(TestPause), let the background continue
                            // so an assertion reports the error instead of hanging the drop(Worker) join.
                            let _ = gate.release.recv_timeout(std::time::Duration::from_secs(5));
                        }
                    }
                    super::themes::set_active(job.theme, job.icons);
                    super::markdown::highlight_context(job.id, job.epoch);
                    let render = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let index = documents.iter().position(|document| document.id == job.id);
                        let mut document = index
                            .and_then(|i| documents.remove(i))
                            .filter(|document| {
                                document.epoch == job.epoch
                                    && document.theme == job.theme
                                    && document.icons == job.icons
                            })
                            .unwrap_or_else(|| CachedDocument {
                                id: job.id,
                                epoch: job.epoch,
                                theme: job.theme,
                                icons: job.icons,
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
                        let (mut lines, tail) =
                            document.markdown.layout_cached_with_meta(job.width);
                        if job.muted {
                            let muted_fg = super::themes::palette().welcome_muted;
                            for line in &mut lines {
                                *line = std::mem::take(line).style(Style::default().fg(muted_fg));
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
                        (lines, ends_with_table, table_tail_start, tail)
                    }));
                    let (lines, ends_with_table, table_tail_start, tail) =
                        render.unwrap_or_else(|_| {
                            let rows = plain(&job.text, job.width, job.muted);
                            let meta = TailMeta {
                                stable_rows: rows.len(),
                                tail_source_start: job.text.len(),
                                ..TailMeta::default()
                            };
                            (rows, false, None, meta)
                        });
                    // A completed appended version is still displayable; latest-version eligibility is decided by Ticket alone.
                    let mut result = slot.lock().unwrap_or_else(|e| e.into_inner());
                    if result
                        .as_ref()
                        .is_none_or(|r| (job.epoch, job.version) >= (r.epoch, r.version))
                    {
                        *result = Some(Rendered {
                            epoch: job.epoch,
                            version: job.version,
                            theme: job.theme,
                            icons: job.icons,
                            width: job.width,
                            source: job.text,
                            layout: Arc::new(prepared(lines, job.width)),
                            ends_with_table,
                            table_tail_start,
                            tail,
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
        // Condvar::wait registers the waiter and releases the lock atomically: only publishing stopped while
        // holding the lock guarantees the wake-up cannot be lost between the worker's predicate check and registration.
        // A lock-free notify would block join forever.
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
    theme: Theme,
    icons: IconMode,
    source: Arc<str>,
    revision: u64,
    snapshot_revision: u64,
    fallback: Option<Arc<EntryLayout>>,
    displayed: Option<Rendered>,
    width: u16,
    streaming: bool,
    /// Version handed to the layout thread but not yet written back; while non-empty, no new job is queued.
    in_flight: Option<u64>,
    slot: Arc<Mutex<Option<Rendered>>>,
    pub exact: bool,
}
impl Default for Ticket {
    fn default() -> Self {
        let (theme, icons) = super::themes::current();
        Self {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            epoch: 0,
            version: 0,
            theme,
            icons,
            source: Arc::from(""),
            revision: 1,
            snapshot_revision: 0,
            fallback: None,
            displayed: None,
            width: 0,
            streaming: false,
            in_flight: None,
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
        let (theme, icons) = super::themes::current();
        let appearance_changed = self.theme != theme || self.icons != icons;
        let mode_changed = self.streaming != streaming;
        self.streaming = streaming;
        // At the end, lay out the still-previewed trailing line into the table; reuse it directly when the body is already fully laid out.
        let needs_layout_for_mode = mode_changed
            && (streaming
                || !self.exact
                || self
                    .displayed
                    .as_ref()
                    .is_some_and(|result| result.table_tail_start.is_some()));
        let source_changed = self.revision != self.snapshot_revision;
        if source_changed
            || width != self.width
            || self.version == 0
            || needs_layout_for_mode
            || appearance_changed
        {
            self.version += 1;
            if source_changed {
                self.source = Arc::from(text);
                self.snapshot_revision = self.revision;
            }
            self.fallback = None;
            if self.width != width || appearance_changed {
                self.displayed = None;
            }
            self.width = width;
            self.theme = theme;
            self.icons = icons;
            self.exact = false;
        }
        self.absorb(width);
        // Do not queue while the previous job awaits write-back: the version keeps increasing so each streaming
        // delta does not schedule a full re-parse, and the latest version is dispatched in the same frame after write-back.
        if self.in_flight.is_none() && self.unrendered() {
            self.spawn(muted);
        }
        if let Some(result) = &self.displayed {
            self.exact = result.version == self.version;
            if self.exact && result.table_tail_start.is_none() {
                return result.layout.clone();
            }
        }
        if let Some(layout) = &self.fallback {
            return layout.clone();
        }
        // The already-laid-out stable prefix shares an Arc with zero copies, and the active tail uses the same Markdown
        // layout rules so preview and background write-back do not keep refreshing between differing wrap/indent/newline rules.
        let layout: Arc<EntryLayout> = match &self.displayed {
            Some(result) if is_streaming_table_tail(result, text) => {
                // The table layout holds only fully collected rows; the preview always occupies a separate row.
                let start = result.table_tail_start.unwrap_or(result.source.len());
                match table_tail_preview(&text[start..], width, muted) {
                    Some(line) => Arc::new(EntryLayout::appended(
                        result.layout.clone(),
                        false,
                        vec![LayoutLine {
                            content: LayoutContent::Styled(line),
                            height: 1,
                        }],
                    )),
                    None => result.layout.clone(),
                }
            }
            Some(result) if text.starts_with(result.source.as_ref()) => {
                append_preview(result, text, width, muted, streaming)
            }
            _ if text.len() <= PREVIEW_MARKDOWN_LIMIT => Arc::new(prepared(
                preview_markdown_lines(text, width, muted, streaming),
                width,
            )),
            _ => Arc::new(prepared(plain(preview_tail(text), width, muted), width)),
        };
        self.fallback = Some(layout.clone());
        layout
    }

    /// Absorb background write-back: the newer result becomes the preview baseline and the queue slot is released.
    fn absorb(&mut self, width: u16) {
        if self.in_flight.is_none() && self.exact {
            return;
        }
        let result = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(result) = result.as_ref().filter(|result| {
            result.epoch == self.epoch
                && result.width == width
                && result.theme == self.theme
                && result.icons == self.icons
        }) && self
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
        // The queue slot only looks at the version: replacement and width changes invalidate an old write-back, but the job has indeed ended.
        if let Some(in_flight) = self.in_flight
            && result
                .as_ref()
                .is_some_and(|result| result.version >= in_flight)
        {
            self.in_flight = None;
        }
    }

    /// The latest version has not been handed to the layout thread and has not been written back.
    fn unrendered(&self) -> bool {
        self.displayed
            .as_ref()
            .is_none_or(|result| result.version < self.version)
    }

    fn spawn(&mut self, muted: bool) {
        let job = Job {
            id: self.id,
            epoch: self.epoch,
            version: self.version,
            theme: self.theme,
            icons: self.icons,
            text: self.source.clone(),
            width: self.width,
            muted,
            streaming: self.streaming,
            slot: Arc::downgrade(&self.slot),
        };
        let version = self.version;
        self.in_flight = CURRENT.with(|current| {
            let shared = current.borrow();
            let shared = shared.as_ref()?;
            let mut jobs = shared.jobs.lock().unwrap_or_else(|e| e.into_inner());
            jobs.push(job);
            shared.wake.notify_one();
            Some(version)
        });
    }
}

fn is_single_tail_line(text: &str) -> bool {
    let trimmed = text.strip_suffix('\n').unwrap_or(text);
    let trimmed = trimmed.strip_suffix('\r').unwrap_or(trimmed);
    !trimmed.contains('\n')
}

fn is_streaming_table_tail(result: &Rendered, text: &str) -> bool {
    if !text.starts_with(result.source.as_ref()) {
        return false;
    }
    if let Some(start) = result.table_tail_start {
        return is_single_tail_line(&text[start..]);
    }
    if !result.ends_with_table || ends_with_blank_line(&result.source) {
        return false;
    }
    let suffix = &text[result.source.len()..];
    !suffix.starts_with('\n') && !suffix.starts_with("\r\n") && is_single_tail_line(suffix)
}

fn ends_with_blank_line(text: &str) -> bool {
    let mut newlines = 0usize;
    for byte in text.bytes().rev() {
        match byte {
            b'\n' => {
                newlines += 1;
                if newlines >= 2 {
                    return true;
                }
            }
            b' ' | b'\t' | b'\r' => {}
            _ => return false,
        }
    }
    false
}

fn preview_tail(text: &str) -> &str {
    let start = text.len().saturating_sub(PREVIEW_MARKDOWN_LIMIT);
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

fn preview_markdown_lines(
    source: &str,
    width: u16,
    muted: bool,
    streaming: bool,
) -> Vec<Line<'static>> {
    let document = super::markdown::parse_streaming_preview(source, streaming);
    let mut lines = document.layout(width);
    if let Some(start) = document.table_tail_start
        && let Some(line) = table_tail_preview(&source[start..], width, muted)
    {
        lines.push(line);
    }
    apply_muted(&mut lines, muted);
    lines
}

fn apply_muted(lines: &mut [Line<'static>], muted: bool) {
    if muted {
        let style = text_style(true);
        for line in lines {
            *line = std::mem::take(line).style(style);
        }
    }
}

/// Preview tail: the stable prefix is shared across frames via `Arc`, and the active tail reuses the same Markdown layout rules.
fn append_preview(
    result: &Rendered,
    text: &str,
    width: u16,
    muted: bool,
    streaming: bool,
) -> Arc<EntryLayout> {
    let suffix = &text[result.source.len()..];
    if suffix.is_empty() {
        return result.layout.clone();
    }
    if width == 0 {
        return Arc::new(EntryLayout::owned(0, Vec::new()));
    }
    if let Some(rest) = &result.tail.open_code_prefix {
        return append_code_preview(result, suffix, rest, width, muted);
    }
    let tail_start = result.tail.tail_source_start;
    if tail_start <= result.source.len()
        && result.tail.stable_rows <= result.layout.len()
        && text.is_char_boundary(tail_start)
        && text.len() - tail_start <= PREVIEW_MARKDOWN_LIMIT
    {
        let mut rows = preview_markdown_lines(&text[tail_start..], width, muted, streaming);
        if result.tail.needs_separator && !rows.is_empty() {
            let mut blank = Line::from("");
            if muted {
                blank = blank.style(text_style(true));
            }
            rows.insert(0, blank);
        }
        return Arc::new(EntryLayout::with_prefix(
            result.layout.clone(),
            result.tail.stable_rows,
            rows.into_iter()
                .map(|line| LayoutLine {
                    content: LayoutContent::Styled(line),
                    height: 1,
                })
                .collect(),
        ));
    }
    append_tail_fallback(result, suffix, width, muted)
}

/// Streaming continuation of an unclosed code block: keep the syntax-highlighted prefix rows and wrap only the new tail by character, padding in the indent prefix.
fn append_code_preview(
    result: &Rendered,
    suffix: &str,
    rest: &[Span<'static>],
    width: u16,
    muted: bool,
) -> Arc<EntryLayout> {
    let tail = preview_tail(suffix);
    let truncated = tail.len() != suffix.len();
    let continuation = !truncated && !result.source.ends_with('\n') && !result.layout.is_empty();
    let width_usize = usize::from(width);
    let rest = super::markdown::fit_prefix_preview(rest, width_usize);
    let body_width = width_usize
        .saturating_sub(super::markdown::spans_width_preview(&rest))
        .max(1);
    let code_style = Style::new().fg(super::transcript_style::code_block());
    let mut rows: Vec<Line<'static>> = Vec::new();
    let mut parts = tail.split_terminator('\n');

    if truncated {
        rows.push(Line::raw("…"));
    } else if continuation {
        let mut spans = last_layout_spans(result);
        if let Some(first) = parts.next() {
            let first = first.strip_suffix('\r').unwrap_or(first);
            let safe = pp_core::text::display_text(first);
            let expanded = super::markdown::expand_tabs_preview(&safe);
            if !expanded.is_empty() {
                spans.push(Span::styled(expanded, code_style));
            }
        }
        let wrapped = super::markdown::wrap_char_preview(&spans, width_usize, body_width);
        if wrapped.is_empty() {
            rows.push(Line::from(rest.clone()));
        } else {
            for (index, line) in wrapped.into_iter().enumerate() {
                if index == 0 {
                    rows.push(Line::from(line));
                } else {
                    let mut prefixed = super::markdown::fit_prefix_preview(
                        &rest,
                        width_usize
                            .saturating_sub(super::markdown::spans_width_preview(&line))
                            .saturating_add(1),
                    );
                    prefixed.extend(line);
                    rows.push(Line::from(prefixed));
                }
            }
        }
    }

    for part in parts {
        let part = part.strip_suffix('\r').unwrap_or(part);
        let safe = pp_core::text::display_text(part);
        let expanded = super::markdown::expand_tabs_preview(&safe);
        let spans = if expanded.is_empty() {
            Vec::new()
        } else {
            vec![Span::styled(expanded, code_style)]
        };
        let wrapped = super::markdown::wrap_char_preview(&spans, body_width, body_width);
        if wrapped.is_empty() {
            rows.push(Line::from(rest.clone()));
        } else {
            for line in wrapped {
                let mut prefixed = super::markdown::fit_prefix_preview(
                    &rest,
                    width_usize
                        .saturating_sub(super::markdown::spans_width_preview(&line))
                        .saturating_add(1),
                );
                prefixed.extend(line);
                rows.push(Line::from(prefixed));
            }
        }
    }

    apply_muted(&mut rows, muted);
    Arc::new(EntryLayout::appended(
        result.layout.clone(),
        continuation,
        rows.into_iter()
            .map(|line| LayoutLine {
                content: LayoutContent::Styled(line),
                height: 1,
            })
            .collect(),
    ))
}

/// Bounded continuation fallback for an oversized active block (> 8 KiB): keeps word wrapping, hanging indents and no trailing blank row.
fn append_tail_fallback(
    result: &Rendered,
    suffix: &str,
    width: u16,
    muted: bool,
) -> Arc<EntryLayout> {
    let tail = preview_tail(suffix);
    let truncated = tail.len() != suffix.len();
    let width_usize = usize::from(width);
    let rest = super::markdown::fit_prefix_preview(&result.tail.fallback_prefix, width_usize);
    let rest_avail = width_usize
        .saturating_sub(super::markdown::spans_width_preview(&rest))
        .max(1);

    if truncated {
        let mut rows = vec![Line::raw("…")];
        push_word_paragraphs(tail, width_usize, &rest, rest_avail, &mut rows);
        apply_muted(&mut rows, muted);
        return Arc::new(EntryLayout::appended(
            result.layout.clone(),
            false,
            rows.into_iter()
                .map(|line| LayoutLine {
                    content: LayoutContent::Styled(line),
                    height: 1,
                })
                .collect(),
        ));
    }

    let trimmed = tail.trim_end_matches(['\n', '\r']);
    if trimmed.is_empty() {
        return result.layout.clone();
    }
    let src_ends_blank = ends_with_blank_line(&result.source);
    let starts_new_para = src_ends_blank
        || trimmed.starts_with("\n\n")
        || trimmed.starts_with("\r\n\r\n")
        || (result.source.ends_with('\n') && trimmed.starts_with('\n'));
    let continuation = !starts_new_para && !result.layout.is_empty();
    let mut rows: Vec<Line<'static>> = Vec::new();
    let mut paragraphs = split_fallback_paragraphs(trimmed);

    if continuation {
        let last_spans = last_layout_spans(result);
        let (first_prefix, mut body_spans) = split_leading_prefix(last_spans, &rest);
        if let Some(first) = paragraphs.next() {
            let safe = pp_core::text::display_text(&first);
            let expanded = super::markdown::expand_tabs_preview(&safe);
            if !expanded.is_empty() {
                let needs_space = result.source.ends_with('\n')
                    || result.source.ends_with([' ', '\t'])
                    || trimmed.starts_with('\n')
                    || trimmed.starts_with("\r\n");
                let already_spaced = body_spans
                    .last()
                    .is_some_and(|span| span.content.ends_with(char::is_whitespace))
                    || expanded.starts_with(char::is_whitespace);
                if needs_space && !already_spaced && !body_spans.is_empty() {
                    body_spans.push(Span::raw(" "));
                }
                body_spans.push(Span::raw(expanded));
            }
        }
        push_wrapped_spans(
            &body_spans,
            width_usize,
            &first_prefix,
            &rest,
            rest_avail,
            &mut rows,
        );
    } else if !result.layout.is_empty() && starts_new_para {
        rows.push(Line::from(""));
    }

    for (index, paragraph) in paragraphs.enumerate() {
        if continuation || index > 0 {
            rows.push(Line::from(""));
        }
        let safe = pp_core::text::display_text(&paragraph);
        let expanded = super::markdown::expand_tabs_preview(&safe);
        let spans = vec![Span::raw(expanded)];
        push_wrapped_spans(&spans, width_usize, &rest, &rest, rest_avail, &mut rows);
    }

    apply_muted(&mut rows, muted);
    Arc::new(EntryLayout::appended(
        result.layout.clone(),
        continuation,
        rows.into_iter()
            .map(|line| LayoutLine {
                content: LayoutContent::Styled(line),
                height: 1,
            })
            .collect(),
    ))
}

fn last_layout_spans(result: &Rendered) -> Vec<Span<'static>> {
    let Some(LayoutLine {
        content: LayoutContent::Styled(line),
        ..
    }) = result.layout.get(result.layout.len().saturating_sub(1))
    else {
        return Vec::new();
    };
    line.spans.clone()
}

fn split_fallback_paragraphs(text: &str) -> impl Iterator<Item = String> + '_ {
    let mut paragraphs = Vec::new();
    let mut current = String::new();
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.trim().is_empty() {
            if !current.is_empty() {
                paragraphs.push(std::mem::take(&mut current));
            }
        } else {
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(line);
        }
    }
    if !current.is_empty() {
        paragraphs.push(current);
    }
    paragraphs.into_iter()
}

fn split_leading_prefix(
    mut spans: Vec<Span<'static>>,
    rest: &[Span<'static>],
) -> (Vec<Span<'static>>, Vec<Span<'static>>) {
    if !rest.is_empty() && spans.len() >= rest.len() && spans[..rest.len()] == *rest {
        let body = spans.split_off(rest.len());
        return (spans, body);
    }
    if spans.first().is_some_and(|span| {
        !span.content.is_empty() && span.content.chars().all(char::is_whitespace)
    }) {
        let body = spans.split_off(1);
        return (spans, body);
    }
    (Vec::new(), spans)
}

fn push_word_paragraphs(
    text: &str,
    width: usize,
    rest: &[Span<'static>],
    rest_avail: usize,
    out: &mut Vec<Line<'static>>,
) {
    for (index, paragraph) in split_fallback_paragraphs(text).enumerate() {
        if index > 0 {
            out.push(Line::from(""));
        }
        let safe = pp_core::text::display_text(&paragraph);
        let expanded = super::markdown::expand_tabs_preview(&safe);
        push_wrapped_spans(&[Span::raw(expanded)], width, rest, rest, rest_avail, out);
    }
}

fn push_wrapped_spans(
    spans: &[Span<'static>],
    width: usize,
    first_prefix: &[Span<'static>],
    rest: &[Span<'static>],
    rest_avail: usize,
    out: &mut Vec<Line<'static>>,
) {
    let first_avail = width
        .saturating_sub(super::markdown::spans_width_preview(first_prefix))
        .max(1);
    let wrapped = super::markdown::wrap_word_preview(spans, first_avail, rest_avail);
    if wrapped.is_empty() {
        out.push(Line::from(first_prefix.to_vec()));
        return;
    }
    for (index, line) in wrapped.into_iter().enumerate() {
        let prefix = if index == 0 { first_prefix } else { rest };
        let mut prefixed = super::markdown::fit_prefix_preview(
            prefix,
            width
                .saturating_sub(super::markdown::spans_width_preview(&line))
                .saturating_add(1),
        );
        prefixed.extend(line);
        out.push(Line::from(prefixed));
    }
}

fn prepared(lines: Vec<Line<'static>>, width: u16) -> EntryLayout {
    EntryLayout::owned(
        width,
        lines
            .into_iter()
            .map(|line| LayoutLine {
                content: LayoutContent::Styled(line),
                height: 1,
            })
            .collect(),
    )
}
fn text_style(muted: bool) -> Style {
    if muted {
        Style::default().fg(super::themes::palette().welcome_muted)
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
mod tests;
