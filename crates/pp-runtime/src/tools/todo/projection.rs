//! 计划正文到 todo 投影的转换。
use pp_core::todo::{TodoItem, TodoPhase, TodoStatus};

use super::state::normalize_in_progress;

/// Markdown 是任务及勾选的权威；仅在来源及批准基线相同时保留按序号的运行态。
pub(crate) fn project_plan_todos(
    document: &pp_core::plan::PlanDocument,
    previous: &[TodoPhase],
    preserve_runtime: bool,
) -> anyhow::Result<Vec<TodoPhase>> {
    use pp_core::plan::PlanStatus;
    use pp_core::plan::plan_tasks;
    let tasks = plan_tasks(&document.content);
    let executing = document.status == PlanStatus::Executed;
    if executing {
        anyhow::ensure!(
            document.approved_tasks.as_ref().is_some_and(|approved| {
                approved.len() == tasks.len()
                    && approved
                        .iter()
                        .zip(&tasks)
                        .all(|(text, task)| text == &task.text)
            }),
            "Plan tasks changed; re-plan before completing."
        );
    }
    let preserve_runtime = executing
        && preserve_runtime
        && previous
            .iter()
            .flat_map(|phase| &phase.tasks)
            .map(|task| &task.content)
            .eq(tasks.iter().map(|task| &task.text));
    let mut previous = previous.iter().flat_map(|phase| &phase.tasks);
    let mut phases: Vec<TodoPhase> = Vec::new();
    let mut region = None;
    for task in tasks {
        if region != Some(task.phase_index) {
            region = Some(task.phase_index);
            phases.push(TodoPhase {
                name: task.phase,
                tasks: Vec::new(),
            });
        }
        let old = previous.next().filter(|_| preserve_runtime);
        let (status, blocker) = if task.completed {
            (TodoStatus::Completed, None)
        } else if let Some(old) =
            old.filter(|old| matches!(old.status, TodoStatus::InProgress | TodoStatus::Blocked))
        {
            (old.status, old.blocker.clone())
        } else {
            (TodoStatus::Pending, None)
        };
        phases
            .last_mut()
            .expect("task creates its phase")
            .tasks
            .push(TodoItem {
                content: task.text,
                status,
                blocker,
            });
    }
    if executing {
        normalize_in_progress(&mut phases);
    }
    Ok(phases)
}
