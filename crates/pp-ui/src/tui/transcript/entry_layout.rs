//! Real physical line layout: lay out the card body as scrollable, committable lines.

use super::*;

impl TranscriptEntry {
    pub(super) fn ensure_layout(&mut self, width: u16) {
        let streaming = self.status == EntryStatus::Running;
        if matches!(self.kind, EntryKind::Assistant | EntryKind::Think)
            && crate::tui::render_worker::enabled()
        {
            let source = if self.text.trim().is_empty() && !self.refusal.is_empty() {
                &self.refusal
            } else {
                &self.text
            };
            self.layout =
                Some(
                    self.render
                        .layout(source, width, self.kind == EntryKind::Think, streaming),
                );
            return;
        }
        if self.kind == EntryKind::Bash
            && self.status == EntryStatus::Running
            && self.footer.is_empty()
        {
            self.update_bash_footer(self.started.map(|started| started.elapsed()));
        } else if self.kind == EntryKind::Eval
            && self.status == EntryStatus::Running
            && self.footer.is_empty()
        {
            self.update_eval_footer(self.started.map(|started| started.elapsed()));
        }
        if !self.hidden && self.tool.as_deref() == Some("read") && self.display_title.is_none() {
            self.display_title = Some(Self::read_summary(std::slice::from_ref(self)));
            let (path, meta) = self.read_path_parts();
            self.read_group = vec![card::ReadItem { path, meta }];
        }
        if self.markdown_dirty {
            self.body = EntryBody::Markdown(crate::tui::markdown::parse_streaming(
                self.visible_text(),
                streaming,
            ));
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
        // While waiting for an answer only the interaction panel shows; the full Q&A card is kept once it finishes.
        // The checklist shows in the fixed TODO area, so the steps taken and a successful result produce no history card.
        if self.hidden
            || (self.kind == EntryKind::Ask && self.status == EntryStatus::Running)
            || (self.tool.as_deref() == Some("todo") && self.status != EntryStatus::Failed)
        {
            self.layout = Some(std::sync::Arc::new(EntryLayout::owned(width, Vec::new())));
            return;
        }
        if matches!(self.kind, EntryKind::Error | EntryKind::Notice) {
            // Errors and runtime notices share one frame card: only the border color, title and body color differ.
            let notice = self.kind == EntryKind::Notice;
            let border_style = Style::default().fg(if notice {
                palette::accent()
            } else {
                palette::error_border()
            });
            let text_style = if notice {
                Style::default().fg(palette::text())
            } else {
                Style::default()
            };
            let label = if notice {
                format!(" {} ", self.title)
            } else {
                " Error ".to_owned()
            };
            let mut rows = Vec::new();
            // Very narrow windows favor the body; the full frame is regenerated once space returns.
            if width >= 6 {
                let title = if usize::from(width) >= label.len() + 2 {
                    label.as_str()
                } else {
                    ""
                };
                rows.push(Line::styled(
                    format!(
                        "╭{title}{}╮",
                        "─".repeat(usize::from(width) - title.len() - 2)
                    ),
                    border_style,
                ));
            }
            let inner = if width >= 6 { width - 4 } else { width };
            for mut line in wrap_block(block_line(self.text.clone(), text_style), inner, "") {
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
            self.layout = Some(std::sync::Arc::new(EntryLayout::owned(width, lines)));
            return;
        }
        if let Some(lines) = card::build_non_classic_layout(self, width) {
            self.layout = Some(std::sync::Arc::new(EntryLayout::owned(width, lines)));
            return;
        }
        if self.is_wait_card() {
            let lines = tool_display::wait_body(
                tool_display::View {
                    name: "wait",
                    text: &self.text,
                    arguments: &self.arguments,
                    result: self.result.as_ref(),
                    running: self.status == EntryStatus::Running,
                    failed: self.status == EntryStatus::Failed,
                    expanded: self.expanded,
                    context: self.display,
                },
                width,
            )
            .into_iter()
            .map(|line| LayoutLine {
                content: LayoutContent::Styled(line),
                height: 1,
            })
            .collect::<Vec<_>>();
            self.layout = Some(std::sync::Arc::new(EntryLayout::owned(width, lines)));
            return;
        }
        let mut lines = Vec::new();
        let bash = self.kind == EntryKind::Bash;
        let eval = self.kind == EntryKind::Eval;
        let user = self.kind == EntryKind::User;
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
        if bash || search || eval {
            lines.push(border(Source::Top));
        }
        if user {
            lines.push(border(Source::Padding));
        }
        if self.has_file_header() {
            lines.push(border(Source::Title));
        } else if self.kind == EntryKind::Lsp {
            let (status, color) = match self.status {
                EntryStatus::Running => ("running", palette::accent()),
                EntryStatus::Complete => ("ok", palette::added()),
                EntryStatus::Failed => ("failed", palette::removed()),
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
        } else if matches!(self.kind, EntryKind::Diff | EntryKind::Ask) {
            let (status, color) = match self.status {
                EntryStatus::Running => ("running", palette::accent()),
                EntryStatus::Complete => ("ok", palette::added()),
                EntryStatus::Failed => ("failed", palette::removed()),
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
            self.push_bash_command_lines(width, &mut lines);
        } else if eval {
            self.push_eval_code_lines(width, &mut lines);
        } else if search {
            let view = tool_display::View {
                name: self.tool.as_deref().unwrap_or_default(),
                text: self.visible_text(),
                arguments: &self.arguments,
                result: self.result.as_ref(),
                running: self.status == EntryStatus::Running,
                failed: self.status == EntryStatus::Failed,
                expanded: self.expanded,
                context: self.display,
            };
            lines.extend(
                if view.name == "rg" {
                    tool_display::rg_header(&view, width)
                } else if view.name == "web_search" {
                    tool_display::web_search_header(&view, width)
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
            // Paths and arguments keep separate spans so colons or spaces in a file name do not disturb the coloring.
            let summary = self
                .display_title
                .as_ref()
                .expect("read summary initialized");
            for (index, row) in summary.iter().enumerate() {
                let continuation = if index == 0 {
                    "    "
                } else if index + 1 < summary.len() {
                    "    │  "
                } else {
                    "       "
                };
                lines.extend(
                    wrap_block(row.clone(), width, continuation)
                        .into_iter()
                        .map(|line| LayoutLine {
                            content: LayoutContent::Styled(line),
                            height: 1,
                        }),
                );
            }
        } else if self.tool.as_deref() == Some("task") {
            let (count, (status, color)) = card::anatomy::task_header(self);
            lines.extend(
                tool_display::task::header(width, &count, status, color)
                    .into_iter()
                    .map(|line| LayoutLine {
                        content: LayoutContent::Styled(line),
                        height: 1,
                    }),
            );
        } else if self.kind != EntryKind::Lsp
            && !self.kind.hides_title()
            && !matches!(self.body, EntryBody::Tips | EntryBody::Help(_))
        {
            self.add_lines(Source::Title, &mut lines);
        }
        let eval_has_output = !eval
            || self.status != EntryStatus::Complete
            || !self.visible_text().is_empty()
            || !self.stdout.is_empty()
            || !self.stderr.is_empty();
        if bash || (eval && eval_has_output) {
            lines.push(border(Source::Divider));
        }
        let tool_body = (self.is_collapsible_tool() || self.is_file_read())
            && !self.is_argument_preview()
            && !(self.tool.as_deref() == Some("edit") && matches!(self.body, EntryBody::Diff(_)))
            && !matches!(self.body, EntryBody::Lsp(_))
            && eval_has_output;
        if tool_body {
            self.push_tool_body_lines(width, &mut lines);
        } else if let EntryBody::ArgumentsPreview { truncated } = self.body {
            lines.extend(
                super::preview::layout(
                    &self.text,
                    self.tool.as_deref().unwrap_or_default(),
                    width,
                    self.expanded || self.display.full,
                    truncated,
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
        } else if let EntryBody::Help(block) = &self.body {
            lines.extend(
                crate::tui::help::lines(width, block)
                    .into_iter()
                    .flat_map(|line| wrap_block(line, width, ""))
                    .map(|line| LayoutLine {
                        content: LayoutContent::Styled(line),
                        height: 1,
                    }),
            );
        } else if let EntryBody::Markdown(document) = &self.body {
            // Plain thinking text inherits gray while keeping the semantic span colors and modifiers such as bold.
            let base_style =
                (self.kind == EntryKind::Think).then(|| Style::default().fg(muted_text()));
            lines.extend(document.layout(width).into_iter().map(|mut line| {
                if let Some(style) = base_style {
                    line = line.style(style);
                }
                LayoutLine {
                    content: LayoutContent::Styled(line),
                    height: 1,
                }
            }));
            if let Some(start) = document.table_tail_start
                && let Some(line) = crate::tui::render_worker::table_tail_preview(
                    &self.visible_text()[start..],
                    width,
                    self.kind == EntryKind::Think,
                )
            {
                lines.push(LayoutLine {
                    content: LayoutContent::Styled(line),
                    height: 1,
                });
            }
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
                            Style::default().fg(palette::muted()),
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
                        block_line(self.text.clone(), Style::default().fg(palette::muted())),
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
        } else if let EntryBody::Lsp(block) = &self.body {
            lines.extend(
                crate::tui::lsp::body(
                    block,
                    self.arguments["action"].as_str().unwrap_or_default(),
                    width,
                    self.expanded || self.display.full,
                )
                .into_iter()
                .map(|line| LayoutLine {
                    content: LayoutContent::Styled(line),
                    height: 1,
                }),
            );
        } else if !self.visible_text().is_empty() {
            if self.kind == EntryKind::Ask {
                lines.extend(
                    wrap_block(
                        block_line(
                            self.visible_text().to_owned(),
                            Style::default().fg(palette::muted()),
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
            } else if user && let Some(user_lines) = self.user_body_lines(width) {
                lines.extend(user_lines);
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
        if !self.diagnostics.is_empty() {
            lines.extend(
                crate::tui::lsp::diagnostics_lines(
                    &self.diagnostics,
                    width,
                    self.expanded || self.display.full,
                )
                .into_iter()
                .map(|line| LayoutLine {
                    content: LayoutContent::Styled(line),
                    height: 1,
                }),
            );
        }
        if bash || search || eval {
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
        if edit {
            lines.push(border(Source::Bottom));
        }
        for line in &mut lines {
            line.height = if self.kind == EntryKind::Activity
                // Border rows must stay single-line; at narrow widths the title is trimmed by the existing rules.
                || (edit && matches!(line.source(), Some(Source::Title | Source::Bottom)))
                || matches!(line.content, LayoutContent::Styled(_))
            {
                1
            } else {
                Paragraph::new(self.line(line))
                    .wrap(Wrap { trim: false })
                    .line_count(width)
                    .max(1)
            };
        }
        self.layout = Some(std::sync::Arc::new(EntryLayout::owned(width, lines)));
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

    fn push_bash_command_lines(&self, width: u16, lines: &mut Vec<LayoutLine>) {
        if let Some(snapshot) = &self.shell {
            lines.extend(
                tool_display::shell_command(snapshot, self.expanded, self.display, width)
                    .into_iter()
                    .map(|line| LayoutLine {
                        content: LayoutContent::Styled(line),
                        height: 1,
                    }),
            );
            return;
        }
        let bash_surface = if self.tool.as_deref() == Some("bash_task") {
            card::CardSurface::BashTask
        } else {
            card::CardSurface::Bash
        };
        let style = card::surface_style(bash_surface, palette::tool_command());
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
                            palette::muted()
                        } else {
                            palette::tool_command()
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
    }

    fn push_eval_code_lines(&self, width: u16, lines: &mut Vec<LayoutLine>) {
        let lang = tool_display::EvalLang::from_entry(&self.arguments, self.result.as_ref());
        let title_arg = self.arguments["title"].as_str();
        let code = self.arguments["code"].as_str().unwrap_or("");
        lines.extend(
            tool_display::eval_code_lines(
                &lang,
                title_arg,
                code,
                self.expanded,
                self.display,
                width,
            )
            .into_iter()
            .map(|line| LayoutLine {
                content: LayoutContent::Styled(line),
                height: 1,
            }),
        );
    }

    fn push_tool_body_lines(&self, width: u16, lines: &mut Vec<LayoutLine>) {
        let bash = self.kind == EntryKind::Bash;
        let eval = self.kind == EntryKind::Eval;
        // Borrow directly when the body consists of stdout only, avoiding a copy of the whole tool output every frame.
        let joined;
        let text: &str = if self.visible_text().is_empty() && self.stderr.is_empty() {
            self.stdout.strip_suffix('\n').unwrap_or(&self.stdout)
        } else {
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
            joined = text;
            &joined
        };
        let style = if bash {
            let bash_surface = if self.tool.as_deref() == Some("bash_task") {
                card::CardSurface::BashTask
            } else {
                card::CardSurface::Bash
            };
            card::surface_style(bash_surface, muted_text())
        } else if eval {
            card::surface_style(card::CardSurface::Eval, palette::muted())
        } else {
            Style::default().add_modifier(Modifier::DIM)
        };
        lines.extend(
            tool_display::body(
                tool_display::View {
                    name: self.tool.as_deref().unwrap_or_default(),
                    text,
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
    }

    pub(super) fn collect_non_classic_sections(
        &self,
        width: u16,
        allow_inline_bash: bool,
    ) -> (Vec<LayoutLine>, Vec<LayoutLine>) {
        let mut input_lines = Vec::new();
        let mut output_lines = Vec::new();

        if self.is_wait_card() {
            let (_, body) = tool_display::wait_inner_body(
                tool_display::View {
                    name: "wait",
                    text: &self.text,
                    arguments: &self.arguments,
                    result: self.result.as_ref(),
                    running: self.status == EntryStatus::Running,
                    failed: self.status == EntryStatus::Failed,
                    expanded: self.expanded,
                    context: self.display,
                },
                width,
                false,
            );
            for line in body {
                let safe = pp_core::text::display_text(&line.to_string()).into_owned();
                for row in wrap_block(Line::styled(safe, line.style), width.max(1), "") {
                    output_lines.push(LayoutLine {
                        content: LayoutContent::Styled(row),
                        height: 1,
                    });
                }
            }
            return (input_lines, output_lines);
        }

        let bash = self.kind == EntryKind::Bash;
        let eval = self.kind == EntryKind::Eval;
        let edit = self.is_edit();

        if bash {
            if !(allow_inline_bash && card::anatomy::inline_bash_command(self, width).is_some()) {
                self.push_bash_command_lines(width, &mut input_lines);
            }
        } else if eval {
            self.push_eval_code_lines(width, &mut input_lines);
        }

        if self.is_file_read() && self.read_group.len() > 1 {
            for idx in 0..self.read_group.len() {
                output_lines.push(LayoutLine {
                    content: LayoutContent::Source {
                        source: Source::ReadItem(idx),
                        range: 0..0,
                        title_end: false,
                    },
                    height: 1,
                });
            }
        }

        let eval_has_output = !eval
            || self.status != EntryStatus::Complete
            || !self.visible_text().is_empty()
            || !self.stdout.is_empty()
            || !self.stderr.is_empty();
        let tool_body = (self.is_collapsible_tool() || self.is_file_read())
            && !self.is_argument_preview()
            && !(self.tool.as_deref() == Some("edit") && matches!(self.body, EntryBody::Diff(_)))
            && !matches!(self.body, EntryBody::Lsp(_))
            && eval_has_output;

        if tool_body {
            // The "Wrote N bytes" of a successful `write` already shows in the header meta, so only the body content (if any) is kept.
            if !(self.tool.as_deref() == Some("write")
                && self.status == EntryStatus::Complete
                && self.visible_text().starts_with("Wrote ")
                && self.arguments["content"].as_str().unwrap_or("").is_empty())
            {
                self.push_tool_body_lines(width, &mut output_lines);
            }
        } else if let EntryBody::ArgumentsPreview { truncated } = self.body {
            output_lines.extend(
                super::preview::layout(
                    &self.text,
                    self.tool.as_deref().unwrap_or_default(),
                    width,
                    self.expanded || self.display.full,
                    truncated,
                )
                .into_iter()
                .map(|line| LayoutLine {
                    content: LayoutContent::Styled(line),
                    height: 1,
                }),
            );
        } else if let EntryBody::Diff(diff) = &self.body {
            output_lines.extend(
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
                output_lines.extend(
                    wrap_block(
                        block_line(
                            format!("Warning: {warning}"),
                            Style::default().fg(palette::muted()),
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
            output_lines.extend(block.layout(width).into_iter().map(|line| LayoutLine {
                content: LayoutContent::Styled(line),
                height: 1,
            }));
            if !self.text.is_empty() {
                output_lines.extend(
                    wrap_block(
                        block_line(self.text.clone(), Style::default().fg(palette::muted())),
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
        } else if let EntryBody::Lsp(block) = &self.body {
            output_lines.extend(
                crate::tui::lsp::body(
                    block,
                    self.arguments["action"].as_str().unwrap_or_default(),
                    width,
                    self.expanded || self.display.full,
                )
                .into_iter()
                .map(|line| LayoutLine {
                    content: LayoutContent::Styled(line),
                    height: 1,
                }),
            );
        } else if !self.visible_text().is_empty() && !self.is_file_read() {
            self.push_wrapped_source(Source::Text, width, &mut output_lines);
        }

        if !tool_body && !self.stdout.is_empty() {
            if !bash {
                self.push_wrapped_source(Source::StdoutLabel, width, &mut output_lines);
            }
            self.push_wrapped_source(Source::Stdout, width, &mut output_lines);
        }
        if !tool_body && !self.stderr.is_empty() {
            self.push_wrapped_source(Source::StderrLabel, width, &mut output_lines);
            self.push_wrapped_source(Source::Stderr, width, &mut output_lines);
        }
        if !self.diagnostics.is_empty() {
            output_lines.extend(
                crate::tui::lsp::diagnostics_lines(
                    &self.diagnostics,
                    width,
                    self.expanded || self.display.full,
                )
                .into_iter()
                .map(|line| LayoutLine {
                    content: LayoutContent::Styled(line),
                    height: 1,
                }),
            );
        }
        if self.tool.as_deref() == Some("write")
            && self.status == EntryStatus::Complete
            && self.text.starts_with("Warning: ")
        {
            for row in wrap_block(
                block_line(self.text.clone(), Style::default().fg(palette::accent())),
                width,
                "",
            ) {
                output_lines.push(LayoutLine {
                    content: LayoutContent::Styled(row),
                    height: 1,
                });
            }
        }

        (input_lines, output_lines)
    }

    fn push_wrapped_source(&self, source: Source, width: u16, out: &mut Vec<LayoutLine>) {
        let mut raw = Vec::new();
        self.add_lines(source, &mut raw);
        for line in &raw {
            let borrowed = self.line(line);
            let owned = Line::from(
                borrowed
                    .spans
                    .into_iter()
                    .map(|s| Span::styled(s.content.into_owned(), s.style))
                    .collect::<Vec<_>>(),
            )
            .style(borrowed.style);
            for row in wrap_block(owned, width, "") {
                out.push(LayoutLine {
                    content: LayoutContent::Styled(row),
                    height: 1,
                });
            }
        }
    }
}
