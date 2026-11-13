//! TODO 固定区的阶段／任务树状清单布局。

use super::*;

// =============================================================================
// TODO 固定区的完整清单布局
// =============================================================================

/// 阶段编号采用上游 ROMAN_PAIRS 的罗马数字，仅用于显示。
fn phase_roman(mut number: usize) -> String {
    const PAIRS: [(usize, &str); 13] = [
        (1000, "M"),
        (900, "CM"),
        (500, "D"),
        (400, "CD"),
        (100, "C"),
        (90, "XC"),
        (50, "L"),
        (40, "XL"),
        (10, "X"),
        (9, "IX"),
        (5, "V"),
        (4, "IV"),
        (1, "I"),
    ];
    let mut roman = String::new();
    for (value, symbol) in PAIRS {
        while number >= value {
            roman.push_str(symbol);
            number -= value;
        }
    }
    roman
}

/// 使用三列选择框，状态标记与正文统一着色，完成项仅勾选、不划线。
pub(in crate::tui) fn todo_task_styles(
    status: pp_core::todo::TodoStatus,
) -> (&'static str, Style, Style) {
    use pp_core::todo::TodoStatus;
    let (symbol, marker, text) = match status {
        TodoStatus::Pending => ("[ ]", palette::MUTED, Style::default().fg(palette::MUTED)),
        TodoStatus::InProgress => (
            "[ ]",
            palette::TODO_ACTIVE,
            Style::default()
                .fg(palette::TODO_ACTIVE)
                .add_modifier(Modifier::BOLD),
        ),
        TodoStatus::Completed => (
            "[\u{f00c}]",
            palette::ADDED,
            Style::default().fg(palette::ADDED),
        ),
        TodoStatus::Abandoned => ("[ ]", palette::MUTED, Style::default().fg(palette::MUTED)),
        TodoStatus::Blocked => ("[!]", palette::ACCENT, Style::default().fg(palette::ACCENT)),
    };
    (symbol, Style::default().fg(marker), text)
}

/// 阶段行：`I. Name (closed/total)`，颜色跟随阶段状态。
pub(in crate::tui) fn todo_phase_line(
    phase: &pp_core::todo::TodoPhase,
    index: usize,
) -> Line<'static> {
    use pp_core::todo::TodoStatus;
    let closed = phase
        .tasks
        .iter()
        .filter(|task| matches!(task.status, TodoStatus::Completed | TodoStatus::Abandoned))
        .count();
    let active = phase
        .tasks
        .iter()
        .any(|task| task.status == TodoStatus::InProgress);
    let color = if phase
        .tasks
        .iter()
        .all(|task| task.status == TodoStatus::Completed)
    {
        palette::ADDED
    } else if active {
        palette::TODO_ACTIVE
    } else {
        palette::MUTED
    };
    Line::from(vec![
        Span::styled(
            format!("{}. ", phase_roman(index + 1)),
            Style::default().fg(color),
        ),
        Span::styled(
            pp_core::text::display_text(&phase.name).into_owned(),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" ({closed}/{})", phase.tasks.len()),
            Style::default().fg(palette::MUTED),
        ),
    ])
}

/// 阶段行首行树枝：展开清单与收起窗口共用，窄窗口缩短分支。
pub(in crate::tui) fn todo_phase_prefix(width: u16) -> &'static str {
    if width >= 5 { "├─ " } else { "├" }
}

/// 任务行首行树枝：按可用宽度给正文留列，`last` 表示阶段内最后一个可见任务。
pub(in crate::tui) fn todo_task_prefix(width: u16, last: bool) -> &'static str {
    match (width, last) {
        (12.., true) => "│  └─ ",
        (12.., false) => "│  ├─ ",
        (10.., true) => "│ └ ",
        (10.., false) => "│ ├ ",
        (8.., true) => "└ ",
        (8.., false) => "├ ",
        _ => "",
    }
}

/// 折行续行缩进，与首行树枝一起对齐任务正文。
fn todo_task_continuation(width: u16, last: bool) -> &'static str {
    match (width, last) {
        (12.., true) => "│         ",
        (12.., false) => "│  │      ",
        (10.., true) => "│       ",
        (10.., false) => "│ │     ",
        (8.., true) => "      ",
        (8.., false) => "│     ",
        (6.., _) => "    ",
        _ => "",
    }
}

/// 先按正文宽度折行，再逐行补齐树枝，显式换行也保留连线。
fn todo_tree_rows(
    content: Line<'static>,
    width: u16,
    first: Line<'static>,
    continuation: &str,
) -> Vec<Line<'static>> {
    let body_width = width.saturating_sub(first.width() as u16).max(1);
    wrap_block(content, body_width, "")
        .into_iter()
        .enumerate()
        .map(|(index, row)| {
            let mut prefix = if index == 0 {
                first.clone()
            } else {
                Line::styled(continuation.to_owned(), Style::default().fg(palette::DIM))
            };
            prefix.spans.extend(row.spans);
            prefix
        })
        .collect()
}

/// 结构化完整清单：阶段头 `I. Name (closed/total)`，任务行按状态着色。
/// 动态文本全部经 display_text 折行安全展示，重复文案附 phase+occurrence 寻址提示。
pub(in crate::tui) struct TodoChecklist {
    pub(in crate::tui) rows: Vec<Line<'static>>,
    pub(in crate::tui) task_rows: Vec<usize>,
}

pub(in crate::tui) fn todo_checklist(
    phases: &[pp_core::todo::TodoPhase],
    width: u16,
) -> TodoChecklist {
    use pp_core::todo::TodoStatus;
    use std::collections::HashMap;

    let mut totals: HashMap<&str, usize> = HashMap::new();
    for phase in phases {
        for task in &phase.tasks {
            *totals.entry(task.content.as_str()).or_default() += 1;
        }
    }
    let mut occurrence: HashMap<&str, usize> = HashMap::new();

    let mut rows = Vec::new();
    let mut task_rows = Vec::new();
    for (index, phase) in phases.iter().enumerate() {
        rows.extend(todo_tree_rows(
            todo_phase_line(phase, index),
            width,
            Line::styled(todo_phase_prefix(width), Style::default().fg(palette::DIM)),
            if width >= 5 { "│  " } else { "│" },
        ));
        for (task_index, task) in phase.tasks.iter().enumerate() {
            let (symbol, marker, style) = todo_task_styles(task.status);
            let tag = match &task.status {
                TodoStatus::Pending | TodoStatus::Completed => String::new(),
                TodoStatus::InProgress => " (in progress)".to_owned(),
                TodoStatus::Abandoned => " (dropped)".to_owned(),
                TodoStatus::Blocked => match &task.blocker {
                    Some(reason) => format!(" (blocked: {reason})"),
                    None => " (blocked)".to_owned(),
                },
            };
            let mut spans = vec![Span::styled(
                pp_core::text::display_text(&task.content).into_owned(),
                style,
            )];
            if !tag.is_empty() {
                spans.push(Span::styled(
                    pp_core::text::display_text(&tag).into_owned(),
                    style,
                ));
            }
            if totals[task.content.as_str()] > 1 {
                let ordinal = {
                    let slot = occurrence.entry(task.content.as_str()).or_default();
                    *slot += 1;
                    *slot
                };
                spans.push(Span::styled(
                    format!(
                        " · phase \"{}\" occurrence {ordinal}",
                        pp_core::text::display_text(&phase.name)
                    ),
                    Style::default().fg(palette::MUTED),
                ));
            }
            let last = task_index + 1 == phase.tasks.len();
            // 缩短窄窗口的树枝，至少为中文正文保留两列。
            let mut prefix = Line::styled(
                todo_task_prefix(width, last),
                Style::default().fg(palette::DIM),
            );
            if width >= 6 {
                prefix
                    .spans
                    .push(Span::styled(format!("{symbol} "), marker));
            }
            task_rows.push(rows.len());
            rows.extend(todo_tree_rows(
                Line::from(spans),
                width,
                prefix,
                todo_task_continuation(width, last),
            ));
        }
    }
    TodoChecklist { rows, task_rows }
}

/// 失败只展示诊断，不把随结果携带的旧清单再次展开。
pub(super) fn todo_body(view: &View<'_>, width: u16) -> Vec<Line<'static>> {
    if !view.failed {
        return Vec::new();
    }
    let result = view.result.unwrap_or(&Value::Null);
    let error = result["error"]
        .as_str()
        .or_else(|| {
            result["content"]
                .as_str()
                .and_then(|text| text.lines().next())
        })
        .unwrap_or(view.text);
    wrap(
        logical(error).into_iter().map(str::to_owned),
        width,
        Style::default().fg(palette::REMOVED),
    )
}
