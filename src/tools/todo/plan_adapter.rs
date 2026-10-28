use super::*;
use crate::plan::{PlanDocument, patch_task_checks, plan_tasks};
use crate::tools::{plan, save};

// =============================================================================
// Plan adapter
// =============================================================================

/// Approved plan tasks are immutable; adding, removing, replacing or dropping
/// them requires a re-plan.
const PLAN_STRUCTURE_LOCKED: &str =
    "Approved plan tasks cannot be added, removed, replaced, or dropped; re-plan first.";
/// The approved ordered checklist drifted from the executed document.
const TASKS_CHANGED: &str = "Plan tasks changed; re-plan before completing.";
/// Mutating a plan checklist before its plan is executing.
const PLAN_READONLY_BEFORE_EXECUTION: &str = "Plan todos are read-only before execution.";

/// Plan task adapter. `current` is the synced projection of `document` (same
/// task count and order), so targets resolve to document-order ordinals
/// instead of first-content matching. `editable=false` (draft/ready preview)
/// only allows `view`; `editable=true` allows view/start/done/block/unblock
/// and writes checkbox changes back through the conditional plan save. Every
/// semantic, parse and commit failure returns `{ok:false,..}` with the
/// trustworthy pre-call projection; `Err` never carries todo outcomes.
pub(in crate::tools) fn run_plan(
    workspace: &save::Workspace,
    document: &PlanDocument,
    current: &[TodoPhase],
    arguments: &str,
    editable: bool,
) -> anyhow::Result<(Value, Option<TodoChange>)> {
    if !editable {
        return run_readonly(document, arguments, PLAN_READONLY_BEFORE_EXECUTION);
    }
    let op = identifiable_op(arguments);
    // The approved ordered baseline is authoritative for every editable call,
    // including view: a drifted checklist must not be operated on, and the
    // projection must still map one-to-one onto the document tasks.
    let tasks = plan_tasks(&document.content);
    let baseline_ok = document.approved_tasks.as_deref().is_some_and(|approved| {
        approved.len() == tasks.len()
            && approved
                .iter()
                .zip(&tasks)
                .all(|(text, task)| text == &task.text)
    });
    let synced = current
        .iter()
        .flat_map(|phase| &phase.tasks)
        .map(|task| &task.content)
        .eq(tasks.iter().map(|task| &task.text));
    if !baseline_ok || !synced {
        let content = format_summary(current, &[TASKS_CHANGED.to_owned()], true);
        return Ok((
            plan_failure(
                document,
                document.revision,
                op,
                TASKS_CHANGED.to_owned(),
                content,
                current,
                false,
            ),
            None,
        ));
    }

    let params = match resolve_params(arguments, !current.is_empty()) {
        Ok(params) => params,
        Err(message) => {
            return Ok((
                plan_failure(
                    document,
                    document.revision,
                    op,
                    message.clone(),
                    message,
                    current,
                    false,
                ),
                None,
            ));
        }
    };
    if params.occurrence.is_some() && params.task.is_none() {
        let error = "occurrence requires a task target.".to_owned();
        let content = format_summary(current, std::slice::from_ref(&error), true);
        return Ok((
            plan_failure(
                document,
                document.revision,
                Some(params.op.as_str()),
                error,
                content,
                current,
                false,
            ),
            None,
        ));
    }

    if matches!(
        params.op,
        TodoOperation::Init | TodoOperation::Append | TodoOperation::Rm | TodoOperation::Drop
    ) {
        let content = format_summary(current, &[PLAN_STRUCTURE_LOCKED.to_owned()], true);
        return Ok((
            plan_failure(
                document,
                document.revision,
                Some(params.op.as_str()),
                PLAN_STRUCTURE_LOCKED.to_owned(),
                content,
                current,
                false,
            ),
            None,
        ));
    }

    let mut errors = Vec::new();
    let operation = match params.op {
        TodoOperation::View => Some(Operation::View),
        TodoOperation::Start => {
            plan_start_target(current, &params, &mut errors).map(Operation::Start)
        }
        TodoOperation::Done => Some(Operation::Complete(plan_task_targets(
            current,
            &params,
            &mut errors,
        ))),
        TodoOperation::Block => {
            if !has_target(&params) {
                errors.push("block requires a task or phase target".to_owned());
                None
            } else {
                Some(Operation::Block {
                    targets: plan_task_targets(current, &params, &mut errors),
                    reason: normalize_reason(params.reason.as_deref()),
                })
            }
        }
        TodoOperation::Unblock => {
            if !has_target(&params) {
                errors.push("unblock requires a task or phase target".to_owned());
                None
            } else {
                Some(Operation::Unblock(plan_task_targets(
                    current,
                    &params,
                    &mut errors,
                )))
            }
        }
        TodoOperation::Init | TodoOperation::Append | TodoOperation::Rm | TodoOperation::Drop => {
            unreachable!("structure-locked operations returned above")
        }
    };
    if !errors.is_empty() {
        let error = errors.join("; ");
        let content = format_summary(current, &errors, false);
        return Ok((
            plan_failure(
                document,
                document.revision,
                Some(params.op.as_str()),
                error,
                content,
                current,
                false,
            ),
            None,
        ));
    }
    let operation = operation.expect("operation exists when no errors remain");

    if params.op == TodoOperation::View {
        // Pure read of the synced projection: no activation, no write-back.
        let content = format_summary(current, &[], true);
        return Ok((
            plan_success(
                document,
                document.revision,
                "view",
                content,
                current,
                false,
                None,
            ),
            None,
        ));
    }

    let mut next = apply_operation(current, &operation);
    normalize_in_progress(&mut next);
    // Document-order checkbox vector derived from the new statuses; runtime
    // state (active/blocker) never changes checkboxes.
    let checked: Vec<bool> = next
        .iter()
        .flat_map(|phase| phase.tasks.iter())
        .map(|task| task.status == TodoStatus::Completed)
        .collect();
    let checkbox_changed = tasks
        .iter()
        .zip(&checked)
        .any(|(task, &state)| task.completed != state);
    let (revision, content, warning) = if checkbox_changed {
        match write_plan_progress(workspace, document, &tasks, &checked) {
            Ok(committed) => committed,
            Err(error) => {
                // The plan file is untouched; report the failure with the
                // pre-call projection instead of a generic error-only shape.
                let message = format!("{error:#}");
                let failure = plan_failure(
                    document,
                    document.revision,
                    Some(params.op.as_str()),
                    message.clone(),
                    message,
                    current,
                    false,
                );
                return Ok((failure, None));
            }
        }
    } else {
        // Runtime-only transition: current revision, no file copy, no save.
        (document.revision, None, None)
    };

    let change = TodoChange {
        phases: next.clone(),
        plan: Some(PlanTodoChange {
            plan_id: document.id.clone(),
            revision,
            content,
        }),
    };
    let summary = format_summary(&next, &[], false);
    let value = plan_success(
        document,
        revision,
        params.op.as_str(),
        summary,
        &next,
        false,
        warning,
    );
    Ok((value, Some(change)))
}

/// One conditional plan-file commit: patch exactly the changed status bytes,
/// check the next revision before saving, and write through the shared plan
/// save boundary. The caller (the agent's Execute+Executed loop) holds the
/// task's PlanLease; no lease is reacquired here.
fn write_plan_progress(
    workspace: &save::Workspace,
    document: &PlanDocument,
    tasks: &[crate::plan::PlanTask],
    checked: &[bool],
) -> anyhow::Result<(u64, Option<String>, Option<String>)> {
    let content = patch_task_checks(&document.content, tasks, checked)?
        .ok_or_else(|| anyhow::anyhow!("No checkbox changed; plan progress was not saved."))?;
    let revision = document.next_revision()?;
    let warning = plan::save_progress(workspace, document, &content)?;
    Ok((revision, Some(content), warning))
}

/// Read-only plan projection entry (Plan-mode preview and bound finished
/// plans). Only `view` runs; every other operation fails with the supplied
/// diagnostic and the pre-call projection. Never touches files or runtime.
pub(in crate::tools) fn run_readonly(
    document: &PlanDocument,
    arguments: &str,
    mutation_error: &'static str,
) -> anyhow::Result<(Value, Option<TodoChange>)> {
    let phases = readonly_projection(document)?;
    let params = match resolve_params(arguments, !phases.is_empty()) {
        Ok(params) => params,
        Err(message) => {
            let op = identifiable_op(arguments);
            let content = format_summary(&phases, std::slice::from_ref(&message), true);
            return Ok((
                plan_failure(
                    document,
                    document.revision,
                    op,
                    message,
                    content,
                    &phases,
                    true,
                ),
                None,
            ));
        }
    };
    if params.op != TodoOperation::View {
        let content = format_summary(&phases, &[mutation_error.to_owned()], true);
        return Ok((
            plan_failure(
                document,
                document.revision,
                Some(params.op.as_str()),
                mutation_error.to_owned(),
                content,
                &phases,
                true,
            ),
            None,
        ));
    }
    if params.occurrence.is_some() && params.task.is_none() {
        let error = "occurrence requires a task target.".to_owned();
        let content = format_summary(&phases, std::slice::from_ref(&error), true);
        return Ok((
            plan_failure(
                document,
                document.revision,
                Some(params.op.as_str()),
                error,
                content,
                &phases,
                true,
            ),
            None,
        ));
    }
    let content = format_summary(&phases, &[], true);
    Ok((
        plan_success(
            document,
            document.revision,
            "view",
            content,
            &phases,
            true,
            None,
        ),
        None,
    ))
}

/// File-authoritative read-only projection: unchecked → pending, checked →
/// completed, no active/blocker. A revision-0 document has no saved bytes, so
/// its preview is empty rather than a projection of unsaved draft content.
fn readonly_projection(document: &PlanDocument) -> anyhow::Result<Vec<TodoPhase>> {
    if document.revision == 0 {
        return Ok(Vec::new());
    }
    project_plan_todos(document, &[], false)
}

/// 计划按完整任务原文、完整阶段名和筛选后的一基 occurrence 定位。
/// 同名阶段区域全部参与筛选；显式空目标也只做精确匹配，不扩大成全部任务。
pub(super) fn plan_task_targets(
    current: &[TodoPhase],
    params: &TodoParams,
    errors: &mut Vec<String>,
) -> Vec<TaskOrdinal> {
    if let Some(task) = params.task.as_deref() {
        let mut candidates = locate_task_matches(current, task);
        if let Some(phase) = params.phase.as_deref() {
            candidates.retain(|target| current[target.phase].name == phase);
        }
        if let Some(occurrence) = params.occurrence {
            let index = occurrence as usize - 1;
            if let Some(target) = candidates.get(index) {
                return vec![*target];
            }
            errors.push(format!(
                "Task \"{task}\" not found: occurrence {occurrence} exceeds {} match(es).",
                candidates.len()
            ));
            return Vec::new();
        }
        return match candidates.len() {
            0 => {
                errors.push(format!("Task \"{task}\" not found"));
                Vec::new()
            }
            1 => candidates,
            _ => {
                errors.push("Ambiguous plan task; specify phase or occurrence.".to_owned());
                Vec::new()
            }
        };
    }
    if params.occurrence.is_some() {
        errors.push("occurrence requires a task target.".to_owned());
        return Vec::new();
    }
    if let Some(name) = params.phase.as_deref() {
        let matches: Vec<TaskOrdinal> = current
            .iter()
            .enumerate()
            .flat_map(|(phase, p)| (0..p.tasks.len()).map(move |task| TaskOrdinal { phase, task }))
            .filter(|target| current[target.phase].name == name)
            .collect();
        if matches.is_empty() {
            errors.push(format!("Phase \"{name}\" not found"));
        }
        return matches;
    }
    current
        .iter()
        .enumerate()
        .flat_map(|(phase, p)| (0..p.tasks.len()).map(move |task| TaskOrdinal { phase, task }))
        .collect()
}

/// `start` still addresses exactly one task; missing text, ambiguity and
/// out-of-range occurrences fail the call.
fn plan_start_target(
    current: &[TodoPhase],
    params: &TodoParams,
    errors: &mut Vec<String>,
) -> Option<TaskOrdinal> {
    if params.task.is_none() {
        errors.push("Missing task content".to_owned());
        return None;
    }
    plan_task_targets(current, params, errors).first().copied()
}

fn has_target(params: &TodoParams) -> bool {
    params.task.is_some() || params.phase.is_some()
}

fn append_plan_target_hints(mut content: String, phases: &[TodoPhase]) -> String {
    use std::{collections::HashMap, fmt::Write};
    let mut counts = HashMap::new();
    for task in phases.iter().flat_map(|phase| &phase.tasks) {
        *counts.entry(task.content.as_str()).or_insert(0usize) += 1;
    }
    let mut occurrences = HashMap::new();
    let mut first = true;
    for phase in phases {
        for task in &phase.tasks {
            if counts.get(task.content.as_str()).copied().unwrap_or(0) < 2 {
                continue;
            }
            let occurrence = occurrences
                .entry((phase.name.as_str(), task.content.as_str()))
                .or_insert(0usize);
            *occurrence += 1;
            if first {
                content.push_str("\nDuplicate task targets:");
                first = false;
            }
            write!(
                content,
                "\n  task={}, phase={}, occurrence={occurrence}",
                json!(task.content),
                json!(phase.name)
            )
            .expect("writing to String");
        }
    }
    content
}

/// `plan` metadata attached to plan adapter results; `path` is `null` while
/// the document has no saved lifecycle path (revision 0).
fn plan_object(document: &PlanDocument, revision: u64, readonly: bool) -> Value {
    json!({
        "id": document.id,
        "path": document.path(),
        "revision": revision,
        "status": serde_json::to_value(document.status).unwrap_or_default(),
        "readonly": readonly,
    })
}

fn plan_success(
    document: &PlanDocument,
    revision: u64,
    op: &str,
    content: String,
    phases: &[TodoPhase],
    readonly: bool,
    warning: Option<String>,
) -> Value {
    let mut value = success_value(op, append_plan_target_hints(content, phases), phases);
    value["plan"] = plan_object(document, revision, readonly);
    if let Some(warning) = warning {
        value["warning"] = json!(warning);
    }
    value
}

fn plan_failure(
    document: &PlanDocument,
    revision: u64,
    op: Option<&'static str>,
    error: String,
    content: String,
    phases: &[TodoPhase],
    readonly: bool,
) -> Value {
    let mut value = failure_value(phases, op, error, append_plan_target_hints(content, phases));
    value["plan"] = plan_object(document, revision, readonly);
    value
}
