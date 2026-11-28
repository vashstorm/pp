//! Titles, footers, paths and consecutive read summaries of tool cards.

use super::*;

/// Shorten only paths inside the working directory; never touches the disk and does not alter the original tool call arguments.
pub(super) fn display_path<'a>(cwd: Option<&std::path::Path>, path: &'a str) -> &'a str {
    let Some(cwd) = cwd else { return path };
    let Ok(relative) = std::path::Path::new(path).strip_prefix(cwd) else {
        return path;
    };
    // Keep paths that may escape the working directory so external files are not shown as project files.
    let mut depth = 0usize;
    for component in relative.components() {
        match component {
            std::path::Component::Normal(_) => depth += 1,
            std::path::Component::ParentDir => {
                let Some(parent) = depth.checked_sub(1) else {
                    return path;
                };
                depth = parent;
            }
            _ => {}
        }
    }
    relative
        .to_str()
        .filter(|path| !path.is_empty())
        .unwrap_or(path)
}

impl TranscriptEntry {
    pub(super) fn tool_accent(&self) -> Color {
        palette::tool_accent(self.tool.as_deref().unwrap_or_default())
    }

    pub(super) fn is_collapsible_tool(&self) -> bool {
        // Late diagnostics carry no tool call ID but can still be expanded to the full report with Ctrl + O.
        self.kind == EntryKind::Lsp
            || (self.tool.is_some() && self.kind != EntryKind::Ask && !self.is_file_read())
    }

    fn format_elapsed(&self, elapsed: std::time::Duration) -> String {
        if self.status == EntryStatus::Running {
            format!("Elapsed: {}s", elapsed.as_secs())
        } else {
            format!("Wall: {:.2}s", elapsed.as_secs_f64())
        }
    }

    fn format_timeout(seconds: u64) -> String {
        if seconds == 0 {
            "Timeout: none".into()
        } else {
            format!("Timeout: {seconds}s")
        }
    }

    fn apply_footer(&mut self, footer: String, footer_warning: String) -> bool {
        if self.footer == footer && self.footer_warning == footer_warning {
            return false;
        }
        self.footer = footer;
        self.footer_warning = footer_warning;
        self.layout = None;
        true
    }

    pub(super) fn update_bash_footer(&mut self, elapsed: Option<std::time::Duration>) -> bool {
        let mut parts = Vec::new();
        if let Some(elapsed) = elapsed {
            parts.push(self.format_elapsed(elapsed));
        }
        if let Some(seconds) = self.bash_timeout_seconds {
            parts.push(Self::format_timeout(seconds));
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
        self.apply_footer(parts.join(" · "), footer_warning)
    }

    pub(super) fn update_eval_footer(&mut self, elapsed: Option<std::time::Duration>) -> bool {
        let mut parts = Vec::new();
        if let Some(count) = self
            .result
            .as_ref()
            .and_then(|result| result["execution_count"].as_u64())
            .filter(|count| *count > 0)
        {
            parts.push(format!("Cell #{count}"));
        }
        if let Some(elapsed) = elapsed {
            parts.push(self.format_elapsed(elapsed));
        }
        if let Some(seconds) = self.eval_timeout_seconds {
            parts.push(Self::format_timeout(seconds));
        }
        if self.arguments["reset"] == true {
            parts.push("reset".into());
        }
        if let Some(result) = &self.result {
            if let Some(uri) = result["artifact"]["uri"].as_str() {
                let id = uri.strip_prefix("artifact://").unwrap_or(uri);
                parts.push(format!("Artifact: {}", pp_core::text::display_text(id)));
            } else if let Some(artifact) =
                result.get("artifact_id").filter(|value| !value.is_null())
            {
                let artifact = artifact
                    .as_str()
                    .map_or_else(|| artifact.to_string(), str::to_owned);
                parts.push(format!(
                    "Artifact: {}",
                    pp_core::text::display_text(&artifact)
                ));
            }
            if result["cancelled"] == true {
                parts.push("cancelled".into());
            } else if result["timed_out"] == true {
                parts.push("timed out".into());
            } else if self.status == EntryStatus::Failed {
                parts.push("failed".into());
            }
        } else if self.status == EntryStatus::Failed {
            parts.push("failed".into());
        }
        let footer_warning = if self
            .result
            .as_ref()
            .is_some_and(|result| result["kernel_restarted"] == true)
        {
            "Kernel restarted".to_owned()
        } else {
            String::new()
        };
        self.apply_footer(parts.join(" · "), footer_warning)
    }

    /// Rebuild the `/pipes` entry from the final `result.pipes`; clear it when there is no real pipeline.
    pub(super) fn sync_pipes_badge(&mut self, restored: bool) {
        self.pipes_badge = self
            .result
            .as_ref()
            .and_then(|result| result.get("pipes"))
            .and_then(pp_workspace::output::pipes_status)
            .map(|status| super::PipesBadge { status, restored });
    }

    pub(super) fn is_file_read(&self) -> bool {
        self.tool.as_deref() == Some("read")
    }

    pub(super) fn is_edit(&self) -> bool {
        self.tool.as_deref() == Some("edit")
    }

    /// Argument preview of a file tool being generated: the body is the uncommitted JSON chunks.
    pub(super) fn is_argument_preview(&self) -> bool {
        matches!(self.body, EntryBody::ArgumentsPreview { .. })
    }

    /// A committed `edit` card and an in-progress `write` preview share the file card title;
    /// a committed `write` card still uses the generic tool title.
    pub(super) fn has_file_header(&self) -> bool {
        self.is_edit() || (self.tool.as_deref() == Some("write") && self.is_argument_preview())
    }

    pub(super) fn is_search(&self) -> bool {
        matches!(self.tool.as_deref(), Some("rg" | "glob" | "web_search"))
    }

    pub(super) fn is_wait_card(&self) -> bool {
        self.tool.as_deref() == Some("wait")
            && (self.result.is_none()
                || self
                    .result
                    .as_ref()
                    .is_some_and(|r| r["jobs"].is_array() || r["ok"] == false))
    }

    pub(super) fn awaits_subagent_injection(&self) -> bool {
        self.tool.as_deref() == Some("wait")
            && self.result.as_ref().is_some_and(|result| {
                result["cancelled"] != true
                    && result["jobs"].as_array().is_some_and(|jobs| {
                        jobs.iter().any(|job| {
                            job["type"] == "task"
                                && job["delivered"] == false
                                && job["injected"] != true
                        })
                    })
            })
    }

    pub(super) fn edit_path(&self) -> &str {
        self.arguments["path"]
            .as_str()
            .or_else(|| pp_tools::edit_path(&self.arguments))
            .unwrap_or_default()
    }

    /// File card title shared by the argument preview and the committed `edit` card.
    pub(super) fn file_header(&self) -> Line<'static> {
        let tool = self.tool.as_deref().unwrap_or_default();
        let style = card::surface_style(card::CardSurface::Edit, palette::tool_accent(tool));
        let meta = style.fg(palette::edit_meta());
        let label = if tool == "write" { "Write" } else { "Edit" };
        let preview_path;
        let path = if self.is_argument_preview() {
            preview_path = super::preview::path(&self.text);
            preview_path.as_str()
        } else {
            self.edit_path()
        };
        let path = display_path(self.cwd.as_deref(), path);
        let icons = crate::tui::themes::current_icons();
        let mut spans = vec![
            Span::styled(format!("{} {label}: ", palette::tool_icon(tool)), style),
            Span::styled(palette::file_icon(path, icons), meta),
            Span::styled(pp_core::text::display_text(path).into_owned(), style),
        ];
        if let EntryBody::Diff(FileDiff::Text { added, removed, .. }) = &self.body {
            spans.extend([
                Span::styled(" ⟨", meta),
                Span::styled(format!("+{added}"), style.fg(palette::edit_added())),
                Span::styled("/", meta),
                Span::styled(format!("-{removed}"), style.fg(palette::edit_removed())),
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
                    style.fg(palette::edit_removed())
                } else {
                    meta
                },
            ));
        } else {
            spans.push(Span::styled(" ", style));
        }
        Line::from(spans)
    }

    pub(super) fn read_path_parts(&self) -> (String, String) {
        let path = self
            .result
            .as_ref()
            .and_then(|r| r["source"].as_str())
            .or_else(|| self.arguments["path"].as_str())
            .unwrap_or_default();
        let path = display_path(self.cwd.as_deref(), path);
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
        (
            pp_core::text::display_text(path).into_owned(),
            format!("{range}{image}{status}{truncated}"),
        )
    }

    fn read_path_spans(&self) -> Vec<Span<'static>> {
        let (path, meta) = self.read_path_parts();
        vec![
            Span::styled(path, Style::default().fg(palette::read_summary_path())),
            Span::styled(meta, Style::default().fg(palette::activity())),
        ]
    }

    fn read_header(&self, count: usize) -> Line<'static> {
        let icons = crate::tui::themes::current_icons();
        let (icon, color) = match self.status {
            EntryStatus::Running => (self.read_clock, palette::accent()),
            EntryStatus::Complete => (palette::done_icon(icons), palette::added()),
            EntryStatus::Failed => (palette::fail_icon(icons), palette::removed()),
        };
        let mut header = Line::from(vec![
            Span::raw("  "),
            Span::styled(icon, Style::default().fg(color)),
            Span::styled(" Read", Style::default().fg(palette::read_summary_title())),
        ]);
        if count > 1 {
            header.spans.push(Span::styled(
                format!(" ({count})"),
                Style::default().fg(palette::read_summary_meta()),
            ));
        }
        header
    }

    pub(super) fn read_summary(entries: &[Self]) -> Vec<Line<'static>> {
        // While reads in the group are still unfinished, the group head follows that entry's clock frame.
        let status_entry = entries
            .iter()
            .find(|entry| entry.status == EntryStatus::Running)
            .unwrap_or(&entries[0]);
        let mut header = status_entry.read_header(entries.len());
        let mut rows = Vec::with_capacity(entries.len() + 1);
        if entries.len() == 1 {
            header.spans.push(Span::styled(
                " · ",
                Style::default().fg(palette::read_summary_meta()),
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
                    Style::default().fg(palette::read_summary_meta()),
                )];
                spans.extend(entry.read_path_spans());
                rows.push(Line::from(spans));
            }
        }
        rows
    }
}
