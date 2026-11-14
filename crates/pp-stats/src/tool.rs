use super::{Store, warn};
use anyhow::Result;
use serde_json::Value;
use std::path::Path;

/// 工具执行的采集状态；错误与耗时只在 runtime 实际执行工具后写入。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    Ok,
    Error,
    Cancelled,
}

impl ToolStatus {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Error => "error",
            Self::Cancelled => "cancelled",
        }
    }
}

/// 一条工具执行记录。`request_key` 关联发起该调用的模型请求，缺失时只能按工具聚合。
pub struct ToolCall<'a> {
    pub request_key: Option<&'a str>,
    pub session_file: &'a Path,
    pub call_id: &'a str,
    pub tool: &'a str,
    pub status: ToolStatus,
    pub error_message: Option<&'a str>,
    pub timestamp: i64,
    pub duration_ms: f64,
    /// 回填模型的结果文本字符数；历史或未采集结果为 None，不保存正文。
    pub result_chars: Option<u64>,
}

/// 工具调用参数的字符数：字符串按字符、null 为 0、其余按 JSON 文本，与旧聚合口径一致。
fn args_chars(args: &Value) -> u64 {
    match args {
        Value::String(args) => args.chars().count() as u64,
        Value::Null => 0,
        args => args.to_string().chars().count() as u64,
    }
}

/// 从原生模型输出中提取本请求发出的工具调用，不按 id 去重。
///
/// Chat 读 `tool_calls`；Responses 读输出数组（或 `output` 数组）里
/// `type == "function_call"` 的项。
pub(super) fn call_tools(output: &Value) -> Vec<(String, u64)> {
    if let Some(calls) = output["tool_calls"].as_array() {
        return calls
            .iter()
            .filter_map(|call| {
                let name = call["function"]["name"].as_str()?;
                Some((name.to_owned(), args_chars(&call["function"]["arguments"])))
            })
            .collect();
    }
    output
        .as_array()
        .or_else(|| output["output"].as_array())
        .into_iter()
        .flatten()
        .filter(|call| call["type"] == "function_call")
        .filter_map(|call| {
            let name = call["name"].as_str()?;
            Some((name.to_owned(), args_chars(&call["arguments"])))
        })
        .collect()
}

/// 从旧库的已提交会话条目快照里提取工具调用，仅在迁移时使用。
///
/// 与迁移前的聚合顺序一致：`message.content` 中 `type == "toolCall"` 的块优先；
/// [`maintain`] 在快照为空时回退到 [`call_tools`] 的模型输出。
pub(super) fn saved_tool_calls(entry: &Value) -> Vec<(String, u64)> {
    entry
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|entry| entry["message"]["content"].as_array().into_iter().flatten())
        .filter(|call| call["type"] == "toolCall")
        .filter_map(|call| {
            let name = call["name"].as_str()?;
            Some((name.to_owned(), args_chars(&call["arguments"])))
        })
        .collect()
}

/// 记录一次工具执行；存储失败只提示，不改变工具结果。
pub fn tool(database: &Path, call: &ToolCall<'_>) {
    if (|| -> Result<()> { Store::open(database)?.tool(call) })().is_err() {
        warn();
    }
}
