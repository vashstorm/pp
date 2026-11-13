//! 按序数的目标定位与状态转换。
use pp_core::todo::{TodoItem, TodoPhase, TodoStatus};
use std::collections::HashSet;

use super::params::{InitEntry, TodoOperation, TodoParams};

/// Phase name for `init` given a flat `items` list with no explicit `phase`.
const DEFAULT_INIT_PHASE: &str = "Tasks";

/// A task addressed by its ordinal position (phase index, task index within
/// the phase), resolved once before any state transformation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TaskOrdinal {
    pub(crate) phase: usize,
    pub(crate) task: usize,
}

/// Resolved operation for one todo call. Structural ops carry their payload;
/// task-touching ops carry ordinals so the plan adapter can supply
/// document-order positions without re-running first-text matching.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Operation {
    Init(Vec<InitEntry>),
    Append {
        phase: String,
        items: Vec<String>,
    },
    Start(TaskOrdinal),
    Complete(Vec<TaskOrdinal>),
    Abandon(Vec<TaskOrdinal>),
    Remove(Vec<TaskOrdinal>),
    Block {
        targets: Vec<TaskOrdinal>,
        reason: Option<String>,
    },
    Unblock(Vec<TaskOrdinal>),
    View,
}

impl Operation {
    pub(crate) fn is_view(&self) -> bool {
        matches!(self, Self::View)
    }
}

/// Resolve validated params against the current phases. Every semantic error
/// (missing/not-found targets, duplicates) aborts the whole call.
pub(super) fn resolve(
    current: &[TodoPhase],
    params: &TodoParams,
) -> Result<Operation, Vec<String>> {
    let mut errors = Vec::new();
    let operation = match params.op {
        TodoOperation::Init => {
            let list = init_list(params, &mut errors);
            if !errors.is_empty() {
                return Err(errors);
            }
            Operation::Init(list)
        }
        TodoOperation::Append => {
            let Some((phase, items)) = append_payload(current, params, &mut errors) else {
                return Err(errors);
            };
            Operation::Append { phase, items }
        }
        TodoOperation::Start => {
            let Some(target) = resolve_task(current, params, &mut errors) else {
                return Err(errors);
            };
            Operation::Start(target)
        }
        TodoOperation::Done | TodoOperation::Drop => {
            let targets = task_targets(current, params, &mut errors);
            if !errors.is_empty() {
                return Err(errors);
            }
            if params.op == TodoOperation::Done {
                Operation::Complete(targets)
            } else {
                Operation::Abandon(targets)
            }
        }
        TodoOperation::Rm => {
            let targets = task_targets(current, params, &mut errors);
            if !errors.is_empty() {
                return Err(errors);
            }
            Operation::Remove(targets)
        }
        TodoOperation::Block | TodoOperation::Unblock => {
            let targeted = params.task.as_deref().is_some_and(|t| !t.is_empty())
                || params.phase.as_deref().is_some_and(|p| !p.is_empty());
            if !targeted {
                errors.push(format!(
                    "{} requires a task or phase target",
                    params.op.as_str()
                ));
                return Err(errors);
            }
            let targets = task_targets(current, params, &mut errors);
            if !errors.is_empty() {
                return Err(errors);
            }
            if params.op == TodoOperation::Block {
                Operation::Block {
                    targets,
                    reason: normalize_reason(params.reason.as_deref()),
                }
            } else {
                Operation::Unblock(targets)
            }
        }
        TodoOperation::View => Operation::View,
    };
    Ok(operation)
}

/// Task targets for done/drop/rm/block/unblock: task first, then all tasks of
/// a phase, otherwise every task. Matches upstream truthiness: an empty
/// `task`/`phase` string does not select anything and falls through.
fn task_targets(
    current: &[TodoPhase],
    params: &TodoParams,
    errors: &mut Vec<String>,
) -> Vec<TaskOrdinal> {
    if params.occurrence.is_some() {
        return resolve_task(current, params, errors).into_iter().collect();
    }
    if let Some(task) = params.task.as_deref().filter(|t| !t.is_empty()) {
        return locate_task(current, task, errors).into_iter().collect();
    }
    if let Some(name) = params.phase.as_deref().filter(|p| !p.is_empty()) {
        return locate_phase(current, name, errors)
            .map(|phase| {
                (0..current[phase].tasks.len())
                    .map(|task| TaskOrdinal { phase, task })
                    .collect()
            })
            .unwrap_or_default();
    }
    current
        .iter()
        .enumerate()
        .flat_map(|(phase, p)| (0..p.tasks.len()).map(move |task| TaskOrdinal { phase, task }))
        .collect()
}

/// Every task whose content matches exactly, in document order. The plan
/// adapter filters this by full phase name and picks the one-based
/// `occurrence` from the filtered candidates.
pub(crate) fn locate_task_matches(current: &[TodoPhase], content: &str) -> Vec<TaskOrdinal> {
    let mut matches = Vec::new();
    for (phase, p) in current.iter().enumerate() {
        for (task, t) in p.tasks.iter().enumerate() {
            if t.content == content {
                matches.push(TaskOrdinal { phase, task });
            }
        }
    }
    matches
}

fn locate_task(
    current: &[TodoPhase],
    content: &str,
    errors: &mut Vec<String>,
) -> Option<TaskOrdinal> {
    for (phase, p) in current.iter().enumerate() {
        for (task, t) in p.tasks.iter().enumerate() {
            if t.content == content {
                return Some(TaskOrdinal { phase, task });
            }
        }
    }
    if is_task_id_shaped(content) {
        errors.push(format!(
            "Task \"{content}\" not found. Tasks are referenced by content, not by IDs — pass the task's full text from the previous result."
        ));
    } else {
        let total: usize = current.iter().map(|p| p.tasks.len()).sum();
        let hint = if total == 0 {
            " (todo list is empty — was it replaced or not yet created?)"
        } else {
            ""
        };
        errors.push(format!("Task \"{content}\" not found{hint}"));
    }
    None
}

/// `^task-\d+$` without pulling in a regex engine.
fn is_task_id_shaped(content: &str) -> bool {
    match content.strip_prefix("task-") {
        Some(rest) => !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()),
        None => false,
    }
}

fn locate_phase(current: &[TodoPhase], name: &str, errors: &mut Vec<String>) -> Option<usize> {
    if name.is_empty() {
        errors.push("Missing phase name".into());
        return None;
    }
    match current.iter().position(|p| p.name == name) {
        Some(index) => Some(index),
        None => {
            errors.push(format!("Phase \"{name}\" not found"));
            None
        }
    }
}

/// 单任务定位支持可选阶段筛选及一基 occurrence；缺失或空任务原文报错。
fn resolve_task(
    current: &[TodoPhase],
    params: &TodoParams,
    errors: &mut Vec<String>,
) -> Option<TaskOrdinal> {
    let Some(content) = params.task.as_deref().filter(|c| !c.is_empty()) else {
        errors.push("Missing task content".into());
        return None;
    };
    if let Some(occurrence) = params.occurrence {
        let mut candidates = locate_task_matches(current, content);
        if let Some(phase) = params.phase.as_deref() {
            candidates.retain(|target| current[target.phase].name == phase);
        }
        let target = usize::try_from(occurrence - 1)
            .ok()
            .and_then(|index| candidates.get(index))
            .copied();
        if target.is_none() {
            errors.push(format!(
                "Task \"{content}\" not found: occurrence {occurrence} exceeds {} match(es).",
                candidates.len()
            ));
        }
        return target;
    }
    locate_task(current, content, errors)
}

pub(crate) fn normalize_reason(raw: Option<&str>) -> Option<String> {
    let raw = raw?;
    // Collapse whitespace runs to single spaces (round-trip safety for one
    // checklist line / one summary line); an empty result means no note.
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        None
    } else {
        Some(collapsed)
    }
}

fn find_task_by_content<'a>(current: &'a [TodoPhase], content: &str) -> Option<&'a TodoItem> {
    current
        .iter()
        .flat_map(|phase| phase.tasks.iter())
        .find(|task| task.content == content)
}

fn init_list(params: &TodoParams, errors: &mut Vec<String>) -> Vec<InitEntry> {
    // An explicit `list` wins, including `[]`; otherwise non-empty flat items
    // initialize into the given phase or `Tasks`.
    let list = params.list.clone().or_else(|| {
        params
            .items
            .as_ref()
            .filter(|items| !items.is_empty())
            .map(|items| {
                vec![InitEntry {
                    phase: params
                        .phase
                        .clone()
                        .filter(|p| !p.is_empty())
                        .unwrap_or_else(|| DEFAULT_INIT_PHASE.to_owned()),
                    items: items.clone(),
                }]
            })
    });
    let Some(list) = list else {
        errors.push("Missing list for init operation".into());
        return Vec::new();
    };
    // Duplicate phase names / task contents would be permanently
    // unaddressable, so reject them up front.
    let mut seen_phases = HashSet::new();
    let mut seen_tasks = HashSet::new();
    for entry in &list {
        if !seen_phases.insert(entry.phase.as_str()) {
            errors.push(format!("Duplicate phase \"{}\" in init list", entry.phase));
        }
        for content in &entry.items {
            if !seen_tasks.insert(content.as_str()) {
                errors.push(format!("Duplicate task \"{content}\" in init list"));
            }
        }
    }
    list
}

fn append_payload(
    current: &[TodoPhase],
    params: &TodoParams,
    errors: &mut Vec<String>,
) -> Option<(String, Vec<String>)> {
    let Some(phase) = params.phase.clone().filter(|p| !p.is_empty()) else {
        errors.push("Missing phase name for append operation".into());
        return None;
    };
    let Some(items) = params.items.clone().filter(|items| !items.is_empty()) else {
        errors.push("Missing items for append operation".into());
        return None;
    };
    // Validate the whole batch before mutating so a failing op reports every
    // duplicate and leaves nothing half-applied.
    let mut seen = HashSet::new();
    let mut duplicate = false;
    for content in &items {
        if !seen.insert(content.as_str()) || find_task_by_content(current, content).is_some() {
            errors.push(format!("Task \"{content}\" already exists"));
            duplicate = true;
        }
    }
    if duplicate {
        return None;
    }
    Some((phase, items))
}

/// Apply a resolved operation to a fresh transaction snapshot. Total: no
/// error path, so any caller discard-on-error keeps the previous state.
pub(crate) fn apply_operation(current: &[TodoPhase], operation: &Operation) -> Vec<TodoPhase> {
    match operation {
        Operation::Init(list) => list
            .iter()
            .map(|entry| TodoPhase {
                name: entry.phase.clone(),
                tasks: entry
                    .items
                    .iter()
                    .map(|content| TodoItem {
                        content: content.clone(),
                        status: TodoStatus::Pending,
                        blocker: None,
                    })
                    .collect(),
            })
            .collect(),
        Operation::Append { phase, items } => {
            let mut next = clone_phases(current);
            let index = match next.iter().position(|p| p.name == *phase) {
                Some(index) => index,
                None => {
                    next.push(TodoPhase {
                        name: phase.clone(),
                        tasks: Vec::new(),
                    });
                    next.len() - 1
                }
            };
            next[index]
                .tasks
                .extend(items.iter().map(|content| TodoItem {
                    content: content.clone(),
                    status: TodoStatus::Pending,
                    blocker: None,
                }));
            next
        }
        Operation::Start(target) => {
            let mut next = clone_phases(current);
            // Upstream allows explicitly re-opening closed/blocked tasks.
            next[target.phase].tasks[target.task].status = TodoStatus::InProgress;
            next
        }
        Operation::Complete(targets) => {
            let mut next = clone_phases(current);
            set_status(&mut next, targets, TodoStatus::Completed);
            next
        }
        Operation::Abandon(targets) => {
            let mut next = clone_phases(current);
            set_status(&mut next, targets, TodoStatus::Abandoned);
            next
        }
        Operation::Remove(targets) => {
            let mut next = clone_phases(current);
            let removed: HashSet<(usize, usize)> =
                targets.iter().map(|t| (t.phase, t.task)).collect();
            for (phase_index, phase) in next.iter_mut().enumerate() {
                let mut index = 0usize;
                phase.tasks.retain(|_| {
                    let current_index = index;
                    index += 1;
                    !removed.contains(&(phase_index, current_index))
                });
            }
            next
        }
        Operation::Block { targets, reason } => {
            let mut next = clone_phases(current);
            for target in targets {
                let task = &mut next[target.phase].tasks[target.task];
                // Only actionable open work can be blocked; already-blocked
                // tasks stay eligible so a later block can refine its note.
                if matches!(
                    task.status,
                    TodoStatus::Pending | TodoStatus::InProgress | TodoStatus::Blocked
                ) {
                    task.status = TodoStatus::Blocked;
                    task.blocker = reason.clone();
                }
            }
            next
        }
        Operation::Unblock(targets) => {
            let mut next = clone_phases(current);
            for target in targets {
                let task = &mut next[target.phase].tasks[target.task];
                if task.status == TodoStatus::Blocked {
                    task.status = TodoStatus::Pending;
                    task.blocker = None;
                }
            }
            next
        }
        Operation::View => clone_phases(current),
    }
}

fn set_status(phases: &mut [TodoPhase], targets: &[TaskOrdinal], status: TodoStatus) {
    for target in targets {
        phases[target.phase].tasks[target.task].status = status;
    }
}

fn clone_phases(current: &[TodoPhase]) -> Vec<TodoPhase> {
    current
        .iter()
        .map(|phase| TodoPhase {
            name: phase.name.clone(),
            tasks: phase.tasks.clone(),
        })
        .collect()
}

/// 保留所有进行项；仅在没有进行项时启动最早的 pending。
/// 不自动启动 blocked，也不重开 completed。
pub(crate) fn normalize_in_progress(phases: &mut [TodoPhase]) {
    if phases
        .iter()
        .flat_map(|phase| &phase.tasks)
        .any(|task| task.status == TodoStatus::InProgress)
    {
        return;
    }
    if let Some(task) = phases
        .iter_mut()
        .flat_map(|phase| &mut phase.tasks)
        .find(|task| task.status == TodoStatus::Pending)
    {
        task.status = TodoStatus::InProgress;
    }
}
