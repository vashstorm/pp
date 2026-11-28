//! Lay out the dispatch card from the Task call arguments and the returned snapshot.

use super::*;
use crate::tui::transcript::card::anatomy::compose_header;
use pp_config::config::IconMode;
use ratatui::style::{Color, Stylize};

const PREVIEW_TASKS: usize = 4;

fn tasks(value: &Value) -> &[Value] {
    value["tasks"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
}

pub(in crate::tui) fn count_label(arguments: &Value, result: Option<&Value>) -> String {
    let count = tasks(arguments)
        .len()
        .max(result.map_or(0, |value| tasks(value).len()));
    match count {
        0 => String::new(),
        1 => "1 agent".into(),
        _ => format!("{count} agents"),
    }
}

pub(in crate::tui) fn batch_status(
    result: Option<&Value>,
    running: bool,
    failed: bool,
) -> (&'static str, Color) {
    if running {
        return ("Dispatching", palette::accent());
    }
    if result.is_some_and(|r| r["cancelled"] == true) {
        return ("Cancelled", palette::muted());
    }
    if failed {
        return ("Failed", palette::removed());
    }
    let tasks = result.map_or(&[][..], tasks);
    let launch_errors = tasks
        .iter()
        .filter(|task| task["status"] == "failed")
        .count();
    match launch_errors {
        0 if tasks.is_empty() => ("Returned", palette::muted()),
        0 => ("Dispatched", palette::added()),
        count if count == tasks.len() => ("Failed", palette::removed()),
        _ => ("Partial", palette::removed()),
    }
}

pub(in crate::tui) fn header(
    width: u16,
    count: &str,
    status: &str,
    color: Color,
) -> Vec<Line<'static>> {
    let title = Span::styled(
        format!("{} Task", palette::tool_icon("task")),
        Style::default()
            .fg(palette::tool_accent("task"))
            .add_modifier(Modifier::BOLD),
    );
    let count = Span::styled(
        if count.is_empty() {
            String::new()
        } else {
            format!("  {count}")
        },
        Style::default().fg(palette::muted()),
    );
    let status = Span::styled(status.to_owned(), Style::default().fg(color));
    let columns = usize::from(width);
    let bg = palette::tool_pending_bg();
    if title.width() + count.width() + status.width() < columns {
        vec![Line::from(compose_header(
            vec![title.bg(bg), count.bg(bg)],
            vec![status.bg(bg)],
            columns,
            Some(bg),
        ))]
    } else {
        let mut rows = wrap_block(Line::from(vec![title, count]), width, "");
        rows.extend(wrap_block(Line::from(status), width, ""));
        rows
    }
}

pub(super) fn body(view: &View<'_>, width: u16) -> Vec<Line<'static>> {
    let expanded = view.expanded || view.context.full;
    let requested = tasks(view.arguments);
    let returned = view.result.map_or(&[][..], tasks);
    let count = requested.len().max(returned.len());
    let shown = if expanded {
        count
    } else {
        count.min(PREVIEW_TASKS)
    };
    let detail = Style::default().fg(palette::muted());
    let error = Style::default().fg(palette::removed());
    let mut rows = Vec::new();

    if view.failed || returned.is_empty() {
        // Old records and unknown results keep their original body; validation errors are placed before the task list.
        rows.extend(wrap(
            logical(view.text).into_iter().map(str::to_owned),
            width,
            if view.failed { error } else { detail },
        ));
    }
    if view.failed && returned.is_empty() {
        return rows;
    }

    for index in 0..shown {
        // The scheduler returns identifiers in submission order while names, roles and task descriptions stay in the call arguments.
        let request = requested.get(index).unwrap_or(&Value::Null);
        let response = returned.get(index).unwrap_or(&Value::Null);
        push_task(
            &mut rows,
            request,
            response,
            index,
            view.running,
            expanded,
            width,
        );
    }
    let context = field(view.arguments, "context");
    if expanded {
        if let Some(context) = context {
            rows.extend(wrap_block(
                Line::from(vec![
                    Span::styled("Context  ", detail.add_modifier(Modifier::BOLD)),
                    Span::styled(context.to_owned(), detail),
                ]),
                width,
                "",
            ));
        }
    } else if shown < count {
        let more = if width >= 36 {
            hint(count - shown, MoreHint::Expand("agents"))
        } else {
            format!("+{} agents · Ctrl + O", count - shown)
        };
        rows.extend(wrap_block(block_line(more, detail), width, ""));
    } else if count > 0 {
        let shortcut = if width >= 18 {
            "Ctrl + O to expand"
        } else {
            "Ctrl + O"
        };
        rows.extend(wrap_block(block_line(shortcut, detail), width, ""));
    }
    rows
}

fn push_task(
    rows: &mut Vec<Line<'static>>,
    request: &Value,
    response: &Value,
    index: usize,
    running: bool,
    expanded: bool,
    width: u16,
) {
    let role = field(request, "role");
    let name = field(request, "name")
        .or(role)
        .or_else(|| field(response, "agent_id"))
        .map_or_else(|| format!("Agent {}", index + 1), single_line);
    let primary = Style::default()
        .fg(palette::text())
        .add_modifier(Modifier::BOLD);
    let detail = Style::default().fg(palette::muted());
    let marker = match crate::tui::themes::current_icons() {
        IconMode::Nerd => "◆ ",
        IconMode::Ascii => "* ",
    };
    let role = role.map(single_line).filter(|role| *role != name);
    let mut left = vec![
        Span::styled(marker, Style::default().fg(palette::tool_accent("task"))),
        Span::styled(name, primary),
    ];
    if let Some(role) = role {
        left.push(Span::styled(format!("  {role}"), detail));
    }
    let result = response["results"]["results"]
        .as_array()
        .and_then(|results| {
            results
                .iter()
                .find(|item| item["run"]["id"] == response["run_id"])
        })
        .map(|item| &item["run"]);
    if let Some((status, color)) = task_status(result, response, running) {
        let badge = Span::styled(status, Style::default().fg(color));
        let columns = usize::from(width);
        let label_width: usize = left.iter().map(Span::width).sum();
        let required = if expanded { label_width + 1 } else { 8 };
        if columns >= badge.width() + required {
            rows.push(Line::from(compose_header(left, vec![badge], columns, None)));
        } else {
            rows.extend(wrap_block(Line::from(left), width, "  "));
            rows.extend(wrap_block(
                Line::from(vec![Span::raw("  "), badge]),
                width,
                "  ",
            ));
        }
    } else {
        rows.extend(wrap_block(Line::from(left), width, "  "));
    }
    if let Some(assignment) = field(request, "task") {
        if expanded {
            rows.extend(wrap_block(
                block_line(format!("  {assignment}"), detail),
                width,
                "  ",
            ));
        } else if width > 2 {
            let summary = clip(&single_line(assignment), usize::from(width - 2), true);
            rows.push(block_line(format!("  {summary}"), detail));
        }
    }
    let output = result
        .and_then(|run| run.get("result"))
        .filter(|output| !output.is_null());
    let message =
        field(response, "error").or_else(|| output.and_then(|output| field(output, "error")));
    if let Some(message) = message {
        rows.extend(wrap_block(
            block_line(
                format!("  {message}"),
                Style::default().fg(palette::removed()),
            ),
            width,
            "  ",
        ));
    }
    if expanded {
        let mut metadata = Vec::new();
        if let Some(id) = field(response, "run_id") {
            metadata.push(format!("Run {}", single_line(id)));
        }
        if let Some(uri) = field(response, "output_uri").or_else(|| field(response, "agent_id")) {
            metadata.push(single_line(uri));
        }
        for (enabled, label) in [
            (request["readonly"] == true, "Read-only"),
            (request["isolated"] == true, "Isolated"),
            (response["blocking"] == true, "Blocking"),
        ] {
            if enabled {
                metadata.push(label.into());
            }
        }
        if !metadata.is_empty() {
            rows.extend(wrap_block(
                block_line(format!("  {}", metadata.join("  ·  ")), detail),
                width,
                "  ",
            ));
        }
        if let Some(output) = output.filter(|_| message.is_none()) {
            let output = field(output, "content")
                .or_else(|| output.as_str())
                .map_or_else(|| output.to_string(), str::to_owned);
            rows.extend(wrap_block(
                block_line(format!("  Result  {output}"), detail),
                width,
                "  ",
            ));
        }
    }
}

fn task_status(
    run: Option<&Value>,
    response: &Value,
    running: bool,
) -> Option<(&'static str, Color)> {
    let status = run
        .and_then(|run| field(run, "status"))
        .or_else(|| field(response, "status"));
    Some(match status {
        Some("queued") => ("Queued", palette::accent()),
        Some("running") => ("Running", palette::accent()),
        Some("completed") => ("Completed", palette::added()),
        Some("failed") => ("Failed", palette::removed()),
        Some("cancelled" | "stopped" | "interrupted") => ("Cancelled", palette::muted()),
        Some(_) => ("Unknown", palette::muted()),
        None if running => ("Preparing", palette::muted()),
        None => return None,
    })
}

fn field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value[key].as_str().filter(|text| !text.trim().is_empty())
}

fn single_line(text: &str) -> String {
    pp_core::text::display_text(text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}
