//! LSP 专用终端渲染：工具卡片、诊断分组与多文件变更。
//!
//! 这里只做展示；报告本身由工具结果或 `LspDiagnostics` 事件冻结携带，
//! 渲染不会重新读取磁盘或查询 language server。

use super::transcript::{block_line, diff_layout_styled, wrap_block};
use super::transcript_style as palette;
use crate::events::FileDiff;
use crate::lsp::{DiagnosticReport, ReportDiagnostic, ReportStatus, ServerReport};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use serde_json::Value;

/// 折叠时最多展示的诊断正文行数（摘要行也在其中）。
pub(super) const COLLAPSED_LINES: usize = 10;

/// LSP 卡片累积的多文件变更；普通 write/edit 仍只保留单文件 diff。
#[derive(Default)]
pub(super) struct Block {
    pub(super) content: Option<String>,
    pub(super) changes: Vec<Change>,
}

pub(super) struct Change {
    pub(super) path: String,
    pub(super) diff: FileDiff,
}

impl Block {
    /// 同一路径的后续通知替换前一份 diff，路径顺序保持首次出现顺序。
    pub(super) fn record_change(&mut self, path: String, diff: FileDiff) {
        if let Some(existing) = self.changes.iter_mut().find(|change| change.path == path) {
            existing.diff = diff;
        } else {
            self.changes.push(Change { path, diff });
        }
    }

    pub(super) fn set_content(&mut self, content: Option<String>) {
        if content.is_some() {
            self.content = content;
        }
    }
}

/// 标题固定为 `LSP · <action> · <file/query>`；缺失目标时省略末段。
pub(super) fn tool_title(arguments: &Value) -> String {
    let action = arguments["action"].as_str().unwrap_or("lsp");
    let target = arguments["file"]
        .as_str()
        .or_else(|| arguments["query"].as_str())
        .or_else(|| arguments["symbol"].as_str())
        .or_else(|| arguments["new_name"].as_str());
    let title = match target {
        Some(target) if !target.is_empty() => format!("LSP · {action} · {target}"),
        _ => format!("LSP · {action}"),
    };
    palette::display_text(&title).into_owned()
}

/// 迟到报告的独立卡片标题；不附带工具名，避免看起来像一次新调用。
pub(super) fn late_title() -> String {
    "LSP diagnostics (late)".into()
}

pub(super) fn body(block: &Block, width: u16, expanded: bool) -> Vec<Line<'static>> {
    let mut rows = Vec::new();
    if let Some(content) = &block.content
        && !content.trim().is_empty()
    {
        let mut content = super::markdown::parse(content).layout(width);
        if !expanded && content.len() > COLLAPSED_LINES {
            let hidden = content.len() - (COLLAPSED_LINES - 1);
            content.truncate(COLLAPSED_LINES - 1);
            content.extend(wrap_block(
                block_line(
                    format!("… {hidden} more lines (Ctrl + O to expand)"),
                    Style::default().fg(palette::DIM),
                ),
                width,
                "",
            ));
        }
        rows.extend(content);
    }
    for change in &block.changes {
        rows.extend(change_lines(change, width, expanded));
    }
    rows
}

fn path_spans(path: &str, color: Color) -> Vec<Span<'static>> {
    let path = palette::display_text(path).into_owned();
    match path.rsplit_once('/') {
        Some((directory, file)) => vec![
            Span::styled(format!("{directory}/"), Style::default().fg(palette::MUTED)),
            Span::styled(file.to_owned(), Style::default().fg(color)),
        ],
        None => vec![Span::styled(path, Style::default().fg(color))],
    }
}

fn change_label(change: &Change) -> (Option<&'static str>, Color) {
    match &change.diff {
        FileDiff::Text { deleted: true, .. } => (Some("Deleted · "), palette::REMOVED),
        FileDiff::Text { created: true, .. } => (Some("Created · "), palette::ADDED),
        FileDiff::Unavailable { .. } => (None, palette::MUTED),
        _ => (None, palette::READ_SUMMARY_PATH),
    }
}

fn change_lines(change: &Change, width: u16, expanded: bool) -> Vec<Line<'static>> {
    let (prefix, color) = change_label(change);
    let mut spans = Vec::new();
    if let Some(prefix) = prefix {
        spans.push(Span::styled(
            prefix,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ));
    }
    spans.extend(path_spans(&change.path, color));
    let mut rows = wrap_block(Line::from(spans), width, "  ");
    rows.extend(diff_layout_styled(
        &change.diff,
        width,
        expanded,
        Some(&change.path),
    ));
    rows
}

fn status_label(status: &ReportStatus) -> &'static str {
    match status {
        ReportStatus::ReportedIssues => "issues",
        ReportStatus::ReportedClean => "clean",
        ReportStatus::Unreported => "unverified",
        ReportStatus::Failed => "failed",
        ReportStatus::Unsupported => "unsupported",
    }
}

fn status_color(status: &ReportStatus) -> Color {
    match status {
        ReportStatus::ReportedIssues => palette::ERROR_BORDER,
        ReportStatus::ReportedClean => palette::ADDED,
        ReportStatus::Unreported => palette::ACCENT,
        ReportStatus::Failed => palette::REMOVED,
        ReportStatus::Unsupported => palette::MUTED,
    }
}

fn severity_label(severity: u8) -> (&'static str, Color) {
    match severity {
        1 => ("[error]", palette::REMOVED),
        2 => ("[warning]", palette::ACCENT),
        3 => ("[info]", palette::LINK),
        _ => ("[hint]", palette::MUTED),
    }
}

fn summary(report: &DiagnosticReport) -> String {
    let mut counts: Vec<(&'static str, usize)> = Vec::new();
    for status in [
        ReportStatus::ReportedIssues,
        ReportStatus::ReportedClean,
        ReportStatus::Unreported,
        ReportStatus::Failed,
        ReportStatus::Unsupported,
    ] {
        let count = report
            .servers
            .iter()
            .filter(|server| server.status == status)
            .count();
        if count > 0 {
            let label = match status {
                ReportStatus::ReportedIssues => "issues",
                ReportStatus::ReportedClean => "clean",
                ReportStatus::Unreported => "unverified",
                ReportStatus::Failed => "failed",
                ReportStatus::Unsupported => "unsupported",
            };
            counts.push((label, count));
        }
    }
    let total = report.total();
    let mut parts = Vec::new();
    if total > 0 {
        parts.push(format!(
            "{total} {}",
            if total == 1 { "issue" } else { "issues" }
        ));
    }
    if counts.is_empty() && total == 0 {
        parts.push("no servers".into());
    } else if !counts.is_empty() {
        parts.push(
            counts
                .into_iter()
                .map(|(label, count)| format!("{label} ×{count}"))
                .collect::<Vec<_>>()
                .join(" · "),
        );
    }
    if report.deduplicated > 0 {
        parts.push(format!(
            "{} duplicate{} suppressed",
            report.deduplicated,
            if report.deduplicated == 1 { "" } else { "s" }
        ));
    }
    if report.omitted > 0 {
        parts.push(format!("{} omitted", report.omitted));
    }
    parts.join(" · ")
}

fn report_header(report: &DiagnosticReport, width: u16) -> Vec<Line<'static>> {
    let total = report.total();
    let color = if report.has_issues() {
        palette::ERROR_BORDER
    } else if report.omitted > 0 {
        palette::ACCENT
    } else if report.all_clean() {
        palette::ADDED
    } else {
        palette::ACCENT
    };
    let mut spans = vec![Span::styled("LSP ", Style::default().fg(palette::ACCENT))];
    spans.extend(path_spans(&report.path, palette::READ_SUMMARY_PATH));
    spans.push(Span::styled(
        format!(" · {} ", summary(report)),
        Style::default().fg(color),
    ));
    if total > 0 {
        spans.push(Span::styled(
            if total == 1 {
                "1 diagnostic"
            } else {
                "diagnostics"
            },
            Style::default().fg(palette::MUTED),
        ));
    }
    wrap_block(Line::from(spans), width, "  ")
}

fn server_lines(server: &ServerReport, width: u16) -> Vec<Line<'static>> {
    let mut rows = wrap_block(
        Line::from(vec![
            Span::styled(
                format!("  {} · ", palette::display_text(&server.server)),
                Style::default().fg(palette::MUTED),
            ),
            Span::styled(
                status_label(&server.status),
                Style::default()
                    .fg(status_color(&server.status))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" ({})", server.diagnostics.len()),
                Style::default().fg(palette::DIM),
            ),
        ]),
        width,
        "  ",
    );
    if let Some(note) = &server.note {
        rows.extend(wrap_block(
            block_line(
                format!("  {}", palette::display_text(note)),
                Style::default().fg(palette::DIM),
            ),
            width,
            "  ",
        ));
    }
    rows
}

fn diagnostic_line(diagnostic: &ReportDiagnostic, width: u16) -> Vec<Line<'static>> {
    let (severity, color) = severity_label(diagnostic.severity);
    let mut spans = vec![
        Span::styled(format!("  {severity} "), Style::default().fg(color)),
        Span::styled(
            if diagnostic.line_only {
                format!("{}", diagnostic.line)
            } else {
                format!("{}:{}", diagnostic.line, diagnostic.column)
            },
            Style::default().fg(palette::DIM),
        ),
        Span::styled(" ", Style::default()),
        Span::styled(
            palette::display_text(&diagnostic.message).into_owned(),
            Style::default().fg(palette::GREP_TEXT),
        ),
    ];
    let mut detail = Vec::new();
    if let Some(source) = &diagnostic.source
        && !source.is_empty()
    {
        detail.push(palette::display_text(source).into_owned());
    }
    if let Some(code) = &diagnostic.code
        && !code.is_empty()
    {
        detail.push(palette::display_text(code).into_owned());
    }
    if !detail.is_empty() {
        spans.push(Span::styled(
            format!(" ({})", detail.join(" ")),
            Style::default().fg(palette::MUTED),
        ));
    }
    wrap_block(Line::from(spans), width, "    ")
}

fn formatter_line(formatter: &crate::lsp::FormatterStatus, width: u16) -> Vec<Line<'static>> {
    let mut text = format!(
        "  format · {} · {}",
        status_label(&formatter.status),
        palette::display_text(&formatter.server)
    );
    if let Some(note) = &formatter.note {
        text.push_str(" · ");
        text.push_str(&palette::display_text(note));
    }
    wrap_block(
        block_line(text, Style::default().fg(palette::MUTED)),
        width,
        "  ",
    )
}

/// 按文件/服务器分组渲染报告；折叠时保留摘要与省略数。
pub(super) fn diagnostics_lines(
    reports: &[DiagnosticReport],
    width: u16,
    expanded: bool,
) -> Vec<Line<'static>> {
    let mut rows = Vec::new();
    for report in reports {
        rows.extend(report_header(report, width));
        for server in &report.servers {
            rows.extend(server_lines(server, width));
            for diagnostic in &server.diagnostics {
                rows.extend(diagnostic_line(diagnostic, width));
            }
        }
        if let Some(formatter) = &report.formatter {
            rows.extend(formatter_line(formatter, width));
        }
    }
    if rows.is_empty() {
        return rows;
    }
    if !expanded && rows.len() > COLLAPSED_LINES {
        let hidden = rows.len() - (COLLAPSED_LINES - 1);
        rows.truncate(COLLAPSED_LINES - 1);
        rows.extend(wrap_block(
            block_line(
                format!("… {hidden} more diagnostic lines (Ctrl + O to expand)"),
                Style::default().fg(palette::DIM),
            ),
            width,
            "",
        ));
    }
    rows
}

/// 工具结果里的结构化报告；单文件是对象，glob 是数组。只为内联展示使用，
/// 迟到投递走事件。
pub(super) fn reports_from_result(result: &Value) -> Vec<DiagnosticReport> {
    let Some(value) = result.get("diagnostics") else {
        return Vec::new();
    };
    if value.is_array() {
        serde_json::from_value::<Vec<DiagnosticReport>>(value.clone()).unwrap_or_default()
    } else {
        serde_json::from_value::<DiagnosticReport>(value.clone())
            .ok()
            .into_iter()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::{FormatterStatus, ReportStatus};

    fn diagnostic(severity: u8, line: u32, message: &str) -> ReportDiagnostic {
        ReportDiagnostic {
            line,
            column: 3,
            end_line: line,
            end_column: 4,
            severity,
            message: message.into(),
            code: None,
            source: Some("test".into()),
            line_only: false,
        }
    }

    fn report() -> DiagnosticReport {
        DiagnosticReport {
            path: "src/demo.rs".into(),
            origin: "write".into(),
            owner: "root".into(),
            servers: vec![
                ServerReport {
                    server: "rust-analyzer".into(),
                    status: ReportStatus::ReportedIssues,
                    note: None,
                    diagnostics: vec![diagnostic(1, 4, "expected i32")],
                },
                ServerReport {
                    server: "biome".into(),
                    status: ReportStatus::ReportedClean,
                    note: None,
                    diagnostics: Vec::new(),
                },
            ],
            formatter: Some(FormatterStatus {
                server: "biome".into(),
                status: ReportStatus::ReportedClean,
                note: None,
            }),
            deduplicated: 1,
            omitted: 0,
            at_ms: 1,
        }
    }

    fn text(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn lsp_terminal_report_groups_servers_and_uses_one_based_positions() {
        let text = text(&diagnostics_lines(&[report()], 120, true));
        assert!(text.contains("src/demo.rs"), "{text}");
        assert!(text.contains("rust-analyzer · issues"), "{text}");
        assert!(text.contains("[error] 4:3 expected i32"), "{text}");
        assert!(text.contains("biome · clean"), "{text}");
        assert!(text.contains("duplicate suppressed"), "{text}");
    }

    #[test]
    fn lsp_terminal_report_collapses_with_summary_and_expands() {
        let mut report = report();
        report.deduplicated = 0;
        report.servers[0].diagnostics = (1..=20)
            .map(|line| diagnostic(2, line, "warning"))
            .collect();
        let collapsed = text(&diagnostics_lines(&[report.clone()], 120, false));
        assert!(collapsed.lines().count() <= COLLAPSED_LINES, "{collapsed}");
        assert!(collapsed.contains("more diagnostic lines"), "{collapsed}");
        assert!(collapsed.contains("src/demo.rs"), "{collapsed}");
        let expanded = text(&diagnostics_lines(&[report], 120, true));
        assert!(expanded.lines().count() > COLLAPSED_LINES, "{expanded}");
        assert!(expanded.contains("20"), "{expanded}");
    }

    #[test]
    fn lsp_terminal_result_accepts_single_and_glob_reports() {
        let single = serde_json::json!({"diagnostics": report()});
        assert_eq!(reports_from_result(&single).len(), 1);
        let glob = serde_json::json!({"diagnostics": [report(), report()]});
        assert_eq!(reports_from_result(&glob).len(), 2);
        assert!(reports_from_result(&serde_json::json!({"content":"clean"})).is_empty());
    }

    #[test]
    fn lsp_terminal_reports_omitted_messages_without_claiming_clean() {
        let mut report = report();
        report.servers[0].diagnostics.clear();
        report.servers[0].status = ReportStatus::ReportedIssues;
        report.deduplicated = 0;
        // 自动钩子只保留前 50 条结构化消息，其余计入 omitted。
        report.omitted = 25;
        let text = text(&diagnostics_lines(&[report], 120, true));
        assert!(text.contains("25 issues"), "{text}");
        assert!(text.contains("25 omitted"), "{text}");
        assert!(!text.contains("no servers"), "{text}");
    }

    #[test]
    fn lsp_terminal_changes_accumulate_by_path_and_show_deletion() {
        let mut block = Block::default();
        block.record_change(
            "old/name.rs".into(),
            FileDiff::Text {
                created: false,
                deleted: true,
                added: 0,
                removed: 2,
                hunks: Vec::new(),
            },
        );
        block.record_change(
            "new/name.rs".into(),
            FileDiff::Text {
                created: true,
                deleted: false,
                added: 2,
                removed: 0,
                hunks: Vec::new(),
            },
        );
        // 同一路径重复通知替换而不是追加第二张卡。
        block.record_change(
            "new/name.rs".into(),
            FileDiff::Text {
                created: true,
                deleted: false,
                added: 3,
                removed: 0,
                hunks: Vec::new(),
            },
        );
        assert_eq!(block.changes.len(), 2);
        let rendered = text(&body(&block, 120, true));
        assert!(rendered.contains("Deleted · old/name.rs"), "{rendered}");
        assert!(rendered.contains("Created · new/name.rs"), "{rendered}");
    }
}
