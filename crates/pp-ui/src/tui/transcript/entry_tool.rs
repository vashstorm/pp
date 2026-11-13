//! 工具卡片的标题、页脚、路径与连续读取摘要。

use super::*;

impl TranscriptEntry {
    pub(super) fn tool_accent(&self) -> Color {
        palette::tool_accent(self.tool.as_deref().unwrap_or_default())
    }

    pub(super) fn is_collapsible_tool(&self) -> bool {
        // 迟到诊断没有工具调用 ID，但同样可以用 Ctrl + O 展开完整报告。
        self.kind == EntryKind::Lsp || (self.tool.is_some() && self.kind != EntryKind::Ask)
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
            parts.push(if seconds == 0 {
                "Timeout: none".into()
            } else {
                format!("Timeout: {seconds}s")
            });
        }
        if let Some(result) = &self.result {
            if let Some(artifact) = result.get("artifact_id").filter(|value| !value.is_null()) {
                let artifact = artifact
                    .as_str()
                    .map_or_else(|| artifact.to_string(), str::to_owned);
                parts.push(format!(
                    "Artifact: {}",
                    pp_core::text::display_text(&artifact)
                ));
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
        if let Some(badge) = &self.pipes_badge {
            parts.push("Pipes: /pipes (F8)".into());
            if badge.restored {
                parts.push("temporary pipe bodies unavailable".into());
            }
        }
        let footer_warning = self.pipes_badge.as_ref().map_or_else(String::new, |badge| {
            let mut warnings = Vec::new();
            if badge.status.nonzero_stages > 0 {
                warnings.push(format!(
                    "Non-zero stage results: {}",
                    badge.status.nonzero_stages
                ));
            }
            if badge.status.incomplete {
                warnings.push("stage output incomplete".into());
            }
            warnings.join(" · ")
        });
        let footer = parts.join(" · ");
        if self.footer == footer && self.footer_warning == footer_warning {
            return false;
        }
        self.footer = footer;
        self.footer_warning = footer_warning;
        self.layout = None;
        true
    }

    /// 以最终 `result.pipes` 为准重建 `/pipes` 入口；没有真实管道时清空。
    pub(super) fn sync_pipes_badge(&mut self, restored: bool) {
        self.pipes_badge = self
            .result
            .as_ref()
            .and_then(|result| result.get("pipes"))
            .and_then(pp_runtime::tools::output::pipes_status)
            .map(|status| super::PipesBadge { status, restored });
    }

    pub(super) fn is_file_read(&self) -> bool {
        self.tool.as_deref() == Some("read")
    }

    pub(super) fn is_edit(&self) -> bool {
        self.tool.as_deref() == Some("edit")
    }

    /// 生成中的文件工具参数预览：正文是未提交的 JSON 分片。
    pub(super) fn is_argument_preview(&self) -> bool {
        matches!(self.body, EntryBody::ArgumentsPreview { .. })
    }

    /// 已提交 `edit` 卡片与生成中的 `write` 预览共用文件卡片标题；
    /// 已提交 `write` 卡片仍走通用工具标题。
    pub(super) fn has_file_header(&self) -> bool {
        self.is_edit() || (self.tool.as_deref() == Some("write") && self.is_argument_preview())
    }

    pub(super) fn is_search(&self) -> bool {
        matches!(self.tool.as_deref(), Some("rg" | "glob"))
    }

    pub(super) fn is_wait_card(&self) -> bool {
        self.tool.as_deref() == Some("wait")
            && (self.result.is_none()
                || self
                    .result
                    .as_ref()
                    .is_some_and(|r| r["jobs"].is_array() || r["ok"] == false))
    }

    pub(super) fn edit_path(&self) -> &str {
        self.arguments["path"]
            .as_str()
            .or_else(|| pp_runtime::tools::edit_path(&self.arguments))
            .unwrap_or_default()
    }

    /// 参数预览与已提交 `edit` 卡片共用的文件卡片标题。
    pub(super) fn file_header(&self) -> Line<'static> {
        let tool = self.tool.as_deref().unwrap_or_default();
        let style = Style::default()
            .fg(palette::tool_accent(tool))
            .bg(palette::EDIT_BG);
        let meta = style.fg(palette::EDIT_META);
        let label = if tool == "write" { "Write" } else { "Edit" };
        let preview_path;
        let path = if self.is_argument_preview() {
            preview_path = super::preview::path(&self.text);
            preview_path.as_str()
        } else {
            self.edit_path()
        };
        let mut spans = vec![
            Span::styled(format!("{} {label}: ", palette::tool_icon(tool)), style),
            Span::styled(
                if path.ends_with(".rs") {
                    "\u{f1617} "
                } else {
                    "\u{f022e} "
                },
                meta,
            ),
            Span::styled(pp_core::text::display_text(path).into_owned(), style),
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
                if self.is_argument_preview() {
                    " · preview · not saved "
                } else if self.status == EntryStatus::Failed {
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

    fn read_path_spans(&self) -> Vec<Span<'static>> {
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
        vec![
            Span::styled(
                pp_core::text::display_text(path).into_owned(),
                Style::default().fg(palette::READ_SUMMARY_PATH),
            ),
            Span::styled(
                format!("{range}{image}{status}{truncated}"),
                Style::default().fg(palette::ACTIVITY),
            ),
        ]
    }

    fn read_header(&self, count: usize) -> Line<'static> {
        let (icon, color) = match self.status {
            EntryStatus::Running => (self.read_clock, palette::ACCENT),
            EntryStatus::Complete => ("\u{f00c}", palette::ADDED),
            EntryStatus::Failed => ("\u{f00d}", palette::REMOVED),
        };
        let mut header = Line::from(vec![
            Span::raw("  "),
            Span::styled(icon, Style::default().fg(color)),
            Span::styled(" Read", Style::default().fg(palette::READ_SUMMARY_TITLE)),
        ]);
        if count > 1 {
            header.spans.push(Span::styled(
                format!(" ({count})"),
                Style::default().fg(palette::READ_SUMMARY_META),
            ));
        }
        header
    }

    pub(super) fn read_summary(entries: &[Self]) -> Vec<Line<'static>> {
        // 分组中仍有读取未完成时，组首跟随该条目的时钟帧。
        let status_entry = entries
            .iter()
            .find(|entry| entry.status == EntryStatus::Running)
            .unwrap_or(&entries[0]);
        let mut header = status_entry.read_header(entries.len());
        let mut rows = Vec::with_capacity(entries.len() + 1);
        if entries.len() == 1 {
            header.spans.push(Span::styled(
                " · ",
                Style::default().fg(palette::READ_SUMMARY_META),
            ));
            header.spans.extend(entries[0].read_path_spans());
            rows.push(header);
        } else {
            rows.push(header);
            for (index, entry) in entries.iter().enumerate() {
                let mut spans = vec![Span::styled(
                    if index + 1 == entries.len() {
                        "    └─ "
                    } else {
                        "    ├─ "
                    },
                    Style::default().fg(palette::READ_SUMMARY_META),
                )];
                spans.extend(entry.read_path_spans());
                rows.push(Line::from(spans));
            }
        }
        for row in &mut rows {
            row.style = Style::default().bg(palette::READ_SUMMARY_BG);
        }
        rows
    }
}
