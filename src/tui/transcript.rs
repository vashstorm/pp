use crate::agent::persistence::ReplayRecord;
use crate::events::{AgentEvent, OutputStream};
use ratatui::{
    Frame,
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget, Wrap},
};
use serde_json::Value;
use std::{
    io::{self, Write},
    ops::Range,
    time::{Duration, Instant},
};

const BASH_BACKGROUND: Color = Color::Rgb(22, 27, 33);
const BASH_BORDER: Color = Color::Rgb(76, 85, 98);
const MUTED_TEXT: Color = Color::Rgb(110, 117, 129);
const USER_BACKGROUND: Color = Color::Rgb(48, 42, 63);
const ASSISTANT_BACKGROUND: Color = Color::Rgb(35, 39, 50);
const BASH_PREVIEW_LINES: usize = 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum EntryKind {
    User,
    Assistant,
    Bash,
    Think,
    Grep,
    Skill,
    Data,
    Activity,
}

impl EntryKind {
    fn color(self) -> Color {
        match self {
            Self::User => Color::Blue,
            Self::Assistant => Color::Reset,
            Self::Bash => Color::Yellow,
            Self::Think | Self::Activity => MUTED_TEXT,
            Self::Grep => Color::Cyan,
            Self::Skill => Color::Magenta,
            Self::Data => Color::Reset,
        }
    }

    fn conversation_background(self) -> Option<Color> {
        match self {
            Self::User => Some(USER_BACKGROUND),
            Self::Assistant | Self::Think => Some(ASSISTANT_BACKGROUND),
            _ => None,
        }
    }

    fn hides_title(self) -> bool {
        matches!(
            self,
            Self::User | Self::Assistant | Self::Think | Self::Activity
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum EntryStatus {
    Running,
    Complete,
    Failed,
}

#[derive(Clone, Copy)]
enum Source {
    Top,
    Divider,
    Bottom,
    Padding,
    Elision,
    Footer,
    Title,
    Text,
    StdoutLabel,
    Stdout,
    StderrLabel,
    Stderr,
}

struct LayoutLine {
    source: Source,
    range: Range<usize>,
    height: usize,
    title_end: bool,
}

struct EntryLayout {
    width: u16,
    height: usize,
    lines: Vec<LayoutLine>,
}

struct TranscriptEntry {
    kind: EntryKind,
    status: EntryStatus,
    title: String,
    text: String,
    refusal: String,
    stdout: String,
    stderr: String,
    tool: Option<String>,
    layout: Option<EntryLayout>,
    expanded: bool,
    elision: String,
    footer: String,
    started: Instant,
}

impl TranscriptEntry {
    fn new(kind: EntryKind, title: String, text: String, tool: Option<String>) -> Self {
        Self {
            kind,
            status: EntryStatus::Complete,
            title,
            text,
            refusal: String::new(),
            stdout: String::new(),
            stderr: String::new(),
            tool,
            layout: None,
            expanded: false,
            elision: String::new(),
            footer: String::new(),
            started: Instant::now(),
        }
    }

    fn visible_text(&self) -> &str {
        if self.text.trim().is_empty() && !self.refusal.is_empty() {
            &self.refusal
        } else {
            &self.text
        }
    }

    fn source(&self, source: Source) -> &str {
        match source {
            Source::Top | Source::Divider | Source::Bottom | Source::Padding => "",
            Source::Elision => &self.elision,
            Source::Footer => &self.footer,
            Source::Title => &self.title,
            Source::Text => self.visible_text(),
            Source::StdoutLabel => "[stdout]",
            Source::Stdout if self.kind == EntryKind::Bash => {
                self.stdout.strip_suffix('\n').unwrap_or(&self.stdout)
            }
            Source::Stdout => &self.stdout,
            Source::StderrLabel => "[stderr]",
            Source::Stderr => &self.stderr,
        }
    }

    fn line(&self, line: &LayoutLine) -> Line<'_> {
        let text = &self.source(line.source)[line.range.clone()];
        if self.kind == EntryKind::Activity {
            return Line::styled(text, Style::default().fg(MUTED_TEXT));
        }
        if self.kind == EntryKind::Bash {
            let color = if matches!(line.source, Source::Title) {
                Color::Rgb(190, 188, 139)
            } else if matches!(line.source, Source::Footer) && self.status == EntryStatus::Failed {
                Color::Red
            } else {
                MUTED_TEXT
            };
            return Line::styled(text, Style::default().fg(color).bg(BASH_BACKGROUND));
        }
        if self.kind == EntryKind::Skill {
            return Line::styled(
                text,
                Style::default().fg(
                    if matches!(line.source, Source::Footer) && self.status == EntryStatus::Failed {
                        Color::Red
                    } else {
                        Color::Reset
                    },
                ),
            );
        }
        if self.kind == EntryKind::Think {
            return Line::styled(
                text,
                Style::default().fg(MUTED_TEXT).bg(ASSISTANT_BACKGROUND),
            );
        }
        if matches!(line.source, Source::Title) {
            let mut spans = vec![Span::styled(
                text,
                Style::default()
                    .fg(self.kind.color())
                    .add_modifier(Modifier::BOLD),
            )];
            if self.tool.is_some() && line.title_end {
                let (status, color) = match self.status {
                    EntryStatus::Running => (" · running", Color::Yellow),
                    EntryStatus::Complete => (" · ok", Color::Green),
                    EntryStatus::Failed => (" · failed", Color::Red),
                };
                spans.push(Span::styled(status, Style::default().fg(color)));
            }
            Line::from(spans)
        } else {
            let style = if self.tool.is_some() {
                Style::default().add_modifier(Modifier::DIM)
            } else {
                Style::default()
            };
            Line::styled(text, style)
        }
    }

    fn ensure_layout(&mut self, width: u16) {
        if self
            .layout
            .as_ref()
            .is_some_and(|layout| layout.width == width)
        {
            return;
        }
        let mut lines = Vec::new();
        let bash = self.kind == EntryKind::Bash;
        let skill = self.kind == EntryKind::Skill;
        let user = self.kind == EntryKind::User;
        let border = |source| LayoutLine {
            source,
            range: 0..0,
            height: 1,
            title_end: false,
        };
        if bash || skill {
            lines.push(border(Source::Top));
        }
        if user {
            lines.push(border(Source::Padding));
        }
        if !self.kind.hides_title() && !skill {
            self.add_lines(Source::Title, &mut lines);
        }
        if bash {
            lines.push(border(Source::Divider));
        }
        let output_start = lines.len();
        if !self.visible_text().is_empty() {
            self.add_lines(Source::Text, &mut lines);
        }
        if !self.stdout.is_empty() {
            if !bash {
                self.add_lines(Source::StdoutLabel, &mut lines);
            }
            self.add_lines(Source::Stdout, &mut lines);
        }
        if !self.stderr.is_empty() {
            self.add_lines(Source::StderrLabel, &mut lines);
            self.add_lines(Source::Stderr, &mut lines);
        }
        let count = lines.len() - output_start;
        if bash && !self.expanded && count > BASH_PREVIEW_LINES {
            let omitted = count - BASH_PREVIEW_LINES;
            lines.drain(output_start..output_start + omitted);
            self.elision = format!(
                "… ({omitted} earlier lines, showing {BASH_PREVIEW_LINES} of {count}) (Ctrl+O: Expand)"
            );
            lines.insert(
                output_start,
                LayoutLine {
                    source: Source::Elision,
                    range: 0..self.elision.len(),
                    height: 0,
                    title_end: false,
                },
            );
        }
        if skill && !self.expanded && count > BASH_PREVIEW_LINES {
            let omitted = count - BASH_PREVIEW_LINES;
            lines.truncate(output_start + BASH_PREVIEW_LINES);
            self.elision = format!("… {omitted} more lines (Ctrl-O to expand)");
            lines.push(LayoutLine {
                source: Source::Elision,
                range: 0..self.elision.len(),
                height: 0,
                title_end: false,
            });
        }
        if skill {
            self.footer = match self.status {
                EntryStatus::Running => "Loading",
                EntryStatus::Complete => "Loaded",
                EntryStatus::Failed => "Load failed",
            }
            .into();
            self.add_lines(Source::Footer, &mut lines);
            lines.push(border(Source::Bottom));
        }
        if bash {
            if self.footer.is_empty() {
                self.footer = "⟨running⟩".into();
            }
            self.add_lines(Source::Footer, &mut lines);
            lines.push(border(Source::Bottom));
        }
        if user {
            lines.push(border(Source::Padding));
        }
        let mut height = 0;
        for line in &mut lines {
            line.height = if self.kind == EntryKind::Activity {
                1
            } else {
                Paragraph::new(self.line(line))
                    .wrap(Wrap { trim: false })
                    .line_count(width)
                    .max(1)
            };
            height += line.height;
        }
        self.layout = Some(EntryLayout {
            width,
            height,
            lines,
        });
    }

    fn add_lines(&self, source: Source, lines: &mut Vec<LayoutLine>) {
        let text = self.source(source);
        let mut start = 0;
        for logical in text.split('\n') {
            // Bound a Paragraph's local offset independently of the transcript's usize
            // offset. Grapheme boundaries preserve CJK, emoji, and combining marks.
            let mut chunk_start = start;
            if logical.len() > 8192 {
                let mut bytes = 0;
                let mut graphemes = 0;
                let logical_line = Line::raw(logical);
                for grapheme in logical_line.styled_graphemes(Style::default()) {
                    if graphemes == 8192 {
                        lines.push(LayoutLine {
                            source,
                            range: chunk_start..start + bytes,
                            height: 0,
                            title_end: false,
                        });
                        chunk_start = start + bytes;
                        graphemes = 0;
                    }
                    bytes += grapheme.symbol.len();
                    graphemes += 1;
                }
            }
            lines.push(LayoutLine {
                source,
                range: chunk_start..start + logical.len(),
                height: 0,
                title_end: matches!(source, Source::Title) && start + logical.len() == text.len(),
            });
            start += logical.len() + 1;
        }
    }
}

struct Activity {
    entry: TranscriptEntry,
    started: Option<Instant>,
    label: String,
    seconds: u64,
}

impl Activity {
    fn new(started: Instant) -> Self {
        Self {
            entry: TranscriptEntry::new(
                EntryKind::Activity,
                String::new(),
                "✻ Requesting model · 0s".into(),
                None,
            ),
            started: Some(started),
            label: "Requesting model".into(),
            seconds: 0,
        }
    }

    fn tick(&mut self, now: Instant) -> bool {
        let Some(started) = self.started else {
            return false;
        };
        let seconds = now.saturating_duration_since(started).as_secs();
        if seconds == self.seconds {
            return false;
        }
        self.seconds = seconds;
        self.refresh();
        true
    }

    fn refresh(&mut self) {
        self.entry.text = format!("✻ {} · {}s", self.label, self.seconds);
        self.entry.layout = None;
    }

    fn phase(&mut self, label: &str) {
        if self.started.is_some() && self.label != "Cancelling" && self.label != label {
            self.label = label.into();
            self.tick(Instant::now());
            self.refresh();
        }
    }

    fn stop(&mut self, label: &str) {
        if self.started.is_some() {
            self.tick(Instant::now());
            self.started = None;
            self.label = label.into();
            self.refresh();
        }
    }
}

#[derive(Default)]
pub(super) struct Transcript {
    entries: Vec<TranscriptEntry>,
    // 状态独立于历史条目，始终渲染在最新消息后面。
    activity: Option<Activity>,
    assistant: Option<usize>,
    thinking: Option<usize>,
    running_tools: Vec<(String, usize)>,
    offset: usize,
    total: usize,
    viewport_height: usize,
    viewport_bottom: u16,
    follow_bottom: bool,
    // 重放恢复时不伪造 bash 卡片的 wall duration。
    replaying: bool,
}

impl Transcript {
    pub(super) fn new() -> Self {
        Self {
            follow_bottom: true,
            ..Self::default()
        }
    }

    pub(super) fn append(&mut self, label: &str, text: &str) {
        let (kind, title) = match label {
            "You" => (EntryKind::User, String::new()),
            "Assistant" => (EntryKind::Assistant, String::new()),
            _ => (EntryKind::Data, label.into()),
        };
        self.entries
            .push(TranscriptEntry::new(kind, title, text.into(), None));
    }

    pub(super) fn begin_run(&mut self) {
        self.activity = Some(Activity::new(Instant::now()));
    }

    pub(super) fn tick(&mut self, now: Instant) -> bool {
        self.activity
            .as_mut()
            .is_some_and(|activity| activity.tick(now))
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
        let activity = self
            .activity
            .get_or_insert_with(|| Activity::new(Instant::now()));
        activity.started = None;
        activity.label = label.into();
        activity.seconds = elapsed.as_secs();
        activity.entry.text = format!(
            "✻ {label} · {}s · done {}",
            activity.seconds,
            finished_at.format("%H:%M")
        );
        activity.entry.layout = None;
    }

    /// 按日志顺序恢复可见对话，并返回最后一条错误供弹窗展示。
    pub(super) fn replay(
        &mut self,
        records: Vec<ReplayRecord>,
        input_history: &mut Vec<String>,
    ) -> Option<String> {
        self.replaying = true;
        let mut assistant_running = false;
        let mut last_error = None;
        for record in records {
            match record {
                ReplayRecord::User(text) => {
                    self.append("You", &text);
                    input_history.push(text);
                }
                ReplayRecord::Event(event) => {
                    match &event {
                        AgentEvent::AssistantStarted => assistant_running = true,
                        AgentEvent::AssistantFinished { .. } => assistant_running = false,
                        _ => {}
                    }
                    self.progress(event);
                }
                ReplayRecord::Error(message) => {
                    assistant_running = false;
                    self.fail();
                    last_error = Some(message);
                }
                ReplayRecord::Cancelled => {
                    self.cancel();
                    assistant_running = false;
                    last_error = None;
                }
            }
        }
        self.replaying = false;
        if assistant_running || self.assistant.is_some() || !self.running_tools.is_empty() {
            self.fail();
            last_error = Some("Previous request was interrupted.".into());
        }
        last_error
    }

    pub(super) fn write_scrollback(
        &mut self,
        writer: &mut impl Write,
        snapshot: &Buffer,
    ) -> io::Result<()> {
        let footer_y = self.viewport_bottom;
        // 退出时回放完整输出，不把折叠预览当作终端历史。
        for entry in &mut self.entries {
            if matches!(entry.kind, EntryKind::Bash | EntryKind::Skill) && !entry.expanded {
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

    pub(super) fn progress(&mut self, event: AgentEvent) {
        if !self.replaying
            && let Some(activity) = &mut self.activity
        {
            match &event {
                AgentEvent::AssistantStarted => activity.phase("Requesting model"),
                AgentEvent::ThinkingDelta { .. } => activity.phase("Thinking"),
                AgentEvent::AssistantDelta { .. } => activity.phase("Responding"),
                AgentEvent::AssistantFinished { .. } => activity.phase("Processing response"),
                AgentEvent::ToolStarted { name, .. } => {
                    activity.phase(&format!("Running {}", name.replace(char::is_control, " ")))
                }
                AgentEvent::ToolFinished { .. } => activity.phase("Processing tool result"),
                AgentEvent::ToolOutput { .. } => {}
                // 运行时上下文管理：自动压缩开始切换阶段；用量更新不影响活动显示。
                AgentEvent::ContextUpdated { .. } => {}
                AgentEvent::CompactionStarted { .. } => activity.phase("Compacting context"),
                AgentEvent::CompactionFinished { .. } => {}
            }
        }
        match event {
            // 用量更新不产生对话条目。
            AgentEvent::ContextUpdated { .. } => {}
            AgentEvent::CompactionStarted { .. } => {}
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
                    entry.layout = None;
                } else {
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
                entry.layout = None;
            }
            AgentEvent::AssistantFinished { text } => {
                if text.trim().is_empty() {
                    if let Some(index) = self.assistant.take() {
                        self.entries.remove(index);
                    }
                } else {
                    let index = self.assistant_entry();
                    let entry = &mut self.entries[index];
                    entry.text = text;
                    entry.refusal.clear();
                    entry.status = EntryStatus::Complete;
                    entry.layout = None;
                }
                self.assistant = None;
                self.thinking = None;
            }
            AgentEvent::ToolStarted {
                id,
                name,
                arguments,
            } => {
                let arguments = serde_json::from_str::<Value>(&arguments).ok();
                let detail = arguments.as_ref().and_then(|args| {
                    args.get(match name.as_str() {
                        "bash" => "command",
                        "load_skill" => "name",
                        _ => "path",
                    })?
                    .as_str()
                });
                let kind = if name == "bash" {
                    EntryKind::Bash
                } else if name == "grep" {
                    EntryKind::Grep
                } else if name == "load_skill" {
                    EntryKind::Skill
                } else {
                    EntryKind::Data
                };
                let search_args = arguments
                    .as_ref()
                    .and_then(|args| args.get("args"))
                    .and_then(Value::as_array)
                    .filter(|args| args.iter().all(Value::is_string));
                let title = if matches!(name.as_str(), "grep" | "find") {
                    search_args.map_or_else(
                        || name.clone(),
                        |args| {
                            format!(
                                "{name} · {}",
                                serde_json::to_string(args).expect("JSON values serialize")
                            )
                        },
                    )
                } else {
                    match detail {
                        Some(name) if kind == EntryKind::Skill => {
                            format!("skill · {}", name.replace(char::is_control, " "))
                        }
                        None if kind == EntryKind::Skill => "skill · unknown".into(),
                        Some(command) if name == "bash" => format!("$ {command}"),
                        Some(path) => format!("{name} · {path}"),
                        None => name.clone(),
                    }
                };
                let mut entry = TranscriptEntry::new(kind, title, String::new(), Some(name));
                entry.status = EntryStatus::Running;
                self.running_tools.push((id, self.entries.len()));
                self.entries.push(entry);
            }
            AgentEvent::ToolOutput { id, stream, text } => {
                if let Some(index) = self.tool_index(&id) {
                    let entry = &mut self.entries[index];
                    match stream {
                        OutputStream::Stdout => entry.stdout.push_str(&text),
                        OutputStream::Stderr => entry.stderr.push_str(&text),
                    }
                    entry.layout = None;
                }
            }
            AgentEvent::ToolFinished { id, result } => {
                if let Some(position) = self.running_tools.iter().position(|(call, _)| call == &id)
                {
                    let (_, index) = self.running_tools.remove(position);
                    let entry = &mut self.entries[index];
                    entry.status = if result.get("ok").and_then(Value::as_bool) == Some(false) {
                        EntryStatus::Failed
                    } else {
                        EntryStatus::Complete
                    };
                    entry.stdout.clear();
                    entry.stderr.clear();
                    entry.text = tool_result(
                        entry.kind,
                        entry.tool.as_deref().unwrap_or_default(),
                        &result,
                    );
                    if entry.kind == EntryKind::Bash {
                        let status = result.get("exit_code").map_or_else(
                            || {
                                if entry.status == EntryStatus::Failed {
                                    "failed".into()
                                } else {
                                    "ok".into()
                                }
                            },
                            |code| format!("Exit code: {code}"),
                        );
                        entry.footer = if self.replaying {
                            format!("⟨{status}⟩")
                        } else {
                            format!(
                                "⟨Wall: {:.2}s | {status}⟩",
                                entry.started.elapsed().as_secs_f64()
                            )
                        };
                    }
                    entry.layout = None;
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
        self.entries.push(entry);
        self.assistant = Some(index);
        index
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
            self.entries[index].status = EntryStatus::Failed;
            if self.entries[index].kind == EntryKind::Bash {
                self.entries[index].footer = "⟨failed⟩".into();
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
        if let Some(entry) = self
            .entries
            .iter_mut()
            .rev()
            .find(|entry| matches!(entry.kind, EntryKind::Bash | EntryKind::Skill))
        {
            entry.expanded = !entry.expanded;
            entry.layout = None;
        }
    }

    pub(super) fn page_down(&mut self) {
        self.scroll_down(self.viewport_height.max(1));
    }

    pub(super) fn scroll_up(&mut self, lines: usize) {
        self.offset = self.offset.saturating_sub(lines);
        self.follow_bottom = false;
    }

    pub(super) fn scroll_down(&mut self, lines: usize) {
        let bottom = self.total.saturating_sub(self.viewport_height);
        self.offset = self.offset.saturating_add(lines).min(bottom);
        self.follow_bottom = self.offset == bottom;
    }

    pub(super) fn draw(&mut self, frame: &mut Frame, area: Rect) {
        self.viewport_bottom = area.bottom();
        self.viewport_height = usize::from(area.height);
        if area.width == 0 || area.height == 0 {
            return;
        }
        self.prepare_layout(area.width);
        let bottom = self.total.saturating_sub(self.viewport_height);
        self.offset = if self.follow_bottom {
            bottom
        } else {
            self.offset.min(bottom)
        };
        self.render(frame.buffer_mut(), area, self.offset);
    }

    fn prepare_layout(&mut self, width: u16) {
        self.total = 0;
        for entry in self
            .entries
            .iter_mut()
            .chain(self.activity.iter_mut().map(|activity| &mut activity.entry))
        {
            let padding = if matches!(entry.kind, EntryKind::Bash | EntryKind::Skill) {
                4
            } else {
                2
            };
            entry.ensure_layout(width.saturating_sub(padding).max(1));
            self.total += entry.layout.as_ref().expect("layout initialized").height + 1;
        }
        self.total = self.total.saturating_sub(1);
    }

    fn render(&self, buffer: &mut Buffer, area: Rect, mut skip: usize) {
        let inset = area.width.min(2);
        let mut y = area.y;
        let end = area.bottom();
        for entry in self
            .entries
            .iter()
            .chain(self.activity.iter().map(|activity| &activity.entry))
        {
            let layout = entry.layout.as_ref().expect("layout initialized");
            if skip > layout.height {
                skip -= layout.height + 1;
                continue;
            }
            for line in &layout.lines {
                if skip >= line.height {
                    skip -= line.height;
                    continue;
                }
                if y == end {
                    return;
                }
                let height = (line.height - skip).min(usize::from(end - y)) as u16;
                let bash = entry.kind == EntryKind::Bash;
                let skill = entry.kind == EntryKind::Skill;
                let think = entry.kind == EntryKind::Think;
                let background = if bash {
                    Some(BASH_BACKGROUND)
                } else {
                    entry.kind.conversation_background()
                };
                for row in y..y + height {
                    if let Some(background) = background {
                        buffer.set_style(
                            Rect::new(area.x, row, area.width, 1),
                            Style::default().bg(background),
                        );
                    }
                    if bash || skill {
                        let skill_title = format!(" {} ", entry.title);
                        let (left, right, label) = match line.source {
                            Source::Top => {
                                ("╭", "╮", if skill { skill_title.as_str() } else { "" })
                            }
                            Source::Divider => ("├", "┤", " Output "),
                            Source::Bottom => ("╰", "╯", ""),
                            _ => ("│", "│", ""),
                        };
                        let style = if skill {
                            Style::default().fg(Color::Magenta)
                        } else {
                            Style::default().fg(BASH_BORDER).bg(BASH_BACKGROUND)
                        };
                        let horizontal =
                            matches!(line.source, Source::Top | Source::Divider | Source::Bottom);
                        if horizontal {
                            Span::styled("─".repeat(usize::from(area.width)), style)
                                .render(Rect::new(area.x, row, area.width, 1), buffer);
                        }
                        Span::styled(left, style).render(Rect::new(area.x, row, 1, 1), buffer);
                        if area.width > 1 {
                            Span::styled(right, style)
                                .render(Rect::new(area.right() - 1, row, 1, 1), buffer);
                        }
                        if !label.is_empty() && area.width > 4 {
                            Span::styled(
                                label,
                                if skill {
                                    Style::default()
                                        .fg(Color::Magenta)
                                        .add_modifier(Modifier::BOLD)
                                } else {
                                    Style::default().fg(Color::Gray).bg(BASH_BACKGROUND)
                                },
                            )
                            .render(Rect::new(area.x + 2, row, area.width - 4, 1), buffer);
                        }
                    } else if !think && entry.kind != EntryKind::Activity && background.is_none() {
                        Span::styled("│", Style::default().fg(entry.kind.color()))
                            .render(Rect::new(area.x, row, 1, 1), buffer);
                    }
                }
                let padding = if bash || skill { 4 } else { inset };
                if area.width > padding
                    && !matches!(line.source, Source::Top | Source::Divider | Source::Bottom)
                {
                    Paragraph::new(entry.line(line))
                        .wrap(Wrap { trim: false })
                        .scroll((u16::try_from(skip).expect("bounded logical window"), 0))
                        .render(
                            Rect::new(area.x + inset, y, area.width - padding, height),
                            buffer,
                        );
                }
                y += height;
                skip = 0;
            }
            if y == end {
                return;
            }
            y += 1;
            skip = 0;
        }
    }
}

fn tool_result(kind: EntryKind, name: &str, result: &Value) -> String {
    if kind == EntryKind::Bash || matches!(name, "grep" | "find") {
        let mut lines = Vec::new();
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
        return lines.join("\n");
    }
    if result.get("ok").and_then(Value::as_bool) == Some(false) {
        if let Some(error) = result.get("error") {
            return error
                .as_str()
                .map_or_else(|| error.to_string(), str::to_owned);
        }
    } else if name == "load_skill" {
        return format!(
            "Source: {}\n{}",
            result
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            result
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or_default()
        );
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
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend, buffer::Buffer};
    use serde_json::json;

    fn render(transcript: &mut Transcript, width: u16, height: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| transcript.draw(frame, frame.area()))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn screen(buffer: &Buffer) -> String {
        (buffer.area.y..buffer.area.bottom())
            .map(|y| {
                (buffer.area.x..buffer.area.right())
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn delta(transcript: &mut Transcript, text: &str, refusal: bool) {
        transcript.progress(AgentEvent::AssistantDelta {
            text: text.into(),
            refusal,
        });
    }

    #[test]
    fn activity_timer_ticks_without_events_and_stops_on_failure() {
        let mut transcript = Transcript::new();
        transcript.append("You", "waiting task");
        transcript.begin_run();
        let started = transcript.activity.as_ref().unwrap().started.unwrap();
        assert!(!transcript.tick(started + Duration::from_millis(999)));
        assert!(transcript.tick(started + Duration::from_secs(1)));
        assert!(!transcript.tick(started + Duration::from_millis(1500)));
        let text = screen(&render(&mut transcript, 100, 30));
        assert!(text.contains("✻ Requesting model · 1s"));
        assert_eq!(transcript.entries.len(), 1);
        transcript.fail();
        assert!(!transcript.tick(started + Duration::from_secs(90)));
        assert!(screen(&render(&mut transcript, 100, 30)).contains("✻ Failed ·"));
    }

    #[test]
    fn activity_follows_latest_message_across_model_and_tool_phases() {
        let mut transcript = Transcript::new();
        transcript.append("You", "run task");
        transcript.begin_run();
        let phases = [
            (AgentEvent::AssistantStarted, "Requesting model"),
            (
                AgentEvent::ThinkingDelta {
                    text: "thinking content".into(),
                },
                "Thinking",
            ),
            (
                AgentEvent::AssistantDelta {
                    text: "answer content".into(),
                    refusal: false,
                },
                "Responding",
            ),
            (
                AgentEvent::AssistantFinished {
                    text: "answer content".into(),
                },
                "Processing response",
            ),
            (
                AgentEvent::ToolStarted {
                    id: "read-1".into(),
                    name: "read".into(),
                    arguments: r#"{"path":"test.txt"}"#.into(),
                },
                "Running read",
            ),
            (
                AgentEvent::ToolFinished {
                    id: "read-1".into(),
                    result: json!({"ok":true,"content":"tool content"}),
                },
                "Processing tool result",
            ),
            (AgentEvent::AssistantStarted, "Requesting model"),
        ];
        for (event, label) in phases {
            transcript.progress(event);
            let started = transcript.activity.as_ref().unwrap().started.unwrap();
            transcript.tick(started + Duration::from_secs(5));
            let text = screen(&render(&mut transcript, 100, 30));
            assert!(text.contains(&format!("✻ {label} · 5s")), "{text}");
            assert_eq!(text.matches('✻').count(), 1);
            assert!(text.find("run task").unwrap() < text.find('✻').unwrap());
            if text.contains("answer content") {
                assert!(text.find("answer content").unwrap() < text.find('✻').unwrap());
            }
        }
        transcript.cancelling();
        transcript.progress(AgentEvent::ThinkingDelta {
            text: "late delta".into(),
        });
        assert!(screen(&render(&mut transcript, 100, 30)).contains("✻ Cancelling ·"));
        transcript.cancel();
        assert!(screen(&render(&mut transcript, 100, 30)).contains("✻ Cancelled ·"));
        transcript.begin_run();
        transcript.append("You", "next task");
        let text = screen(&render(&mut transcript, 100, 30));
        assert_eq!(text.matches('✻').count(), 1);
        assert!(text.contains("✻ Requesting model · 0s"));
        assert!(text.find("next task").unwrap() < text.find('✻').unwrap());
        // 窄屏也只占一行；恢复宽度后显示完整状态。
        let text = screen(&render(&mut transcript, 18, 40));
        assert_eq!(text.lines().filter(|line| line.contains('✻')).count(), 1);
        assert_eq!(
            transcript
                .activity
                .as_ref()
                .unwrap()
                .entry
                .layout
                .as_ref()
                .unwrap()
                .height,
            1
        );
        assert!(screen(&render(&mut transcript, 100, 30)).contains("✻ Requesting model · 0s"));
    }

    #[test]
    fn replay_restores_visible_history_and_marks_interruption() {
        let mut transcript = Transcript::new();
        let mut input_history = Vec::new();
        let error = transcript.replay(
            vec![
                ReplayRecord::User("remember replay".into()),
                ReplayRecord::Event(AgentEvent::AssistantStarted),
                ReplayRecord::Event(AgentEvent::ToolStarted {
                    id: "b1".into(),
                    name: "bash".into(),
                    arguments: r#"{"command":"printf replay"}"#.into(),
                }),
                ReplayRecord::Event(AgentEvent::ToolFinished {
                    id: "b1".into(),
                    result: json!({"ok":true,"stdout":"replay\n","exit_code":0}),
                }),
                ReplayRecord::Event(AgentEvent::AssistantDelta {
                    text: "partial".into(),
                    refusal: false,
                }),
            ],
            &mut input_history,
        );
        assert_eq!(input_history, ["remember replay"]);
        let text = screen(&render(&mut transcript, 100, 30));
        assert!(text.contains("remember replay"));
        assert!(text.contains("replay"));
        assert!(text.contains("Exit code: 0"));
        assert!(!text.contains("Wall:"));
        assert_eq!(error.as_deref(), Some("Previous request was interrupted."));
        assert!(!text.contains("Previous request was interrupted."));
        assert_eq!(
            transcript.entries.last().unwrap().status,
            EntryStatus::Failed
        );

        // 完整结束的记录不会误报中断。
        let mut transcript = Transcript::new();
        let mut input_history = Vec::new();
        let error = transcript.replay(
            vec![
                ReplayRecord::User("done task".into()),
                ReplayRecord::Event(AgentEvent::AssistantStarted),
                ReplayRecord::Event(AgentEvent::AssistantFinished {
                    text: "final answer".into(),
                }),
                ReplayRecord::Error("task failed".into()),
            ],
            &mut input_history,
        );
        let text = screen(&render(&mut transcript, 100, 30));
        assert!(text.contains("final answer"));
        assert_eq!(error.as_deref(), Some("task failed"));
        assert!(!text.contains("task failed"));
        assert!(!text.contains("Previous request was interrupted."));
    }

    #[test]
    fn replay_cancel_preserves_partial_and_clears_interruption() {
        let mut transcript = Transcript::new();
        let error = transcript.replay(
            vec![
                ReplayRecord::Error("previous failure".into()),
                ReplayRecord::User("cancelled prompt".into()),
                ReplayRecord::Event(AgentEvent::AssistantStarted),
                ReplayRecord::Event(AgentEvent::AssistantDelta {
                    text: "visible partial".into(),
                    refusal: false,
                }),
                ReplayRecord::Cancelled,
            ],
            &mut Vec::new(),
        );
        assert!(error.is_none());
        let text = screen(&render(&mut transcript, 100, 30));
        assert!(text.contains("visible partial"));
        assert!(text.contains(crate::cancellation::CANCELLED_NOTE));
        assert_eq!(transcript.entries[1].status, EntryStatus::Failed);
        assert!(transcript.assistant.is_none());
        assert!(transcript.thinking.is_none());
    }

    #[test]
    fn replay_marks_interruption_before_first_answer_text() {
        for thinking in [false, true] {
            let mut transcript = Transcript::new();
            let mut records = vec![ReplayRecord::Event(AgentEvent::AssistantStarted)];
            if thinking {
                records.push(ReplayRecord::Event(AgentEvent::ThinkingDelta {
                    text: "unfinished thought".into(),
                }));
            }
            let error = transcript.replay(records, &mut Vec::new());
            assert_eq!(error.as_deref(), Some("Previous request was interrupted."));
            assert!(!screen(&render(&mut transcript, 100, 30)).contains("interrupted"));
        }
    }

    fn tool(transcript: &mut Transcript, id: &str, name: &str, arguments: Value) {
        transcript.progress(AgentEvent::ToolStarted {
            id: id.into(),
            name: name.into(),
            arguments: arguments.to_string(),
        });
    }

    #[test]
    fn styled_scrollback_keeps_full_history_and_live_tool_output() {
        let mut transcript = Transcript::new();
        transcript.append("You", "保留记录 中😀");
        let history = (0..60)
            .map(|line| format!("line-{line:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        transcript.append("Assistant", &history);
        transcript.progress(AgentEvent::AssistantStarted);
        delta(&mut transcript, "hidden-refusal", true);
        delta(&mut transcript, "partial-answer", false);
        tool(
            &mut transcript,
            "live",
            "bash",
            json!({"command":"echo live"}),
        );
        for (stream, text) in [
            (OutputStream::Stdout, "out-中\n"),
            (OutputStream::Stderr, "err\u{1b}[2J\r\u{7}"),
        ] {
            transcript.progress(AgentEvent::ToolOutput {
                id: "live".into(),
                stream,
                text: text.into(),
            });
        }
        let snapshot = render(&mut transcript, 100, 4);
        transcript.page_up();
        let mut output = Vec::new();
        transcript.write_scrollback(&mut output, &snapshot).unwrap();
        let mut parser = vt100::Parser::new(100, 100, 0);
        parser.process(&output);
        let text = parser.screen().contents();
        assert!(text.lines().next().unwrap().trim().is_empty());
        assert_eq!(text.lines().nth(1).unwrap().trim(), "保留记录 中😀");
        for line in history.lines() {
            assert!(text.lines().any(|row| row.trim() == line), "{text}");
        }
        assert!(text.lines().any(|row| row.trim() == "partial-answer"));
        assert!(!text.contains("You\n"));
        assert!(!text.contains("Assistant\n"));
        for expected in [
            "╭─",
            "│ $ echo live",
            "├─ Output ─",
            "│ out-中",
            "│ [stderr]",
            "│ err[2J",
            "│ ⟨running⟩",
            "╰─",
        ] {
            assert!(text.contains(expected), "{text}");
        }
        assert!(!text.contains("hidden-refusal"));
        assert!(!parser.screen().alternate_screen());
    }

    #[test]
    fn streaming_category_colors_and_statuses_preserve_default_body() {
        for kind in [EntryKind::Grep, EntryKind::Data] {
            let mut transcript = Transcript::new();
            let tool = matches!(kind, EntryKind::Grep | EntryKind::Skill);
            let mut entry = TranscriptEntry::new(
                kind,
                "title".into(),
                "  body 中😀\n\n    code".into(),
                tool.then(|| "tool".into()),
            );
            entry.status = EntryStatus::Failed;
            transcript.entries.push(entry);
            let buffer = render(&mut transcript, 60, 8);
            assert_eq!(buffer[(0, 0)].symbol(), "│");
            assert_eq!(buffer[(0, 0)].fg, kind.color());
            assert_eq!(buffer[(2, 0)].fg, kind.color());
            assert!(buffer[(2, 0)].modifier.contains(Modifier::BOLD));
            assert_eq!(buffer[(4, 1)].fg, Color::Reset);
            if tool {
                assert!(buffer[(4, 1)].modifier.contains(Modifier::DIM));
            }
            assert!(screen(&buffer).contains("  body 中"));
            assert!(screen(&buffer).contains("    code"));
        }
    }

    #[test]
    fn conversation_entries_use_backgrounds_without_role_labels() {
        let mut transcript = Transcript::new();
        transcript.append("You", "user input");
        transcript.append("Assistant", "assistant output");

        let buffer = render(&mut transcript, 60, 8);
        let text = screen(&buffer);
        assert_eq!(transcript.entries[0].kind, EntryKind::User);
        assert_eq!(transcript.entries[1].kind, EntryKind::Assistant);
        assert_eq!(buffer[(0, 0)].symbol(), " ");
        assert_eq!(buffer[(0, 0)].bg, USER_BACKGROUND);
        assert_eq!(buffer[(2, 0)].bg, USER_BACKGROUND);
        assert_eq!(buffer[(2, 1)].symbol(), "u");
        assert_eq!(buffer[(2, 1)].bg, USER_BACKGROUND);
        assert_eq!(buffer[(0, 2)].symbol(), " ");
        assert_eq!(buffer[(0, 2)].bg, USER_BACKGROUND);
        assert_eq!(buffer[(0, 4)].bg, ASSISTANT_BACKGROUND);
        assert_eq!(buffer[(2, 4)].bg, ASSISTANT_BACKGROUND);
        assert_ne!(USER_BACKGROUND, ASSISTANT_BACKGROUND);
        assert!(text.contains("user input"));
        assert!(text.contains("assistant output"));
        assert!(!text.contains("You"));
        assert!(!text.contains("Assistant"));
    }

    #[test]
    fn thinking_and_assistant_share_background_but_not_foreground() {
        let mut transcript = Transcript::new();
        transcript.append("Assistant", "answer");
        transcript.progress(AgentEvent::ThinkingDelta {
            text: "reasoning".into(),
        });

        let buffer = render(&mut transcript, 60, 4);
        assert_eq!(buffer[(2, 0)].fg, Color::Reset);
        assert_eq!(buffer[(2, 0)].bg, ASSISTANT_BACKGROUND);
        assert_eq!(buffer[(2, 2)].fg, MUTED_TEXT);
        assert_eq!(buffer[(2, 2)].bg, ASSISTANT_BACKGROUND);
    }

    #[test]
    fn bash_card_and_thinking_match_reference_and_keep_full_output() {
        let mut transcript = Transcript::new();
        tool(&mut transcript, "b", "bash", json!({"command":"git diff"}));
        let output: String = (0..24).map(|i| format!("line-{i:02}\n")).collect();
        transcript.progress(AgentEvent::ToolFinished {
            id: "b".into(),
            result: json!({"ok":true,"stdout":output,"exit_code":0}),
        });
        transcript.progress(AgentEvent::ThinkingDelta {
            text: "Check the diff 中😀".into(),
        });
        let buffer = render(&mut transcript, 100, 30);
        let text = screen(&buffer);
        for expected in [
            "╭─",
            "$ git diff",
            "├─ Output ─",
            "14 earlier lines, showing 10 of 24",
            "Ctrl+O: Expand",
            "line-14",
            "line-23",
            "Wall:",
            "Exit code: 0",
            "╰─",
            "Check the diff 中",
        ] {
            assert!(text.contains(expected), "{text}");
        }
        assert!(!text.contains("line-00"));
        assert_eq!(buffer[(0, 0)].fg, BASH_BORDER);
        assert_eq!(buffer[(2, 1)].fg, Color::Rgb(190, 188, 139));
        assert_eq!(buffer[(2, 1)].bg, BASH_BACKGROUND);
        let think_row = text
            .lines()
            .position(|line| line.contains("Check the diff"))
            .unwrap() as u16;
        assert_eq!(buffer[(0, think_row)].symbol(), " ");
        assert_eq!(buffer[(2, think_row)].fg, MUTED_TEXT);
        assert_eq!(buffer[(2, think_row)].bg, ASSISTANT_BACKGROUND);
        transcript.toggle_tool_output();
        let expanded = screen(&render(&mut transcript, 100, 40));
        assert!(expanded.contains("line-00"));
        assert!(!expanded.contains("earlier lines"));
        transcript.toggle_tool_output();
        render(&mut transcript, 5, 2);
        let snapshot = render(&mut transcript, 100, 30);
        let mut replay = Vec::new();
        transcript.write_scrollback(&mut replay, &snapshot).unwrap();
        let mut parser = vt100::Parser::new(80, 100, 0);
        parser.process(&replay);
        assert!(parser.screen().contents().contains("line-00"));
        for width in [1, 2, 3, 4, 10] {
            render(&mut transcript, width, 8);
        }
    }

    #[test]
    fn thinking_turns_stay_separate_from_answers_and_each_other() {
        let mut transcript = Transcript::new();
        for _ in 0..2 {
            transcript.progress(AgentEvent::AssistantStarted);
            transcript.progress(AgentEvent::ThinkingDelta {
                text: "think".into(),
            });
            transcript.progress(AgentEvent::ThinkingDelta {
                text: " more".into(),
            });
            transcript.progress(AgentEvent::AssistantFinished {
                text: String::new(),
            });
        }
        assert_eq!(transcript.entries.len(), 2);
        assert_eq!(transcript.entries[0].text, "think more");
        transcript.progress(AgentEvent::AssistantStarted);
        transcript.progress(AgentEvent::ThinkingDelta {
            text: "partial thinking".into(),
        });
        delta(&mut transcript, "answer", false);
        transcript.fail();
        let text = screen(&render(&mut transcript, 80, 20));
        assert!(text.contains("partial thinking"));
        assert!(text.contains("answer"));
        assert!(!text.contains("stream failed"));
    }

    #[test]
    fn streaming_scrolling_keeps_viewport_until_follow_is_restored() {
        let mut transcript = Transcript::new();
        for index in 0..60 {
            transcript.append("Assistant", &format!("history-{index:02}"));
        }
        assert!(screen(&render(&mut transcript, 40, 8)).contains("history-59"));
        transcript.page_up();
        let before = render(&mut transcript, 40, 8);
        let offset = transcript.offset;
        transcript.progress(AgentEvent::AssistantStarted);
        delta(&mut transcript, "live-first", false);
        let after = render(&mut transcript, 40, 8);
        assert_eq!(transcript.offset, offset);
        assert_eq!(before, after);
        assert!(!transcript.follow_bottom);
        while !transcript.follow_bottom {
            transcript.page_down();
        }
        assert!(screen(&render(&mut transcript, 40, 8)).contains("live-first"));
        delta(&mut transcript, "-last", false);
        assert!(screen(&render(&mut transcript, 40, 8)).contains("live-first-last"));
    }

    #[test]
    fn mouse_scroll_lines_clamp_and_restore_follow() {
        let mut transcript = Transcript::new();
        let history = (0..60)
            .map(|index| format!("line-{index:02}-abcdefghijk"))
            .collect::<Vec<_>>()
            .join("\n");
        delta(&mut transcript, &history, false);
        let bottom = screen(&render(&mut transcript, 12, 8));
        assert!(bottom.contains("line-59"));
        transcript.scroll_up(3);
        let older = screen(&render(&mut transcript, 12, 8));
        assert_eq!(
            older.lines().skip(3).collect::<Vec<_>>(),
            bottom.lines().take(5).collect::<Vec<_>>()
        );
        assert!(!older.contains("line-59"));
        delta(&mut transcript, "\nnew-a\nnew-b", false);
        assert_eq!(screen(&render(&mut transcript, 12, 8)), older);
        assert!(!transcript.follow_bottom);

        transcript.scroll_up(usize::MAX);
        let top = screen(&render(&mut transcript, 12, 8));
        assert!(top.starts_with("  line-00"));
        assert!(top.contains("line-00"));
        transcript.scroll_up(3);
        assert_eq!(screen(&render(&mut transcript, 12, 8)), top);
        transcript.scroll_down(usize::MAX);
        assert!(screen(&render(&mut transcript, 12, 8)).contains("new-b"));
        assert!(transcript.follow_bottom);
        delta(&mut transcript, "\nfollow", false);
        assert!(screen(&render(&mut transcript, 12, 8)).contains("follow"));

        let resized = screen(&render(&mut transcript, 9, 5));
        transcript.scroll_up(3);
        let older = screen(&render(&mut transcript, 9, 5));
        assert_eq!(
            older.lines().skip(3).collect::<Vec<_>>(),
            resized.lines().take(2).collect::<Vec<_>>()
        );
        transcript.scroll_down(usize::MAX);
        assert_eq!(screen(&render(&mut transcript, 9, 5)), resized);

        for text in ["", "short"] {
            let mut transcript = Transcript::new();
            if !text.is_empty() {
                delta(&mut transcript, text, false);
            }
            let original = render(&mut transcript, 20, 8);
            transcript.scroll_up(usize::MAX);
            assert_eq!(render(&mut transcript, 20, 8), original);
            transcript.scroll_down(usize::MAX);
            assert_eq!(render(&mut transcript, 20, 8), original);
            assert!(transcript.follow_bottom);
        }
    }

    #[test]
    fn streaming_resize_reflows_without_losing_text_or_follow_state() {
        let mut transcript = Transcript::new();
        transcript.append("You", "draft submitted 中😀");
        transcript.progress(AgentEvent::AssistantStarted);
        delta(&mut transcript, "first\n  indented 中😀\n\nlast", false);
        let before = render(&mut transcript, 40, 8);
        render(&mut transcript, 5, 2);
        let after = render(&mut transcript, 40, 8);
        assert_eq!(before, after);
        assert_eq!(transcript.entries[1].text, "first\n  indented 中😀\n\nlast");
        transcript.page_up();
        render(&mut transcript, 20, 4);
        assert!(!transcript.follow_bottom);
        assert!(transcript.offset <= transcript.total.saturating_sub(4));
    }

    #[test]
    fn streaming_completion_refusal_and_empty_turns_do_not_duplicate_cards() {
        let mut transcript = Transcript::new();
        transcript.progress(AgentEvent::AssistantStarted);
        transcript.progress(AgentEvent::AssistantFinished {
            text: String::new(),
        });
        assert!(transcript.entries.is_empty());
        transcript.progress(AgentEvent::AssistantStarted);
        transcript.progress(AgentEvent::AssistantFinished {
            text: "without delta".into(),
        });
        assert_eq!(transcript.entries.len(), 1);
        assert_eq!(
            screen(&render(&mut transcript, 60, 15))
                .matches("without delta")
                .count(),
            1
        );
        transcript.progress(AgentEvent::AssistantStarted);
        delta(&mut transcript, "provisional refusal", true);
        assert!(screen(&render(&mut transcript, 60, 15)).contains("provisional refusal"));
        delta(&mut transcript, "content wins", false);
        let text = screen(&render(&mut transcript, 60, 15));
        assert!(text.contains("content wins"));
        assert!(!text.contains("provisional refusal"));
        transcript.progress(AgentEvent::AssistantFinished {
            text: "content wins".into(),
        });
        assert_eq!(transcript.entries.len(), 2);
        transcript.progress(AgentEvent::AssistantStarted);
        delta(&mut transcript, "only refusal", true);
        transcript.progress(AgentEvent::AssistantFinished {
            text: "only refusal".into(),
        });
        assert_eq!(
            screen(&render(&mut transcript, 60, 15))
                .matches("only refusal")
                .count(),
            1
        );
        transcript.progress(AgentEvent::AssistantStarted);
        delta(&mut transcript, "partial visible", false);
        transcript.fail();
        assert_eq!(transcript.entries[3].status, EntryStatus::Failed);
        let text = screen(&render(&mut transcript, 60, 20));
        assert!(text.contains("partial visible"));
        assert!(!text.contains("Error"));
        assert!(!text.contains("stream failed"));
        assert_eq!(transcript.entries.len(), 4);
    }

    #[test]
    fn streaming_tool_channels_results_and_reused_ids_are_calibrated() {
        let mut transcript = Transcript::new();
        tool(
            &mut transcript,
            "call",
            "bash",
            json!({"command": "grep foo"}),
        );
        assert_eq!(transcript.entries[0].kind, EntryKind::Bash);
        for (stream, text) in [
            (OutputStream::Stdout, "out-first"),
            (OutputStream::Stderr, "err-first"),
        ] {
            transcript.progress(AgentEvent::ToolOutput {
                id: "call".into(),
                stream,
                text: text.into(),
            });
        }
        let live = screen(&render(&mut transcript, 70, 15));
        assert!(live.contains(" Output "));
        assert!(!live.contains("[stdout]"));
        assert!(live.contains("[stderr]"));
        assert!(live.contains("out-first"));
        assert!(live.contains("err-first"));
        transcript.progress(AgentEvent::ToolFinished {
            id: "call".into(),
            result: json!({
                "ok": false, "stdout": "out-firstout-last", "stderr": "err-first",
                "exit_code": 9, "stdout_truncated": true, "stderr_truncated": false,
            }),
        });
        let complete = screen(&render(&mut transcript, 70, 15));
        assert_eq!(complete.matches("out-first").count(), 1);
        assert_eq!(complete.matches("err-first").count(), 1);
        assert!(complete.contains("Exit code: 9"));
        assert_eq!(transcript.entries[0].status, EntryStatus::Failed);
        tool(&mut transcript, "call", "bash", json!({"command": "true"}));
        transcript.progress(AgentEvent::ToolFinished {
            id: "call".into(),
            result: json!({"ok": true, "stdout": "", "stderr": "", "exit_code": 0}),
        });
        assert_eq!(transcript.entries[0].status, EntryStatus::Failed);
        assert_eq!(transcript.entries[1].status, EntryStatus::Complete);
        assert_eq!(transcript.entries[1].text, "");
        assert!(transcript.entries[1].footer.contains("Exit code: 0"));
        tool(&mut transcript, "read", "read", json!({"path": "SKILL.md"}));
        transcript.progress(AgentEvent::ToolFinished {
            id: "read".into(),
            result: json!({"ok": true, "content": "read contents"}),
        });
        assert_eq!(transcript.entries[2].kind, EntryKind::Data);
        assert_eq!(transcript.entries[2].text, "read contents");
        tool(
            &mut transcript,
            "write",
            "write",
            json!({"path": "file", "content": "private write payload"}),
        );
        transcript.progress(AgentEvent::ToolFinished {
            id: "write".into(),
            result: json!({"ok": true, "bytes_written": 21}),
        });
        assert_eq!(transcript.entries[3].text, "Wrote 21 bytes");
        assert!(
            !transcript.entries[3]
                .title
                .contains("private write payload")
        );
        assert!(
            !tool_result(
                EntryKind::Bash,
                "bash",
                &json!({"ok": false, "error": "spawn failed"})
            )
            .contains("Exit code")
        );
    }

    #[test]
    fn streaming_long_entry_offsets_stay_usize_and_only_layout_active_entry() {
        let mut transcript = Transcript::new();
        let mut text = "line\n".repeat(70_000);
        text.push_str("long-entry-tail");
        transcript.append("Assistant", &text);
        let bottom = render(&mut transcript, 30, 5);
        assert!(transcript.offset > usize::from(u16::MAX));
        assert!(screen(&bottom).contains("long-entry-tail"));
        let cache = transcript.entries[0]
            .layout
            .as_ref()
            .unwrap()
            .lines
            .as_ptr();
        transcript.progress(AgentEvent::AssistantStarted);
        delta(&mut transcript, "active", false);
        render(&mut transcript, 30, 5);
        assert_eq!(
            transcript.entries[0]
                .layout
                .as_ref()
                .unwrap()
                .lines
                .as_ptr(),
            cache
        );
        delta(&mut transcript, "-chunk", false);
        render(&mut transcript, 30, 5);
        assert_eq!(
            transcript.entries[0]
                .layout
                .as_ref()
                .unwrap()
                .lines
                .as_ptr(),
            cache
        );
        transcript.page_up();
        let offset = transcript.offset;
        delta(&mut transcript, "-after-scroll", false);
        render(&mut transcript, 30, 5);
        assert_eq!(transcript.offset, offset);
    }

    #[test]
    fn streaming_single_long_logical_line_uses_bounded_grapheme_windows() {
        let mut transcript = Transcript::new();
        transcript.append("Assistant", &format!("{}tail中😀", "a".repeat(140_000)));
        let buffer = render(&mut transcript, 3, 8);
        assert!(transcript.offset > usize::from(u16::MAX));
        let narrow = screen(&buffer);
        assert!(narrow.contains('t'));
        assert!(narrow.contains('l'));
        let layout = transcript.entries[0].layout.as_ref().unwrap();
        assert!(
            layout
                .lines
                .iter()
                .all(|line| line.height <= usize::from(u16::MAX))
        );
        // CJK and emoji need two cells; show the same tail at a useful width.
        assert!(screen(&render(&mut transcript, 30, 5)).contains("tail中"));
    }
    #[test]
    fn skill_card_has_named_magenta_border_preview_status_and_full_expansion() {
        let mut transcript = Transcript::new();
        tool(
            &mut transcript,
            "skill-call",
            "load_skill",
            serde_json::json!({"name":"中文技能"}),
        );
        let running = render(&mut transcript, 70, 40);
        assert!(
            screen(&running)
                .replace(' ', "")
                .contains("╭─skill·中文技能")
        );
        assert_eq!(transcript.entries[0].status, EntryStatus::Running);
        assert_eq!(running[(0, 0)].fg, Color::Magenta);
        assert!(running[(2, 0)].modifier.contains(Modifier::BOLD));
        let content = (0..20)
            .map(|index| format!("skill-body-{index:02}\n"))
            .collect::<String>();
        transcript.progress(AgentEvent::ToolFinished {
            id: "skill-call".into(),
            result: serde_json::json!({"ok":true,"path":"/skills/test/SKILL.md","content":content}),
        });
        let collapsed = screen(&render(&mut transcript, 70, 40));
        assert!(collapsed.contains("skill-body-00"));
        assert!(!collapsed.contains("skill-body-19"));
        assert!(collapsed.contains("Ctrl-O"));
        assert_eq!(transcript.entries[0].status, EntryStatus::Complete);
        transcript.toggle_tool_output();
        assert!(screen(&render(&mut transcript, 70, 40)).contains("skill-body-19"));
        for width in [1, 2, 4, 20, 70] {
            render(&mut transcript, width, 5);
        }
        transcript.toggle_tool_output();
        tool(
            &mut transcript,
            "failed",
            "load_skill",
            serde_json::json!({"name":"disabled"}),
        );
        transcript.progress(AgentEvent::ToolFinished {
            id: "failed".into(),
            result: serde_json::json!({"ok":false,"error":"skill 未启用"}),
        });
        assert_eq!(transcript.entries[1].status, EntryStatus::Failed);
        tool(
            &mut transcript,
            "read",
            "read",
            serde_json::json!({"path":"/skills/test/SKILL.md"}),
        );
        assert_eq!(transcript.entries.last().unwrap().kind, EntryKind::Data);
    }
}
