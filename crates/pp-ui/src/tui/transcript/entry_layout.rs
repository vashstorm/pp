//! 真实物理行布局：把卡片正文排成可滚动、可提交的行。

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
        }
        if !self.hidden && self.tool.as_deref() == Some("read") && self.display_title.is_none() {
            self.display_title = Some(Self::read_summary(std::slice::from_ref(self)));
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
        // 等待回答时只显示交互面板；结束后再保留完整问答卡片。
        // 清单在固定 TODO 区展示，操作过程与成功结果不再生成历史卡片。
        if self.hidden
            || (self.kind == EntryKind::Ask && self.status == EntryStatus::Running)
            || (self.tool.as_deref() == Some("todo") && self.status != EntryStatus::Failed)
        {
            self.layout = Some(std::sync::Arc::new(EntryLayout {
                width,
                height: 0,
                lines: Vec::new(),
            }));
            return;
        }
        if matches!(self.kind, EntryKind::Error | EntryKind::Notice) {
            // 错误与运行时提示共用同一幅框卡：只有边框色、标题与正文色不同。
            let notice = self.kind == EntryKind::Notice;
            let border_style = Style::default().fg(if notice {
                palette::ACCENT
            } else {
                palette::ERROR_BORDER
            });
            let text_style = if notice {
                Style::default().fg(palette::TEXT)
            } else {
                Style::default()
            };
            let label = if notice {
                format!(" {} ", self.title)
            } else {
                " Error ".to_owned()
            };
            let mut rows = Vec::new();
            // 极窄窗口优先保留正文；空间恢复后重新生成完整外框。
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
            self.layout = Some(std::sync::Arc::new(EntryLayout {
                width,
                height: lines.len(),
                lines,
            }));
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
        let edit = self.is_edit();
        let search = self.is_search();
        let file_read = self.is_file_read();
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
        if user || file_read {
            lines.push(border(Source::Padding));
        }
        if self.has_file_header() {
            lines.push(border(Source::Title));
        } else if self.kind == EntryKind::Lsp {
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
            if let Some(snapshot) = &self.shell {
                lines.extend(
                    tool_display::shell_command(snapshot, self.expanded, self.display, width)
                        .into_iter()
                        .map(|line| LayoutLine {
                            content: LayoutContent::Styled(line),
                            height: 1,
                        }),
                );
            } else {
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
            }
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
            // 路径与参数保留独立 span，避免文件名中的冒号或空格干扰着色。
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
        } else if self.kind != EntryKind::Lsp
            && !self.kind.hides_title()
            && !matches!(self.body, EntryBody::Tips)
        {
            self.add_lines(Source::Title, &mut lines);
        }
        if bash {
            lines.push(border(Source::Divider));
        }
        let tool_body = self.is_collapsible_tool()
            && !self.is_argument_preview()
            && !(self.tool.as_deref() == Some("edit") && matches!(self.body, EntryBody::Diff(_)))
            && !matches!(self.body, EntryBody::Lsp(_));
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
            if let Some(preview) = &self.pipeline_preview {
                text.clone_from(preview);
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
        } else if let EntryBody::Lsp(block) = &self.body {
            lines.extend(
                crate::tui::lsp::body(block, width, self.expanded || self.display.full)
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
        if user || file_read {
            lines.push(border(Source::Padding));
        }
        if edit {
            lines.push(border(Source::Bottom));
        }
        let mut height = 0;
        for line in &mut lines {
            line.height = if self.kind == EntryKind::Activity
                // 边框行必须保持单行，窄宽度下标题按现有规则裁剪。
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
            height += line.height;
        }
        self.layout = Some(std::sync::Arc::new(EntryLayout {
            width,
            height,
            lines,
        }));
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
