use crate::events::{AgentEvent, OutputStream};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};
use serde_json::Value;
use std::ops::Range;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum EntryKind {
    User,
    Bash,
    // Reserved display categories; no production tool is classified heuristically.
    #[allow(dead_code)]
    Grep,
    #[allow(dead_code)]
    Skill,
    Data,
    Note,
    Error,
}

impl EntryKind {
    fn color(self) -> Color {
        match self {
            Self::User => Color::Blue,
            Self::Bash => Color::Yellow,
            Self::Grep => Color::Cyan,
            Self::Skill => Color::Magenta,
            Self::Data => Color::Reset,
            Self::Note => Color::DarkGray,
            Self::Error => Color::Red,
        }
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
            Source::Title => &self.title,
            Source::Text => self.visible_text(),
            Source::StdoutLabel => "[stdout]",
            Source::Stdout => &self.stdout,
            Source::StderrLabel => "[stderr]",
            Source::Stderr => &self.stderr,
        }
    }

    fn line(&self, line: &LayoutLine) -> Line<'_> {
        let text = &self.source(line.source)[line.range.clone()];
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
        self.add_lines(Source::Title, &mut lines);
        if !self.visible_text().is_empty() {
            self.add_lines(Source::Text, &mut lines);
        }
        if !self.stdout.is_empty() {
            self.add_lines(Source::StdoutLabel, &mut lines);
            self.add_lines(Source::Stdout, &mut lines);
        }
        if !self.stderr.is_empty() {
            self.add_lines(Source::StderrLabel, &mut lines);
            self.add_lines(Source::Stderr, &mut lines);
        }
        let mut height = 0;
        for line in &mut lines {
            line.height = Paragraph::new(self.line(line))
                .wrap(Wrap { trim: false })
                .line_count(width)
                .max(1);
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

#[derive(Default)]
pub(super) struct Transcript {
    entries: Vec<TranscriptEntry>,
    assistant: Option<usize>,
    running_tools: Vec<(String, usize)>,
    offset: usize,
    total: usize,
    viewport_height: usize,
    follow_bottom: bool,
}

impl Transcript {
    pub(super) fn new() -> Self {
        Self {
            follow_bottom: true,
            ..Self::default()
        }
    }

    pub(super) fn append(&mut self, label: &str, text: &str) {
        let kind = match label {
            "You" => EntryKind::User,
            "Note" => EntryKind::Note,
            "Error" => EntryKind::Error,
            _ => EntryKind::Data,
        };
        self.entries
            .push(TranscriptEntry::new(kind, label.into(), text.into(), None));
    }

    pub(super) fn progress(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::AssistantStarted => {
                self.assistant = None;
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
            }
            AgentEvent::ToolStarted {
                id,
                name,
                arguments,
            } => {
                let arguments = serde_json::from_str::<Value>(&arguments).ok();
                let detail = arguments.as_ref().and_then(|args| {
                    args.get(if name == "bash" { "command" } else { "path" })?
                        .as_str()
                });
                let kind = if name == "bash" {
                    EntryKind::Bash
                } else {
                    EntryKind::Data
                };
                let title = match detail {
                    Some(command) if name == "bash" => format!("bash · $ {command}"),
                    Some(path) => format!("{name} · {path}"),
                    None => name.clone(),
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
            TranscriptEntry::new(EntryKind::Data, "Assistant".into(), String::new(), None);
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

    pub(super) fn fail(&mut self, message: &str) {
        if let Some(index) = self.assistant.take() {
            self.entries[index].status = EntryStatus::Failed;
            self.entries[index].layout = None;
        }
        for (_, index) in self.running_tools.drain(..) {
            self.entries[index].status = EntryStatus::Failed;
            self.entries[index].layout = None;
        }
        self.append("Error", message);
    }

    pub(super) fn page_up(&mut self) {
        self.scroll_up(self.viewport_height.max(1));
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
        if area.width == 0 || area.height == 0 {
            return;
        }
        let inset = area.width.min(2);
        let width = area.width.saturating_sub(inset).max(1);
        self.total = 0;
        for entry in &mut self.entries {
            entry.ensure_layout(width);
            self.total += entry.layout.as_ref().expect("layout initialized").height + 1;
        }
        self.total = self.total.saturating_sub(1);
        self.viewport_height = usize::from(area.height);
        let bottom = self.total.saturating_sub(self.viewport_height);
        self.offset = if self.follow_bottom {
            bottom
        } else {
            self.offset.min(bottom)
        };
        let mut skip = self.offset;
        let mut y = area.y;
        let end = area.bottom();
        for entry in &self.entries {
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
                for row in y..y + height {
                    frame.render_widget(
                        Span::styled("│", Style::default().fg(entry.kind.color())),
                        Rect::new(area.x, row, 1, 1),
                    );
                }
                if area.width > inset {
                    frame.render_widget(
                        Paragraph::new(entry.line(line))
                            .wrap(Wrap { trim: false })
                            .scroll((u16::try_from(skip).expect("bounded logical window"), 0)),
                        Rect::new(area.x + inset, y, area.width - inset, height),
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
    if kind == EntryKind::Bash {
        let mut lines = Vec::new();
        for (field, label) in [("stdout", "stdout"), ("stderr", "stderr")] {
            if let Some(text) = result
                .get(field)
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
            {
                lines.push(format!("[{label}]\n{text}"));
            }
            if result
                .get(format!("{field}_truncated"))
                .and_then(Value::as_bool)
                == Some(true)
            {
                lines.push(format!("[{label} truncated at 65536 bytes]"));
            }
        }
        if let Some(code) = result.get("exit_code") {
            lines.push(format!("Exit code: {code}"));
        }
        if let Some(error) = result.get("error") {
            lines.push(
                error
                    .as_str()
                    .map_or_else(|| error.to_string(), str::to_owned),
            );
        }
        if !lines.is_empty() {
            return lines.join("\n");
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

    fn tool(transcript: &mut Transcript, id: &str, name: &str, arguments: Value) {
        transcript.progress(AgentEvent::ToolStarted {
            id: id.into(),
            name: name.into(),
            arguments: arguments.to_string(),
        });
    }

    #[test]
    fn streaming_category_colors_and_statuses_preserve_default_body() {
        let mut transcript = Transcript::new();
        for (kind, title) in [
            (EntryKind::User, "You"),
            (EntryKind::Bash, "bash · $ echo"),
            (EntryKind::Grep, "grep"),
            (EntryKind::Skill, "skill"),
            (EntryKind::Data, "Assistant"),
        ] {
            let tool = matches!(kind, EntryKind::Bash | EntryKind::Grep | EntryKind::Skill);
            let mut entry = TranscriptEntry::new(
                kind,
                title.into(),
                "  body 中😀\n\n    code".into(),
                tool.then(|| title.into()),
            );
            entry.status = if kind == EntryKind::Grep {
                EntryStatus::Failed
            } else {
                EntryStatus::Running
            };
            transcript.entries.push(entry);
        }
        let buffer = render(&mut transcript, 60, 30);
        let colors = [
            Color::Blue,
            Color::Yellow,
            Color::Cyan,
            Color::Magenta,
            Color::Reset,
        ];
        for (index, color) in colors.into_iter().enumerate() {
            let y = (index * 5) as u16;
            assert_eq!(buffer[(0, y)].symbol(), "│");
            assert_eq!(buffer[(0, y)].fg, color);
            assert_eq!(buffer[(2, y)].fg, color);
            assert!(buffer[(2, y)].modifier.contains(Modifier::BOLD));
            assert_eq!(buffer[(4, y + 1)].fg, Color::Reset);
            if (1..=3).contains(&index) {
                assert!(buffer[(4, y + 1)].modifier.contains(Modifier::DIM));
            }
        }
        for (index, color) in colors.into_iter().enumerate() {
            assert!(!colors[index + 1..].contains(&color));
        }
        let text = screen(&buffer);
        assert!(text.contains("bash · $ echo · running"));
        assert!(text.contains("grep · failed"));
        assert!(text.contains("  body 中"));
        assert!(text.contains("    code"));
        assert!((0..60).any(|x| buffer[(x, 10)].fg == Color::Red));
        assert!((0..60).any(|x| buffer[(x, 5)].fg == Color::Yellow));
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
        assert!(top.starts_with("│ Assistant"));
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
        transcript.fail("stream failed");
        assert_eq!(transcript.entries[3].status, EntryStatus::Failed);
        let text = screen(&render(&mut transcript, 60, 20));
        assert!(text.contains("partial visible"));
        assert!(text.contains("Error"));
        assert!(text.contains("stream failed"));
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
        assert!(live.contains("[stdout]"));
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
        assert!(complete.contains("[stdout truncated at 65536 bytes]"));
        assert_eq!(transcript.entries[0].status, EntryStatus::Failed);
        tool(&mut transcript, "call", "bash", json!({"command": "true"}));
        transcript.progress(AgentEvent::ToolFinished {
            id: "call".into(),
            result: json!({"ok": true, "stdout": "", "stderr": "", "exit_code": 0}),
        });
        assert_eq!(transcript.entries[0].status, EntryStatus::Failed);
        assert_eq!(transcript.entries[1].status, EntryStatus::Complete);
        assert_eq!(transcript.entries[1].text, "Exit code: 0");
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
}
