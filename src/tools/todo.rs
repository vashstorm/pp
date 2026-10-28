//! 普通 todo 工具：严格复刻 oh-my-pi 的 schema、状态转换、诊断与 summary。
//!
//! 参数解析/目标定位（`resolve_params`/`resolve`）与按序数的状态转换
//! （`apply_operation`）刻意分离：普通路径先按完整原文定位出
//! [`TaskOrdinal`]，计划适配器后续直接传入按文档顺序解析出的全局序数，
//! 不再通过首条同文案匹配定位。

use async_openai::types::chat::FunctionObject;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::HashSet;

use super::{plan, save};
use crate::plan::{PlanDocument, patch_task_checks, plan_tasks};

const TOOL_NAME: &str = "todo";
/// Phase name for `init` given a flat `items` list with no explicit `phase`.
const DEFAULT_INIT_PHASE: &str = "Tasks";
const OPERATIONS: &[&str] = &[
    "init", "start", "done", "rm", "drop", "block", "unblock", "append", "view",
];
const ALLOWED_FIELDS: &[&str] = &[
    "op",
    "list",
    "task",
    "phase",
    "items",
    "reason",
    "occurrence",
];

const TODO_DESCRIPTION: &str = "\
`todo` NEVER takes a turn of its own: emit it in the same response as your next real tool call (parallel tool calls), `init` beside the first read or action, `done`/`start` beside the next action or check. A response whose only call is `todo` wastes a full model round trip.
Tasks identified by verbatim content, NEVER generated IDs (task-1). Unique, stable task/phase names; lost text: view, NEVER guess.
Before work, init for 3+ steps, requested task sets, or new instructions. MUST list EVERY user item separately (phased/numbered/bulleted/N); NEVER omit or remember leftovers.
After successful mutation: no active means earliest pending starts (phase order); multiple active means only earliest stays. Blocked NEVER starts automatically; unblock returns pending. Done out of order may rewind pointer but NEVER reopen completed. Mark done immediately; follow phase order.
External waits (user/agent/service): block with optional reason suppresses stop reminder, starts next pending. Unblock when actionable; append a clearing task for agent-actionable blocker.";

// =============================================================================
// Models
// =============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TodoStatus {
    Pending,
    InProgress,
    Completed,
    Abandoned,
    Blocked,
}

impl TodoStatus {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
            Self::Abandoned => "abandoned",
            Self::Blocked => "blocked",
        }
    }

    /// Closed = completed + abandoned, mirroring upstream closed/total counts.
    fn is_closed(self) -> bool {
        matches!(self, Self::Completed | Self::Abandoned)
    }

    /// Open = pending + in_progress, the only statuses counted as remaining.
    fn is_open(self) -> bool {
        matches!(self, Self::Pending | Self::InProgress)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TodoItem {
    pub(crate) content: String,
    pub(crate) status: TodoStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) blocker: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TodoPhase {
    pub(crate) name: String,
    pub(crate) tasks: Vec<TodoItem>,
}

/// Effect of one successful todo call, installed on the workflow at the batch
/// commit boundary. `plan: None` marks a standalone list (or detaching from a
/// finished plan); `Some` marks a plan projection, with `content` present only
/// when the plan Markdown bytes changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TodoChange {
    pub(crate) phases: Vec<TodoPhase>,
    pub(crate) plan: Option<PlanTodoChange>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlanTodoChange {
    pub(crate) plan_id: String,
    pub(crate) revision: u64,
    pub(crate) content: Option<String>,
}

/// Markdown 是任务及勾选的权威；仅在来源及批准基线相同时保留按序号的运行态。
pub(crate) fn project_plan_todos(
    document: &crate::plan::PlanDocument,
    previous: &[TodoPhase],
    preserve_runtime: bool,
) -> anyhow::Result<Vec<TodoPhase>> {
    use crate::plan::{PlanStatus, plan_tasks};
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

// =============================================================================
// Definitions
// =============================================================================

pub(super) fn definition() -> FunctionObject {
    FunctionObject {
        name: TOOL_NAME.into(),
        description: Some(TODO_DESCRIPTION.into()),
        parameters: Some(schema_with_ops(OPERATIONS)),
        ..Default::default()
    }
}

/// Read-only plan preview: same tool name, `op` restricted to `view`.
pub(super) fn preview_definition() -> FunctionObject {
    FunctionObject {
        name: TOOL_NAME.into(),
        description: Some(
            "Read-only preview of the current plan's task checklist. Only {\"op\":\"view\"} is accepted; state-changing operations are unavailable until the plan is approved and executing.".into(),
        ),
        parameters: Some(schema_with_ops(&["view"])),
        ..Default::default()
    }
}

fn schema_with_ops(ops: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": {
            "op": {
                "type": "string",
                "enum": ops,
                "description": "todo operation",
            },
            "list": {
                "type": "array",
                "description": "phases for init",
                "items": {
                    "type": "object",
                    "properties": {
                        "phase": {"type": "string"},
                        "items": {"type": "array", "items": {"type": "string"}, "minItems": 1},
                    },
                    "required": ["phase", "items"],
                    "additionalProperties": false,
                },
            },
            "task": {"type": "string", "description": "verbatim task content"},
            "phase": {"type": "string"},
            "items": {
                "type": "array",
                "items": {"type": "string"},
                "description": "tasks for flat init or append",
            },
            "reason": {"type": "string", "description": "blocker note for block"},
            "occurrence": {
                "type": "integer",
                "minimum": 1,
                "description": "one-based pick among identical task texts; only plan tasks support occurrence",
            },
        },
        "required": ["op"],
        "additionalProperties": false,
    })
}

// =============================================================================
// Argument parsing
// =============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TodoOperation {
    Init,
    Start,
    Done,
    Rm,
    Drop,
    Block,
    Unblock,
    Append,
    View,
}

impl TodoOperation {
    pub(crate) fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "init" => Self::Init,
            "start" => Self::Start,
            "done" => Self::Done,
            "rm" => Self::Rm,
            "drop" => Self::Drop,
            "block" => Self::Block,
            "unblock" => Self::Unblock,
            "append" => Self::Append,
            "view" => Self::View,
            _ => return None,
        })
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Init => "init",
            Self::Start => "start",
            Self::Done => "done",
            Self::Rm => "rm",
            Self::Drop => "drop",
            Self::Block => "block",
            Self::Unblock => "unblock",
            Self::Append => "append",
            Self::View => "view",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InitEntry {
    pub(crate) phase: String,
    pub(crate) items: Vec<String>,
}

#[derive(Debug)]
pub(crate) struct TodoParams {
    pub(crate) op: TodoOperation,
    pub(crate) list: Option<Vec<InitEntry>>,
    pub(crate) task: Option<String>,
    pub(crate) phase: Option<String>,
    pub(crate) items: Option<Vec<String>>,
    pub(crate) reason: Option<String>,
    pub(crate) occurrence: Option<u64>,
}

/// Validate execute-time arguments, repairing an omitted `op`. Unknown fields,
/// explicit nulls and wrong types are rejected; the only repair is a missing
/// `op` alongside an unambiguous payload (models routinely send `{list:[...]}`
/// with no op). Reused by the plan adapter for identical argument semantics.
pub(crate) fn resolve_params(raw: &str, has_existing: bool) -> Result<TodoParams, String> {
    let value: Value = match serde_json::from_str(raw) {
        Ok(value) => value,
        Err(error) => return Err(format!("Invalid todo arguments: {error}")),
    };
    let Some(map) = value.as_object() else {
        return Err("Invalid todo arguments: expected a single todo operation object.".into());
    };
    let mut problems: Vec<String> = Vec::new();
    for key in map.keys() {
        if !ALLOWED_FIELDS.contains(&key.as_str()) {
            problems.push(format!("unknown field \"{key}\""));
        }
    }
    let task = strict_string(map, "task", &mut problems);
    let phase = strict_string(map, "phase", &mut problems);
    let reason = strict_string(map, "reason", &mut problems);
    let items = strict_string_array(map, "items", &mut problems);
    let list = strict_list(map, &mut problems);
    let occurrence = strict_occurrence(map, &mut problems);

    let op = match map.get("op") {
        Some(Value::String(s)) => match TodoOperation::parse(s) {
            Some(op) => Some(op),
            None => {
                problems.push(format!(
                    "field \"op\" must be one of {}; got \"{s}\"",
                    OPERATIONS.join(", ")
                ));
                None
            }
        },
        // An explicit null op is not "missing": no inference repair.
        Some(Value::Null) => {
            problems.push("field \"op\" must not be null".into());
            None
        }
        Some(_) => {
            problems.push("field \"op\" must be a string".into());
            None
        }
        None => infer_todo_op(map, has_existing),
    };

    let Some(op) = op else {
        if problems.is_empty() {
            problems.push("missing field \"op\"; no unambiguous payload to infer it from".into());
        }
        return Err(invalid_arguments(&problems));
    };
    if !problems.is_empty() {
        return Err(invalid_arguments(&problems));
    }
    Ok(TodoParams {
        op,
        list,
        task,
        phase,
        items,
        reason,
        occurrence,
    })
}

fn invalid_arguments(problems: &[String]) -> String {
    format!("Invalid todo arguments: {}", problems.join("; "))
}

/// Only unambiguous shapes are inferred: non-empty `list` → `init`;
/// non-empty `items` + non-empty `phase` → `append`; bare non-empty `items`
/// with no existing todos → `init`. Targeting args alone stay an error.
pub(crate) fn infer_todo_op(map: &Map<String, Value>, has_existing: bool) -> Option<TodoOperation> {
    if matches!(map.get("list"), Some(Value::Array(entries)) if !entries.is_empty()) {
        return Some(TodoOperation::Init);
    }
    if let Some(Value::Array(items)) = map.get("items")
        && !items.is_empty()
    {
        if matches!(map.get("phase"), Some(Value::String(phase)) if !phase.is_empty()) {
            return Some(TodoOperation::Append);
        }
        if !has_existing {
            return Some(TodoOperation::Init);
        }
    }
    None
}

fn strict_string(
    map: &Map<String, Value>,
    key: &str,
    problems: &mut Vec<String>,
) -> Option<String> {
    match map.get(key) {
        None => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Null) => {
            problems.push(format!("field \"{key}\" must not be null"));
            None
        }
        Some(_) => {
            problems.push(format!("field \"{key}\" must be a string"));
            None
        }
    }
}

fn strict_string_array(
    map: &Map<String, Value>,
    key: &str,
    problems: &mut Vec<String>,
) -> Option<Vec<String>> {
    match map.get(key) {
        None => None,
        Some(Value::Array(entries)) => {
            let mut out = Vec::with_capacity(entries.len());
            for (index, entry) in entries.iter().enumerate() {
                match entry.as_str() {
                    Some(s) => out.push(s.to_owned()),
                    None => {
                        problems.push(format!(
                            "field \"{key}\" must contain only strings (item {index})"
                        ));
                        return None;
                    }
                }
            }
            Some(out)
        }
        Some(Value::Null) => {
            problems.push(format!("field \"{key}\" must not be null"));
            None
        }
        Some(_) => {
            problems.push(format!("field \"{key}\" must be an array of strings"));
            None
        }
    }
}

fn strict_list(map: &Map<String, Value>, problems: &mut Vec<String>) -> Option<Vec<InitEntry>> {
    let Some(Value::Array(entries)) = map.get("list") else {
        if map.contains_key("list") {
            if map["list"].is_null() {
                problems.push("field \"list\" must not be null".into());
            } else {
                problems.push("field \"list\" must be an array of {phase, items} objects".into());
            }
        }
        return None;
    };
    let before = problems.len();
    let mut list = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        let Some(obj) = entry.as_object() else {
            problems.push(format!("list[{index}] must be an object"));
            continue;
        };
        for key in obj.keys() {
            if key != "phase" && key != "items" {
                problems.push(format!("list[{index}] has unknown field \"{key}\""));
            }
        }
        let phase = match obj.get("phase") {
            Some(Value::String(s)) => Some(s.clone()),
            Some(Value::Null) => {
                problems.push(format!("list[{index}].phase must not be null"));
                None
            }
            Some(_) => {
                problems.push(format!("list[{index}].phase must be a string"));
                None
            }
            None => {
                problems.push(format!("list[{index}] is missing field \"phase\""));
                None
            }
        };
        let items = match obj.get("items") {
            Some(Value::Array(items)) => {
                let mut out = Vec::with_capacity(items.len());
                let mut ok = true;
                for (item_index, item) in items.iter().enumerate() {
                    match item.as_str() {
                        Some(s) => out.push(s.to_owned()),
                        None => {
                            problems.push(format!(
                                "list[{index}].items must contain only strings (item {item_index})"
                            ));
                            ok = false;
                            break;
                        }
                    }
                }
                if ok && out.is_empty() {
                    problems.push(format!(
                        "list[{index}].items must contain at least one string"
                    ));
                    None
                } else if ok {
                    Some(out)
                } else {
                    None
                }
            }
            Some(Value::Null) => {
                problems.push(format!("list[{index}].items must not be null"));
                None
            }
            Some(_) => {
                problems.push(format!("list[{index}].items must be an array of strings"));
                None
            }
            None => {
                problems.push(format!("list[{index}] is missing field \"items\""));
                None
            }
        };
        if let (Some(phase), Some(items)) = (phase, items) {
            list.push(InitEntry { phase, items });
        }
    }
    if problems.len() > before {
        return None;
    }
    Some(list)
}

fn strict_occurrence(map: &Map<String, Value>, problems: &mut Vec<String>) -> Option<u64> {
    match map.get("occurrence") {
        None => None,
        Some(Value::Number(n)) if n.is_u64() => match n.as_u64() {
            Some(v @ 1..) => Some(v),
            _ => {
                problems.push("field \"occurrence\" must be an integer >= 1".into());
                None
            }
        },
        Some(Value::Null) => {
            problems.push("field \"occurrence\" must not be null".into());
            None
        }
        Some(_) => {
            problems.push("field \"occurrence\" must be an integer >= 1".into());
            None
        }
    }
}

// =============================================================================
// Target location (ordinal-based)
// =============================================================================

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
fn resolve(current: &[TodoPhase], params: &TodoParams) -> Result<Operation, Vec<String>> {
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
            let Some(target) = resolve_task(current, params.task.as_deref(), &mut errors) else {
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

/// `start` must name one task; missing or empty content is "Missing task content".
fn resolve_task(
    current: &[TodoPhase],
    content: Option<&str>,
    errors: &mut Vec<String>,
) -> Option<TaskOrdinal> {
    let Some(content) = content.filter(|c| !c.is_empty()) else {
        errors.push("Missing task content".into());
        return None;
    };
    locate_task(current, content, errors)
}

fn normalize_reason(raw: Option<&str>) -> Option<String> {
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

// =============================================================================
// State transformation
// =============================================================================

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
            for (phase_index, phase) in next.iter_mut().enumerate() {
                for (task_index, task) in phase.tasks.iter_mut().enumerate() {
                    if task.status == TodoStatus::InProgress
                        && (phase_index, task_index) != (target.phase, target.task)
                    {
                        task.status = TodoStatus::Pending;
                    }
                }
            }
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

/// After every non-view success: keep only the earliest active; with none,
/// activate the earliest pending. Blocked never starts automatically and
/// completed is never re-opened.
pub(crate) fn normalize_in_progress(phases: &mut [TodoPhase]) {
    if phases.iter().all(|phase| phase.tasks.is_empty()) {
        return;
    }
    let mut first_active: Option<(usize, usize)> = None;
    let mut first_pending: Option<(usize, usize)> = None;
    for (phase_index, phase) in phases.iter().enumerate() {
        for (task_index, task) in phase.tasks.iter().enumerate() {
            match task.status {
                TodoStatus::InProgress if first_active.is_none() => {
                    first_active = Some((phase_index, task_index))
                }
                TodoStatus::Pending if first_pending.is_none() => {
                    first_pending = Some((phase_index, task_index))
                }
                _ => {}
            }
        }
    }
    match first_active {
        Some((active_phase, active_task)) => {
            for (phase_index, phase) in phases.iter_mut().enumerate() {
                for (task_index, task) in phase.tasks.iter_mut().enumerate() {
                    if task.status == TodoStatus::InProgress
                        && (phase_index, task_index) != (active_phase, active_task)
                    {
                        task.status = TodoStatus::Pending;
                    }
                }
            }
        }
        None => {
            if let Some((phase_index, task_index)) = first_pending {
                phases[phase_index].tasks[task_index].status = TodoStatus::InProgress;
            }
        }
    }
}

// =============================================================================
// Summary
// =============================================================================

pub(crate) fn format_summary(phases: &[TodoPhase], errors: &[String], read_only: bool) -> String {
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

    // The active phase is the EARLIEST one still holding open work, so the
    // in-progress pointer can sit in a phase whose successors already have
    // completed tasks.
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
            " — earliest phase with open tasks; the in-progress pointer auto-advances to the earliest open task on each completion, so it can sit behind out-of-order work (nothing was un-completed)."
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

// =============================================================================
// Tool entry
// =============================================================================

/// Standalone todo entry: parses arguments against the strict schema, resolves
/// targets by verbatim content, and transforms a single transaction snapshot.
/// Success returns `{ok,op,content,phases}` plus the effect; any failure
/// returns `{ok:false,error,op?,content,phases}` with the pre-call state and
/// discards every modification.
pub(crate) fn run(current: &[TodoPhase], arguments: &str) -> (Value, Option<TodoChange>) {
    let has_existing = !current.is_empty();
    let params = match resolve_params(arguments, has_existing) {
        Ok(params) => params,
        Err(message) => {
            // Schema failures echo the error text, with op attached when the
            // raw arguments name a known operation.
            let op = identifiable_op(arguments);
            return (failure_value(current, op, message.clone(), message), None);
        }
    };
    if params.occurrence.is_some() {
        let errors = vec!["occurrence is only available for plan tasks.".to_owned()];
        let content = format_summary(current, &errors, params.op == TodoOperation::View);
        return (
            failure_value(
                current,
                Some(params.op.as_str()),
                errors.join("; "),
                content,
            ),
            None,
        );
    }
    let operation = match resolve(current, &params) {
        Ok(operation) => operation,
        Err(errors) => {
            let content = format_summary(current, &errors, params.op == TodoOperation::View);
            return (
                failure_value(
                    current,
                    Some(params.op.as_str()),
                    errors.join("; "),
                    content,
                ),
                None,
            );
        }
    };
    if operation.is_view() {
        // Pure reads: no normalization, no clone, no change.
        let content = format_summary(current, &[], true);
        let phases = serde_json::to_value(current).unwrap_or_default();
        return (
            json!({"ok": true, "op": "view", "content": content, "phases": phases}),
            None,
        );
    }

    let mut next = apply_operation(current, &operation);
    normalize_in_progress(&mut next);
    let content = format_summary(&next, &[], false);
    let phases = serde_json::to_value(&next).unwrap_or_default();
    let change = TodoChange {
        phases: next,
        plan: None,
    };
    (
        json!({"ok": true, "op": params.op.as_str(), "content": content, "phases": phases}),
        Some(change),
    )
}

fn identifiable_op(arguments: &str) -> Option<&'static str> {
    let value: Value = serde_json::from_str(arguments).ok()?;
    value
        .get("op")?
        .as_str()
        .and_then(TodoOperation::parse)
        .map(TodoOperation::as_str)
}

/// pp result JSON for a failed call: pre-call state, error text, and op when
/// identifiable. Shared with the plan adapter for identical failure shapes.
pub(crate) fn failure_value(
    current: &[TodoPhase],
    op: Option<&'static str>,
    error: String,
    content: String,
) -> Value {
    let mut value = json!({
        "ok": false,
        "error": error,
        "content": content,
        "phases": serde_json::to_value(current).unwrap_or_default(),
    });
    if let Some(op) = op {
        value["op"] = Value::String(op.into());
    }
    value
}

/// pp result JSON for a successful call: `{ok,op,content,phases}`. The plan
/// adapter layers `plan` info on top of this shape.
pub(crate) fn success_value(op: &str, content: String, phases: &[TodoPhase]) -> Value {
    json!({
        "ok": true,
        "op": op,
        "content": content,
        "phases": serde_json::to_value(phases).unwrap_or_default(),
    })
}

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
pub(crate) fn run_plan(
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
pub(crate) fn run_readonly(
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
fn plan_task_targets(
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

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_todo_target_limits_apply_after_phase_filter_even_for_unique_tasks() {
        let current = phases(&[
            ("First", &[("same", TodoStatus::InProgress)]),
            (
                "Second",
                &[
                    ("same", TodoStatus::Pending),
                    ("unique", TodoStatus::Pending),
                ],
            ),
        ]);
        for args in [
            r#"{"op":"done","task":"unique","occurrence":2}"#,
            r#"{"op":"done","task":"unique","phase":"First"}"#,
            r#"{"op":"done","task":"same","phase":"First","occurrence":2}"#,
            r#"{"op":"done","task":""}"#,
        ] {
            let params = resolve_params(args, true).unwrap();
            let mut errors = Vec::new();
            assert!(plan_task_targets(&current, &params, &mut errors).is_empty());
            assert!(
                errors.iter().any(|error| error.contains("not found")),
                "{errors:?}"
            );
        }
        let params = resolve_params(
            r#"{"op":"done","task":"same","phase":"Second","occurrence":1}"#,
            true,
        )
        .unwrap();
        let mut errors = Vec::new();
        assert_eq!(
            plan_task_targets(&current, &params, &mut errors),
            vec![TaskOrdinal { phase: 1, task: 0 }]
        );
        assert!(errors.is_empty());
    }

    fn item(content: &str, status: TodoStatus) -> TodoItem {
        TodoItem {
            content: content.to_owned(),
            status,
            blocker: None,
        }
    }

    fn phase(name: &str, tasks: &[(&str, TodoStatus)]) -> TodoPhase {
        TodoPhase {
            name: name.to_owned(),
            tasks: tasks
                .iter()
                .map(|(content, status)| item(content, *status))
                .collect(),
        }
    }

    fn phases(list: &[(&str, &[(&str, TodoStatus)])]) -> Vec<TodoPhase> {
        list.iter()
            .map(|(name, tasks)| phase(name, tasks))
            .collect()
    }

    fn init_args(entries: &[(&str, &[&str])]) -> String {
        let list: Vec<Value> = entries
            .iter()
            .map(|(name, items)| json!({"phase": name, "items": items}))
            .collect();
        json!({"op": "init", "list": list}).to_string()
    }

    /// init with fresh state; first task of the first phase becomes active.
    fn inited(entries: &[(&str, &[&str])]) -> Vec<TodoPhase> {
        let (value, change) = run(&[], &init_args(entries));
        assert_eq!(value["ok"], json!(true), "{value}");
        change.expect("init must return a change").phases
    }

    fn status_of(phases: &[TodoPhase], content: &str) -> TodoStatus {
        phases
            .iter()
            .flat_map(|phase| phase.tasks.iter())
            .find(|task| task.content == content)
            .unwrap_or_else(|| panic!("task {content} missing"))
            .status
    }

    fn blocked_reason(phases: &[TodoPhase], content: &str) -> Option<String> {
        phases
            .iter()
            .flat_map(|phase| phase.tasks.iter())
            .find(|task| task.content == content)
            .unwrap_or_else(|| panic!("task {content} missing"))
            .blocker
            .clone()
    }

    #[test]
    fn todo_init_activates_first_task_and_reports_change() {
        let (value, change) = run(
            &[],
            &init_args(&[("First", &["A", "B"]), ("Second", &["C"])]),
        );
        assert_eq!(value["ok"], json!(true));
        assert_eq!(value["op"], json!("init"));
        assert!(
            value["content"]
                .as_str()
                .unwrap()
                .contains("Remaining items (3):")
        );
        let change = change.expect("init must return a change");
        assert_eq!(
            value["phases"],
            serde_json::to_value(&change.phases).unwrap()
        );
        assert_eq!(status_of(&change.phases, "A"), TodoStatus::InProgress);
        assert_eq!(status_of(&change.phases, "B"), TodoStatus::Pending);
        assert_eq!(status_of(&change.phases, "C"), TodoStatus::Pending);
    }

    #[test]
    fn todo_done_keeps_active_until_it_finishes_then_advances() {
        let current = inited(&[("First", &["A", "B"]), ("Second", &["C"])]);
        let (value, change) = run(&current, r#"{"op":"done","task":"B"}"#);
        assert_eq!(value["ok"], json!(true));
        let next = change.unwrap().phases;
        assert_eq!(status_of(&next, "B"), TodoStatus::Completed);
        assert_eq!(status_of(&next, "A"), TodoStatus::InProgress);

        let (value, change) = run(&next, r#"{"op":"done","task":"A"}"#);
        assert_eq!(value["ok"], json!(true));
        let next = change.unwrap().phases;
        assert_eq!(status_of(&next, "A"), TodoStatus::Completed);
        assert_eq!(status_of(&next, "B"), TodoStatus::Completed);
        // No active left: earliest pending starts; completed stays closed.
        assert_eq!(status_of(&next, "C"), TodoStatus::InProgress);
    }

    #[test]
    fn todo_done_without_target_completes_everything() {
        let current = inited(&[("First", &["A"]), ("Second", &["C"])]);
        let (_, change) = run(&current, r#"{"op":"done"}"#);
        let next = change.unwrap().phases;
        assert_eq!(status_of(&next, "A"), TodoStatus::Completed);
        assert_eq!(status_of(&next, "C"), TodoStatus::Completed);
    }

    #[test]
    fn todo_done_phase_targets_whole_phase() {
        let current = inited(&[("First", &["A", "B"]), ("Second", &["C"])]);
        let (_, change) = run(&current, r#"{"op":"done","phase":"First"}"#);
        let next = change.unwrap().phases;
        assert_eq!(status_of(&next, "A"), TodoStatus::Completed);
        assert_eq!(status_of(&next, "B"), TodoStatus::Completed);
        assert_eq!(status_of(&next, "C"), TodoStatus::InProgress);
    }

    #[test]
    fn todo_start_moves_other_active_to_pending_and_reopens() {
        let current = inited(&[("First", &["A", "B"]), ("Second", &["C"])]);
        let (_, change) = run(&current, r#"{"op":"start","task":"C"}"#);
        let next = change.unwrap().phases;
        assert_eq!(status_of(&next, "C"), TodoStatus::InProgress);
        assert_eq!(status_of(&next, "A"), TodoStatus::Pending);

        // Explicit re-open of a closed task is allowed upstream.
        let (_, change) = run(&next, r#"{"op":"done","task":"C"}"#);
        let next = change.unwrap().phases;
        let (_, change) = run(&next, r#"{"op":"start","task":"C"}"#);
        let next = change.unwrap().phases;
        assert_eq!(status_of(&next, "C"), TodoStatus::InProgress);
    }

    #[test]
    fn todo_start_requires_single_task_target() {
        let current = inited(&[("First", &["A"])]);
        for arguments in [r#"{"op":"start"}"#, r#"{"op":"start","phase":"First"}"#] {
            let (value, change) = run(&current, arguments);
            assert_eq!(value["ok"], json!(false), "{value}");
            assert!(
                value["error"]
                    .as_str()
                    .unwrap()
                    .contains("Missing task content")
            );
            assert!(change.is_none());
        }
    }

    #[test]
    fn todo_drop_abandons_and_counts_closed() {
        let current = inited(&[("First", &["A", "B"])]);
        let (value, change) = run(&current, r#"{"op":"drop","task":"B"}"#);
        assert_eq!(value["ok"], json!(true));
        let next = change.unwrap().phases;
        assert_eq!(status_of(&next, "B"), TodoStatus::Abandoned);
        let content = value["content"].as_str().unwrap();
        assert!(content.contains("Overall: 1/2 done, 1 open."));
        assert!(content.contains("    - [ ] B (dropped)"));
    }

    #[test]
    fn todo_rm_clears_tasks_but_keeps_phase_objects() {
        let current = inited(&[("First", &["A", "B"]), ("Second", &["C"])]);
        let (_, change) = run(&current, r#"{"op":"rm","phase":"First"}"#);
        let next = change.unwrap().phases;
        assert_eq!(next.len(), 2);
        assert!(next[0].tasks.is_empty());
        assert_eq!(next[1].tasks.len(), 1);

        let (_, change) = run(&next, r#"{"op":"rm"}"#);
        let next = change.unwrap().phases;
        assert_eq!(next.len(), 2);
        assert!(next.iter().all(|phase| phase.tasks.is_empty()));
        // Empty after mutation reads as cleared.
        assert_eq!(next.len(), 2);
        let (value, change) = run(&next, r#"{"op":"view"}"#);
        assert_eq!(value["content"], json!("Todo list is empty."));
        assert!(change.is_none());
    }

    #[test]
    fn todo_block_rules() {
        let current = inited(&[("Tasks", &["A", "B"])]);
        // Whitespace runs collapse; the blocker note rides on one line.
        let (value, change) = run(
            &current,
            r#"{"op":"block","task":"B","reason":"wait\n  on\tx"}"#,
        );
        assert_eq!(value["ok"], json!(true));
        let next = change.unwrap().phases;
        assert_eq!(status_of(&next, "B"), TodoStatus::Blocked);
        assert_eq!(blocked_reason(&next, "B").as_deref(), Some("wait on x"));
        // Active task untouched by blocking another.
        assert_eq!(status_of(&next, "A"), TodoStatus::InProgress);

        // Completing the active task does not auto-start the blocked one.
        let (_, change) = run(&next, r#"{"op":"done","task":"A"}"#);
        let next = change.unwrap().phases;
        assert_eq!(status_of(&next, "A"), TodoStatus::Completed);
        assert_eq!(status_of(&next, "B"), TodoStatus::Blocked);

        // Re-blocking a blocked task refines its note.
        let (_, change) = run(
            &next,
            r#"{"op":"block","task":"B","reason":"still waiting"}"#,
        );
        let next = change.unwrap().phases;
        assert_eq!(blocked_reason(&next, "B").as_deref(), Some("still waiting"));

        // Blocking without a reason clears an existing note (upstream keeps).
        let (_, change) = run(&next, r#"{"op":"block","task":"B"}"#);
        let next = change.unwrap().phases;
        assert_eq!(blocked_reason(&next, "B"), None);

        // Unblock returns pending and the cleared task becomes active.
        let (_, change) = run(&next, r#"{"op":"unblock","task":"B"}"#);
        let next = change.unwrap().phases;
        assert_eq!(status_of(&next, "B"), TodoStatus::InProgress);
        assert_eq!(blocked_reason(&next, "B"), None);
    }

    #[test]
    fn todo_block_phase_never_reopens_closed_tasks() {
        let current = inited(&[("First", &["A", "B"])]);
        let (_, change) = run(&current, r#"{"op":"done","task":"A"}"#);
        let next = change.unwrap().phases;
        let (_, change) = run(&next, r#"{"op":"block","phase":"First","reason":"halt"}"#);
        let next = change.unwrap().phases;
        assert_eq!(status_of(&next, "A"), TodoStatus::Completed);
        assert_eq!(status_of(&next, "B"), TodoStatus::Blocked);
    }

    #[test]
    fn todo_unblock_touches_only_blocked_tasks() {
        let current = phases(&[(
            "Tasks",
            &[("A", TodoStatus::InProgress), ("B", TodoStatus::Blocked)],
        )]);
        let (_, change) = run(&current, r#"{"op":"unblock","phase":"Tasks"}"#);
        let next = change.unwrap().phases;
        assert_eq!(status_of(&next, "A"), TodoStatus::InProgress);
        assert_eq!(status_of(&next, "B"), TodoStatus::Pending);
        assert_eq!(blocked_reason(&next, "B"), None);
    }

    #[test]
    fn todo_block_unblock_require_target() {
        let current = inited(&[("Tasks", &["A"])]);
        for arguments in [r#"{"op":"block"}"#, r#"{"op":"unblock"}"#] {
            let (value, change) = run(&current, arguments);
            assert_eq!(value["ok"], json!(false), "{value}");
            assert!(
                value["error"]
                    .as_str()
                    .unwrap()
                    .ends_with("requires a task or phase target")
            );
            assert!(change.is_none());
        }
    }

    #[test]
    fn todo_append_creates_phase_and_validates_batch() {
        let current = inited(&[("First", &["A"])]);
        let (_, change) = run(
            &current,
            r#"{"op":"append","phase":"Second","items":["C","D"]}"#,
        );
        let next = change.unwrap().phases;
        assert_eq!(next.len(), 2);
        assert_eq!(next[1].name, "Second");
        assert_eq!(next[1].tasks.len(), 2);

        // In-batch duplicate: whole op rejected, nothing half-applied.
        let (value, change) = run(
            &next,
            r#"{"op":"append","phase":"Third","items":["X","X"]}"#,
        );
        assert_eq!(value["ok"], json!(false));
        assert!(
            value["error"]
                .as_str()
                .unwrap()
                .contains("Task \"X\" already exists")
        );
        assert!(change.is_none());

        // Existing duplicate likewise.
        let (value, change) = run(
            &next,
            r#"{"op":"append","phase":"Third","items":["X","A"]}"#,
        );
        assert_eq!(value["ok"], json!(false));
        assert!(
            value["error"]
                .as_str()
                .unwrap()
                .contains("Task \"A\" already exists")
        );
        assert!(change.is_none());
    }

    #[test]
    fn todo_init_rejects_duplicates_and_keeps_previous_state() {
        let current = inited(&[("First", &["A"])]);
        for arguments in [
            r#"{"op":"init","list":[{"phase":"P","items":["a"]},{"phase":"P","items":["b"]}]}"#,
            r#"{"op":"init","list":[{"phase":"P","items":["a"]},{"phase":"Q","items":["a"]}]}"#,
        ] {
            let (value, change) = run(&current, arguments);
            assert_eq!(value["ok"], json!(false), "{value}");
            assert!(value["error"].as_str().unwrap().contains("Duplicate"));
            assert!(change.is_none());
            assert_eq!(value["phases"], serde_json::to_value(&current).unwrap());
        }
    }

    #[test]
    fn todo_init_with_empty_list_clears() {
        let current = inited(&[("First", &["A"])]);
        let (value, change) = run(&current, r#"{"op":"init","list":[]}"#);
        assert_eq!(value["ok"], json!(true));
        assert_eq!(value["content"], json!("Todo list cleared."));
        let change = change.expect("init must return a change");
        assert!(change.phases.is_empty());
    }

    #[test]
    fn todo_init_missing_list_fails_and_keeps_old_list() {
        let current = inited(&[("First", &["A"])]);
        let (value, change) = run(&current, r#"{"op":"init"}"#);
        assert_eq!(value["ok"], json!(false));
        assert_eq!(value["error"], json!("Missing list for init operation"));
        assert!(change.is_none());
        assert_eq!(value["phases"], serde_json::to_value(&current).unwrap());
    }

    #[test]
    fn todo_flat_init_targets_given_phase_or_tasks_default() {
        let (value, change) = run(&[], r#"{"op":"init","items":["A","B"],"phase":"Custom"}"#);
        assert_eq!(value["ok"], json!(true));
        let next = change.unwrap().phases;
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].name, "Custom");

        let (value, change) = run(&[], r#"{"op":"init","items":["A"]}"#);
        assert_eq!(value["ok"], json!(true));
        let next = change.unwrap().phases;
        assert_eq!(next[0].name, "Tasks");
    }

    #[test]
    fn todo_infer_missing_op() {
        // Non-empty list → init.
        let (value, _) = run(&[], r#"{"list":[{"phase":"P","items":["a"]}]}"#);
        assert_eq!(value["op"], json!("init"));
        // Bare items with nothing existing → init into Tasks.
        let (value, change) = run(&[], r#"{"items":["a"]}"#);
        assert_eq!(value["op"], json!("init"));
        assert_eq!(change.unwrap().phases[0].name, "Tasks");
        // Items + phase with existing phases → append.
        let current = inited(&[("P", &["a"])]);
        let (value, change) = run(&current, r#"{"items":["b"],"phase":"P"}"#);
        assert_eq!(value["op"], json!("append"));
        assert_eq!(change.unwrap().phases[0].tasks.len(), 2);
        // Bare items with existing phases → no inference.
        let (value, _) = run(&current, r#"{"items":["b"]}"#);
        assert_eq!(value["ok"], json!(false));
        // Task-only never infers done.
        let (value, _) = run(&current, r#"{"task":"a"}"#);
        assert_eq!(value["ok"], json!(false));
        // Empty list is not an init payload.
        let (value, _) = run(&current, r#"{"list":[]}"#);
        assert_eq!(value["ok"], json!(false));
    }

    #[test]
    fn todo_schema_rejects_unknown_fields_nulls_and_types() {
        let current = inited(&[("P", &["a"])]);
        let cases = [
            (r#"{"op":"view","ops":[]}"#, "unknown field \"ops\""),
            (
                r#"{"op":"view","task":null}"#,
                "field \"task\" must not be null",
            ),
            (
                r#"{"op":"init","list":"x"}"#,
                "field \"list\" must be an array",
            ),
            (
                r#"{"op":"init","list":[{"phase":"P","items":[]}]}"#,
                "list[0].items must contain at least one string",
            ),
            (
                r#"{"op":"init","list":[{"phase":"P","items":["a"],"extra":1}]}"#,
                "list[0] has unknown field \"extra\"",
            ),
            (
                r#"{"op":"init","list":[{"items":["a"]}]}"#,
                "list[0] is missing field \"phase\"",
            ),
            (
                r#"{"op":"done","items":"x"}"#,
                "field \"items\" must be an array of strings",
            ),
            (
                r#"{"op":"done","occurrence":0}"#,
                "field \"occurrence\" must be an integer >= 1",
            ),
            (
                r#"{"op":"done","occurrence":1.5}"#,
                "field \"occurrence\" must be an integer >= 1",
            ),
            (
                r#"{"op":"done","occurrence":true}"#,
                "field \"occurrence\" must be an integer >= 1",
            ),
            (r#"{"op":"explode"}"#, "field \"op\" must be one of"),
            (r#"{"op":null}"#, "field \"op\" must not be null"),
            (r#"{"op":5}"#, "field \"op\" must be a string"),
            (r#"["op"]"#, "expected a single todo operation object"),
            (r#"not json"#, "Invalid todo arguments"),
        ];
        for (arguments, expected) in cases {
            let (value, change) = run(&current, arguments);
            assert_eq!(value["ok"], json!(false), "{arguments}: {value}");
            assert!(change.is_none(), "{arguments}");
            let error = value["error"].as_str().unwrap();
            assert!(error.contains(expected), "{arguments}: {error}");
            // Schema failures echo the error text as content.
            assert_eq!(value["content"], value["error"], "{arguments}");
            // Pre-call state survives.
            assert_eq!(
                value["phases"],
                serde_json::to_value(&current).unwrap(),
                "{arguments}"
            );
        }
        // Known op is attached when the raw arguments name one.
        let (value, _) = run(&current, r#"{"op":"view","task":null}"#);
        assert_eq!(value["op"], json!("view"));
        let (value, _) = run(&current, r#"{"op":"explode"}"#);
        assert!(value.get("op").is_none());
    }

    #[test]
    fn todo_occurrence_rejected_for_plain_tasks() {
        let current = inited(&[("P", &["a"])]);
        let (value, change) = run(&current, r#"{"op":"done","task":"a","occurrence":1}"#);
        assert_eq!(value["ok"], json!(false));
        assert_eq!(
            value["error"],
            json!("occurrence is only available for plan tasks.")
        );
        assert_eq!(value["op"], json!("done"));
        assert!(change.is_none());
        assert_eq!(value["phases"], serde_json::to_value(&current).unwrap());
    }

    #[test]
    fn todo_view_borrows_and_never_normalizes() {
        let current = phases(&[
            (
                "P",
                &[
                    ("A", TodoStatus::InProgress),
                    ("B", TodoStatus::InProgress),
                    ("C", TodoStatus::Completed),
                ],
            ),
            ("Q", &[("D", TodoStatus::Blocked)]),
        ]);
        let (value, change) = run(&current, r#"{"op":"view","task":"whatever"}"#);
        assert_eq!(value["ok"], json!(true));
        assert_eq!(value["op"], json!("view"));
        assert!(change.is_none());
        let next: Vec<TodoPhase> = serde_json::from_value(value["phases"].clone()).unwrap();
        assert_eq!(next, current);
        assert_eq!(status_of(&next, "B"), TodoStatus::InProgress);
        let content = value["content"].as_str().unwrap();
        assert!(content.contains("  - A [in_progress] (P)"));
        assert!(content.contains("  - B [in_progress] (P)"));
        // Blocked tasks are not "remaining"; they appear in the checklist only.
        assert!(!content.contains("  - D [blocked] (Q)"));
        assert!(content.contains("    - [ ] D (blocked)"));
    }

    #[test]
    fn todo_view_empty_list() {
        let (value, change) = run(&[], r#"{"op":"view"}"#);
        assert_eq!(value["content"], json!("Todo list is empty."));
        assert!(change.is_none());
    }

    #[test]
    fn todo_exact_matching_no_trim_or_folding() {
        let current = inited(&[("P", &["Fix bug"])]);
        for probe in ["fix bug", "Fix Bug", " fix bug", "Fix bug "] {
            let (value, change) = run(&current, &json!({"op": "done", "task": probe}).to_string());
            assert_eq!(value["ok"], json!(false), "{probe}");
            assert!(change.is_none());
            assert!(
                value["error"]
                    .as_str()
                    .unwrap()
                    .starts_with(&format!("Task \"{probe}\" not found"))
            );
        }
    }

    #[test]
    fn todo_task_id_misuse_diagnostic() {
        let current = inited(&[("P", &["a"])]);
        let (value, _) = run(&current, r#"{"op":"done","task":"task-1"}"#);
        assert_eq!(
            value["error"],
            json!(
                "Task \"task-1\" not found. Tasks are referenced by content, not by IDs — pass the task's full text from the previous result."
            )
        );
    }

    #[test]
    fn todo_not_found_hint_on_empty_list() {
        let (value, _) = run(&[], r#"{"op":"done","task":"x"}"#);
        assert_eq!(
            value["error"],
            json!(
                "Task \"x\" not found (todo list is empty — was it replaced or not yet created?)"
            )
        );
    }

    #[test]
    fn todo_phase_diagnostics() {
        let current = inited(&[("P", &["a"])]);
        // Upstream truthiness: an empty phase string selects nothing and
        // falls through to every task, so done succeeds with no error.
        let (value, change) = run(&current, r#"{"op":"done","phase":""}"#);
        assert_eq!(value["ok"], json!(true));
        assert_eq!(
            status_of(&change.unwrap().phases, "a"),
            TodoStatus::Completed
        );
        let (value, _) = run(&current, r#"{"op":"done","phase":"Nope"}"#);
        assert_eq!(value["error"], json!("Phase \"Nope\" not found"));
    }

    #[test]
    fn plan_todo_projection_preserves_ordinals_and_markdown_authority() {
        use crate::plan::{PlanDocument, PlanStatus};
        let mut document = PlanDocument::new().unwrap();
        document.content = "## Same\n- [ ] duplicate\n## Same\n- [ ] duplicate\n".into();
        let preview = project_plan_todos(&document, &[], false).unwrap();
        assert_eq!(preview.len(), 2);
        assert!(
            preview
                .iter()
                .flat_map(|p| &p.tasks)
                .all(|t| t.status == TodoStatus::Pending)
        );
        document.approved_tasks = Some(vec!["duplicate".into(), "duplicate".into()]);
        document.status = PlanStatus::Executed;
        let mut runtime = project_plan_todos(&document, &[], false).unwrap();
        assert_eq!(runtime[0].tasks[0].status, TodoStatus::InProgress);
        runtime[1].tasks[0].status = TodoStatus::Blocked;
        runtime[1].tasks[0].blocker = Some("waiting".into());
        document.content = "## Renamed\n- [x] duplicate\n## Same\n- [ ] duplicate\n".into();
        let merged = project_plan_todos(&document, &runtime, true).unwrap();
        assert_eq!(merged[0].name, "Renamed");
        assert_eq!(merged[0].tasks[0].status, TodoStatus::Completed);
        assert_eq!(merged[1].tasks[0].status, TodoStatus::Blocked);
        assert_eq!(merged[1].tasks[0].blocker.as_deref(), Some("waiting"));
        document.content = document.content.replace("[x]", "[ ]");
        let reopened = project_plan_todos(&document, &merged, true).unwrap();
        assert_eq!(reopened[0].tasks[0].status, TodoStatus::InProgress);
        let fresh = project_plan_todos(&document, &merged, false).unwrap();
        assert_eq!(fresh[1].tasks[0].status, TodoStatus::Pending);
        document.status = PlanStatus::Completed;
        let readonly = project_plan_todos(&document, &runtime, true).unwrap();
        assert!(
            readonly
                .iter()
                .flat_map(|p| &p.tasks)
                .all(|t| t.status == TodoStatus::Pending && t.blocker.is_none())
        );
        document.status = PlanStatus::Executed;
        document.content.push_str("- [ ] unapproved\n");
        assert_eq!(
            project_plan_todos(&document, &runtime, true)
                .unwrap_err()
                .to_string(),
            "Plan tasks changed; re-plan before completing."
        );
    }
}
