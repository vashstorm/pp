use super::super::*;
use pp_config::config::IconMode;

#[derive(Clone)]
pub(in crate::tui::transcript) struct ReadItem {
    pub path: String,
    pub meta: String,
}

pub(super) fn status_info(
    entry: &TranscriptEntry,
    icons: IconMode,
) -> (&'static str, Color, &'static str) {
    match entry.status {
        EntryStatus::Running => (entry.read_clock, palette::accent(), "running"),
        EntryStatus::Complete => (palette::done_icon(icons), palette::added(), "done"),
        EntryStatus::Failed => (palette::fail_icon(icons), palette::removed(), "failed"),
    }
}

pub(super) fn tool_label(entry: &TranscriptEntry) -> String {
    match entry.tool.as_deref() {
        Some("read") => "Read".into(),
        Some("bash") => "Bash".into(),
        Some("bash_task") => "Bash task".into(),
        Some("eval") => tool_display::EvalLang::from_entry(&entry.arguments, entry.result.as_ref())
            .label()
            .to_owned(),
        Some("rg") => "Rg".into(),
        Some("glob") => "Glob".into(),
        Some("web_search") => "Web".into(),
        Some("edit") => "Edit".into(),
        Some("write") => "Write".into(),
        Some("wait") => "Wait".into(),
        Some("ask") => "Ask".into(),
        Some("lsp") => "LSP".into(),
        Some("todo") => "Todo".into(),
        Some("find") => "Find".into(),
        Some("plan_search") => "Search".into(),
        Some("plan_find") => "Find".into(),
        Some("plan_submit") => "Plan".into(),
        Some(name) if name.starts_with("mcp__") => "MCP".into(),
        Some(other) => pp_core::text::display_text(other).into_owned(),
        None if entry.kind == EntryKind::Lsp => "LSP".into(),
        None => "Tool".into(),
    }
}

pub(super) fn tool_accent_color(entry: &TranscriptEntry) -> Color {
    match entry.tool.as_deref() {
        Some("eval") => {
            tool_display::EvalLang::from_entry(&entry.arguments, entry.result.as_ref()).accent()
        }
        Some(name) => palette::tool_accent(name),
        None if entry.kind == EntryKind::Lsp => palette::accent(),
        None => palette::activity(),
    }
}

pub(super) fn tool_glyph(entry: &TranscriptEntry, icons: IconMode) -> String {
    match entry.tool.as_deref() {
        Some("eval") => tool_display::EvalLang::from_entry(&entry.arguments, entry.result.as_ref())
            .icon(icons)
            .to_owned(),
        Some(name) => palette::tool_icon_for(name, icons).to_owned(),
        None if entry.kind == EntryKind::Lsp => palette::tool_icon_for("lsp", icons).to_owned(),
        None => palette::tool_icon_for("", icons).to_owned(),
    }
}

pub(super) fn clip_spans(
    spans: Vec<Span<'static>>,
    max_width: usize,
    ellipsis: bool,
) -> Vec<Span<'static>> {
    let total: usize = spans.iter().map(Span::width).sum();
    if total <= max_width {
        return spans;
    }
    let budget = if ellipsis && max_width > 0 {
        max_width - 1
    } else {
        max_width
    };
    let mut out = Vec::with_capacity(spans.len());
    let mut used = 0usize;
    let mut last_style = Style::default();
    for span in spans {
        let sw = span.width();
        last_style = span.style;
        if used + sw <= budget {
            used += sw;
            out.push(span);
            continue;
        }
        let mut clipped = String::new();
        for grapheme in span.styled_graphemes(Style::default()) {
            let gw = Span::raw(grapheme.symbol).width();
            if used + gw > budget {
                break;
            }
            clipped.push_str(grapheme.symbol);
            used += gw;
        }
        if !clipped.is_empty() {
            out.push(Span::styled(clipped, span.style));
        }
        break;
    }
    if ellipsis && max_width > 0 {
        out.push(Span::styled("…", last_style));
    }
    out
}

/// 唯一头部合成器：右列贴右边缘，空间不足时左侧收缩并加省略号，确保状态列不消失。
pub(super) fn compose_header(
    left: Vec<Span<'static>>,
    right: Vec<Span<'static>>,
    width: usize,
    bg: Option<Color>,
) -> Vec<Span<'static>> {
    if width == 0 {
        return Vec::new();
    }
    let style_bg = |s: Style| match bg {
        Some(c) => s.bg(c),
        None => s,
    };
    let right_w: usize = right.iter().map(Span::width).sum();
    if right_w == 0 {
        let mut out = clip_spans(left, width, true);
        let used: usize = out.iter().map(Span::width).sum();
        if used < width && bg.is_some() {
            out.push(Span::styled(
                " ".repeat(width - used),
                style_bg(Style::default()),
            ));
        }
        return out;
    }
    if right_w >= width {
        return clip_spans(right, width, false);
    }
    let left_w: usize = left.iter().map(Span::width).sum();
    let min_gap = usize::from(!left.is_empty());
    let mut out = if left_w + min_gap + right_w <= width {
        let mut v = left;
        let gap = width - left_w - right_w;
        if gap > 0 {
            v.push(Span::styled(" ".repeat(gap), style_bg(Style::default())));
        }
        v
    } else {
        let budget = width.saturating_sub(right_w + min_gap);
        let mut v = clip_spans(left, budget, true);
        let used: usize = v.iter().map(Span::width).sum();
        let gap = width.saturating_sub(used + right_w);
        if gap > 0 {
            v.push(Span::styled(" ".repeat(gap), style_bg(Style::default())));
        }
        v
    };
    out.extend(right);
    out
}

pub(in crate::tui::transcript) fn inline_bash_command(
    entry: &TranscriptEntry,
    inner_width: u16,
) -> Option<String> {
    if entry.kind != EntryKind::Bash {
        return None;
    }
    if entry.shell.as_ref().is_some_and(|s| s.rows.len() > 1) {
        return None;
    }
    let raw = entry.title.strip_prefix("$ ").unwrap_or(&entry.title);
    if raw.contains('\n') || raw.trim().is_empty() {
        return None;
    }
    let clean = pp_core::text::display_text(raw).into_owned();
    let w = Line::raw(&clean).width();
    (w + 14 <= usize::from(inner_width)).then_some(clean)
}

pub(super) fn footer_spans(
    entry: &TranscriptEntry,
    avail: usize,
    bg: Option<Color>,
) -> Vec<Span<'static>> {
    let style_bg = |s: Style| match bg {
        Some(c) => s.bg(c),
        None => s,
    };
    let warning_needed = !entry.footer_warning.is_empty();
    let warning_width = Line::raw(entry.footer_warning.as_str()).width();
    let footer_budget = if warning_needed {
        avail.saturating_sub(warning_width + if entry.footer.is_empty() { 0 } else { 3 })
    } else {
        avail
    };
    let footer = tool_display::clip(&entry.footer, footer_budget, warning_needed);
    let footer_width = Line::raw(footer.as_str()).width();
    let separator = if footer.is_empty() { "" } else { " · " };
    let remaining = avail.saturating_sub(footer_width + Line::raw(separator).width());
    let warning = if warning_needed {
        tool_display::clip(&entry.footer_warning, remaining, false)
    } else {
        String::new()
    };
    let mut spans = Vec::new();
    if !footer.is_empty() {
        spans.push(Span::styled(
            footer,
            style_bg(Style::default().fg(palette::muted())),
        ));
    }
    if !warning.is_empty() {
        if !separator.is_empty() && !spans.is_empty() {
            spans.push(Span::styled(
                separator.to_owned(),
                style_bg(Style::default().fg(palette::muted())),
            ));
        }
        spans.push(Span::styled(
            warning,
            style_bg(Style::default().fg(palette::accent())),
        ));
    }
    spans
}

/// 合成带有标题与可选项的正文主体与右侧元数据文本。
pub(super) fn subject_and_meta(
    entry: &TranscriptEntry,
    icons: IconMode,
    inline_bash: Option<String>,
) -> (Vec<Span<'static>>, String) {
    let tool = entry.tool.as_deref().unwrap_or_default();
    let text_style = Style::default().fg(palette::text());
    let meta_style = Style::default().fg(palette::muted());

    if entry.is_file_read() {
        if entry.read_group.len() > 1 {
            return (
                vec![Span::styled(
                    format!("{} files", entry.read_group.len()),
                    meta_style,
                )],
                String::new(),
            );
        }
        if let Some(item) = entry.read_group.first() {
            return (
                vec![Span::styled(
                    item.path.clone(),
                    Style::default().fg(palette::accent()),
                )],
                item.meta.clone(),
            );
        }
        let (path, meta) = entry.read_path_parts();
        return (
            vec![Span::styled(path, Style::default().fg(palette::accent()))],
            meta,
        );
    }

    if entry.has_file_header() {
        let preview_path;
        let path = if entry.is_argument_preview() {
            preview_path = super::super::preview::path(&entry.text);
            preview_path.as_str()
        } else {
            entry.edit_path()
        };
        let path = super::super::entry_tool::display_path(entry.cwd.as_deref(), path);
        let file_icon = palette::file_icon(path, icons);
        let subject = vec![
            Span::styled(file_icon, meta_style),
            Span::styled(
                pp_core::text::display_text(path).into_owned(),
                Style::default().fg(tool_accent_color(entry)),
            ),
        ];
        if let EntryBody::Diff(FileDiff::Text { added, removed, .. }) = &entry.body {
            return (subject, format!("+{added}/-{removed}"));
        }
        let meta = if entry.is_argument_preview() {
            "preview · not saved"
        } else if entry.status == EntryStatus::Failed {
            "failed"
        } else if entry.status == EntryStatus::Running {
            "running"
        } else {
            ""
        };
        return (subject, meta.to_owned());
    }

    if tool == "write" {
        let path = entry.arguments["path"].as_str().unwrap_or_default();
        let path = super::super::entry_tool::display_path(entry.cwd.as_deref(), path);
        let file_icon = palette::file_icon(path, icons);
        let meta = entry
            .result
            .as_ref()
            .and_then(|r| r["bytes_written"].as_u64())
            .map(|b| format!("{b} bytes"))
            .unwrap_or_default();
        return (
            vec![
                Span::styled(file_icon, meta_style),
                Span::styled(
                    pp_core::text::display_text(path).into_owned(),
                    Style::default().fg(tool_accent_color(entry)),
                ),
            ],
            meta,
        );
    }

    if entry.kind == EntryKind::Bash {
        let compact_meta = compact_footer(&entry.footer);
        if let Some(cmd) = inline_bash {
            return (vec![Span::styled(cmd, text_style)], compact_meta);
        }
        let task_badge = entry
            .footer
            .split(" · ")
            .find(|s| s.trim().starts_with("Task "))
            .map(|s| vec![Span::styled(s.trim().to_owned(), meta_style)])
            .unwrap_or_default();
        return (task_badge, compact_meta);
    }

    if entry.kind == EntryKind::Eval {
        let title_arg = entry.arguments["title"].as_str().unwrap_or_default();
        let compact_meta = compact_footer(&entry.footer);
        let subject = if title_arg.is_empty() {
            Vec::new()
        } else {
            vec![Span::styled(
                pp_core::text::display_text(title_arg).into_owned(),
                text_style,
            )]
        };
        return (subject, compact_meta);
    }

    if tool == "rg" {
        let pattern = entry.arguments["pattern"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| {
                entry.arguments["args"]
                    .as_array()
                    .map(|a| serde_json::to_string(a).unwrap_or_default())
                    .unwrap_or_default()
            });
        let details = entry.result.as_ref().map(|r| &r["details"]);
        let output_mode = details
            .and_then(|d| d["outputMode"].as_str())
            .or_else(|| entry.arguments["output_mode"].as_str())
            .unwrap_or("content");
        let scope = details
            .and_then(|d| {
                (d["fileCount"] == 1)
                    .then(|| d["fileMatches"][0]["path"].as_str())
                    .flatten()
            })
            .or_else(|| details.and_then(|d| d["scopePath"].as_str()))
            .or_else(|| entry.arguments["path"].as_str())
            .unwrap_or(".");
        let count_meta = if output_mode == "files_with_matches" {
            details
                .and_then(|d| d["fileCount"].as_u64())
                .map(|files| format!("{files} files · {scope}"))
        } else {
            details
                .and_then(|d| {
                    let m = if output_mode == "count" {
                        d["totalMatchCount"].as_u64()?
                    } else {
                        d["matchCount"].as_u64()?
                    };
                    let f = d["fileCount"].as_u64()?;
                    Some(format!("{m} matches · {f} files"))
                })
                .or_else(|| Some(scope.to_owned()))
        };
        return (
            vec![Span::styled(
                pp_core::text::display_text(&pattern).into_owned(),
                text_style,
            )],
            count_meta.unwrap_or_default(),
        );
    }

    if tool == "glob" {
        let pattern = entry.arguments["pattern"].as_str();
        let path = entry.arguments["path"].as_str().unwrap_or(".");
        let subj = match pattern {
            Some(p) => format!("{p} · {path}"),
            None => path.to_owned(),
        };
        return (
            vec![Span::styled(
                pp_core::text::display_text(&subj).into_owned(),
                text_style,
            )],
            String::new(),
        );
    }

    if tool == "web_search" {
        let query = entry.arguments["query"].as_str().unwrap_or_default();
        let provider = entry
            .result
            .as_ref()
            .and_then(|r| {
                r["provider_label"]
                    .as_str()
                    .or_else(|| r["provider"].as_str())
            })
            .or_else(|| entry.arguments["provider"].as_str());
        let count = entry
            .result
            .as_ref()
            .and_then(|r| r["details"]["source_count"].as_u64());
        let meta = match (provider, count) {
            (Some(p), Some(c)) => format!("{p} · {c} results"),
            (Some(p), None) => p.to_owned(),
            (None, Some(c)) => format!("{c} results"),
            (None, None) => String::new(),
        };
        return (
            vec![Span::styled(
                pp_core::text::display_text(query).into_owned(),
                text_style,
            )],
            meta,
        );
    }

    if let Some((server, mcp_tool)) = palette::mcp_parts(tool) {
        let (_, detail) = entry
            .title
            .find(" · ")
            .map_or((entry.title.as_str(), ""), |i| entry.title.split_at(i));
        let detail = detail.strip_prefix(" · ").unwrap_or(detail);
        return (
            vec![
                Span::styled(
                    pp_core::text::display_text(server).into_owned(),
                    Style::default()
                        .fg(palette::mcp_server())
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(" {}", pp_core::text::display_text(mcp_tool)),
                    Style::default()
                        .fg(palette::mcp_tool())
                        .add_modifier(Modifier::BOLD),
                ),
            ],
            detail.to_owned(),
        );
    }

    if entry.is_wait_card() {
        let jobs = entry
            .result
            .as_ref()
            .and_then(|r| r["jobs"].as_array())
            .map(Vec::as_slice)
            .unwrap_or_default();
        let running = jobs
            .iter()
            .filter(|j| matches!(j["status"].as_str(), Some("queued" | "running")))
            .count();
        let failed = jobs.iter().filter(|j| j["status"] == "failed").count();
        let done = jobs.len().saturating_sub(running + failed);
        return (
            vec![Span::styled(
                format!("{running} running · {done} done · {failed} failed"),
                text_style,
            )],
            format!("{}/{}", jobs.len() - running, jobs.len()),
        );
    }

    // 通用工具标题：剥离图标与标签前缀后的剩余描述作为 subject。
    let icon = tool_glyph(entry, icons);
    let trimmed = entry
        .title
        .strip_prefix(&icon)
        .map(str::trim_start)
        .unwrap_or(&entry.title);
    let label = tool_label(entry);
    let rest = trimmed
        .strip_prefix(&label)
        .or_else(|| trimmed.strip_prefix(tool))
        .map(|s| s.trim_start_matches([':', ' ']))
        .unwrap_or(trimmed);
    let (subj, detail) = rest.find(" · ").map_or((rest, ""), |i| {
        let (a, b) = rest.split_at(i);
        (a, b.strip_prefix(" · ").unwrap_or(b))
    });
    let spans = if subj.is_empty() {
        Vec::new()
    } else {
        vec![Span::styled(
            pp_core::text::display_text(subj).into_owned(),
            text_style,
        )]
    };
    (spans, detail.to_owned())
}

fn compact_footer(footer: &str) -> String {
    if footer.is_empty() {
        return String::new();
    }
    let mut parts = Vec::new();
    for seg in footer.split(" · ") {
        let seg = seg.trim();
        if let Some(wall) = seg.strip_prefix("Wall: ") {
            parts.push(wall.to_owned());
        } else if let Some(elapsed) = seg.strip_prefix("Elapsed: ") {
            parts.push(elapsed.to_owned());
        } else if let Some(code) = seg.strip_prefix("Exit: ") {
            parts.push(format!("exit {code}"));
        } else if matches!(seg, "cancelled" | "timed out" | "failed") {
            parts.push(seg.to_owned());
        }
    }
    parts.join(" · ")
}
