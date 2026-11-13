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
    collections::{BTreeMap, VecDeque},
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

mod bash_tasks;
mod diff;
mod entry;
mod entry_ask;
mod entry_layout;
mod entry_title;
mod entry_tool;
mod native;
mod preview;
mod progress;
mod recorded;
mod render;
mod scroll;
mod tool_result;
mod tree_layout;
mod wrap;

pub(super) use diff::{diff_layout, diff_layout_styled};
use tool_result::{background_status, tool_result};
pub(super) use wrap::{block_line, visit_wrapped_rows, wrap_block};

static NEXT_NATIVE_GENERATION: AtomicU64 = AtomicU64::new(1);

/// 已提交的源位置与仍留在屏幕上的冻结行；不复制完整历史。
#[derive(Clone, Default)]
struct NativeState {
    entry: usize,
    row: usize,
    retained: VecDeque<Vec<ratatui::buffer::Cell>>,
    revision: u64,
    width: u16,
    commit_layout: Option<Arc<EntryLayout>>,
    commit_full_rows: bool,
}

pub(super) struct NativeBatch {
    generation: u64,
    revision: u64,
    next: NativeState,
    pub rows: Vec<Vec<ratatui::buffer::Cell>>,
}

const BASH_BACKGROUND: Color = palette::TOOL_BG;
const BASH_BORDER: Color = palette::TOOL_BORDER;
const MUTED_TEXT: Color = Color::Rgb(110, 117, 129);
const USER_BACKGROUND: Color = Color::Rgb(48, 42, 63);
const ASSISTANT_BACKGROUND: Color = palette::MD_BG;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum EntryKind {
    User,
    Assistant,
    Bash,
    Think,
    Rg,
    Diff,
    Ask,
    Data,
    Error,
    /// 醒目提示卡片：黄色边框，用于会改变后续请求的运行时切换（如备用模型）。
    Notice,
    Activity,
    /// LSP 专用卡片：内联工具结果或迟到的独立诊断。
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
}

pub(super) enum LayoutContent {
    Source {
        source: Source,
        range: Range<usize>,
        title_end: bool,
    },
    Styled(Line<'static>),
}

pub(super) struct LayoutLine {
    pub(super) content: LayoutContent,
    pub(super) height: usize,
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
    /// LSP 工具卡片：可读正文与按路径累积的多文件变更。
    Lsp(super::lsp::Block),
}

struct AskBlock {
    args: pp_core::interaction::AskArgs,
    answers: Option<Vec<pp_core::interaction::AskAnswer>>,
    previews: Vec<Vec<Option<super::markdown::MarkdownDocument>>>,
}

pub(super) struct EntryLayout {
    pub(super) width: u16,
    pub(super) height: usize,
    pub(super) lines: Vec<LayoutLine>,
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
    layout: Option<Arc<EntryLayout>>,
    expanded: bool,
    footer: String,
    /// 非零阶段/观测不完整等警示，绘制为底部边框的 warning 色 span。
    footer_warning: String,
    /// 有管道时保留有界元数据。
    pipes_badge: Option<PipesBadge>,
    /// 运行中的阶段正文；与命令真实输出独立，完成时丢弃。
    pipeline_preview: Option<String>,
    shell: Option<pp_core::shell_display::ShellPresentation>,
    started: Option<Instant>,
    bash_timeout_seconds: Option<u64>,
    read_clock: &'static str,
    arguments: Value,
    result: Option<Value>,
    /// 内联或迟到报告；每个路径保留最新一份，按出现顺序展示在正文之后。
    diagnostics: Vec<pp_core::lsp_report::DiagnosticReport>,
    display: DisplayContext,
    display_title: Option<Vec<Line<'static>>>,
    hidden: bool,
}

/// Bash 卡片的管道入口：只保留有界元数据，不复制阶段正文。
#[derive(Clone, PartialEq, Eq)]
struct PipesBadge {
    status: pp_runtime::tools::output::PipesStatus,
    /// 恢复的历史结果不再保留临时正文，仅保留阶段状态。
    restored: bool,
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
    native_generation: u64,
    native: NativeState,
    native_preview: Option<NativeState>,
    entries: Vec<TranscriptEntry>,
    read_groups_key: Option<(usize, bool, bool)>,
    // 运行阶段显示在输入框上方，结束摘要显示在对话尾部。
    activity: Option<Activity>,
    assistant: Option<usize>,
    thinking: Option<usize>,
    running_tools: Vec<(String, usize)>,
    argument_previews: BTreeMap<u32, usize>,
    // 同一轮用户任务中的后台进程共用原始 Bash 卡片。
    background_bash: BTreeMap<String, usize>,
    offset: usize,
    browse_anchor: Option<BrowseAnchor>,
    // 上一帧的几何信息独立保留，流式事件使 layout 失效后仍能处理滚轮。
    displayed_heights: Vec<usize>,
    total: usize,
    viewport_height: usize,
    viewport_width: u16,
    viewport_bottom: u16,
    follow_bottom: bool,
    tools_expanded: bool,
    display: DisplayContext,
    // 重放恢复时不伪造 bash 卡片的 wall duration。
    replaying: bool,
    bash_timeout_seconds: u64,
    tree_layout: Option<TreeLayout>,
}

impl Transcript {
    pub(super) fn new() -> Self {
        Self {
            native_generation: NEXT_NATIVE_GENERATION.fetch_add(1, Ordering::Relaxed),
            follow_bottom: true,
            bash_timeout_seconds: pp_config::config::ToolLimits::default().bash_timeout_seconds,
            ..Self::default()
        }
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

    pub(super) fn fresh(&self) -> Self {
        Self {
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
        self.offset = self.total.saturating_sub(self.viewport_height);
        self.follow_bottom = true;
        self.browse_anchor = None;
        self.remember_heights();
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
        self.read_groups_key = None;
        self.entries
            .push(TranscriptEntry::new(kind, title, text.into(), None));
    }

    /// 本地错误仅进入可见历史，不生成模型消息或持久事件。
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

    /// 需要用户立刻注意的运行时提示：卡片形式，不进入模型消息。
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

    /// 主会话和观察日志共用完成状态；缺失的历史时间保持未知。
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

    /// 按日志顺序恢复可见对话，仅报告未完成请求，不重放旧错误提醒。
    /// 输入回忆是独立持久数据：回放只重建可见正文，不生成输入记录。
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
        // 树节点独立预览，不能让后一个节点改写前一个节点的卡片。
        self.background_bash.clear();
        let start = self.entries.len();
        self.replay(records);
        self.read_groups_key = None;
        // 树预览使用完整展示，不受交互预览配置或额度影响。
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

    /// 仅用于验证完整查看器的投影，不是生产环境的退出输出路径。
    #[cfg(test)]
    pub(super) fn write_full_for_test(
        &mut self,
        writer: &mut impl Write,
        snapshot: &Buffer,
    ) -> io::Result<()> {
        let footer_y = self.viewport_bottom;
        self.display.full = true;
        // 完整查看器使用原始保留输出，独立于主屏幕历史的折叠快照。
        for entry in &mut self.entries {
            if entry.is_collapsible_tool() && !entry.expanded {
                entry.expanded = true;
                entry.layout = None;
            }
        }
        self.prepare_layout(snapshot.area.width);
        // 分块回放全部历史，避免把整个会话分配成一个巨大的屏幕缓冲区。
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
            self.resolve_browse_anchor();
            self.remember_heights();
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

    /// 定位到指定条目；隐藏的 read 组落到其可见组首。返回是否可用。
    pub(super) fn reveal_entry(&mut self, index: usize) -> bool {
        if index >= self.entries.len() || self.viewport_width == 0 {
            return false;
        }
        self.prepare_layout(self.viewport_width);
        let mut target = index;
        while target > 0 && self.entries[target].hidden {
            target -= 1;
        }
        let offset = self.entries[..target]
            .iter()
            .filter_map(|entry| entry.layout.as_ref())
            .filter(|layout| layout.height > 0)
            .map(|layout| layout.height + 1)
            .sum::<usize>();
        let bottom = self.total.saturating_sub(self.viewport_height);
        self.offset = offset.min(bottom);
        self.follow_bottom = self.offset == bottom;
        self.remember_heights();
        self.capture_browse_anchor();
        true
    }

    /// 换宽后按"条目索引 + 条目内行偏移"恢复可见锚点；follow 时保持底部。
    /// 只重排派生布局，不重建条目或正文。
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
        self.resolve_browse_anchor();
        self.remember_heights();
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
        let bottom = self.total.saturating_sub(self.viewport_height);
        self.offset = if self.follow_bottom {
            bottom
        } else {
            self.resolve_browse_anchor();
            self.offset.min(bottom)
        };
        self.render(frame.buffer_mut(), area, self.offset);
        self.remember_heights();
    }
}

#[cfg(test)]
mod tests;
