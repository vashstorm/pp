use super::tool_display::{self, Context as DisplayContext};
use super::transcript_style as palette;
use pp_core::events::AgentEvent;
use pp_core::events::DiffLineKind;
use pp_core::events::FileDiff;
use pp_core::events::OutputStream;
use pp_runtime::agent::persistence::ReplayRecord;
use ratatui::{
    Frame,
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget, Wrap},
};
use serde_json::Value;
#[cfg(test)]
use std::io::{self, Write};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

mod bash_tasks;
pub(super) mod card;
mod diff;
mod entry;
mod entry_ask;
mod entry_layout;
mod entry_title;
mod entry_tool;
mod entry_user;
mod geometry;
mod layout;
mod native;
mod preview;
mod progress;
mod recorded;
mod render;
mod scroll;
mod tool_result;
mod tree_layout;
mod wrap;

pub(super) use diff::diff_layout_styled;
pub(super) use layout::{EntryLayout, LayoutContent, LayoutLine};
use tool_result::{background_status, tool_result};
pub(super) use wrap::{block_line, visit_wrapped_rows, wrap_block};

static NEXT_NATIVE_GENERATION: AtomicU64 = AtomicU64::new(1);

/// One frozen terminal row of native history; the committed state, previews and batches share the same row buffer.
pub(super) type NativeRow = Arc<[ratatui::buffer::Cell]>;

/// Committed source position plus the frozen lines still on screen; the full history is not copied.
#[derive(Clone, Default)]
struct NativeState {
    entry: usize,
    row: usize,
    retained: VecDeque<NativeRow>,
    revision: u64,
    width: u16,
    commit_layout: Option<Arc<EntryLayout>>,
    commit_full_rows: bool,
}

pub(super) struct NativeBatch {
    generation: u64,
    revision: u64,
    next: NativeState,
    pub rows: Vec<NativeRow>,
}

fn bash_background() -> Color {
    palette::tool_bg()
}
fn bash_border() -> Color {
    palette::tool_border()
}
fn eval_background() -> Color {
    palette::eval_bg()
}
fn eval_border() -> Color {
    palette::eval_border()
}
fn muted_text() -> Color {
    super::themes::palette().welcome_muted
}
fn user_background() -> Color {
    super::themes::palette().user_bg
}
fn assistant_background() -> Color {
    palette::md_bg()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum EntryKind {
    User,
    Assistant,
    Bash,
    Eval,
    Think,
    Rg,
    Diff,
    Ask,
    Data,
    Error,
    /// Highlight notice card: yellow border, for runtime switches that change later requests (for example the fallback model).
    Notice,
    Activity,
    /// LSP-only card: an inline tool result or a late standalone diagnostic.
    Lsp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum EntryStatus {
    Running,
    Complete,
    Failed,
}

#[derive(Clone, Copy)]
pub(super) enum Source {
    Top,
    Divider,
    Bottom,
    Padding,
    Footer,
    Title,
    Text,
    StdoutLabel,
    Stdout,
    StderrLabel,
    Stderr,
    ReadItem(usize),
}

enum EntryBody {
    Plain,
    ArgumentsPreview {
        truncated: bool,
    },
    Tips,
    Markdown(super::markdown::MarkdownDocument),
    Diff(FileDiff),
    Ask(AskBlock),
    /// LSP tool card: readable body plus per-path accumulated multi-file changes.
    Lsp(super::lsp::Block),
}

struct AskBlock {
    args: pp_core::interaction::AskArgs,
    answers: Option<Vec<pp_core::interaction::AskAnswer>>,
    previews: Vec<Vec<Option<super::markdown::MarkdownDocument>>>,
}

struct TranscriptEntry {
    render: super::render_worker::Ticket,
    kind: EntryKind,
    body: EntryBody,
    markdown_dirty: bool,
    status: EntryStatus,
    title: String,
    text: String,
    refusal: String,
    stdout: String,
    stderr: String,
    tool: Option<String>,
    cwd: Option<Arc<std::path::Path>>,
    layout: Option<Arc<EntryLayout>>,
    expanded: bool,
    footer: String,
    /// Warnings such as a non-zero pipeline stage, drawn as a warning-colored span on the bottom border.
    footer_warning: String,
    shell: Option<pp_core::shell_display::ShellPresentation>,
    started: Option<Instant>,
    bash_timeout_seconds: Option<u64>,
    eval_timeout_seconds: Option<u64>,
    read_clock: &'static str,
    arguments: Value,
    result: Option<Value>,
    /// Inline or late reports; only the latest one per path is kept, shown after the body in order of appearance.
    diagnostics: Vec<pp_core::lsp_report::DiagnosticReport>,
    display: DisplayContext,
    display_title: Option<Vec<Line<'static>>>,
    read_group: Vec<card::ReadItem>,
    hidden: bool,
}

struct Activity {
    entry: TranscriptEntry,
    frame: u128,
    clock: &'static str,
    started: Option<Instant>,
    waiting_started: Option<Instant>,
    label: String,
    summary_label: Option<String>,
    pending_summary: Option<(String, Instant)>,
    summary_updated_at: Option<Instant>,
    summary_line: String,
    summary_has_title: bool,
    seconds: Option<u64>,
    run_seconds: u64,
    running: bool,
}

pub(super) struct TreeBlock {
    pub entries: Range<usize>,
    pub label: Line<'static>,
}

struct TreeGroup {
    index: usize,
    entries: Range<usize>,
    rows: Range<usize>,
}

struct TreeLayout {
    width: u16,
    groups: Vec<TreeGroup>,
}

#[derive(Clone, Copy)]
struct BrowseAnchor {
    entry: usize,
    row: usize,
}

#[derive(Default)]
pub(super) struct Transcript {
    cwd: Option<Arc<std::path::Path>>,
    native_generation: u64,
    native: NativeState,
    native_preview: Option<NativeState>,
    entries: Vec<TranscriptEntry>,
    read_groups_key: Option<(usize, bool, bool)>,
    // The running stage shows above the input box; the final summary shows at the tail of the conversation.
    activity: Option<Activity>,
    assistant: Option<usize>,
    thinking: Option<usize>,
    running_tools: Vec<(String, usize)>,
    argument_previews: BTreeMap<u32, usize>,
    // Background processes of the same user turn share the original Bash card.
    background_bash: BTreeMap<String, usize>,
    injected_subagent_runs: BTreeSet<String>,
    offset: usize,
    browse_anchor: Option<BrowseAnchor>,
    // Alternate-screen geometry syncs incrementally by layout identity; the tree view clears it on takeover and forces a full recompute afterwards.
    geometry: geometry::Geometry,
    total: usize,
    viewport_height: usize,
    viewport_width: u16,
    viewport_bottom: u16,
    follow_bottom: bool,
    tools_expanded: bool,
    display: DisplayContext,
    // Replay restore does not forge the wall duration of a bash card.
    replaying: bool,
    bash_timeout_seconds: u64,
    tree_layout: Option<TreeLayout>,
}

impl Transcript {
    pub(super) fn new() -> Self {
        Self {
            cwd: std::env::current_dir().ok().map(Arc::from),
            native_generation: NEXT_NATIVE_GENERATION.fetch_add(1, Ordering::Relaxed),
            follow_bottom: true,
            bash_timeout_seconds: pp_config::config::ToolLimits::default().bash_timeout_seconds,
            ..Self::default()
        }
    }

    pub(super) fn has_entries(&self) -> bool {
        !self.entries.is_empty()
    }

    pub(super) fn with_read_preview(read_preview: bool) -> Self {
        Self {
            display: DisplayContext {
                read_preview,
                ..DisplayContext::default()
            },
            ..Self::new()
        }
    }

    pub(super) fn with_cwd(mut self, cwd: &std::path::Path) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub(super) fn fresh(&self) -> Self {
        Self {
            cwd: self.cwd.clone(),
            bash_timeout_seconds: self.bash_timeout_seconds,
            ..Self::with_read_preview(self.display.read_preview)
        }
    }

    pub(super) fn set_bash_timeout(&mut self, seconds: u64) {
        self.bash_timeout_seconds = seconds;
    }

    pub(super) fn native_generation(&self) -> u64 {
        self.native_generation
    }

    pub(super) fn following(&self) -> bool {
        self.follow_bottom
    }

    pub(super) fn follow(&mut self) {
        self.follow_bottom = true;
        self.browse_anchor = None;
    }

    pub(super) fn begin_browse(&mut self) {
        self.prepare_layout(self.viewport_width);
        self.sync_geometry();
        self.offset = self.total.saturating_sub(self.viewport_height);
        self.follow_bottom = true;
        self.browse_anchor = None;
    }

    pub(super) fn set_terminal_rows(&mut self, rows: u16) {
        self.display.terminal_rows = rows;
    }

    pub(super) fn tools_expanded(&self) -> bool {
        self.tools_expanded
    }

    pub(super) fn append(&mut self, label: &str, text: &str) {
        let (kind, title) = match label {
            "You" => (EntryKind::User, String::new()),
            "Assistant" => (EntryKind::Assistant, String::new()),
            _ => (EntryKind::Data, label.into()),
        };
        let mut entry = TranscriptEntry::new(kind, title, text.into(), None);
        if kind == EntryKind::User {
            entry.cwd = self.cwd.clone();
        }
        self.read_groups_key = None;
        self.entries.push(entry);
    }

    /// Local errors only enter the visible history; they produce no model message or persisted event.
    pub(super) fn append_error(&mut self, text: &str) {
        if text.trim().is_empty() {
            return;
        }
        self.read_groups_key = None;
        self.entries.push(TranscriptEntry::new(
            EntryKind::Error,
            "Error".into(),
            text.into(),
            None,
        ));
    }

    /// Runtime notice that needs the user's immediate attention: card form, never entering model messages.
    pub(super) fn append_notice(&mut self, title: &str, text: &str) {
        if text.trim().is_empty() {
            return;
        }
        self.read_groups_key = None;
        self.entries.push(TranscriptEntry::new(
            EntryKind::Notice,
            title.into(),
            text.into(),
            None,
        ));
    }

    pub(super) fn append_tips(&mut self) {
        let mut entry =
            TranscriptEntry::new(EntryKind::Data, String::new(), crate::tips::text(), None);
        entry.body = EntryBody::Tips;
        self.read_groups_key = None;
        self.entries.push(entry);
    }

    pub(super) fn begin_run(&mut self) {
        self.background_bash.clear();
        self.activity = Some(Activity::new(Instant::now()));
    }

    pub(super) fn task_duration(&self) -> Option<String> {
        self.activity.as_ref().and_then(|activity| {
            let seconds = if activity.running {
                Some(activity.run_seconds)
            } else {
                activity.seconds
            };
            seconds.map(Activity::format_duration)
        })
    }

    pub(super) fn tick(&mut self, now: Instant) -> bool {
        let mut changed = self
            .activity
            .as_mut()
            .is_some_and(|activity| activity.tick(now));
        for index in self
            .running_tools
            .iter()
            .map(|(_, index)| index)
            .chain(self.background_bash.values())
        {
            let entry = &mut self.entries[*index];
            if entry.tool.as_deref() == Some("read") && entry.status == EntryStatus::Running {
                let clock = palette::waiting_clock(entry.started.map_or(0, |started| {
                    now.saturating_duration_since(started).as_millis()
                }));
                if entry.read_clock != clock {
                    entry.read_clock = clock;
                    entry.layout = None;
                    entry.display_title = None;
                    self.read_groups_key = None;
                    changed = true;
                }
            }
            if entry.kind == EntryKind::Bash && entry.status == EntryStatus::Running {
                changed |= entry.update_bash_footer(
                    entry
                        .started
                        .map(|started| now.saturating_duration_since(started)),
                );
            } else if entry.kind == EntryKind::Eval && entry.status == EntryStatus::Running {
                changed |= entry.update_eval_footer(
                    entry
                        .started
                        .map(|started| now.saturating_duration_since(started)),
                );
            }
        }
        changed
    }

    pub(super) fn waiting_for_input(&mut self) {
        if let Some(activity) = &mut self.activity {
            activity.phase("Waiting for input");
        }
    }

    pub(super) fn has_live_activity(&self) -> bool {
        self.activity
            .as_ref()
            .is_some_and(|activity| activity.running)
    }

    fn live_activity_line(&self) -> Option<Line<'static>> {
        self.activity
            .as_ref()
            .filter(|activity| activity.running)
            .map(Activity::live_line)
    }

    pub(super) fn draw_live_activity(&self, frame: &mut Frame, area: Rect) {
        if !area.is_empty()
            && let Some(line) = self.live_activity_line()
        {
            let inset = area.width.saturating_sub(1).min(2);
            frame.render_widget(
                line,
                Rect::new(area.x + inset, area.y, area.width - inset, 1),
            );
        }
    }

    pub(super) fn cancelling(&mut self) {
        if let Some(activity) = &mut self.activity {
            activity.phase("Cancelling");
        }
    }

    pub(super) fn finish_run(
        &mut self,
        label: &str,
        elapsed: Duration,
        finished_at: chrono::DateTime<chrono::Local>,
    ) {
        self.finish_run_with_timing(label, Some(elapsed), Some(finished_at));
    }

    /// The main session and the observation log share the completion state; a missing history time stays unknown.
    pub(super) fn finish_run_with_timing(
        &mut self,
        label: &str,
        elapsed: Option<Duration>,
        finished_at: Option<chrono::DateTime<chrono::Local>>,
    ) {
        self.clear_argument_previews();
        self.background_bash.clear();
        self.finish_markdown_streams();
        let activity = self
            .activity
            .get_or_insert_with(|| Activity::new(Instant::now()));
        activity.started = None;
        activity.waiting_started = None;
        activity.running = false;
        activity.label = label.into();
        activity.seconds = elapsed.map(|elapsed| elapsed.as_secs());
        activity.entry.text = format!(
            "✻ {label} · {}",
            finished_at
                .map(|time| time.format("%H:%M").to_string())
                .unwrap_or_else(|| "—".into())
        );
        activity.entry.layout = None;
    }

    /// Restore the visible conversation in log order, only reporting unfinished requests without replaying old error notices.
    /// Input recall is separate persisted data: replay only rebuilds the visible body and creates no input records.
    pub(super) fn replay(&mut self, records: Vec<ReplayRecord>) -> Option<String> {
        self.replaying = true;
        let mut assistant_running = false;
        for record in records {
            match record {
                ReplayRecord::User(text) => self.append("You", &text),
                ReplayRecord::Event(event) => {
                    match &event {
                        AgentEvent::AssistantStarted => assistant_running = true,
                        AgentEvent::AssistantFinished { .. }
                        | AgentEvent::ModelAttemptFailed { .. } => assistant_running = false,
                        _ => {}
                    }
                    self.progress(event);
                }
                ReplayRecord::Error(_message) => {
                    assistant_running = false;
                    self.fail();
                }
                ReplayRecord::Cancelled => {
                    self.cancel();
                    assistant_running = false;
                }
            }
        }
        self.replaying = false;
        self.background_bash.clear();
        if assistant_running || self.assistant.is_some() || !self.running_tools.is_empty() {
            self.fail();
            return Some("Previous request was interrupted.".into());
        }
        self.finish_thinking();
        None
    }

    pub(super) fn append_tree_node(
        &mut self,
        records: Vec<ReplayRecord>,
        fallback: &str,
    ) -> Range<usize> {
        // Tree nodes are previewed independently; a later node must not rewrite an earlier node's card.
        self.background_bash.clear();
        let start = self.entries.len();
        self.replay(records);
        self.read_groups_key = None;
        // Tree previews show everything in full, ignoring the interactive preview setting and quotas.
        for entry in &mut self.entries[start..] {
            entry.display.full = true;
            entry.expanded = true;
            entry.layout = None;
        }
        // Empty assistant records have no physical rows. Give every safe node a target.
        if self.entries[start..].iter().all(|entry| {
            entry.text.trim().is_empty()
                && matches!(entry.kind, EntryKind::Assistant | EntryKind::Think)
        }) {
            self.entries.truncate(start);
            self.entries.push(TranscriptEntry::new(
                EntryKind::Data,
                String::new(),
                if fallback.is_empty() {
                    "(empty response)"
                } else {
                    fallback
                }
                .into(),
                None,
            ));
        }
        self.tree_layout = None;
        start..self.entries.len()
    }

    /// Only used to verify the projection of the full viewer; not the production exit output path.
    #[cfg(test)]
    pub(super) fn write_full_for_test(
        &mut self,
        writer: &mut impl Write,
        snapshot: &Buffer,
    ) -> io::Result<()> {
        let footer_y = self.viewport_bottom;
        self.display.full = true;
        // The full viewer uses the raw retained output, independent of the main screen history's folded snapshot.
        for entry in &mut self.entries {
            if entry.is_collapsible_tool() && !entry.expanded {
                entry.expanded = true;
                entry.layout = None;
            }
        }
        self.prepare_layout(snapshot.area.width);
        self.sync_geometry();
        // Replay all history in chunks to avoid allocating the whole session as one huge screen buffer.
        for offset in (0..self.total).step_by(64) {
            let height = (self.total - offset).min(64) as u16;
            let area = Rect::new(0, 0, snapshot.area.width, height);
            let mut buffer = Buffer::empty(area);
            self.render(&mut buffer, area, offset);
            super::scrollback::write_rows(writer, &buffer, area)?;
        }
        let footer = Rect::new(
            snapshot.area.x,
            footer_y,
            snapshot.area.width,
            snapshot.area.bottom().saturating_sub(footer_y),
        );
        super::scrollback::write_rows(writer, snapshot, footer)?;
        writer.flush()
    }

    fn assistant_entry(&mut self) -> usize {
        if let Some(index) = self.assistant {
            return index;
        }
        let index = self.entries.len();
        let mut entry =
            TranscriptEntry::new(EntryKind::Assistant, String::new(), String::new(), None);
        entry.status = EntryStatus::Running;
        self.read_groups_key = None;
        self.entries.push(entry);
        self.assistant = Some(index);
        index
    }

    fn finish_thinking(&mut self) {
        if let Some(index) = self.thinking.take() {
            self.entries[index].finish_markdown(EntryStatus::Complete);
        }
    }

    pub(super) fn finish_markdown_streams(&mut self) {
        self.finish_thinking();
        if let Some(index) = self.assistant.take() {
            self.entries[index].finish_markdown(EntryStatus::Complete);
        }
    }

    pub(super) fn has_running_ask(&self, id: &str) -> bool {
        self.tool_index(id)
            .is_some_and(|index| self.entries[index].kind == EntryKind::Ask)
    }

    pub(in crate::tui) fn tool_index(&self, id: &str) -> Option<usize> {
        self.running_tools
            .iter()
            .find(|(call, _)| call == id)
            .map(|(_, index)| *index)
    }

    pub(super) fn fail(&mut self) {
        self.background_bash.clear();
        if let Some(activity) = &mut self.activity {
            activity.stop("Failed");
        }
        if let Some(index) = self.assistant.take() {
            self.entries[index].finish_markdown(EntryStatus::Failed);
            self.entries[index].layout = None;
        }
        self.finish_thinking();
        for (_, index) in self.running_tools.drain(..) {
            self.read_groups_key = None;
            self.entries[index].status = EntryStatus::Failed;
            if self.entries[index].kind == EntryKind::Bash {
                let entry = &mut self.entries[index];
                entry.update_bash_footer(
                    entry
                        .started
                        .filter(|_| !self.replaying)
                        .map(|started| started.elapsed()),
                );
            } else if self.entries[index].kind == EntryKind::Eval {
                let entry = &mut self.entries[index];
                entry.update_eval_footer(
                    entry
                        .started
                        .filter(|_| !self.replaying)
                        .map(|started| started.elapsed()),
                );
            } else if self.entries[index].kind == EntryKind::Ask {
                self.entries[index].text = "Previous request was interrupted.".into();
            }
            self.entries[index].layout = None;
        }
    }

    pub(super) fn cancel(&mut self) {
        self.fail();
        if let Some(activity) = &mut self.activity {
            activity.label = "Cancelled".into();
            activity.refresh_summary();
        }
    }

    pub(super) fn page_up(&mut self) {
        self.scroll_up(self.viewport_height.max(1));
    }

    pub(super) fn toggle_tool_output(&mut self) {
        self.ensure_browse_anchor();
        self.tools_expanded = !self.tools_expanded;
        self.read_groups_key = None;
        for entry in &mut self.entries {
            if entry.is_collapsible_tool() {
                entry.expanded = self.tools_expanded;
                entry.layout = None;
            }
        }
        if self.browse_anchor.is_some() {
            self.prepare_layout(self.viewport_width);
            self.sync_geometry();
            self.resolve_browse_anchor();
        }
    }

    pub(super) fn page_down(&mut self) {
        self.scroll_down(self.viewport_height.max(1));
    }

    pub(super) fn scroll_up(&mut self, lines: usize) {
        self.offset = self.offset.saturating_sub(lines);
        self.follow_bottom = false;
        self.capture_browse_anchor();
    }

    pub(super) fn scroll_down(&mut self, lines: usize) {
        let bottom = self.total.saturating_sub(self.viewport_height);
        self.offset = self.offset.saturating_add(lines).min(bottom);
        self.follow_bottom = self.offset == bottom;
        self.capture_browse_anchor();
    }

    pub(super) fn entry_count(&self) -> usize {
        self.entries.len()
    }

    /// Position at the given entry; a hidden read group falls back to its first visible group. Returns whether positioning is possible.
    pub(super) fn reveal_entry(&mut self, index: usize) -> bool {
        if index >= self.entries.len() || self.viewport_width == 0 {
            return false;
        }
        self.prepare_layout(self.viewport_width);
        self.sync_geometry();
        let mut target = index;
        while target > 0 && self.entries[target].hidden {
            target -= 1;
        }
        let offset = self.geometry.start(target);
        let bottom = self.total.saturating_sub(self.viewport_height);
        self.offset = offset.min(bottom);
        self.follow_bottom = self.offset == bottom;
        self.capture_browse_anchor();
        true
    }

    /// After a width change, restore the visible anchor by "entry index + line offset within the entry"; stay at the bottom when following.
    /// Only the derived layout is re-laid out; entries and bodies are not rebuilt.
    pub(super) fn resize_preserving_anchor(&mut self, width: u16) {
        let width = width.max(1);
        if width == self.viewport_width {
            return;
        }
        self.ensure_browse_anchor();
        self.viewport_width = width;
        for entry in &mut self.entries {
            entry.layout = None;
        }
        if let Some(activity) = &mut self.activity {
            activity.entry.layout = None;
        }
        self.read_groups_key = None;
        self.prepare_layout(width);
        self.sync_geometry();
        self.resolve_browse_anchor();
    }

    pub(super) fn invalidate_theme(&mut self) {
        let current_icons = super::themes::current_icons();
        for entry in &mut self.entries {
            entry.layout = None;
            entry.display_title = None;
            entry.read_group.clear();
            entry.markdown_dirty = matches!(entry.kind, EntryKind::Assistant | EntryKind::Think);
            entry.render.invalidate(true);
            entry.read_clock = palette::waiting_clock(0);
            if let Some(tool) = entry.tool.as_deref() {
                let target_icon = palette::tool_icon_for(tool, current_icons);
                let other_icons = match current_icons {
                    pp_config::config::IconMode::Nerd => pp_config::config::IconMode::Ascii,
                    pp_config::config::IconMode::Ascii => pp_config::config::IconMode::Nerd,
                };
                let old_icon = palette::tool_icon_for(tool, other_icons);
                let old_prefix = format!("{old_icon} ");
                if let Some(rest) = entry.title.strip_prefix(&old_prefix) {
                    entry.title = format!("{target_icon} {rest}");
                }
            }
        }
        if let Some(activity) = &mut self.activity {
            activity.entry.layout = None;
            activity.entry.render.invalidate(true);
            activity.clock = palette::waiting_clock(0);
        }
        self.read_groups_key = None;
        self.tree_layout = None;
        self.native_generation = NEXT_NATIVE_GENERATION.fetch_add(1, Ordering::Relaxed);
        self.native_preview = None;
    }

    pub(super) fn draw(&mut self, frame: &mut Frame, area: Rect) {
        self.viewport_width = area.width;
        self.viewport_bottom = area.bottom();
        self.viewport_height = usize::from(area.height);
        if area.width == 0 || area.height == 0 {
            return;
        }
        self.ensure_browse_anchor();
        self.prepare_layout(area.width);
        self.sync_geometry();
        let bottom = self.total.saturating_sub(self.viewport_height);
        self.offset = if self.follow_bottom {
            bottom
        } else {
            self.resolve_browse_anchor();
            self.offset.min(bottom)
        };
        self.render_visible(frame.buffer_mut(), area);
    }

    /// Render the alternate-screen window starting at the first visible entry located by the geometry.
    fn render_visible(&self, buffer: &mut Buffer, area: Rect) {
        let Some((first, skip)) = self.geometry.locate(self.offset) else {
            return;
        };
        // Total slots = entries + the optional final summary slot; first beyond entries yields an empty slice,
        // and the summary slot follows directly without misalignment.
        let entries = &self.entries[first.min(self.entries.len())..];
        Self::render_entry_layouts(
            entries.iter().map(|entry| (entry, None, false)).chain(
                self.activity
                    .as_ref()
                    .and_then(Activity::summary)
                    .map(|entry| (entry, None, false)),
            ),
            buffer,
            area,
            skip,
            false,
        );
    }
}

#[cfg(test)]
mod tests;
