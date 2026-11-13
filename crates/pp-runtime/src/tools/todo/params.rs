//! `todo` 参数模型、严格解析与变更载荷。
use pp_core::todo::TodoPhase;
use serde_json::{Map, Value};

use super::schema::{ALLOWED_FIELDS, OPERATIONS};

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
