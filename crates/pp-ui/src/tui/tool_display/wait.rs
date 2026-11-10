//! 等待消息框只绘制 worker 发布的任务概况，不展示结果正文、不查询任务或读取日志。
use super::*;

pub(in crate::tui) fn render(view: View<'_>, width: u16) -> Vec<Line<'static>> {
    let result = view.result;
    let jobs = result.and_then(|r| r["jobs"].as_array());
    let jobs = jobs.map(Vec::as_slice).unwrap_or_default();
    let pending = |j: &&Value| matches!(j["status"].as_str(), Some("queued" | "running"));
    let running = jobs.iter().filter(pending).count();
    let failed = jobs.iter().filter(|j| j["status"] == "failed").count();
    let cancelled = jobs
        .iter()
        .filter(|j| {
            matches!(
                j["status"].as_str(),
                Some("cancelled" | "stopped" | "interrupted")
            )
        })
        .count();
    let settled = jobs.len() - running;
    let done = settled.saturating_sub(failed + cancelled);
    let color = if view.failed || failed > 0 {
        palette::REMOVED
    } else if view.running {
        palette::ACCENT
    } else {
        palette::ADDED
    };
    let style = Style::default().fg(palette::READ_TEXT);
    let border = Style::default().fg(color);
    let framed = width >= 8;
    let inner = if framed { width - 4 } else { width };
    let mut body = Vec::new();
    let suffix = if result.is_some_and(|r| r["cancelled"] == true) {
        " · cancelled"
    } else if result.is_some_and(|r| r["interrupted"] == true) {
        " · interrupted by steering"
    } else if result.is_some_and(|r| r["timed_out"] == true) {
        " · timed out"
    } else if view.running {
        " · waiting"
    } else {
        " · snapshot"
    };
    body.push(Line::styled(format!("Wait · {running} running · {done} done · {failed} failed · {cancelled} cancelled{suffix}"), border.add_modifier(Modifier::BOLD)));
    if !jobs.is_empty() {
        let cells = usize::from(inner).saturating_sub(14).clamp(1, 24);
        let filled = cells * settled / jobs.len();
        body.push(Line::styled(
            format!(
                "[{}{}] {settled}/{} settled",
                "█".repeat(filled),
                "░".repeat(cells - filled),
                jobs.len()
            ),
            border,
        ));
    } else {
        body.push(Line::styled(
            if view.failed {
                result
                    .and_then(|r| r["error"].as_str())
                    .unwrap_or(view.text)
                    .to_owned()
            } else if view.running {
                "Preparing wait…".into()
            } else {
                "Nothing to wait for".into()
            },
            style,
        ));
    }
    let shown = if view.expanded || view.context.full {
        jobs.len()
    } else {
        jobs.len().min(8)
    };
    for job in jobs.iter().take(shown) {
        let duration_ms = job["duration_ms"].as_u64().unwrap_or(0);
        let status = job["status"].as_str().unwrap_or("unknown");
        let active = matches!(status, "queued" | "running");
        let (icon, tone) = match status {
            "queued" | "running" if view.running => (
                palette::waiting_clock(u128::from(duration_ms)),
                palette::ACCENT,
            ),
            "queued" | "running" => ("○", palette::ACCENT),
            "failed" => ("\u{f00d}", palette::REMOVED),
            "cancelled" | "stopped" | "interrupted" => ("■", palette::MUTED),
            _ => ("\u{f00c}", palette::ADDED),
        };
        let seconds = duration_ms / 1000;
        let delivered = if job["delivered"] == true && !active {
            " · delivered"
        } else {
            ""
        };
        body.push(Line::styled(
            format!(
                "{icon} [{}] {} · {status} · {}m{:02}s{delivered}",
                job["type"].as_str().unwrap_or("job"),
                job["id"].as_str().unwrap_or("?"),
                seconds / 60,
                seconds % 60
            ),
            style.fg(tone),
        ));
        body.push(Line::styled(
            format!("  {}", job["label"].as_str().unwrap_or_default()),
            style,
        ));
    }
    if shown < jobs.len() {
        body.push(Line::styled(
            format!("… {} more jobs (Ctrl + O to expand)", jobs.len() - shown),
            style.fg(palette::MUTED),
        ));
    }
    let mut rows = Vec::new();
    if framed {
        rows.push(Line::styled(
            format!("╭{}╮", "─".repeat(usize::from(width) - 2)),
            border,
        ));
    }
    for line in body {
        // 控制字符与换行先归一化，再按真实终端列宽换行。
        let safe = pp_core::text::display_text(&line.to_string()).into_owned();
        for mut row in wrap_block(Line::styled(safe, line.style), inner.max(1), "") {
            if framed {
                let padding = usize::from(inner).saturating_sub(row.width());
                row.spans.insert(0, Span::styled("│ ", border));
                row.spans
                    .push(Span::styled(format!("{} │", " ".repeat(padding)), border));
            }
            rows.push(row);
        }
    }
    if framed {
        rows.push(Line::styled(
            format!("╰{}╯", "─".repeat(usize::from(width) - 2)),
            border,
        ));
    }
    rows
}
