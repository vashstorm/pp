//! 卡片标题：着色、标题拆段与布局行的来源映射。

use super::*;

impl EntryKind {
    pub(super) fn color(self) -> Color {
        match self {
            Self::User => Color::Blue,
            Self::Assistant => palette::md_text(),
            Self::Bash => Color::Yellow,
            Self::Think | Self::Activity => muted_text(),
            Self::Rg => Color::Cyan,
            Self::Diff | Self::Ask => palette::accent(),
            Self::Lsp => palette::accent(),
            Self::Data => Color::Reset,
            Self::Error => palette::error_border(),
            Self::Notice => palette::accent(),
        }
    }

    pub(super) fn conversation_background(self) -> Option<Color> {
        match self {
            Self::User => Some(user_background()),
            Self::Assistant | Self::Think => Some(assistant_background()),
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

impl TranscriptEntry {
    pub(super) fn tool_title(&self, text: &str) -> Line<'static> {
        if let Some(line) = self.mcp_title(text) {
            return line;
        }
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
            Span::styled(detail.to_owned(), Style::default().fg(palette::activity())),
        ])
    }

    /// MCP 卡片标题：图标与 `mcp` 前缀、服务名、工具名各用独立配色。
    /// 标题被外部改写（例如按结果修正）时返回 `None`，退回通用的两段着色。
    fn mcp_title(&self, text: &str) -> Option<Line<'static>> {
        let name = self.tool.as_deref()?;
        let (server, tool) = palette::mcp_parts(name)?;
        let icon = palette::tool_icon(name);
        let (label, detail) = text
            .find(" · ")
            .map_or((text, ""), |end| text.split_at(end));
        let head = label.strip_prefix(icon)?.strip_prefix(' ')?;
        let server = pp_core::text::display_text(server);
        let tool = pp_core::text::display_text(tool);
        if head != format!("mcp {server} {tool}") {
            return None;
        }
        let title = Style::default().add_modifier(Modifier::BOLD);
        Some(Line::from(vec![
            Span::styled(format!("{icon} mcp"), title.fg(palette::mcp_title())),
            Span::styled(format!(" {server}"), title.fg(palette::mcp_server())),
            Span::styled(format!(" {tool}"), title.fg(palette::mcp_tool())),
            Span::styled(detail.to_owned(), Style::default().fg(palette::activity())),
        ]))
    }

    pub(super) fn source(&self, source: Source) -> &str {
        match source {
            Source::Top | Source::Divider | Source::Bottom | Source::Padding => "",
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

    pub(super) fn line<'a>(&'a self, line: &'a LayoutLine) -> Line<'a> {
        if self.has_file_header() && matches!(line.source(), Some(Source::Title)) {
            return self.file_header();
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
            pp_core::text::display_text(text)
        } else {
            std::borrow::Cow::Borrowed(text)
        };
        if self.kind == EntryKind::Activity {
            return Line::styled(
                text,
                Style::default()
                    .fg(palette::activity())
                    .add_modifier(Modifier::BOLD),
            );
        }
        if self.kind == EntryKind::Bash {
            let color = if matches!(source, Source::Title) {
                palette::tool_command()
            } else if matches!(source, Source::Footer) && self.status == EntryStatus::Failed {
                palette::removed()
            } else if matches!(source, Source::Footer) && self.status == EntryStatus::Complete {
                palette::added()
            } else {
                muted_text()
            };
            return Line::styled(text, Style::default().fg(color).bg(bash_background()));
        }
        if matches!(source, Source::Title) {
            let mut spans = if self.tool.is_some() {
                if range.start == 0 {
                    self.tool_title(text.as_ref()).spans
                } else {
                    vec![Span::styled(text, Style::default().fg(palette::activity()))]
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
                    EntryStatus::Running => (" · running", palette::accent()),
                    EntryStatus::Complete => (" · ok", palette::added()),
                    EntryStatus::Failed => (" · failed", palette::removed()),
                };
                spans.push(Span::styled(status, Style::default().fg(color)));
            }
            Line::from(spans)
        } else {
            let style = if matches!((self.kind, source), (EntryKind::Diff, _)) {
                Style::default().fg(palette::muted())
            } else if self.tool.is_some() {
                Style::default().add_modifier(Modifier::DIM)
            } else {
                Style::default()
            };
            Line::styled(text, style)
        }
    }
}
