use super::DisplayContext;

use super::{
    ASSISTANT_BACKGROUND, Activity, AskBlock, BASH_BACKGROUND, Color, EntryBody, EntryKind,
    EntryLayout, EntryStatus, FileDiff, Instant, LayoutContent, LayoutLine, Line, MUTED_TEXT,
    Modifier, Paragraph, Source, Span, Style, TranscriptEntry, USER_BACKGROUND, Value, Wrap,
    block_line, diff_layout_styled, palette, tool_display, wrap_block,
};

impl EntryKind {
    pub(super) fn color(self) -> Color {
        match self {
            Self::User => Color::Blue,
            Self::Assistant => Color::Reset,
            Self::Bash => Color::Yellow,
            Self::Think | Self::Activity => MUTED_TEXT,
            Self::Grep => Color::Cyan,
            Self::Read => Color::Reset,
            Self::Diff | Self::Ask => palette::ACCENT,
            Self::Data => Color::Reset,
            Self::Error => palette::ERROR_BORDER,
        }
    }

    pub(super) fn conversation_background(self) -> Option<Color> {
        match self {
            Self::User => Some(USER_BACKGROUND),
            Self::Assistant | Self::Think => Some(ASSISTANT_BACKGROUND),
            _ => None,
        }
    }

    pub(super) fn hides_title(self) -> bool {
        matches!(
            self,
            Self::User | Self::Assistant | Self::Think | Self::Activity
        )
    }
}

impl LayoutLine {
    pub(super) fn source(&self) -> Option<Source> {
        match self.content {
            LayoutContent::Source { source, .. } => Some(source),
            LayoutContent::Styled(_) => None,
        }
    }
}

impl AskBlock {
    pub(super) fn new(args: crate::interaction::AskArgs) -> Self {
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
                            .map(crate::tui::markdown::parse)
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

    pub(super) fn finish(&mut self, result: &Value) -> bool {
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

    pub(super) fn layout(&self, width: u16) -> Vec<Line<'static>> {
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
                        format!(
                            "{} {}",
                            crate::tui::ask::selection_icon(question.multi, selected),
                            option.label
                        ),
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

impl TranscriptEntry {
    pub(super) fn tool_accent(&self) -> Color {
        if self.tool.as_deref() == Some("read")
            && tool_display::is_skill_read(&self.arguments, self.result.as_ref())
        {
            palette::SKILL_TITLE
        } else {
            palette::tool_accent(self.tool.as_deref().unwrap_or_default())
        }
    }

    fn tool_title(&self, text: &str) -> Line<'static> {
        let (label, detail) = text
            .find(" · ")
            .map_or((text, ""), |end| text.split_at(end));
        Line::from(vec![
            Span::styled(
                label.to_owned(),
                Style::default()
                    .fg(self.tool_accent())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(detail.to_owned(), Style::default().fg(palette::ACTIVITY)),
        ])
    }

    pub(super) fn is_collapsible_tool(&self) -> bool {
        self.tool.is_some() && self.kind != EntryKind::Ask
    }

    pub(super) fn new(kind: EntryKind, title: String, text: String, tool: Option<String>) -> Self {
        Self {
            render: Default::default(),
            kind,
            body: EntryBody::Plain,
            markdown_dirty: matches!(kind, EntryKind::Assistant | EntryKind::Think),
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
            started: Some(Instant::now()),
            bash_timeout_seconds: None,
            arguments: Value::Null,
            result: None,
            display: DisplayContext::default(),
            display_title: None,
            hidden: false,
        }
    }

    pub(super) fn visible_text(&self) -> &str {
        if self.text.trim().is_empty() && !self.refusal.is_empty() {
            &self.refusal
        } else {
            &self.text
        }
    }

    pub(super) fn update_bash_footer(&mut self, elapsed: Option<std::time::Duration>) -> bool {
        let mut parts = Vec::new();
        if let Some(elapsed) = elapsed {
            parts.push(if self.status == EntryStatus::Running {
                format!("Elapsed: {}s", elapsed.as_secs())
            } else {
                format!("Wall: {:.2}s", elapsed.as_secs_f64())
            });
        }
        if let Some(seconds) = self.bash_timeout_seconds {
            parts.push(format!("Timeout: {seconds}s"));
        }
        if let Some(result) = &self.result {
            if let Some(artifact) = result.get("artifact_id").filter(|value| !value.is_null()) {
                let artifact = artifact
                    .as_str()
                    .map_or_else(|| artifact.to_string(), str::to_owned);
                parts.push(format!("Artifact: {}", palette::display_text(&artifact)));
            }
            if let Some(status) = super::background_status(result) {
                parts.push(status);
            }
            if result["cancelled"] == true {
                parts.push("cancelled".into());
            } else if result["timed_out"] == true {
                parts.push("timed out".into());
            }
            if let Some(code) = result["exit_code"].as_i64().filter(|code| *code != 0) {
                parts.push(format!("Exit: {code}"));
            } else if self.status == EntryStatus::Failed
                && result["cancelled"] != true
                && result["timed_out"] != true
            {
                parts.push("failed".into());
            }
        } else if self.status == EntryStatus::Failed {
            parts.push("failed".into());
        }
        let footer = parts.join(" · ");
        if self.footer == footer {
            return false;
        }
        self.footer = footer;
        self.layout = None;
        true
    }

    pub(super) fn is_file_read(&self) -> bool {
        self.tool.as_deref() == Some("read") && self.kind != EntryKind::Read
    }

    pub(super) fn is_edit(&self) -> bool {
        self.tool.as_deref() == Some("edit")
    }

    pub(super) fn is_search(&self) -> bool {
        matches!(self.tool.as_deref(), Some("grep" | "glob"))
    }

    pub(super) fn edit_path(&self) -> &str {
        self.arguments["path"]
            .as_str()
            .or_else(|| crate::tools::edit_path(&self.arguments))
            .unwrap_or_default()
    }

    pub(super) fn edit_header(&self) -> Line<'static> {
        let style = Style::default()
            .fg(palette::EDIT_TITLE)
            .bg(palette::EDIT_BG);
        let meta = style.fg(palette::EDIT_META);
        let path = self.edit_path();
        let mut spans = vec![
            Span::styled(format!("{} Edit: ", palette::tool_icon("edit")), style),
            Span::styled(
                if path.ends_with(".rs") {
                    "\u{e7a8} "
                } else {
                    "\u{f1c9} "
                },
                meta,
            ),
            Span::styled(
                palette::display_text(path).into_owned(),
                style.fg(palette::ACTIVITY),
            ),
        ];
        if let EntryBody::Diff(FileDiff::Text { added, removed, .. }) = &self.body {
            spans.extend([
                Span::styled(" ⟨", meta),
                Span::styled(format!("+{added}"), style.fg(palette::EDIT_ADDED)),
                Span::styled("/", meta),
                Span::styled(format!("-{removed}"), style.fg(palette::EDIT_REMOVED)),
                Span::styled("⟩ ", meta),
            ]);
        } else if self.status != EntryStatus::Complete {
            spans.push(Span::styled(
                if self.status == EntryStatus::Failed {
                    " · failed "
                } else {
                    " · running "
                },
                if self.status == EntryStatus::Failed {
                    style.fg(palette::EDIT_REMOVED)
                } else {
                    meta
                },
            ));
        } else {
            spans.push(Span::styled(" ", style));
        }
        Line::from(spans)
    }

    pub(super) fn source(&self, source: Source) -> &str {
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

    pub(super) fn line<'a>(&'a self, line: &'a LayoutLine) -> Line<'a> {
        if self.is_edit() && matches!(line.source(), Some(Source::Title)) {
            return self.edit_header();
        }
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
            if let EntryBody::Activity(frame) = self.body {
                return Activity::sweep_line(text.as_ref(), frame);
            }
            return Line::styled(
                text,
                Style::default()
                    .fg(palette::ACTIVITY)
                    .add_modifier(Modifier::BOLD),
            );
        }
        if self.kind == EntryKind::Bash {
            let color = if matches!(source, Source::Title) {
                palette::TOOL_COMMAND
            } else if matches!(source, Source::Footer) && self.status == EntryStatus::Failed {
                palette::REMOVED
            } else if matches!(source, Source::Footer) && self.status == EntryStatus::Complete {
                palette::ADDED
            } else {
                MUTED_TEXT
            };
            return Line::styled(text, Style::default().fg(color).bg(BASH_BACKGROUND));
        }
        if self.kind == EntryKind::Read && matches!(source, Source::Title) {
            let mut spans = Vec::new();
            if title_end {
                let color = match self.status {
                    EntryStatus::Running => palette::ACCENT,
                    EntryStatus::Complete => palette::ADDED,
                    EntryStatus::Failed => palette::REMOVED,
                };
                spans.push(Span::styled(" · ", Style::default().fg(color)));
            }
            if range.start == 0 {
                let (label, path) = text
                    .find(' ')
                    .map_or((text.as_ref(), ""), |end| text.split_at(end));
                spans.push(Span::styled(
                    label.to_owned(),
                    Style::default()
                        .fg(self.tool_accent())
                        .add_modifier(Modifier::BOLD),
                ));
                spans.push(Span::styled(
                    path.to_owned(),
                    Style::default().fg(palette::READ_TEXT),
                ));
            } else {
                spans.push(Span::styled(text, Style::default().fg(palette::READ_TEXT)));
            }
            spans.push(Span::raw(" "));
            return Line::from(spans).style(Style::default().bg(palette::READ_BG));
        }
        if matches!(source, Source::Title) {
            let mut spans = if self.tool.is_some() {
                if range.start == 0 {
                    self.tool_title(text.as_ref()).spans
                } else {
                    vec![Span::styled(text, Style::default().fg(palette::ACTIVITY))]
                }
            } else {
                vec![Span::styled(
                    text,
                    Style::default()
                        .fg(self.kind.color())
                        .add_modifier(Modifier::BOLD),
                )]
            };
            if self.tool.is_some() && title_end && self.display_title.is_none() {
                let (status, color) = match self.status {
                    EntryStatus::Running => (" · running", palette::ACCENT),
                    EntryStatus::Complete => (" · ok", palette::ADDED),
                    EntryStatus::Failed => (" · failed", palette::REMOVED),
                };
                spans.push(Span::styled(status, Style::default().fg(color)));
            }
            Line::from(spans)
        } else {
            let style = if self.kind == EntryKind::Read {
                Style::default().fg(palette::DIM).bg(palette::READ_BG)
            } else if matches!((self.kind, source), (EntryKind::Diff, _)) {
                Style::default().fg(palette::MUTED)
            } else if self.tool.is_some() {
                Style::default().add_modifier(Modifier::DIM)
            } else {
                Style::default()
            };
            Line::styled(text, style)
        }
    }

    pub(super) fn ensure_layout(&mut self, width: u16) {
        if matches!(self.kind, EntryKind::Assistant | EntryKind::Think)
            && crate::tui::render_worker::enabled()
        {
            let source = if self.text.trim().is_empty() && !self.refusal.is_empty() {
                &self.refusal
            } else {
                &self.text
            };
            self.layout = Some(
                self.render
                    .layout(source, width, self.kind == EntryKind::Think),
            );
            return;
        }
        if self.kind == EntryKind::Bash
            && self.status == EntryStatus::Running
            && self.footer.is_empty()
        {
            self.update_bash_footer(self.started.map(|started| started.elapsed()));
        }
        if !self.hidden
            && self.tool.as_deref() == Some("read")
            && self.kind != EntryKind::Read
            && self.display_title.is_none()
        {
            self.display_title = Some(self.read_summary(true));
        }
        if self.markdown_dirty {
            self.body = EntryBody::Markdown(crate::tui::markdown::parse(self.visible_text()));
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
            self.layout = Some(std::sync::Arc::new(EntryLayout {
                width,
                height: 0,
                lines: Vec::new(),
            }));
            return;
        }
        if self.kind == EntryKind::Error {
            let border_style = Style::default().fg(palette::ERROR_BORDER);
            let mut rows = Vec::new();
            // 极窄窗口优先保留正文；空间恢复后重新生成完整外框。
            if width >= 6 {
                let title = if width >= 9 { " Error " } else { "" };
                rows.push(Line::styled(
                    format!(
                        "╭{title}{}╮",
                        "─".repeat(usize::from(width) - title.len() - 2)
                    ),
                    border_style,
                ));
            }
            let inner = if width >= 6 { width - 4 } else { width };
            for mut line in wrap_block(block_line(self.text.clone(), Style::default()), inner, "") {
                if width >= 6 {
                    let padding = usize::from(inner).saturating_sub(line.width());
                    line.spans.insert(0, Span::styled("│ ", border_style));
                    line.spans.push(Span::styled(
                        format!("{} │", " ".repeat(padding)),
                        border_style,
                    ));
                }
                rows.push(line);
            }
            if width >= 6 {
                rows.push(Line::styled(
                    format!("╰{}╯", "─".repeat(usize::from(width) - 2)),
                    border_style,
                ));
            }
            let lines: Vec<_> = rows
                .into_iter()
                .map(|line| LayoutLine {
                    content: LayoutContent::Styled(line),
                    height: 1,
                })
                .collect();
            self.layout = Some(std::sync::Arc::new(EntryLayout {
                width,
                height: lines.len(),
                lines,
            }));
            return;
        }
        let mut lines = Vec::new();
        let bash = self.kind == EntryKind::Bash;
        let user = self.kind == EntryKind::User;
        let read = self.kind == EntryKind::Read;
        let edit = self.is_edit();
        let search = self.is_search();
        let border = |source| LayoutLine {
            content: LayoutContent::Source {
                source,
                range: 0..0,
                title_end: false,
            },
            height: 1,
        };
        if bash || search {
            lines.push(border(Source::Top));
        }
        if user {
            lines.push(border(Source::Padding));
        }
        if read {
            // 顶部边框行内嵌标题与状态圆点。
            self.add_lines(Source::Title, &mut lines);
        }
        if edit {
            lines.push(border(Source::Title));
        } else if matches!(self.kind, EntryKind::Diff | EntryKind::Ask) {
            let (status, color) = match self.status {
                EntryStatus::Running => ("running", palette::ACCENT),
                EntryStatus::Complete => ("ok", palette::ADDED),
                EntryStatus::Failed => ("failed", palette::REMOVED),
            };
            let mut header = self.tool_title(&self.title);
            header.spans.push(Span::styled(
                format!(" · {status}"),
                Style::default().fg(color),
            ));
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
                .fg(palette::TOOL_COMMAND)
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
                command
                    .into_iter()
                    .flat_map(|command| {
                        let line = if let Some(text) = command.strip_prefix("$ ") {
                            Line::from(vec![
                                Span::styled("$ ", style.fg(self.tool_accent())),
                                Span::styled(text.to_owned(), style),
                            ])
                            .style(style)
                        } else {
                            let color = if command.starts_with('…') {
                                palette::MUTED
                            } else {
                                palette::TOOL_COMMAND
                            };
                            Line::styled(command, style.fg(color))
                        };
                        wrap_block(line, width, "")
                    })
                    .map(|line| LayoutLine {
                        content: LayoutContent::Styled(line),
                        height: 1,
                    }),
            );
        } else if search {
            let view = tool_display::View {
                name: self.tool.as_deref().unwrap_or_default(),
                text: self.visible_text(),
                arguments: &self.arguments,
                result: self.result.as_ref(),
                running: self.status == EntryStatus::Running,
                failed: self.status == EntryStatus::Failed,
                expanded: self.expanded,
                instruction_read: false,
                context: self.display,
            };
            lines.extend(
                if view.name == "grep" {
                    tool_display::grep_header(&view, width)
                } else {
                    tool_display::glob_header(&view, width)
                }
                .into_iter()
                .map(|line| LayoutLine {
                    content: LayoutContent::Styled(line),
                    height: 1,
                }),
            );
        } else if self.is_file_read() {
            // 普通文件摘要使用无边框树形列表，分开绘制标题、数量、连线与路径。
            for text in self.source(Source::Title).lines() {
                let meta = Style::default().fg(palette::READ_SUMMARY_META);
                let path = Style::default().fg(palette::READ_SUMMARY_PATH);
                let (spans, continuation) = if let Some(count) = text.strip_prefix("● Read (") {
                    (
                        vec![
                            Span::styled(
                                "● Read",
                                Style::default().fg(palette::READ_SUMMARY_TITLE),
                            ),
                            Span::styled(format!(" ({count}"), meta),
                        ],
                        "  ",
                    )
                } else if let Some(path_text) = text.strip_prefix("● Read · ") {
                    (
                        vec![
                            Span::styled(
                                "● Read",
                                Style::default().fg(palette::READ_SUMMARY_TITLE),
                            ),
                            Span::styled(" · ", meta),
                            Span::styled(path_text.to_owned(), path),
                        ],
                        "  ",
                    )
                } else if let Some((branch, path_text)) = text.split_once("─ ") {
                    (
                        vec![
                            Span::styled(format!("{branch}─ "), meta),
                            Span::styled(path_text.to_owned(), path),
                        ],
                        if branch.ends_with('├') {
                            "  │  "
                        } else {
                            "     "
                        },
                    )
                } else {
                    (vec![Span::styled(text.to_owned(), path)], "     ")
                };
                lines.extend(
                    wrap_block(
                        Line::from(spans).style(Style::default().bg(palette::READ_SUMMARY_BG)),
                        width,
                        continuation,
                    )
                    .into_iter()
                    .map(|line| LayoutLine {
                        content: LayoutContent::Styled(line),
                        height: 1,
                    }),
                );
            }
        } else if !read && !self.kind.hides_title() && !matches!(self.body, EntryBody::Tips) {
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
            } else if read {
                Style::default()
                    .fg(if self.status == EntryStatus::Failed {
                        palette::REMOVED
                    } else {
                        palette::READ_TEXT
                    })
                    .bg(palette::READ_BG)
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
                        instruction_read: self.kind == EntryKind::Read,
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
                crate::tui::tips::lines(width)
                    .into_iter()
                    .flat_map(|line| wrap_block(line, width, ""))
                    .map(|line| LayoutLine {
                        content: LayoutContent::Styled(line),
                        height: 1,
                    }),
            );
        } else if let EntryBody::Markdown(document) = &self.body {
            // 思考普通文本继承灰色，保留语义 span 的颜色与粗体等修饰。
            let base_style =
                (self.kind == EntryKind::Think).then(|| Style::default().fg(MUTED_TEXT));
            lines.extend(document.layout(width).into_iter().map(|mut line| {
                if let Some(style) = base_style {
                    line = line.style(style);
                }
                LayoutLine {
                    content: LayoutContent::Styled(line),
                    height: 1,
                }
            }));
        } else if let EntryBody::Diff(diff) = &self.body {
            lines.extend(
                diff_layout_styled(
                    diff,
                    width,
                    self.expanded || self.display.full,
                    edit.then(|| self.edit_path()),
                )
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
        if bash {
            lines.push(border(Source::Bottom));
        }
        if search {
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
        if read {
            if !self.footer.is_empty() {
                lines.push(border(Source::Divider));
                self.add_lines(Source::Footer, &mut lines);
            }
            lines.push(border(Source::Bottom));
        }
        if user {
            lines.push(border(Source::Padding));
        }
        if edit {
            lines.push(border(Source::Bottom));
        }
        let mut height = 0;
        for line in &mut lines {
            line.height = if self.kind == EntryKind::Activity
                // 边框行必须保持单行，窄宽度下标题按现有规则裁剪。
                || ((read || edit) && matches!(line.source(), Some(Source::Title | Source::Bottom)))
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
        self.layout = Some(std::sync::Arc::new(EntryLayout {
            width,
            height,
            lines,
        }));
    }

    pub(super) fn read_summary(&self, show_success: bool) -> String {
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
            EntryStatus::Running => " · running",
            EntryStatus::Complete if show_success => " · ok",
            EntryStatus::Complete => "",
            EntryStatus::Failed => " · failed",
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
            "● Read · {}{range}{image}{status}{truncated}",
            palette::display_text(path)
        )
    }

    pub(super) fn add_lines(&self, source: Source, lines: &mut Vec<LayoutLine>) {
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

impl Activity {
    pub(super) fn new(started: Instant) -> Self {
        let mut activity = Self {
            entry: TranscriptEntry::new(EntryKind::Activity, String::new(), String::new(), None),
            started: Some(started),
            waiting_started: None,
            label: "Requesting model".into(),
            seconds: Some(0),
            run_seconds: 0,
            running: true,
        };
        activity.entry.body = EntryBody::Activity(0);
        activity.refresh();
        activity
    }

    pub(super) fn tick(&mut self, now: Instant) -> bool {
        let Some(started) = self.started else {
            return false;
        };
        let elapsed = now.saturating_duration_since(started);
        let run_seconds = elapsed.as_secs();
        let frame = elapsed.as_millis() / 80;
        let seconds = now
            .saturating_duration_since(self.waiting_started.unwrap_or(started))
            .as_secs();
        if Some(seconds) == self.seconds
            && run_seconds == self.run_seconds
            && matches!(self.entry.body, EntryBody::Activity(previous) if previous == frame)
        {
            return false;
        }
        self.entry.body = EntryBody::Activity(frame);
        self.run_seconds = run_seconds;
        if self.seconds != Some(seconds) {
            self.seconds = Some(seconds);
            self.refresh();
        }
        true
    }

    pub(super) fn refresh(&mut self) {
        self.entry.text = if self.running {
            if self.waiting_started.is_some() {
                format!("\u{f12b7} {} · {}", self.label, self.duration_text())
            } else {
                format!("\u{f12b7} {}", self.label)
            }
        } else {
            format!("✻ {} · {}", self.label, self.duration_text())
        };
        self.entry.layout = None;
    }

    pub(super) fn sweep_line(text: &str, frame: u128) -> Line<'static> {
        let base = Style::default().fg(palette::DIM);
        let label = text.strip_prefix("\u{f12b7} ").unwrap_or(text);
        let line = Line::raw(label);
        let graphemes: Vec<_> = line.styled_graphemes(base).collect();
        // 金色光带从首字移至末字，尾部留出短暂停顿后重新开始。
        let head = (frame % (graphemes.len() as u128 + 8)) as usize;
        let mut spans = vec![Span::styled(
            "\u{f12b7} ",
            base.add_modifier(Modifier::BOLD),
        )];
        spans.extend(graphemes.into_iter().enumerate().map(|(index, grapheme)| {
            let color = match index.abs_diff(head) {
                0 | 1 => palette::ACCENT,
                2 => Color::Rgb(171, 135, 68),
                3 => Color::Rgb(119, 109, 87),
                _ => palette::DIM,
            };
            Span::styled(grapheme.symbol.to_owned(), base.fg(color))
        }));
        Line::from(spans)
    }

    pub(super) fn duration_text(&self) -> String {
        self.seconds
            .map(Self::format_duration)
            .unwrap_or_else(|| "—".into())
    }

    pub(super) fn format_duration(seconds: u64) -> String {
        match seconds {
            0..60 => format!("{seconds}s"),
            60..3600 => format!("{}m {}s", seconds / 60, seconds % 60),
            _ => format!(
                "{}h {}m {}s",
                seconds / 3600,
                seconds / 60 % 60,
                seconds % 60
            ),
        }
    }

    pub(super) fn phase(&mut self, label: &str) {
        if self.running && self.label != "Cancelling" && self.label != label {
            self.label = label.into();
            let now = Instant::now();
            // ask 单独计算等待时间；其他阶段继续使用整轮任务的起点。
            self.waiting_started =
                (label == "Waiting for input" && self.started.is_some()).then_some(now);
            self.tick(now);
            self.refresh();
        }
    }

    pub(super) fn stop(&mut self, label: &str) {
        if self.running {
            self.waiting_started = None;
            self.tick(Instant::now());
            self.started = None;
            self.running = false;
            self.entry.body = EntryBody::Plain;
            self.label = label.into();
            self.refresh();
        }
    }
}
