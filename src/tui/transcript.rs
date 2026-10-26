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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum EntryKind {
    User,
    Assistant,
    Bash,
    Think,
    Grep,
    Skill,
    Diff,
    Ask,
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
            Self::Diff | Self::Ask => palette::ACCENT,
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
    Footer,
    Title,
    Text,
    StdoutLabel,
    Stdout,
    StderrLabel,
    Stderr,
}

enum LayoutContent {
    Source {
        source: Source,
        range: Range<usize>,
        title_end: bool,
    },
    Styled(Line<'static>),
}

struct LayoutLine {
    content: LayoutContent,
    height: usize,
}

impl LayoutLine {
    fn source(&self) -> Option<Source> {
        match self.content {
            LayoutContent::Source { source, .. } => Some(source),
            LayoutContent::Styled(_) => None,
        }
    }
}

enum EntryBody {
    Plain,
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

impl AskBlock {
    fn new(args: crate::interaction::AskArgs) -> Self {
        let previews = args
            .questions
            .iter()
            .map(|question| {
                question
                    .options
                    .iter()
                    .map(|option| {
                        option
                            .preview
                            .as_deref()
                            .filter(|text| !text.trim().is_empty())
                            .map(super::markdown::parse)
                    })
                    .collect()
            })
            .collect();
        Self {
            args,
            answers: None,
            previews,
        }
    }

    fn finish(&mut self, result: &Value) -> bool {
        match result.get("ok").and_then(Value::as_bool) {
            Some(false) => return true,
            Some(true) => {}
            None => return false,
        }
        let Some(answers) = result.get("answers").and_then(|answers| {
            serde_json::from_value::<Vec<crate::interaction::AskAnswer>>(answers.clone()).ok()
        }) else {
            return false;
        };
        if self.args.validate_answers(&answers).is_err() {
            return false;
        }
        self.answers = Some(self.args.canonicalize_answers(answers));
        true
    }

    fn layout(&self, width: u16) -> Vec<Line<'static>> {
        let mut rows = Vec::new();
        for (question_index, question) in self.args.questions.iter().enumerate() {
            rows.extend(wrap_block(
                block_line(
                    format!(
                        "{} · {}",
                        question.header.as_deref().unwrap_or(&question.id),
                        question.question
                    ),
                    Style::default()
                        .fg(palette::ACCENT)
                        .add_modifier(Modifier::BOLD),
                ),
                width,
                "",
            ));
            let answer = self
                .answers
                .as_ref()
                .and_then(|answers| answers.iter().find(|answer| answer.id == question.id));
            for (option_index, option) in question.options.iter().enumerate() {
                let selected =
                    answer.is_some_and(|answer| answer.selected_options.contains(&option.label));
                rows.extend(wrap_block(
                    block_line(
                        format!("{} {}", if selected { "●" } else { "○" }, option.label),
                        if selected {
                            Style::default()
                        } else {
                            Style::default().fg(palette::MUTED)
                        },
                    ),
                    width,
                    "  ",
                ));
                if let Some(description) = option
                    .description
                    .as_deref()
                    .filter(|text| !text.trim().is_empty())
                {
                    rows.extend(wrap_block(
                        block_line(
                            format!("  {description}"),
                            Style::default().fg(palette::MUTED),
                        ),
                        width,
                        "  ",
                    ));
                }
                if let Some(document) = &self.previews[question_index][option_index] {
                    rows.extend(document.layout(width.saturating_sub(2)).into_iter().map(
                        |mut line| {
                            line.spans.insert(0, Span::raw("  "));
                            line
                        },
                    ));
                }
            }
            if let Some(answer) = answer {
                if let Some(custom) = &answer.custom_input {
                    rows.extend(wrap_block(
                        block_line(format!("Other: {custom}"), Style::default()),
                        width,
                        "  ",
                    ));
                }
                if answer.selected_options.is_empty() && answer.custom_input.is_none() {
                    rows.extend(wrap_block(
                        block_line("No options selected", Style::default().fg(palette::MUTED)),
                        width,
                        "",
                    ));
                }
            }
        }
        rows
    }
}

struct EntryLayout {
    width: u16,
    height: usize,
    lines: Vec<LayoutLine>,
}

struct TranscriptEntry {
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
    layout: Option<EntryLayout>,
    expanded: bool,
    footer: String,
    started: Instant,
    arguments: Value,
    result: Option<Value>,
    display: DisplayContext,
    display_title: Option<String>,
    hidden: bool,
}

impl TranscriptEntry {
    fn is_collapsible_tool(&self) -> bool {
        self.tool.is_some() && self.kind != EntryKind::Ask
    }

    fn new(kind: EntryKind, title: String, text: String, tool: Option<String>) -> Self {
        Self {
            kind,
            body: EntryBody::Plain,
            markdown_dirty: kind == EntryKind::Assistant,
            status: EntryStatus::Complete,
            title,
            text,
            refusal: String::new(),
            stdout: String::new(),
            stderr: String::new(),
            tool,
            layout: None,
            expanded: false,
            footer: String::new(),
            started: Instant::now(),
            arguments: Value::Null,
            result: None,
            display: DisplayContext::default(),
            display_title: None,
            hidden: false,
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
            Source::Footer => &self.footer,
            Source::Title => self.display_title.as_deref().unwrap_or(&self.title),
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

    fn line<'a>(&'a self, line: &'a LayoutLine) -> Line<'a> {
        let (source, range, title_end) = match &line.content {
            LayoutContent::Styled(styled) => {
                return Line::from(
                    styled
                        .spans
                        .iter()
                        .map(|span| Span::styled(span.content.as_ref(), span.style))
                        .collect::<Vec<_>>(),
                )
                .style(styled.style);
            }
            LayoutContent::Source {
                source,
                range,
                title_end,
            } => (*source, range, *title_end),
        };
        let text = &self.source(source)[range.clone()];
        let text = if matches!(self.kind, EntryKind::Diff | EntryKind::Ask) {
            palette::display_text(text)
        } else {
            std::borrow::Cow::Borrowed(text)
        };
        if self.kind == EntryKind::Activity {
            return Line::styled(
                text,
                Style::default()
                    .fg(palette::ACTIVITY)
                    .add_modifier(Modifier::BOLD),
            );
        }
        if self.kind == EntryKind::Bash {
            let color = if matches!(source, Source::Title) {
                Color::Rgb(190, 188, 139)
            } else if matches!(source, Source::Footer) && self.status == EntryStatus::Failed {
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
                    if matches!(source, Source::Footer) && self.status == EntryStatus::Failed {
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
        if matches!(source, Source::Title) {
            let mut spans = vec![Span::styled(
                text,
                Style::default()
                    .fg(self.kind.color())
                    .add_modifier(Modifier::BOLD),
            )];
            if self.tool.is_some() && title_end && self.display_title.is_none() {
                let (status, color) = match self.status {
                    EntryStatus::Running => (" · running", Color::Yellow),
                    EntryStatus::Complete => (" · ok", Color::Green),
                    EntryStatus::Failed => (" · failed", Color::Red),
                };
                spans.push(Span::styled(status, Style::default().fg(color)));
            }
            Line::from(spans)
        } else {
            let style = if self.kind == EntryKind::Diff {
                Style::default().fg(palette::MUTED)
            } else if self.tool.is_some() {
                Style::default().add_modifier(Modifier::DIM)
            } else {
                Style::default()
            };
            Line::styled(text, style)
        }
    }

    fn ensure_layout(&mut self, width: u16) {
        if !self.hidden && self.tool.as_deref() == Some("read") && self.display_title.is_none() {
            self.display_title = Some(self.read_summary());
        }
        if self.markdown_dirty {
            self.body = EntryBody::Markdown(super::markdown::parse(self.visible_text()));
            self.markdown_dirty = false;
            self.layout = None;
        }
        if self
            .layout
            .as_ref()
            .is_some_and(|layout| layout.width == width)
        {
            return;
        }
        if self.hidden {
            self.layout = Some(EntryLayout {
                width,
                height: 0,
                lines: Vec::new(),
            });
            return;
        }
        let mut lines = Vec::new();
        let bash = self.kind == EntryKind::Bash;
        let skill = self.kind == EntryKind::Skill;
        let user = self.kind == EntryKind::User;
        let border = |source| LayoutLine {
            content: LayoutContent::Source {
                source,
                range: 0..0,
                title_end: false,
            },
            height: 1,
        };
        if bash || skill {
            lines.push(border(Source::Top));
        }
        if user {
            lines.push(border(Source::Padding));
        }
        if matches!(self.kind, EntryKind::Diff | EntryKind::Ask) {
            let (status, color) = match self.status {
                EntryStatus::Running => ("running", palette::ACCENT),
                EntryStatus::Complete => ("ok", palette::ADDED),
                EntryStatus::Failed => ("failed", palette::REMOVED),
            };
            let header = Line::from(vec![
                Span::styled(
                    self.title.clone(),
                    Style::default()
                        .fg(palette::ACCENT)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!(" · {status}"), Style::default().fg(color)),
            ]);
            lines.extend(
                wrap_block(header, width, "")
                    .into_iter()
                    .map(|line| LayoutLine {
                        content: LayoutContent::Styled(line),
                        height: 1,
                    }),
            );
        } else if bash {
            let style = Style::default()
                .fg(Color::Rgb(190, 188, 139))
                .bg(BASH_BACKGROUND);
            let mut command = tool_display::command(&self.title, self.expanded, self.display);
            if self.tool.as_deref() != Some("bash")
                && !self.expanded
                && !self.display.full
                && self.title.lines().count() > self.display.window()
                && let Some(marker) = command.first_mut()
            {
                *marker = format!(
                    "{} {marker}",
                    palette::tool_icon(self.tool.as_deref().unwrap_or("bash"))
                );
            }
            lines.extend(
                tool_display::wrap(command, width, style)
                    .into_iter()
                    .map(|line| LayoutLine {
                        content: LayoutContent::Styled(line),
                        height: 1,
                    }),
            );
        } else if !self.kind.hides_title() && !skill && !matches!(self.body, EntryBody::Tips) {
            self.add_lines(Source::Title, &mut lines);
        }
        if bash {
            lines.push(border(Source::Divider));
        }
        let tool_body = self.is_collapsible_tool()
            && !(self.tool.as_deref() == Some("edit") && matches!(self.body, EntryBody::Diff(_)));
        if tool_body {
            let mut text = self.visible_text().to_owned();
            if !self.stdout.is_empty() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(self.stdout.strip_suffix('\n').unwrap_or(&self.stdout));
            }
            if !self.stderr.is_empty() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str("[stderr]\n");
                text.push_str(self.stderr.strip_suffix('\n').unwrap_or(&self.stderr));
            }
            let style = if bash {
                Style::default().fg(MUTED_TEXT).bg(BASH_BACKGROUND)
            } else {
                Style::default().add_modifier(Modifier::DIM)
            };
            lines.extend(
                tool_display::body(
                    tool_display::View {
                        name: self.tool.as_deref().unwrap_or_default(),
                        text: &text,
                        arguments: &self.arguments,
                        result: self.result.as_ref(),
                        running: self.status == EntryStatus::Running,
                        failed: self.status == EntryStatus::Failed,
                        expanded: self.expanded,
                        context: self.display,
                    },
                    width,
                    style,
                )
                .into_iter()
                .map(|line| LayoutLine {
                    content: LayoutContent::Styled(line),
                    height: 1,
                }),
            );
        } else if matches!(self.body, EntryBody::Tips) {
            lines.extend(
                super::tips::lines(width)
                    .into_iter()
                    .flat_map(|line| wrap_block(line, width, ""))
                    .map(|line| LayoutLine {
                        content: LayoutContent::Styled(line),
                        height: 1,
                    }),
            );
        } else if let EntryBody::Markdown(document) = &self.body {
            lines.extend(document.layout(width).into_iter().map(|line| LayoutLine {
                content: LayoutContent::Styled(line),
                height: 1,
            }));
        } else if let EntryBody::Diff(diff) = &self.body {
            lines.extend(
                diff_layout(diff, width, self.expanded || self.display.full)
                    .into_iter()
                    .map(|line| LayoutLine {
                        content: LayoutContent::Styled(line),
                        height: 1,
                    }),
            );
            if let Some(warning) = self.text.strip_prefix("Warning: ") {
                lines.extend(
                    wrap_block(
                        block_line(
                            format!("Warning: {warning}"),
                            Style::default().fg(palette::MUTED),
                        ),
                        width,
                        "",
                    )
                    .into_iter()
                    .map(|line| LayoutLine {
                        content: LayoutContent::Styled(line),
                        height: 1,
                    }),
                );
            }
        } else if let EntryBody::Ask(block) = &self.body {
            lines.extend(block.layout(width).into_iter().map(|line| LayoutLine {
                content: LayoutContent::Styled(line),
                height: 1,
            }));
            if !self.text.is_empty() {
                lines.extend(
                    wrap_block(
                        block_line(self.text.clone(), Style::default().fg(palette::MUTED)),
                        width,
                        "",
                    )
                    .into_iter()
                    .map(|line| LayoutLine {
                        content: LayoutContent::Styled(line),
                        height: 1,
                    }),
                );
            }
        } else if !self.visible_text().is_empty() {
            if self.kind == EntryKind::Ask {
                lines.extend(
                    wrap_block(
                        block_line(
                            self.visible_text().to_owned(),
                            Style::default().fg(palette::MUTED),
                        ),
                        width,
                        "",
                    )
                    .into_iter()
                    .map(|line| LayoutLine {
                        content: LayoutContent::Styled(line),
                        height: 1,
                    }),
                );
            } else {
                self.add_lines(Source::Text, &mut lines);
            }
        }
        if !tool_body && !self.stdout.is_empty() {
            if !bash {
                self.add_lines(Source::StdoutLabel, &mut lines);
            }
            self.add_lines(Source::Stdout, &mut lines);
        }
        if !tool_body && !self.stderr.is_empty() {
            self.add_lines(Source::StderrLabel, &mut lines);
            self.add_lines(Source::Stderr, &mut lines);
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
        if self.tool.as_deref() == Some("write") && self.status == EntryStatus::Complete {
            if let Some(bytes) = self
                .result
                .as_ref()
                .and_then(|result| result["bytes_written"].as_u64())
            {
                self.footer = format!("Wrote {bytes} bytes");
                self.add_lines(Source::Footer, &mut lines);
            }
            if self.text.starts_with("Warning: ") {
                self.add_lines(Source::Text, &mut lines);
            }
        }
        if user {
            lines.push(border(Source::Padding));
        }
        let mut height = 0;
        for line in &mut lines {
            line.height = if self.kind == EntryKind::Activity
                || matches!(line.content, LayoutContent::Styled(_))
            {
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

    fn read_summary(&self) -> String {
        let path = self
            .result
            .as_ref()
            .and_then(|r| r["source"].as_str())
            .or_else(|| self.arguments["path"].as_str())
            .unwrap_or_default();
        let start = self.arguments["offset"].as_u64().unwrap_or(1);
        let count = self
            .result
            .as_ref()
            .and_then(|r| r["lines_read"].as_u64())
            .or_else(|| self.arguments["limit"].as_u64());
        let range = if let Some(ranges) = self.result.as_ref().and_then(|r| r["ranges"].as_array())
        {
            let ranges = ranges
                .iter()
                .filter_map(|r| Some(format!("{}-{}", r["start"].as_u64()?, r["end"].as_u64()?)))
                .collect::<Vec<_>>()
                .join(",");
            if ranges.is_empty() {
                String::new()
            } else {
                format!(":{ranges}")
            }
        } else {
            match count {
                Some(count) if count > 0 => format!(":{start}-{}", start.saturating_add(count - 1)),
                _ if start > 1 => format!(":{start}"),
                _ => String::new(),
            }
        };
        let image = self
            .result
            .as_ref()
            .filter(|r| r["kind"] == "image")
            .map(|r| {
                format!(
                    " · {} · {}×{} · {} bytes",
                    r["mime_type"].as_str().unwrap_or("image"),
                    r["width"],
                    r["height"],
                    r["bytes"]
                )
            })
            .unwrap_or_default();
        let status = match self.status {
            EntryStatus::Running => "running",
            EntryStatus::Complete => "ok",
            EntryStatus::Failed => "failed",
        };
        let truncated = if self
            .result
            .as_ref()
            .is_some_and(|result| result["truncated"] == true)
        {
            " · truncated"
        } else {
            ""
        };
        format!(
            "{} Read · {}{range}{image} · {status}{truncated}",
            palette::tool_icon("read"),
            palette::display_text(path)
        )
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
                            content: LayoutContent::Source {
                                source,
                                range: chunk_start..start + bytes,
                                title_end: false,
                            },
                            height: 0,
                        });
                        chunk_start = start + bytes;
                        graphemes = 0;
                    }
                    bytes += grapheme.symbol.len();
                    graphemes += 1;
                }
            }
            lines.push(LayoutLine {
                content: LayoutContent::Source {
                    source,
                    range: chunk_start..start + logical.len(),
                    title_end: matches!(source, Source::Title)
                        && start + logical.len() == text.len(),
                },
                height: 0,
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

#[derive(Default)]
pub(super) struct Transcript {
    entries: Vec<TranscriptEntry>,
    read_groups_key: Option<(usize, bool, bool)>,
    // 状态独立于历史条目，始终渲染在最新消息后面。
    activity: Option<Activity>,
    assistant: Option<usize>,
    thinking: Option<usize>,
    running_tools: Vec<(String, usize)>,
    offset: usize,
    total: usize,
    viewport_height: usize,
    viewport_width: u16,
    viewport_bottom: u16,
    follow_bottom: bool,
    tools_expanded: bool,
    display: DisplayContext,
    // 重放恢复时不伪造 bash 卡片的 wall duration。
    replaying: bool,
    tree_layout: Option<TreeLayout>,
}

impl Transcript {
    pub(super) fn new() -> Self {
        Self {
            follow_bottom: true,
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
        Self::with_read_preview(self.display.read_preview)
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

    pub(super) fn tick(&mut self, now: Instant) -> bool {
        self.activity
            .as_mut()
            .is_some_and(|activity| activity.tick(now))
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

    pub(super) fn append_tree_node(
        &mut self,
        records: Vec<ReplayRecord>,
        fallback: &str,
    ) -> Range<usize> {
        let start = self.entries.len();
        self.replay(records, &mut Vec::new());
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

    pub(super) fn write_scrollback(
        &mut self,
        writer: &mut impl Write,
        snapshot: &Buffer,
    ) -> io::Result<()> {
        let footer_y = self.viewport_bottom;
        self.display.full = true;
        // 退出时回放完整输出，不把折叠预览当作终端历史。
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
                AgentEvent::ToolOutput { .. } | AgentEvent::FileChanged { .. } => {}
                // 运行时指标只服务状态栏，不改变对话阶段。
                AgentEvent::ModelRequestStarted
                | AgentEvent::RateLimited { .. }
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
        match event {
            // 用量更新不产生对话条目。
            AgentEvent::ContextUpdated { .. } | AgentEvent::PlanProgressUpdated { .. } => {}
            // 运行时指标只服务状态栏，不产生对话条目。
            AgentEvent::ModelRequestStarted
            | AgentEvent::RateLimited { .. }
            | AgentEvent::ModelOutputProgress { .. }
            | AgentEvent::ModelRequestFinished { .. } => {}
            // 已接受的补充输入由 App 直接追加，不生成重复用户卡。
            AgentEvent::UserInputAccepted { .. } => {}
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
                    entry.text = text;
                    entry.refusal.clear();
                    entry.markdown_dirty = true;
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
                let ask_args = (name == "ask")
                    .then(|| crate::interaction::AskArgs::parse(&arguments).ok())
                    .flatten();
                let arguments = serde_json::from_str::<Value>(&arguments).ok();
                let detail = arguments.as_ref().and_then(|args| {
                    args.get(match name.as_str() {
                        "bash" => "command",
                        "load_skill" => "name",
                        _ => "path",
                    })?
                    .as_str()
                });
                let kind = if matches!(name.as_str(), "bash" | "bash_task") {
                    EntryKind::Bash
                } else if name == "grep" {
                    EntryKind::Grep
                } else if name == "load_skill" {
                    EntryKind::Skill
                } else if matches!(name.as_str(), "edit" | "write") {
                    EntryKind::Diff
                } else if name == "ask" {
                    EntryKind::Ask
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
                } else if matches!(name.as_str(), "grep" | "find") {
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
                let title = if matches!(kind, EntryKind::Diff | EntryKind::Ask) {
                    palette::display_text(&title).into_owned()
                } else {
                    title
                };
                let title = if name == "bash" {
                    title
                } else {
                    format!("{} {title}", palette::tool_icon(&name))
                };
                let mut entry = TranscriptEntry::new(kind, title, String::new(), Some(name));
                entry.arguments = arguments.unwrap_or(Value::Null);
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
                    entry.status = if result.get("ok").and_then(Value::as_bool) == Some(false) {
                        EntryStatus::Failed
                    } else {
                        EntryStatus::Complete
                    };
                    entry.stdout.clear();
                    entry.stderr.clear();
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
                    if entry.kind == EntryKind::Bash {
                        let status = background_status(&result).unwrap_or_else(|| {
                            result.get("exit_code").map_or_else(
                                || {
                                    if entry.status == EntryStatus::Failed {
                                        "failed".into()
                                    } else {
                                        "ok".into()
                                    }
                                },
                                |code| format!("Exit code: {code}"),
                            )
                        });
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
                    entry.display_title = None;
                    if entry.tool.as_deref() == Some("read") {
                        self.read_groups_key = None;
                    }
                    entry.result = Some(result);
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
                self.entries[index].footer = "⟨failed⟩".into();
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
        // 锚定卡片而不是整个会话的绝对行号，避免前面卡片高度变化将浏览位置推走。
        let anchor = if !self.follow_bottom && self.viewport_width > 0 {
            self.prepare_layout(self.viewport_width);
            let mut row = self.offset;
            self.entries.iter().enumerate().find_map(|(index, entry)| {
                let height = entry.layout.as_ref()?.height;
                if height == 0 {
                    return None;
                }
                if row <= height {
                    Some((index, row))
                } else {
                    row -= height + 1;
                    None
                }
            })
        } else {
            None
        };
        self.tools_expanded = !self.tools_expanded;
        for entry in &mut self.entries {
            if entry.is_collapsible_tool() {
                entry.expanded = self.tools_expanded;
                entry.layout = None;
            }
        }
        if let Some((index, row)) = anchor {
            self.prepare_layout(self.viewport_width);
            self.offset = self.entries[..index]
                .iter()
                .filter_map(|entry| entry.layout.as_ref())
                .filter(|layout| layout.height > 0)
                .map(|layout| layout.height + 1)
                .sum::<usize>()
                + row.min(
                    self.entries[index]
                        .layout
                        .as_ref()
                        .expect("layout initialized")
                        .height,
                );
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
        self.viewport_width = area.width;
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

    pub(super) fn draw_tree(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        blocks: &[TreeBlock],
        visible: &[usize],
        selected: Option<usize>,
        reveal: bool,
    ) {
        self.viewport_height = usize::from(area.height);
        if area.is_empty() {
            return;
        }
        let framed = area.width >= 3;
        let inset = u16::from(framed);
        let width = area.width.saturating_sub(2 * inset);
        let cached = self.tree_layout.as_ref().is_some_and(|layout| {
            layout.width == area.width
                && layout.groups.len() == visible.len()
                && layout.groups.iter().zip(visible).all(|(group, &index)| {
                    group.index == index && group.entries == blocks[index].entries
                })
        });
        if !cached {
            self.layout_entries(width, !framed);
            let mut total = 0usize;
            let groups = visible
                .iter()
                .map(|&index| {
                    let entries = blocks[index].entries.clone();
                    let body = self.entries[entries.clone()]
                        .iter()
                        .map(|entry| {
                            let height = entry.layout.as_ref().expect("layout initialized").height;
                            if height == 0 { 0 } else { height + 1 }
                        })
                        .sum::<usize>()
                        .saturating_sub(1);
                    let rows = total..total + body + 2;
                    total = rows.end + 1;
                    TreeGroup {
                        index,
                        entries,
                        rows,
                    }
                })
                .collect();
            self.total = total.saturating_sub(1);
            self.tree_layout = Some(TreeLayout {
                width: area.width,
                groups,
            });
        }
        let layout = self.tree_layout.as_ref().expect("tree layout initialized");
        let bottom = self.total.saturating_sub(self.viewport_height);
        self.offset = self.offset.min(bottom);
        if reveal
            && let Some(group) = layout
                .groups
                .iter()
                .find(|group| Some(group.index) == selected)
        {
            if group.rows.len() > self.viewport_height || group.rows.start < self.offset {
                self.offset = group.rows.start;
            } else if group.rows.end > self.offset + self.viewport_height {
                self.offset = group.rows.end.saturating_sub(self.viewport_height);
            }
            self.offset = self.offset.min(bottom);
        }
        self.follow_bottom = false;
        if visible.is_empty() {
            frame.render_widget(
                Line::styled(
                    "Entries hidden by the current filter",
                    Style::default().fg(palette::MUTED),
                ),
                Rect::new(area.x, area.y, area.width, 1),
            );
            return;
        }
        let viewport_end = self.offset + self.viewport_height;
        let buffer = frame.buffer_mut();
        for group in &layout.groups {
            let start = group.rows.start.max(self.offset);
            let end = group.rows.end.min(viewport_end);
            if start >= end {
                continue;
            }
            let chosen = framed && Some(group.index) == selected;
            let style = Style::default().fg(palette::ACCENT);
            if chosen {
                for row in start..end {
                    let y = area.y + (row - self.offset) as u16;
                    Span::styled("╎", style).render(Rect::new(area.x, y, 1, 1), buffer);
                    Span::styled("╎", style).render(Rect::new(area.right() - 1, y, 1, 1), buffer);
                }
            }
            for (row, left, right) in [(group.rows.start, "╭", "╮"), (group.rows.end - 1, "╰", "╯")]
            {
                if row < start || row >= end {
                    continue;
                }
                let y = area.y + (row - self.offset) as u16;
                if chosen {
                    for x in area.x + 1..area.right() - 1 {
                        Span::styled("╌", style).render(Rect::new(x, y, 1, 1), buffer);
                    }
                    Span::styled(left, style).render(Rect::new(area.x, y, 1, 1), buffer);
                    Span::styled(right, style).render(Rect::new(area.right() - 1, y, 1, 1), buffer);
                }
                if row == group.rows.start {
                    (&blocks[group.index].label)
                        .render(Rect::new(area.x + inset, y, width, 1), buffer);
                }
            }
            let body_start = (group.rows.start + 1).max(start);
            let body_end = (group.rows.end - 1).min(end);
            if body_start < body_end {
                Self::render_entries(
                    self.entries[group.entries.clone()].iter(),
                    buffer,
                    Rect::new(
                        area.x + inset,
                        area.y + (body_start - self.offset) as u16,
                        width,
                        (body_end - body_start) as u16,
                    ),
                    body_start - group.rows.start - 1,
                    !framed,
                );
            }
        }
    }

    fn prepare_layout(&mut self, width: u16) {
        self.layout_entries(width, false);
    }

    fn layout_entries(&mut self, width: u16, compact: bool) {
        self.total = 0;
        self.prepare_read_groups();
        for entry in self
            .entries
            .iter_mut()
            .chain(self.activity.iter_mut().map(|activity| &mut activity.entry))
        {
            let display = DisplayContext {
                full: self.display.full || entry.display.full,
                ..self.display
            };
            if entry.display != display {
                entry.display = display;
                entry.layout = None;
            }
            let padding = if compact {
                0
            } else if matches!(entry.kind, EntryKind::Bash | EntryKind::Skill) {
                4
            } else {
                2
            };
            entry.ensure_layout(width.saturating_sub(padding).max(1));
            let height = entry.layout.as_ref().expect("layout initialized").height;
            if height > 0 {
                self.total += height + 1;
            }
        }
        self.total = self.total.saturating_sub(1);
    }

    fn prepare_read_groups(&mut self) {
        let key = (
            self.entries.len(),
            self.display.full,
            self.display.read_preview,
        );
        if self.read_groups_key == Some(key) {
            return;
        }
        let mut index = 0;
        while index < self.entries.len() {
            if self.entries[index].tool.as_deref() != Some("read") {
                index += 1;
                continue;
            }
            let mut end = index + 1;
            let grouped = !self.display.full
                && !self.entries[index].display.full
                && !self.display.read_preview
                && self.entries[index].status != EntryStatus::Failed;
            if grouped {
                while end < self.entries.len()
                    && self.entries[end].tool.as_deref() == Some("read")
                    && self.entries[end].status != EntryStatus::Failed
                    && !self.entries[end].display.full
                {
                    end += 1;
                }
            }
            let title = if end - index > 1 {
                let mut title = format!("{} Read ({})", palette::tool_icon("read"), end - index);
                for (offset, entry) in self.entries[index..end].iter().enumerate() {
                    let summary = entry.read_summary();
                    let path = summary
                        .split_once("Read · ")
                        .map_or(summary.as_str(), |(_, path)| path);
                    title.push_str(&format!(
                        "\n  {} {path}",
                        if index + offset + 1 == end {
                            "└─"
                        } else {
                            "├─"
                        }
                    ));
                }
                title
            } else {
                self.entries[index].read_summary()
            };
            for (offset, entry) in self.entries[index..end].iter_mut().enumerate() {
                let hidden = offset > 0;
                let next_title = if hidden { None } else { Some(title.clone()) };
                if entry.hidden != hidden || entry.display_title != next_title {
                    entry.hidden = hidden;
                    entry.display_title = next_title;
                    entry.layout = None;
                }
            }
            index = end;
        }
        self.read_groups_key = Some(key);
    }

    fn render(&self, buffer: &mut Buffer, area: Rect, skip: usize) {
        Self::render_entries(
            self.entries
                .iter()
                .chain(self.activity.iter().map(|activity| &activity.entry)),
            buffer,
            area,
            skip,
            false,
        );
    }

    fn render_entries<'a>(
        entries: impl Iterator<Item = &'a TranscriptEntry>,
        buffer: &mut Buffer,
        area: Rect,
        mut skip: usize,
        compact: bool,
    ) {
        let inset = if compact { 0 } else { area.width.min(2) };
        let mut y = area.y;
        let end = area.bottom();
        for entry in entries {
            let layout = entry.layout.as_ref().expect("layout initialized");
            if layout.height == 0 {
                continue;
            }
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
                } else if matches!(entry.kind, EntryKind::Diff | EntryKind::Ask) {
                    Some(match entry.status {
                        EntryStatus::Running => palette::TOOL_PENDING_BG,
                        EntryStatus::Complete => palette::TOOL_SUCCESS_BG,
                        EntryStatus::Failed => palette::TOOL_ERROR_BG,
                    })
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
                    if compact {
                        continue;
                    }
                    if bash || skill {
                        let skill_title = format!(" {} ", entry.title);
                        let (left, right, label) = match line.source() {
                            Some(Source::Top) => {
                                ("╭", "╮", if skill { skill_title.as_str() } else { "" })
                            }
                            Some(Source::Divider) => ("├", "┤", " Output "),
                            Some(Source::Bottom) => ("╰", "╯", ""),
                            _ => ("│", "│", ""),
                        };
                        let style = if skill {
                            Style::default().fg(Color::Magenta)
                        } else {
                            Style::default().fg(BASH_BORDER).bg(BASH_BACKGROUND)
                        };
                        let horizontal = matches!(
                            line.source(),
                            Some(Source::Top | Source::Divider | Source::Bottom)
                        );
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
                let padding = if compact {
                    0
                } else if bash || skill {
                    4
                } else {
                    inset
                };
                if area.width > padding
                    && !matches!(
                        line.source(),
                        Some(Source::Top | Source::Divider | Source::Bottom)
                    )
                {
                    let target = Rect::new(area.x + inset, y, area.width - padding, height);
                    let paragraph = Paragraph::new(entry.line(line));
                    let paragraph = if matches!(line.content, LayoutContent::Source { .. }) {
                        paragraph.wrap(Wrap { trim: false })
                    } else {
                        paragraph
                    };
                    paragraph
                        .scroll((u16::try_from(skip).expect("bounded logical window"), 0))
                        .render(target, buffer);
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
    rows.extend(wrap_block(
        Line::from(vec![
            Span::styled(format!("+{added}"), Style::default().fg(palette::ADDED)),
            Span::raw(" "),
            Span::styled(format!("-{removed}"), Style::default().fg(palette::REMOVED)),
        ]),
        width,
        "",
    ));
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
    let continuation = " ".repeat(gutter + 3);
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
        // A contiguous replacement run can contain otherwise indistinguishable EOL edits.
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
                DiffLineKind::Added => ("+", palette::ADDED, line.new_line),
                DiffLineKind::Removed => ("-", palette::REMOVED, line.old_line),
                DiffLineKind::Context => (" ", palette::MUTED, line.new_line),
            };
            let number = number.map(|number| number.to_string()).unwrap_or_default();
            let mut spans = vec![
                Span::styled(
                    format!("{number:>gutter$} {mark} "),
                    Style::default().fg(palette::DIM),
                ),
                Span::styled(text.to_owned(), Style::default().fg(color)),
            ];
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
    if kind == EntryKind::Bash || matches!(name, "grep" | "find") {
        let mut lines = Vec::new();
        if let Some(status) = background_status(result) {
            lines.push(status);
        }
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

    fn entry_text(entry: &mut TranscriptEntry, width: u16) -> String {
        entry.ensure_layout(width);
        entry
            .layout
            .as_ref()
            .unwrap()
            .lines
            .iter()
            .map(|line| entry.line(line).to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn tool_folding_is_global_without_layout_and_future_output_inherits_it() {
        let mut transcript = Transcript::with_read_preview(true);
        for name in [
            "bash",
            "bash_task",
            "read",
            "grep",
            "find",
            "load_skill",
            "mcp__server__tool",
        ] {
            tool(&mut transcript, name, name, json!({}));
            transcript.entries.last_mut().unwrap().text = (0..20)
                .map(|i| format!("{name}-{i:02}"))
                .collect::<Vec<_>>()
                .join("\n");
        }
        transcript.toggle_tool_output();
        for entry in &mut transcript.entries {
            let name = entry.tool.clone().unwrap();
            assert!(entry.expanded);
            let text = entry_text(entry, 100);
            assert!(text.contains(&format!("{name}-00")));
            assert_eq!(
                text.contains(&format!("{name}-19")),
                !name.starts_with("mcp__")
            );
        }
        tool(&mut transcript, "live", "bash", json!({"command":"live"}));
        transcript.progress(AgentEvent::ToolOutput {
            id: "live".into(),
            stream: OutputStream::Stdout,
            text: (0..20).map(|i| format!("live-{i:02}\n")).collect(),
        });
        assert!(entry_text(transcript.entries.last_mut().unwrap(), 100).contains("live-00"));
        transcript.toggle_tool_output();
        assert!(transcript.entries.iter().all(|entry| !entry.expanded));
        let text = entry_text(transcript.entries.last_mut().unwrap(), 100);
        assert!(!text.contains("live-00"));
        assert!(text.contains("10 earlier lines") && text.contains("live-19"));
        transcript.progress(AgentEvent::ToolOutput {
            id: "live".into(),
            stream: OutputStream::Stdout,
            text: "live-20\n".into(),
        });
        assert!(
            entry_text(transcript.entries.last_mut().unwrap(), 100).contains("11 earlier lines")
        );
        transcript.progress(AgentEvent::ToolFinished {
            id: "live".into(),
            result: json!({"ok":false,"error":"failed\n".repeat(20)}),
        });
        assert_eq!(
            transcript.entries.last().unwrap().status,
            EntryStatus::Failed
        );
        assert!(entry_text(transcript.entries.last_mut().unwrap(), 100).contains("to expand"));
    }

    #[test]
    fn read_group_cache_refreshes_status_and_failure_boundaries() {
        let mut transcript = Transcript::new();
        for id in ["one", "two", "three"] {
            tool(&mut transcript, id, "read", json!({"path":id,"offset":20}));
        }
        let text = screen(&render(&mut transcript, 100, 30));
        assert!(
            text.contains("Read (3)") && text.contains("three:20 · running"),
            "{text}"
        );
        transcript.progress(AgentEvent::ToolFinished {
            id: "one".into(),
            result: json!({"ok":true,"content":"one-body","lines_read":2}),
        });
        let text = screen(&render(&mut transcript, 100, 30));
        assert!(
            text.contains("Read (3)") && text.contains("one:20-21 · ok"),
            "{text}"
        );
        transcript.progress(AgentEvent::ToolFinished {
            id: "two".into(),
            result: json!({"ok":false,"error":"second read failure"}),
        });
        let text = screen(&render(&mut transcript, 100, 30));
        for expected in [
            "one:20-21 · ok",
            "two:20 · failed",
            "second read failure",
            "three:20 · running",
        ] {
            assert!(text.contains(expected), "{expected}: {text}");
        }
        assert!(!text.contains("Read ("), "{text}");
        transcript.progress(AgentEvent::ToolFinished {
            id: "three".into(),
            result: json!({"ok":true,"content":"three-body","lines_read":3,"truncated":true}),
        });
        let text = screen(&render(&mut transcript, 100, 30));
        for expected in [
            "one:20-21 · ok",
            "second read failure",
            "three:20-22 · ok · truncated",
        ] {
            assert!(text.contains(expected), "{expected}: {text}");
        }
        assert!(!text.contains("Read ("), "{text}");

        let mut transcript = Transcript::new();
        for id in ["first", "second"] {
            tool(&mut transcript, id, "read", json!({"path":id}));
            transcript.progress(AgentEvent::ToolOutput {
                id: id.into(),
                stream: OutputStream::Stdout,
                text: format!("{id}-partial-output"),
            });
        }
        assert!(screen(&render(&mut transcript, 100, 30)).contains("Read (2)"));
        transcript.fail();
        let text = screen(&render(&mut transcript, 100, 30));
        for expected in [
            "first · failed",
            "second · failed",
            "first-partial-output",
            "second-partial-output",
        ] {
            assert!(text.contains(expected), "{expected}: {text}");
        }
        assert!(!text.contains("Read ("), "{text}");
    }

    #[test]
    fn read_summary_groups_preserve_entries_results_and_full_replay() {
        let mut transcript = Transcript::new();
        for (id, offset) in [("one", 1), ("two", 20)] {
            tool(
                &mut transcript,
                id,
                "read",
                json!({"path":id,"offset":offset}),
            );
            transcript.progress(AgentEvent::ToolFinished {
                id: id.into(),
                result: json!({"ok":true,"content":format!("{id}-body\n"),"lines_read":1}),
            });
        }
        let text = screen(&render(&mut transcript, 100, 15));
        assert!(
            text.contains("Read (2)") && text.contains("one:1-1") && text.contains("two:20-20"),
            "{text}"
        );
        assert!(!text.contains("one-body") && !text.contains("two-body"));
        assert_eq!(transcript.entries.len(), 2);
        transcript.toggle_tool_output();
        assert!(!screen(&render(&mut transcript, 100, 15)).contains("one-body"));
        transcript.append("Assistant", "read group boundary");
        tool(&mut transcript, "failed", "read", json!({"path":"missing"}));
        transcript.progress(AgentEvent::ToolFinished {
            id: "failed".into(),
            result: json!({"ok":false,"error":"read failure is visible"}),
        });
        let snapshot = render(&mut transcript, 100, 20);
        assert!(screen(&snapshot).contains("read failure is visible"));
        let mut output = Vec::new();
        transcript.write_scrollback(&mut output, &snapshot).unwrap();
        let mut parser = vt100::Parser::new(80, 100, 0);
        parser.process(&output);
        let full = parser.screen().contents();
        assert!(
            full.contains("one-body") && full.contains("two-body"),
            "{full}"
        );
    }

    #[test]
    fn tool_preview_counts_logical_lines_and_keeps_other_messages_complete() {
        for count in [3, 4] {
            let mut transcript = Transcript::with_read_preview(true);
            tool(&mut transcript, "read", "read", json!({"path":"file"}));
            transcript.progress(AgentEvent::ToolFinished { id:"read".into(), result:json!({"ok":true,"content":(0..count).map(|i|format!("line-{i:02}")).collect::<Vec<_>>().join("\n")}) });
            let text = entry_text(&mut transcript.entries[0], 80);
            assert_eq!(text.contains("to expand"), count > 3);
            assert!(text.contains("line-02") && !text.contains("line-03"));
        }
        let mut transcript = Transcript::with_read_preview(true);
        tool(&mut transcript, "long", "read", json!({}));
        transcript.entries[0].text = format!("{}\n{}", "中😀".repeat(9000), "tail\n");
        assert!(!entry_text(&mut transcript.entries[0], 10).contains("to expand"));
        let body = "unchanged\n".repeat(20);
        for label in ["You", "Assistant", "Help"] {
            transcript.append(label, &body);
        }
        transcript.progress(AgentEvent::ThinkingDelta { text: body.clone() });
        tool(
            &mut transcript,
            "ask",
            "ask",
            json!({"questions":[{"id":"q","question":body,"options":[]}]}),
        );
        let before: Vec<_> = transcript.entries[1..]
            .iter_mut()
            .map(|entry| entry_text(entry, 80))
            .collect();
        transcript.toggle_tool_output();
        transcript.toggle_tool_output();
        let after: Vec<_> = transcript.entries[1..]
            .iter_mut()
            .map(|entry| entry_text(entry, 80))
            .collect();
        assert_eq!(before, after);
        assert!(matches!(
            transcript.entries.last().unwrap().body,
            EntryBody::Ask(_)
        ));
    }

    #[test]
    fn tool_toggle_anchors_history_and_exit_scrollback_expands_all_tools() {
        let mut transcript = Transcript::with_read_preview(true);
        for i in 0..3 {
            tool(
                &mut transcript,
                &format!("r{i}"),
                "read",
                json!({"path":format!("file{i}")}),
            );
            transcript.progress(AgentEvent::ToolFinished {
                id: format!("r{i}"),
                result: json!({"ok":true,"content":(0..40).map(|j| format!("file{i}-{j:02}")).collect::<Vec<_>>().join("\n")}),
            });
        }
        render(&mut transcript, 100, 10);
        let first_height = transcript.entries[0].layout.as_ref().unwrap().height;
        transcript.follow_bottom = false;
        transcript.offset = first_height + 1;
        assert!(screen(&render(&mut transcript, 100, 10)).contains("Read · file1"));
        transcript.toggle_tool_output();
        assert!(!transcript.follow_bottom);
        assert!(screen(&render(&mut transcript, 100, 10)).contains("Read · file1"));
        // 收起时锚点位于隐藏内容内，也必须留在同一卡片。
        transcript.offset += 30;
        transcript.toggle_tool_output();
        let start = transcript.entries[0].layout.as_ref().unwrap().height + 1;
        let end = start + transcript.entries[1].layout.as_ref().unwrap().height;
        assert!((start..=end).contains(&transcript.offset));
        let snapshot = render(&mut transcript, 100, 10);
        let mut output = Vec::new();
        transcript.write_scrollback(&mut output, &snapshot).unwrap();
        let mut parser = vt100::Parser::new(200, 100, 0);
        parser.process(&output);
        let text = parser.screen().contents();
        for i in 0..3 {
            assert!(text.contains(&format!("file{i}-39")), "{text}");
        }
        assert!(!text.contains("to expand"));
        assert!(!Transcript::with_read_preview(true).tools_expanded);
    }

    #[test]
    fn ask_details_stay_in_messages_after_answering_and_resize() {
        let args = crate::interaction::AskArgs::parse(
            &json!({"questions":[{
                "id":"db", "question":"Database question?", "options":[{
                    "label":"SQLite", "description":"description-marker\u{1b}",
                    "preview":"**Preview**\n\nline-one\n\nline-two\n\npreview-tail"
                }, {"label":"Postgres", "description":"other-description"}]
            }]})
            .to_string(),
        )
        .unwrap();
        let mut block = AskBlock::new(args);
        let text = block
            .layout(80)
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        for expected in [
            "Database question?",
            "○ SQLite",
            "description-marker",
            "Preview",
            "preview-tail",
            "other-description",
        ] {
            assert!(text.contains(expected), "消息中应显示完整详情：{text}");
        }
        assert!(!text.contains('\u{1b}'));
        assert!(!text.contains("**Preview**"), "预览应按 Markdown 渲染");
        assert!(block.finish(&json!({"ok":true,"answers":[{
            "id":"db", "selected_options":["SQLite"], "custom_input":null
        }]})));
        for width in [32, 80] {
            let text = block
                .layout(width)
                .iter()
                .map(Line::to_string)
                .collect::<Vec<_>>()
                .join("\n");
            for expected in [
                "● SQLite",
                "description-marker",
                "preview-tail",
                "○ Postgres",
                "other-description",
            ] {
                assert!(text.contains(expected), "回答和缩放后仍应保留详情：{text}");
            }
        }
    }

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

    fn tree_render(
        transcript: &mut Transcript,
        blocks: &[TreeBlock],
        visible: &[usize],
        selected: Option<usize>,
        reveal: bool,
        width: u16,
        height: u16,
    ) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                transcript.draw_tree(frame, frame.area(), blocks, visible, selected, reveal)
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn answer(text: &str) -> Vec<ReplayRecord> {
        vec![
            ReplayRecord::Event(AgentEvent::AssistantStarted),
            ReplayRecord::Event(AgentEvent::AssistantFinished { text: text.into() }),
        ]
    }

    #[test]
    fn tree_blocks_preserve_markdown_tool_batch_and_geometry_when_selection_moves() {
        let mut transcript = Transcript::new();
        let mut blocks = Vec::new();
        for records in [
            answer(
                "## Markdown\n\nA long **formatted** sentence with 中文 and emoji 😀 that wraps across several physical rows.",
            ),
            answer("ordinary reply"),
            vec![
                ReplayRecord::Event(AgentEvent::AssistantStarted),
                ReplayRecord::Event(AgentEvent::AssistantFinished {
                    text: "tool explanation".into(),
                }),
                ReplayRecord::Event(AgentEvent::ToolStarted {
                    id: "a".into(),
                    name: "read".into(),
                    arguments: "{}".into(),
                }),
                ReplayRecord::Event(AgentEvent::ToolFinished {
                    id: "a".into(),
                    result: json!({"ok":true,"content":"first tool result"}),
                }),
                ReplayRecord::Event(AgentEvent::ToolStarted {
                    id: "b".into(),
                    name: "edit".into(),
                    arguments: r#"{"path":"a.txt"}"#.into(),
                }),
                ReplayRecord::Event(AgentEvent::FileChanged {
                    id: "b".into(),
                    path: "a.txt".into(),
                    diff: FileDiff::Text {
                        created: true,
                        added: 1,
                        removed: 0,
                        hunks: vec![crate::events::DiffHunk {
                            lines: vec![crate::events::DiffLine {
                                kind: DiffLineKind::Added,
                                old_line: None,
                                new_line: Some(1),
                                text: "new diff text".into(),
                            }],
                        }],
                    },
                }),
                ReplayRecord::Event(AgentEvent::ToolFinished {
                    id: "b".into(),
                    result: json!({"ok":true}),
                }),
            ],
        ] {
            let entries = transcript.append_tree_node(records, "");
            blocks.push(TreeBlock {
                entries,
                label: Line::raw("node"),
            });
        }
        let first = tree_render(&mut transcript, &blocks, &[0, 1, 2], Some(0), false, 48, 60);
        let second = tree_render(&mut transcript, &blocks, &[0, 1, 2], Some(2), false, 48, 60);
        let ranges: Vec<_> = transcript
            .tree_layout
            .as_ref()
            .unwrap()
            .groups
            .iter()
            .map(|g| g.rows.clone())
            .collect();
        for range in &ranges {
            for y in range.start + 1..range.end - 1 {
                for x in 1..47 {
                    assert_eq!(first[(x, y as u16)], second[(x, y as u16)]);
                }
            }
        }
        let text = screen(&second);
        assert_eq!(text.matches('╭').count(), 1, "{text}");
        assert_eq!(text.matches('╰').count(), 1, "{text}");
        let tool_rows = ranges[2].clone();
        let tool_text = (tool_rows.start..tool_rows.end)
            .map(|y| {
                (1..47)
                    .map(|x| second[(x, y as u16)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            tool_text.contains("tool explanation")
                && tool_text.contains("first tool result")
                && tool_text.contains("new diff text"),
            "{tool_text}"
        );
        for y in tool_rows.start + 1..tool_rows.end - 1 {
            assert_eq!(second[(0, y as u16)].symbol(), "╎");
            assert_eq!(second[(47, y as u16)].symbol(), "╎");
        }
    }

    #[test]
    fn tree_blocks_clip_real_boundaries_scroll_tall_nodes_and_handle_empty_filters() {
        let mut transcript = Transcript::new();
        let entries = transcript.append_tree_node(
            answer(&format!(
                "```text\n{}tail-marker\n```",
                "line\n".repeat(70_000)
            )),
            "",
        );
        let blocks = [TreeBlock {
            entries,
            label: Line::raw("assistant"),
        }];
        let top = tree_render(&mut transcript, &blocks, &[0], Some(0), true, 40, 12);
        assert!(screen(&top).contains('╭'));
        assert!(!screen(&top).contains('╰'));
        transcript.scroll_down(usize::MAX);
        let bottom = tree_render(&mut transcript, &blocks, &[0], Some(0), false, 40, 12);
        assert!(screen(&bottom).contains("tail-marker"));
        assert!(screen(&bottom).contains('╰'));
        assert!(!screen(&bottom).contains('╭'));
        assert!(transcript.offset > usize::from(u16::MAX));
        tree_render(&mut transcript, &blocks, &[0], Some(0), true, 40, 12);
        assert_eq!(transcript.offset, 0);
        for (width, height) in [(1, 1), (2, 2), (5, 3), (40, 12)] {
            tree_render(&mut transcript, &blocks, &[0], Some(0), true, width, height);
        }
        let empty = tree_render(&mut transcript, &blocks, &[], None, true, 60, 12);
        assert!(screen(&empty).contains("Entries hidden by the current filter"));
        assert!(!screen(&empty).contains('╎'));
        let mut fallback = Transcript::new();
        let range = fallback.append_tree_node(answer(""), "");
        let buffer = tree_render(
            &mut fallback,
            &[TreeBlock {
                entries: range,
                label: Line::raw("empty"),
            }],
            &[0],
            Some(0),
            true,
            40,
            12,
        );
        assert!(screen(&buffer).contains("(empty response)"));
    }

    #[test]
    fn tree_narrow_width_omits_decoration_but_still_displays_content() {
        let mut transcript = Transcript::new();
        let entries = transcript.append_tree_node(answer("x"), "");
        let blocks = [TreeBlock {
            entries,
            label: Line::raw("assistant"),
        }];
        tree_render(&mut transcript, &blocks, &[0], Some(0), true, 3, 6);
        for width in [1, 2] {
            let buffer = tree_render(&mut transcript, &blocks, &[0], Some(0), true, width, 6);
            let text = screen(&buffer);
            assert!(text.contains('x'), "{text}");
            assert!(
                !text.contains('╎') && !text.contains('╭') && !text.contains('╰'),
                "{text}"
            );
        }
    }

    #[test]
    fn committed_diff_survives_result_and_replay_without_reconstructing_old_logs() {
        let mut transcript = Transcript::new();
        transcript.progress(AgentEvent::ToolStarted {
            id: "edit-1".into(),
            name: "edit".into(),
            arguments: r#"{"path":"demo.txt"}"#.into(),
        });
        transcript.progress(AgentEvent::FileChanged {
            id: "edit-1".into(),
            path: "demo.txt".into(),
            diff: FileDiff::Text {
                created: false,
                added: 1,
                removed: 1,
                hunks: vec![crate::events::DiffHunk {
                    lines: vec![
                        crate::events::DiffLine {
                            kind: DiffLineKind::Removed,
                            old_line: Some(1),
                            new_line: None,
                            text: "old\r\n".into(),
                        },
                        crate::events::DiffLine {
                            kind: DiffLineKind::Added,
                            old_line: None,
                            new_line: Some(1),
                            text: "old\n".into(),
                        },
                    ],
                }],
            },
        });
        transcript.progress(AgentEvent::ToolFinished { id: "edit-1".into(), result: serde_json::json!({"ok":true,"edits_applied":1,"bytes_written":4,"warning":"directory sync warning"}) });
        let buffer = render(&mut transcript, 100, 15);
        let text = screen(&buffer);
        assert!(text.contains("+1 -1"), "{text}");
        assert!(text.contains("CRLF") && text.contains("LF"), "{text}");
        assert!(text.contains("directory sync warning"), "{text}");
        assert!(
            buffer
                .content
                .iter()
                .any(|cell| cell.symbol() == "o" && cell.fg == palette::REMOVED)
        );
        assert!(
            buffer
                .content
                .iter()
                .any(|cell| cell.symbol() == "o" && cell.fg == palette::ADDED)
        );
        let mut old = Transcript::new();
        old.replay(
            vec![
                ReplayRecord::Event(AgentEvent::ToolStarted {
                    id: "old".into(),
                    name: "write".into(),
                    arguments: r#"{"path":"old.txt","content":"not a real committed diff"}"#.into(),
                }),
                ReplayRecord::Event(AgentEvent::ToolFinished {
                    id: "old".into(),
                    result: serde_json::json!({"ok":true,"bytes_written":28}),
                }),
            ],
            &mut Vec::new(),
        );
        let old_screen = screen(&render(&mut old, 100, 12));
        assert!(
            old_screen.contains("not a real committed diff"),
            "{old_screen}"
        );
        assert!(!old_screen.contains("+28 -0"));
    }

    #[test]
    fn diff_preview_caps_hunks_and_wrapped_rows_but_full_mode_keeps_tail() {
        let make_line = |number, text| crate::events::DiffLine {
            kind: DiffLineKind::Added,
            old_line: None,
            new_line: Some(number),
            text,
        };
        let diff = FileDiff::Text {
            created: false,
            added: 9,
            removed: 0,
            hunks: (1..=9)
                .map(|i| crate::events::DiffHunk {
                    lines: vec![make_line(i, format!("hunk-{i}\n"))],
                })
                .collect(),
        };
        let collapsed = diff_layout(&diff, 100, false)
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(collapsed.contains("hunk-8") && !collapsed.contains("hunk-9"));
        assert!(
            diff_layout(&diff, 100, true)
                .iter()
                .any(|row| row.to_string().contains("hunk-9"))
        );
        let diff = FileDiff::Text {
            created: false,
            added: 1,
            removed: 0,
            hunks: vec![crate::events::DiffHunk {
                lines: vec![make_line(1, format!("{}tail\n", "x".repeat(2000)))],
            }],
        };
        let rows = diff_layout(&diff, 30, false);
        let marker = rows
            .iter()
            .position(|row| row.to_string().contains("more lines"))
            .unwrap();
        assert_eq!(marker, 41);
        assert!(!rows.iter().any(|row| row.to_string().contains("tail")));
        assert!(
            diff_layout(&diff, 30, true)
                .iter()
                .any(|row| row.to_string().contains("tail"))
        );
    }

    #[test]
    fn write_preview_preserves_committed_diff_and_warning_without_rereading_files() {
        let mut transcript = Transcript::new();
        let content = (0..20)
            .map(|i| format!("saved-{i:02}\n"))
            .collect::<String>();
        tool(
            &mut transcript,
            "write",
            "write",
            json!({"path":"missing-on-disk","content":content}),
        );
        transcript.progress(AgentEvent::FileChanged {
            id: "write".into(),
            path: "missing-on-disk".into(),
            diff: FileDiff::Unavailable {
                reason: "Previous content is not UTF-8; text diff unavailable.".into(),
            },
        });
        transcript.progress(AgentEvent::ToolFinished {id:"write".into(),result:json!({"ok":true,"bytes_written":content.len(),"warning":"directory sync warning"})});
        let collapsed = screen(&render(&mut transcript, 100, 20));
        assert!(collapsed.contains("saved-05") && !collapsed.contains("saved-06"));
        assert!(collapsed.contains("directory sync warning"));
        assert!(matches!(
            transcript.entries[0].body,
            EntryBody::Diff(FileDiff::Unavailable { .. })
        ));
        let mut full = Vec::new();
        transcript
            .write_scrollback(&mut full, &render(&mut Transcript::new(), 100, 20))
            .unwrap();
        let mut parser = vt100::Parser::new(80, 100, 0);
        parser.process(&full);
        assert!(parser.screen().contents().contains("saved-19"));
    }

    #[test]
    fn styled_blocks_expand_tabs_and_keep_explicit_line_boundaries() {
        let rows = wrap_block(
            block_line(
                "ab\t中\nsecond\n\nthird\n",
                Style::default().fg(palette::ADDED),
            ),
            12,
            "",
        );
        let text: Vec<String> = rows
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect();
        assert_eq!(text, ["ab  中", "second", "", "third", ""]);
        assert!(
            rows.iter()
                .flat_map(|line| &line.spans)
                .all(|span| span.style.fg == Some(palette::ADDED))
        );
    }

    #[test]
    fn styled_rows_preserve_graphemes_and_neutralize_terminal_controls() {
        let text = "中e\u{301}🙂\t\u{1b}[31m";
        let rows = wrap_block(block_line(text, Style::default().fg(palette::ADDED)), 3, "");
        let visible: String = rows
            .iter()
            .flat_map(|line| &line.spans)
            .map(|span| span.content.as_ref())
            .collect();
        assert!(visible.starts_with("中e\u{301}🙂"));
        assert!(visible.contains('�'));
        assert!(!visible.contains('\u{1b}') && !visible.contains('\t'));
        assert!(rows.iter().all(|line| line.width() <= 3));
        assert!(
            rows.iter()
                .flat_map(|line| &line.spans)
                .all(|span| span.style.fg == Some(palette::ADDED))
        );
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
    fn tool_icons_are_distinct_and_keep_two_column_prefixes_across_card_styles() {
        let names = [
            "read",
            "edit",
            "write",
            "bash_task",
            "grep",
            "find",
            "load_skill",
            "ask",
            "plan_search",
            "plan_find",
            "plan_write",
            "plan_submit",
            "mcp__fixture__tool",
            "unknown_tool",
        ];
        let mut icons = std::collections::HashSet::new();
        for name in names {
            let icon = palette::tool_icon(name);
            assert!(icons.insert(icon), "工具图标应独立：{name}");
            assert_eq!(Span::raw(icon).width(), 1, "图标应占一列：{name}");
            let mut transcript = Transcript::new();
            tool(
                &mut transcript,
                "call",
                name,
                json!({"path":"file", "command":"true", "name":"rust"}),
            );
            let prefix = format!("{icon} ");
            let title = transcript.entries[0].title.clone();
            assert!(title.starts_with(&prefix));
            assert_eq!(Span::raw(&prefix).width(), 2);
            for result in [None, Some(json!({"ok":false,"error":"fixture error"}))] {
                if let Some(result) = result {
                    transcript.progress(AgentEvent::ToolFinished {
                        id: "call".into(),
                        result,
                    });
                }
                for width in [80, 20, 80] {
                    let buffer = render(&mut transcript, width, 20);
                    let row = (0..buffer.area.height)
                        .find_map(|row| {
                            (0..buffer.area.width)
                                .find(|&column| buffer[(column, row)].symbol() == icon)
                                .map(|column| (column, row))
                        })
                        .unwrap_or_else(|| {
                            panic!("工具标题应显示图标：{name}\n{}", screen(&buffer))
                        });
                    assert_eq!(buffer[(row.0 + 1, row.1)].symbol(), " ");
                    if width == 80 {
                        assert_eq!(
                            buffer[(row.0 + 2, row.1)].symbol(),
                            if name == "read" {
                                "R".into()
                            } else {
                                title[prefix.len()..].chars().next().unwrap().to_string()
                            }
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn bash_card_and_thinking_match_reference_and_keep_full_output() {
        let mut long_command = Transcript::new();
        tool(
            &mut long_command,
            "command",
            "bash",
            json!({"command":(0..30).map(|i|format!("command-{i:02}")).collect::<Vec<_>>().join("\n")}),
        );
        let command_buffer = render(&mut long_command, 100, 30);
        let command = screen(&command_buffer);
        assert_eq!(command_buffer[(2, 1)].symbol(), "…");
        assert!(command.contains("command-29"));
        assert!(!command.contains("command-00"));
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
        assert_eq!(buffer[(2, 1)].symbol(), "$");
        for expected in [
            "╭─",
            "$ git diff",
            "├─ Output ─",
            "14 earlier lines, showing 10 of 24",
            "Ctrl + O to expand",
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
        let expanded_buffer = render(&mut transcript, 100, 40);
        assert_eq!(expanded_buffer[(2, 1)].symbol(), "$");
        let expanded = screen(&expanded_buffer);
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
    fn markdown_visual_rows_cross_u16_and_scrolled_history_stays_anchored() {
        let mut transcript = Transcript::new();
        let text = format!("```text\n{}long-entry-tail\n```", "line\n".repeat(70_000));
        transcript.append("Assistant", &text);
        let bottom = render(&mut transcript, 30, 5);
        assert!(transcript.offset > usize::from(u16::MAX));
        assert!(screen(&bottom).contains("long-entry-tail"));
        transcript.progress(AgentEvent::AssistantStarted);
        delta(&mut transcript, "active", false);
        render(&mut transcript, 30, 5);
        transcript.page_up();
        let before = screen(&render(&mut transcript, 30, 5));
        let offset = transcript.offset;
        delta(&mut transcript, "-after-scroll", false);
        assert_eq!(screen(&render(&mut transcript, 30, 5)), before);
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
                .contains("╭─\u{f02d}skill·中文技能")
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
        assert!(collapsed.contains("Source: /skills/test/SKILL.md"));
        assert!(!collapsed.contains("skill-body-00"));
        assert!(!collapsed.contains("skill-body-19"));
        assert!(collapsed.contains("Ctrl + O"));
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
