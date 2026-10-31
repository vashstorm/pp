use super::tool_display::{self, Context as DisplayContext};
use super::transcript_style as palette;
use crate::agent::persistence::ReplayRecord;
use crate::events::{AgentEvent, DiffLineKind, FileDiff, OutputStream};
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
    collections::VecDeque,
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

mod entry;
mod native;
mod recorded;
mod render;
mod scroll;
mod tree_layout;

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
const ASSISTANT_BACKGROUND: Color = Color::Rgb(35, 39, 50);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum EntryKind {
    User,
    Assistant,
    Bash,
    Think,
    Grep,
    Read,
    Diff,
    Ask,
    Data,
    Error,
    Activity,
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
    Activity(u128),
    Tips,
    Markdown(super::markdown::MarkdownDocument),
    Diff(FileDiff),
    Ask(AskBlock),
}

struct AskBlock {
    args: crate::interaction::AskArgs,
    answers: Option<Vec<crate::interaction::AskAnswer>>,
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
    started: Option<Instant>,
    bash_timeout_seconds: Option<u64>,
    arguments: Value,
    result: Option<Value>,
    display: DisplayContext,
    display_title: Option<String>,
    hidden: bool,
}

struct Activity {
    entry: TranscriptEntry,
    started: Option<Instant>,
    waiting_started: Option<Instant>,
    label: String,
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
    // 状态独立于历史条目，始终渲染在最新消息后面。
    activity: Option<Activity>,
    assistant: Option<usize>,
    thinking: Option<usize>,
    running_tools: Vec<(String, usize)>,
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
            bash_timeout_seconds: crate::config::ToolLimits::default().bash_timeout_seconds,
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

    pub(super) fn append_tips(&mut self) {
        let mut entry =
            TranscriptEntry::new(EntryKind::Data, String::new(), crate::tips::text(), None);
        entry.body = EntryBody::Tips;
        self.read_groups_key = None;
        self.entries.push(entry);
    }

    pub(super) fn begin_run(&mut self) {
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
        for (_, index) in &self.running_tools {
            let entry = &mut self.entries[*index];
            if entry.kind == EntryKind::Bash {
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
        let activity = self
            .activity
            .get_or_insert_with(|| Activity::new(Instant::now()));
        activity.started = None;
        activity.waiting_started = None;
        activity.running = false;
        activity.entry.body = EntryBody::Plain;
        activity.label = label.into();
        activity.seconds = elapsed.map(|elapsed| elapsed.as_secs());
        activity.entry.text = format!(
            "✻ {label} · {} · done {}",
            activity.duration_text(),
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
        if assistant_running || self.assistant.is_some() || !self.running_tools.is_empty() {
            self.fail();
            return Some("Previous request was interrupted.".into());
        }
        None
    }

    pub(super) fn append_tree_node(
        &mut self,
        records: Vec<ReplayRecord>,
        fallback: &str,
    ) -> Range<usize> {
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

    fn progress_phase(&mut self, event: &AgentEvent) {
        if let Some(activity) = &mut self.activity {
            match event {
                AgentEvent::ModelAttemptFailed { .. } => activity.phase("Model attempt failed"),
                AgentEvent::ModelRouteChanged { .. } => activity.phase("Switching to fallback"),
                AgentEvent::AssistantStarted => activity.phase("Requesting model"),
                AgentEvent::ThinkingDelta { .. } => activity.phase("Thinking"),
                AgentEvent::AssistantDelta { .. } => activity.phase("Responding"),
                AgentEvent::AssistantFinished { .. } => activity.phase("Processing response"),
                AgentEvent::ToolStarted { name, .. } => {
                    activity.phase(&format!("Running {}", name.replace(char::is_control, " ")))
                }
                AgentEvent::ToolFinished { .. } => activity.phase("Processing tool result"),
                AgentEvent::ToolOutput { .. } | AgentEvent::FileChanged { .. } => {}
                // 后台自动加载不改变活动阶段，不伪装成模型工具执行。
                AgentEvent::ProjectInstructionsLoaded { .. } => {}
                AgentEvent::TodoSnapshot { .. } | AgentEvent::TodoStateUpdated { .. } => {}
                // 提醒通知不改变活动阶段。
                AgentEvent::TodoReminder { .. } => {}
                // 运行时指标只服务状态栏，不改变对话阶段。
                AgentEvent::ModelRequestStarted
                | AgentEvent::RequestRetrying { .. }
                | AgentEvent::ModelOutputProgress { .. }
                | AgentEvent::ModelRequestFinished { .. } => {}
                // 运行时上下文管理：自动压缩开始切换阶段；用量更新不影响活动显示。
                AgentEvent::ContextUpdated { .. } | AgentEvent::PlanProgressUpdated { .. } => {}
                AgentEvent::CompactionStarted { .. } => activity.phase("Compacting context"),
                AgentEvent::CompactionFinished { .. } => {}
                // 用户输入确认由 App 消费；不重置活动阶段、不产生重复卡片。
                AgentEvent::UserInputAccepted { .. } => {}
            }
        }
    }

    pub(super) fn progress(&mut self, event: AgentEvent) {
        if !self.replaying {
            self.progress_phase(&event);
        }
        match event {
            // 用量更新不产生对话条目。
            AgentEvent::ContextUpdated { .. } | AgentEvent::PlanProgressUpdated { .. } => {}
            // 运行时指标只服务状态栏，不产生对话条目。
            AgentEvent::ModelRequestStarted
            | AgentEvent::RequestRetrying { .. }
            | AgentEvent::ModelOutputProgress { .. }
            | AgentEvent::ModelRequestFinished { .. } => {}
            // 已接受的补充输入由 App 直接追加，不生成重复用户卡。
            AgentEvent::UserInputAccepted { .. } => {}
            AgentEvent::CompactionStarted { .. } => {}
            // 完整任务只在固定区展示，旧日志的快照同样不生成历史卡片。
            AgentEvent::TodoSnapshot { .. } | AgentEvent::TodoStateUpdated { .. } => {}
            AgentEvent::ProjectInstructionsLoaded { path, content } => {
                // 自动加载的 AGENTS 正文：独立完成的 Read 卡片，不注册 running_tools。
                let mut entry = TranscriptEntry::new(
                    EntryKind::Read,
                    "Read AGENTS.md".into(),
                    content,
                    Some("read".into()),
                );
                entry.arguments = serde_json::json!({"path": "AGENTS.md"});
                entry.footer = format!("⟨Resolved path: {}⟩", palette::display_text(&path));
                entry.display = self.display;
                entry.expanded = self.tools_expanded;
                self.read_groups_key = None;
                self.entries.push(entry);
            }
            AgentEvent::CompactionFinished {
                before_tokens,
                after_tokens,
            } => {
                // 简短结果用现有 Note 样式追加，保留原对话。
                self.append(
                    "Note",
                    &format!("Context compacted: ~{before_tokens} → ~{after_tokens} tokens"),
                );
            }
            AgentEvent::TodoReminder {
                todos,
                attempt,
                max_attempts,
            } => {
                // 现有 Note 通知列出未完成项；replay 只重放通知，不触发请求。
                let open: Vec<&crate::tools::todo::TodoItem> = todos
                    .iter()
                    .filter(|item| {
                        !matches!(
                            item.status,
                            crate::tools::todo::TodoStatus::Completed
                                | crate::tools::todo::TodoStatus::Abandoned
                        )
                    })
                    .collect();
                let mut text = format!(
                    "Todo reminder {attempt}/{max_attempts}: {} incomplete todo item(s):",
                    open.len()
                );
                for item in &open {
                    text.push_str("\n- ");
                    text.push_str(&palette::display_text(&item.content));
                }
                self.append("Note", &text);
            }
            AgentEvent::ModelAttemptFailed { attempt } => {
                for index in [self.assistant.take(), self.thinking.take()]
                    .into_iter()
                    .flatten()
                {
                    self.entries[index].status = EntryStatus::Failed;
                }
                self.append(
                    "Error",
                    &format!(
                        "Model attempt {attempt} failed; partial output is excluded from history."
                    ),
                );
            }
            AgentEvent::ModelRouteChanged {
                model, fallback, ..
            } => {
                self.append(
                    "Note",
                    &format!(
                        "{}: {model}",
                        if fallback {
                            "Using fallback model"
                        } else {
                            "Restoring primary model"
                        }
                    ),
                );
            }
            AgentEvent::AssistantStarted => {
                self.assistant = None;
                self.thinking = None;
            }
            AgentEvent::ThinkingDelta { text } => {
                if text.is_empty() {
                    return;
                }
                if let Some(index) = self
                    .thinking
                    .filter(|index| *index + 1 == self.entries.len())
                {
                    let entry = &mut self.entries[index];
                    entry.text.push_str(&text);
                    entry.markdown_dirty = true;
                    entry.render.invalidate(false);
                    entry.layout = None;
                } else {
                    self.read_groups_key = None;
                    self.thinking = Some(self.entries.len());
                    self.entries.push(TranscriptEntry::new(
                        EntryKind::Think,
                        String::new(),
                        text,
                        None,
                    ));
                }
            }
            AgentEvent::AssistantDelta { text, refusal } => {
                if text.is_empty() {
                    return;
                }
                let index = self.assistant_entry();
                let entry = &mut self.entries[index];
                if refusal {
                    entry.refusal.push_str(&text);
                } else {
                    entry.text.push_str(&text);
                }
                entry.markdown_dirty = true;
                entry.render.invalidate(false);
                entry.layout = None;
            }
            AgentEvent::AssistantFinished { text } => {
                if text.trim().is_empty() {
                    if let Some(index) = self.assistant.take() {
                        self.read_groups_key = None;
                        self.entries.remove(index);
                    }
                } else {
                    let index = self.assistant_entry();
                    let entry = &mut self.entries[index];
                    if entry.visible_text() != text {
                        entry.render.invalidate(true);
                        entry.markdown_dirty = true;
                        entry.layout = None;
                    }
                    entry.text = text;
                    entry.refusal.clear();
                    entry.status = EntryStatus::Complete;
                }
                self.assistant = None;
                self.thinking = None;
            }
            AgentEvent::ToolStarted {
                id,
                name,
                arguments,
            } => {
                let ask_args = (name == "ask")
                    .then(|| crate::interaction::AskArgs::parse(&arguments).ok())
                    .flatten();
                let arguments = serde_json::from_str::<Value>(&arguments).ok();
                let detail = arguments.as_ref().and_then(|args| {
                    args.get(match name.as_str() {
                        "bash" => "command",
                        _ => "path",
                    })
                    .and_then(Value::as_str)
                    .or_else(|| {
                        (name == "edit")
                            .then(|| crate::tools::edit_path(args))
                            .flatten()
                    })
                });
                let kind = if matches!(name.as_str(), "bash" | "bash_task") {
                    EntryKind::Bash
                } else if name == "grep" {
                    EntryKind::Grep
                } else if matches!(name.as_str(), "edit" | "write") {
                    EntryKind::Diff
                } else if name == "ask" {
                    EntryKind::Ask
                } else if name == "read"
                    && arguments
                        .as_ref()
                        .is_some_and(|args| tool_display::is_skill_read(args, None))
                {
                    // 技能正文读取使用独立 Read 卡片；普通文件 read 保持 Data。
                    EntryKind::Read
                } else {
                    EntryKind::Data
                };
                let search_args = arguments
                    .as_ref()
                    .and_then(|args| args.get("args"))
                    .and_then(Value::as_array)
                    .filter(|args| args.iter().all(Value::is_string));
                let title = if name == "bash_task" {
                    let args = arguments.as_ref();
                    format!(
                        "bash_task · {} · {}",
                        args.and_then(|args| args["action"].as_str())
                            .unwrap_or("unknown"),
                        args.and_then(|args| args["task_id"].as_str())
                            .unwrap_or("unknown")
                    )
                } else if name == "todo" {
                    // 缺 op 时保持 Todo；完成后从结果修正推断操作。
                    match arguments.as_ref().and_then(|args| args["op"].as_str()) {
                        Some(op) => format!("Todo · {}", palette::display_text(op)),
                        None => "Todo".to_owned(),
                    }
                } else if matches!(name.as_str(), "grep" | "find") {
                    search_args.map_or_else(
                        || {
                            if name == "grep" {
                                let pattern = arguments
                                    .as_ref()
                                    .and_then(|a| a["pattern"].as_str())
                                    .unwrap_or("");
                                let path = arguments
                                    .as_ref()
                                    .and_then(|a| a["path"].as_str())
                                    .unwrap_or(".");
                                format!("grep · {pattern} · {path}")
                            } else {
                                name.clone()
                            }
                        },
                        |args| {
                            format!(
                                "{name} · {}",
                                serde_json::to_string(args).expect("JSON values serialize")
                            )
                        },
                    )
                } else {
                    match detail {
                        Some(command) if name == "bash" => format!("$ {command}"),
                        Some(path) if kind == EntryKind::Read => format!("Read {path}"),
                        Some(path) => format!("{name} · {path}"),
                        None => name.clone(),
                    }
                };
                let title = if matches!(kind, EntryKind::Diff | EntryKind::Ask | EntryKind::Read) {
                    palette::display_text(&title).into_owned()
                } else {
                    title
                };
                let title = if name == "bash" || kind == EntryKind::Read {
                    title
                } else {
                    format!("{} {title}", palette::tool_icon(&name))
                };
                let mut entry = TranscriptEntry::new(kind, title, String::new(), Some(name));
                entry.arguments = arguments.unwrap_or(Value::Null);
                if entry.tool.as_deref() == Some("bash") && entry.arguments["mode"] != "background"
                {
                    entry.bash_timeout_seconds = Some(self.bash_timeout_seconds);
                }
                entry.display = self.display;
                entry.expanded = self.tools_expanded;
                if let Some(args) = ask_args {
                    entry.body = EntryBody::Ask(AskBlock::new(args));
                }
                entry.status = EntryStatus::Running;
                self.read_groups_key = None;
                self.running_tools.push((id, self.entries.len()));
                self.entries.push(entry);
            }
            AgentEvent::FileChanged { id, path: _, diff } => {
                if let Some(index) = self.tool_index(&id) {
                    let entry = &mut self.entries[index];
                    if entry.kind == EntryKind::Diff {
                        entry.body = EntryBody::Diff(diff);
                        entry.layout = None;
                    }
                }
            }
            AgentEvent::ToolOutput { id, stream, text } => {
                if let Some(index) = self.tool_index(&id) {
                    let entry = &mut self.entries[index];
                    match stream {
                        OutputStream::Stdout => entry.stdout.push_str(&text),
                        OutputStream::Stderr => entry.stderr.push_str(&text),
                    }
                    entry.layout = None;
                    entry.display_title = None;
                    if entry.tool.as_deref() == Some("read") {
                        self.read_groups_key = None;
                    }
                }
            }
            AgentEvent::ToolFinished { id, result } => {
                if let Some(position) = self.running_tools.iter().position(|(call, _)| call == &id)
                {
                    let (_, index) = self.running_tools.remove(position);
                    let entry = &mut self.entries[index];
                    entry.status = if result.get("ok").and_then(Value::as_bool) == Some(false)
                        || (entry.kind == EntryKind::Bash
                            && (result["exit_code"].as_i64().is_some_and(|code| code != 0)
                                || result["timed_out"] == true
                                || result["cancelled"] == true))
                    {
                        EntryStatus::Failed
                    } else {
                        EntryStatus::Complete
                    };
                    if entry.tool.as_deref() == Some("todo")
                        && let Some(op) = result["op"].as_str()
                    {
                        // 成功结果携带真实操作；修正开始时无法推断的标题。
                        entry.title = format!(
                            "{} Todo · {}",
                            palette::tool_icon("todo"),
                            palette::display_text(op)
                        );
                        entry.layout = None;
                    }
                    entry.stdout.clear();
                    entry.stderr.clear();
                    if entry.tool.as_deref() == Some("read") {
                        // 开始时无法从参数识别的技能读取，完成后按结果来源兜底归类。
                        if entry.kind != EntryKind::Read
                            && tool_display::is_skill_read(&entry.arguments, Some(&result))
                        {
                            entry.kind = EntryKind::Read;
                            // 此前可能作为普通 read 组成员被隐藏，归类后必须独立可见。
                            entry.hidden = false;
                            if let Some(source) = result["source"]
                                .as_str()
                                .filter(|source| !source.is_empty())
                            {
                                entry.title = format!("Read {}", palette::display_text(source));
                            }
                        }
                        if entry.kind == EntryKind::Read
                            && let Some(path) =
                                result["path"].as_str().filter(|path| !path.is_empty())
                        {
                            entry.footer =
                                format!("⟨Resolved path: {}⟩", palette::display_text(path));
                        }
                    }
                    if let EntryBody::Ask(block) = &mut entry.body {
                        if !block.finish(&result) {
                            entry.body = EntryBody::Plain;
                            entry.status = EntryStatus::Failed;
                            entry.text = "invalid ask response".into();
                        } else {
                            entry.text = if entry.status == EntryStatus::Failed {
                                tool_result(entry.kind, "ask", &result)
                            } else {
                                String::new()
                            };
                        }
                    } else {
                        entry.text = if matches!(entry.body, EntryBody::Diff(_))
                            && entry.status == EntryStatus::Complete
                        {
                            result
                                .get("warning")
                                .and_then(Value::as_str)
                                .map(|warning| format!("Warning: {warning}"))
                                .unwrap_or_default()
                        } else {
                            tool_result(
                                entry.kind,
                                entry.tool.as_deref().unwrap_or_default(),
                                &result,
                            )
                        };
                    }
                    entry.result = Some(result);
                    if entry.kind == EntryKind::Bash {
                        entry.update_bash_footer(
                            entry
                                .started
                                .filter(|_| !self.replaying)
                                .map(|started| started.elapsed()),
                        );
                    }
                    entry.layout = None;
                    entry.display_title = None;
                    if entry.tool.as_deref() == Some("read") {
                        self.read_groups_key = None;
                    }
                }
            }
        }
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

    pub(super) fn has_running_ask(&self, id: &str) -> bool {
        self.tool_index(id)
            .is_some_and(|index| self.entries[index].kind == EntryKind::Ask)
    }

    fn tool_index(&self, id: &str) -> Option<usize> {
        self.running_tools
            .iter()
            .find(|(call, _)| call == id)
            .map(|(_, index)| *index)
    }

    pub(super) fn fail(&mut self) {
        if let Some(activity) = &mut self.activity {
            activity.stop("Failed");
        }
        if let Some(index) = self.assistant.take() {
            self.entries[index].status = EntryStatus::Failed;
            self.entries[index].layout = None;
        }
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
            activity.refresh();
        }
        self.thinking = None;
        self.append("Note", crate::cancellation::CANCELLED_NOTE);
    }

    pub(super) fn page_up(&mut self) {
        self.scroll_up(self.viewport_height.max(1));
    }

    pub(super) fn toggle_tool_output(&mut self) {
        self.ensure_browse_anchor();
        self.tools_expanded = !self.tools_expanded;
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

pub(super) fn block_line(
    text: impl Into<std::borrow::Cow<'static, str>>,
    style: Style,
) -> Line<'static> {
    Line::from(Span::styled(text, style))
}

/// Wrap styled graphemes once; each output is one clipped terminal row.
pub(super) fn wrap_block(
    line: Line<'static>,
    width: u16,
    continuation: &str,
) -> Vec<Line<'static>> {
    let width = usize::from(width.max(1));
    let mut rows = Vec::new();
    let mut spans = Vec::new();
    let mut used = 0;
    let mut has_content = false;
    let mut ended_newline = false;
    for span in line.spans {
        let safe = palette::display_text(&span.content);
        let style = line.style.patch(span.style);
        for part in safe.split_inclusive(['\n', '\t']) {
            let (plain, separator) = if let Some(text) = part.strip_suffix('\n') {
                (text, Some("\n"))
            } else if let Some(text) = part.strip_suffix('\t') {
                (text, Some("\t"))
            } else {
                (part, None)
            };
            let text = Span::raw(plain);
            for (symbol, style) in text
                .styled_graphemes(style)
                .map(|grapheme| (grapheme.symbol, grapheme.style))
                .chain(separator.map(|symbol| (symbol, style)))
            {
                if symbol == "\n" {
                    rows.push(Line::from(std::mem::take(&mut spans)));
                    used = 0;
                    has_content = false;
                    ended_newline = true;
                    continue;
                }
                ended_newline = false;
                let repeats = if symbol == "\t" { 4 - used % 4 } else { 1 };
                let value = if symbol == "\t" { " " } else { symbol };
                let columns = Line::raw(value).width();
                for _ in 0..repeats {
                    if has_content && used + columns > width {
                        rows.push(Line::from(std::mem::take(&mut spans)));
                        let prefix = if Line::raw(continuation).width() + columns <= width {
                            continuation
                        } else {
                            ""
                        };
                        if !prefix.is_empty() {
                            spans.push(Span::styled(
                                prefix.to_owned(),
                                Style::default().fg(palette::DIM),
                            ));
                        }
                        used = Line::raw(prefix).width();
                    }
                    used += columns;
                    has_content = true;
                    if let Some(previous) =
                        spans.last_mut().filter(|previous| previous.style == style)
                    {
                        previous.content.to_mut().push_str(value);
                    } else {
                        spans.push(Span::styled(value.to_owned(), style));
                    }
                }
            }
        }
    }
    if !spans.is_empty() || rows.is_empty() || ended_newline {
        rows.push(Line::from(spans));
    }
    rows
}

fn diff_line_text(text: &str) -> (&str, &'static str) {
    if let Some(text) = text.strip_suffix("\r\n") {
        (text, "CRLF")
    } else if let Some(text) = text.strip_suffix('\n') {
        (text, "LF")
    } else {
        (text, "No newline")
    }
}

pub(super) fn diff_layout(diff: &FileDiff, width: u16, expanded: bool) -> Vec<Line<'static>> {
    diff_layout_styled(diff, width, expanded, None)
}

fn diff_layout_styled(
    diff: &FileDiff,
    width: u16,
    expanded: bool,
    edit_path: Option<&str>,
) -> Vec<Line<'static>> {
    let mut rows = Vec::new();
    let FileDiff::Text {
        created,
        added,
        removed,
        hunks,
    } = diff
    else {
        if let FileDiff::Unavailable { reason } = diff {
            return wrap_block(
                block_line(reason.clone(), Style::default().fg(palette::MUTED)),
                width,
                "",
            );
        }
        unreachable!();
    };
    if edit_path.is_none() {
        rows.extend(wrap_block(
            Line::from(vec![
                Span::styled(format!("+{added}"), Style::default().fg(palette::ADDED)),
                Span::raw(" "),
                Span::styled(format!("-{removed}"), Style::default().fg(palette::REMOVED)),
            ]),
            width,
            "",
        ));
    }
    let summary_rows = rows.len();
    let total: usize = hunks.iter().map(|hunk| hunk.lines.len()).sum();
    if total == 0 {
        rows.extend(wrap_block(
            block_line(
                if *created {
                    "Created empty file."
                } else {
                    "No content changes."
                },
                Style::default().fg(palette::MUTED),
            ),
            width,
            "",
        ));
        return rows;
    }
    let max_number = hunks
        .iter()
        .flat_map(|hunk| &hunk.lines)
        .flat_map(|line| [line.old_line, line.new_line])
        .flatten()
        .max()
        .unwrap_or(0);
    let gutter = max_number.to_string().len().max(3);
    let continuation = " ".repeat(gutter + if edit_path.is_some() { 2 } else { 3 });
    let mut shown = 0;
    for (hunk_index, hunk) in hunks.iter().enumerate() {
        if !expanded
            && (shown >= tool_display::DIFF_LINES || hunk_index >= tool_display::DIFF_HUNKS)
        {
            break;
        }
        if hunk_index > 0 {
            rows.push(Line::styled("…", Style::default().fg(palette::DIM)));
        }
        let highlights = edit_path.map(|_| super::edit_diff::highlights(&hunk.lines));
        // 连续替换段中的行尾变化可能仅靠正文无法区分。
        let mut eol_hints = std::collections::HashSet::new();
        let mut start = 0;
        while start < hunk.lines.len() {
            if hunk.lines[start].kind == DiffLineKind::Context {
                start += 1;
                continue;
            }
            let end = (start..hunk.lines.len())
                .find(|&i| hunk.lines[i].kind == DiffLineKind::Context)
                .unwrap_or(hunk.lines.len());
            let mut endings: std::collections::HashMap<&str, (u8, u8)> =
                std::collections::HashMap::new();
            let ending_bit = |eol| match eol {
                "LF" => 1,
                "CRLF" => 2,
                _ => 4,
            };
            for line in &hunk.lines[start..end] {
                let (text, eol) = diff_line_text(&line.text);
                let entry = endings.entry(text).or_default();
                match line.kind {
                    DiffLineKind::Removed => entry.0 |= ending_bit(eol),
                    DiffLineKind::Added => entry.1 |= ending_bit(eol),
                    DiffLineKind::Context => {}
                }
            }
            for (offset, line) in hunk.lines[start..end].iter().enumerate() {
                let (text, eol) = diff_line_text(&line.text);
                let (old, new) = endings[text];
                let opposite = if line.kind == DiffLineKind::Added {
                    old
                } else {
                    new
                };
                if opposite & !ending_bit(eol) != 0 {
                    eol_hints.insert(start + offset);
                }
            }
            start = end;
        }
        for (index, line) in hunk.lines.iter().enumerate() {
            if !expanded && shown >= tool_display::DIFF_LINES {
                break;
            }
            shown += 1;
            let (text, eol) = diff_line_text(&line.text);
            let (mark, color, number) = match line.kind {
                DiffLineKind::Added => (
                    "+",
                    if edit_path.is_some() {
                        palette::EDIT_ADDED
                    } else {
                        palette::ADDED
                    },
                    line.new_line,
                ),
                DiffLineKind::Removed => (
                    "-",
                    if edit_path.is_some() {
                        palette::EDIT_REMOVED
                    } else {
                        palette::REMOVED
                    },
                    line.old_line,
                ),
                DiffLineKind::Context => (" ", palette::MUTED, line.new_line),
            };
            let number = number.map(|number| number.to_string()).unwrap_or_default();
            let mut spans = if let Some(path) = edit_path {
                let mut spans = vec![Span::styled(
                    format!("{mark}{number:>gutter$}│"),
                    Style::default()
                        .fg(if line.kind == DiffLineKind::Context {
                            palette::EDIT_META
                        } else {
                            color
                        })
                        .bg(palette::EDIT_BG),
                )];
                spans.extend(super::edit_diff::spans(
                    text,
                    path,
                    line.kind,
                    highlights.as_ref().and_then(|ranges| ranges.get(&index)),
                ));
                spans
            } else {
                vec![
                    Span::styled(
                        format!("{number:>gutter$} {mark} "),
                        Style::default().fg(palette::DIM),
                    ),
                    Span::styled(text.to_owned(), Style::default().fg(color)),
                ]
            };
            if text.starts_with('\u{feff}') {
                spans.push(Span::styled("  BOM", Style::default().fg(palette::DIM)));
            }
            if eol_hints.contains(&index) {
                spans.push(Span::styled(
                    format!("  {eol}"),
                    Style::default().fg(palette::DIM),
                ));
            }
            rows.extend(wrap_block(Line::from(spans), width, &continuation));
            if eol == "No newline" && line.kind != DiffLineKind::Context {
                rows.extend(wrap_block(
                    block_line(
                        "No newline at end of file",
                        Style::default().fg(palette::DIM),
                    ),
                    width,
                    &continuation,
                ));
            }
        }
    }
    // 第二道限制按实际换行后的屏幕行计算，统计行不占 diff 正文额度。
    let mut visual_hidden = 0;
    let visual_limit = summary_rows + tool_display::DIFF_LINES;
    if !expanded && rows.len() > visual_limit {
        visual_hidden = rows.len() - visual_limit;
        rows.truncate(visual_limit);
    }
    if shown < total || visual_hidden > 0 {
        rows.extend(wrap_block(
            block_line(
                format!(
                    "… {} more lines (Ctrl + O to expand)",
                    total - shown + visual_hidden
                ),
                Style::default().fg(palette::DIM),
            ),
            width,
            "",
        ));
    }
    rows
}

fn background_status(result: &Value) -> Option<String> {
    Some(format!(
        "Task {}: {}",
        result["task_id"].as_str()?,
        result["status"].as_str()?
    ))
}

fn tool_result(kind: EntryKind, name: &str, result: &Value) -> String {
    if matches!(name, "grep" | "glob")
        && let Some(content) = result["content"].as_str()
    {
        return content.to_owned();
    }
    if name == "todo" {
        // 失败正文已按“诊断 → 可信调用前列表”排列；缺 content 时退回诊断。
        if let Some(content) = result["content"].as_str() {
            return content.to_owned();
        }
        if let Some(error) = result.get("error") {
            return error
                .as_str()
                .map_or_else(|| error.to_string(), str::to_owned);
        }
        return String::new();
    }
    if kind == EntryKind::Bash || matches!(name, "grep" | "find") {
        let mut lines = Vec::new();
        // 后台状态已在 Bash footer 展示，不占用输出正文和尾窗行数。
        for (field, label) in [("stdout", "stdout"), ("stderr", "stderr")] {
            if let Some(text) = result
                .get(field)
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
            {
                lines.push(if field == "stdout" {
                    text.strip_suffix('\n').unwrap_or(text).to_owned()
                } else {
                    format!("[{label}]\n{}", text.strip_suffix('\n').unwrap_or(text))
                });
            }
            if result
                .get(format!("{field}_truncated"))
                .and_then(Value::as_bool)
                == Some(true)
            {
                lines.push(format!("[{field} truncated by output limits]"));
            }
        }
        if let Some(error) = result.get("error") {
            lines.push(
                error
                    .as_str()
                    .map_or_else(|| error.to_string(), str::to_owned),
            );
        }
        if let Some(warning) = result.get("warning").and_then(Value::as_str) {
            lines.push(format!("Warning: {warning}"));
        }
        return lines.join("\n");
    }
    if name.starts_with("mcp__")
        && let Some(blocks) = result["result"]["content"].as_array()
    {
        let text = blocks
            .iter()
            .filter(|block| block["type"] == "text")
            .filter_map(|block| block["text"].as_str())
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        if !text.is_empty() {
            return text;
        }
    }
    if result.get("ok").and_then(Value::as_bool) == Some(false) {
        if let Some(error) = result.get("error") {
            return error
                .as_str()
                .map_or_else(|| error.to_string(), str::to_owned);
        }
    } else if name == "read" {
        if let Some(content) = result.get("content").and_then(Value::as_str) {
            return content.into();
        }
    } else if name == "write"
        && let Some(bytes) = result.get("bytes_written").and_then(Value::as_u64)
    {
        return format!("Wrote {bytes} bytes");
    }
    serde_json::to_string_pretty(result).unwrap_or_else(|_| result.to_string())
}

#[cfg(test)]
mod tests;
