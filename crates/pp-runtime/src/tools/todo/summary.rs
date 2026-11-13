//! todo 结果的可读摘要。
use pp_core::todo::{TodoItem, TodoPhase, TodoStatus};

pub fn format_summary(phases: &[TodoPhase], errors: &[String], read_only: bool) -> String {
    let total: usize = phases.iter().map(|phase| phase.tasks.len()).sum();
    if total == 0 {
        if !errors.is_empty() {
            return format!("Errors: {}", errors.join("; "));
        }
        return if read_only {
            "Todo list is empty.".into()
        } else {
            "Todo list cleared.".into()
        };
    }

    let remaining: Vec<(&TodoItem, &str)> = phases
        .iter()
        .flat_map(|phase| {
            phase
                .tasks
                .iter()
                .filter(|task| task.status.is_open())
                .map(move |task| (task, phase.name.as_str()))
        })
        .collect();

    // 阶段摘要取最早仍有未完成工作的阶段，不限制跨阶段并发执行。
    let current_index = phases
        .iter()
        .position(|phase| phase.tasks.iter().any(|task| task.status.is_open()))
        .unwrap_or(phases.len() - 1);
    let current = &phases[current_index];
    let done_in_current = current
        .tasks
        .iter()
        .filter(|task| task.status.is_closed())
        .count();

    let closed_all = phases
        .iter()
        .flat_map(|phase| phase.tasks.iter())
        .filter(|task| task.status.is_closed())
        .count();
    let blocked_all = phases
        .iter()
        .flat_map(|phase| phase.tasks.iter())
        .filter(|task| task.status == TodoStatus::Blocked)
        .count();
    let worked_ahead = phases.iter().enumerate().any(|(index, phase)| {
        index > current_index && phase.tasks.iter().any(|task| task.status.is_closed())
    });

    let mut lines: Vec<String> = Vec::new();
    if !errors.is_empty() {
        lines.push(format!("Errors: {}", errors.join("; ")));
    }
    if remaining.is_empty() {
        lines.push("Remaining items: none.".into());
    } else {
        lines.push(format!("Remaining items ({}):", remaining.len()));
        for (task, phase) in &remaining {
            lines.push(format!(
                "  - {} [{}] ({})",
                task.content,
                task.status.as_str(),
                phase
            ));
        }
    }
    let blocked_suffix = if blocked_all > 0 {
        format!(", {blocked_all} blocked")
    } else {
        String::new()
    };
    lines.push(format!(
        "Overall: {closed_all}/{total} done, {} open{blocked_suffix}.",
        remaining.len()
    ));
    lines.push(format!(
        "Active phase {}/{} \"{}\" ({}/{}){}",
        current_index + 1,
        phases.len(),
        current.name,
        done_in_current,
        current.tasks.len(),
        if worked_ahead {
            " — earliest phase with open tasks; later phases may run or finish independently (nothing was un-completed)."
        } else {
            "."
        }
    ));
    for phase in phases {
        lines.push(format!("  {}:", phase.name));
        for task in &phase.tasks {
            let checkbox = if task.status == TodoStatus::Completed {
                "[X]"
            } else {
                "[ ]"
            };
            let tag = match &task.status {
                TodoStatus::InProgress => " (in progress)".to_owned(),
                TodoStatus::Abandoned => " (dropped)".to_owned(),
                TodoStatus::Blocked => match &task.blocker {
                    Some(reason) => format!(" (blocked: {reason})"),
                    None => " (blocked)".to_owned(),
                },
                _ => String::new(),
            };
            lines.push(format!("    - {checkbox} {}{tag}", task.content));
        }
    }
    lines.join("\n")
}
