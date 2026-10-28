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

pub(super) mod plan_adapter;

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests;
