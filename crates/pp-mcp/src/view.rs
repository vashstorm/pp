//! 工具表与 `/mcp list` 快照：可用性过滤与来源状态。

use async_openai::types::chat::FunctionObject;
use serde_json::{Map, Value, json};
use std::sync::TryLockError;

use super::connect::exposed_name;
use super::{Access, EntryView, McpManager, RemoteTool, SharedServer, State, View};

const MAX_SUMMARY_DESC_CHARS: usize = 240;
const MAX_PROP_DESC_CHARS: usize = 120;

/// 供 `mcp_search` 返回的精简 MCP 工具摘要（避免把冗长完整 schema 一次塞进上下文）。
#[derive(Clone, Debug, serde::Serialize)]
pub struct ToolSummary {
    pub name: String,
    pub server: String,
    pub original_name: String,
    pub read_only: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters: Option<Value>,
}

impl McpManager {
    pub fn available_tool_count(&self, access: Access, allowed: impl Fn(&str) -> bool) -> usize {
        let state = self.lock_state();
        state
            .tools
            .values()
            .filter(|tool| {
                tool_available(&state, tool)
                    && (access == Access::Full || read_only_tool(&state, tool))
                    && allowed(&tool.definition.name)
            })
            .count()
    }

    /// 按关键词与可选服务名检索当前可访问的 MCP 工具，返回 `(总命中数, 当前页摘要)`。
    pub fn search_tools(
        &self,
        access: Access,
        query: Option<&str>,
        server_filter: Option<&str>,
        limit: usize,
        allowed: impl Fn(&str) -> bool,
    ) -> (usize, Vec<ToolSummary>) {
        let state = self.lock_state();
        let server_needle = server_filter
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_ascii_lowercase);
        let tokens: Vec<String> = query
            .unwrap_or("")
            .split_whitespace()
            .map(|t| t.to_ascii_lowercase())
            .filter(|t| !t.is_empty())
            .collect();
        let query_lower = query
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_ascii_lowercase);

        let mut matches: Vec<(u8, ToolSummary)> = state
            .tools
            .values()
            .filter(|tool| {
                tool_available(&state, tool)
                    && (access == Access::Full || read_only_tool(&state, tool))
                    && allowed(&tool.definition.name)
            })
            .filter(|tool| {
                if let Some(ref srv) = server_needle
                    && !tool.server.to_ascii_lowercase().contains(srv)
                {
                    return false;
                }
                if tokens.is_empty() {
                    return true;
                }
                let name_l = tool.definition.name.to_ascii_lowercase();
                let srv_l = tool.server.to_ascii_lowercase();
                let orig_l = tool.original_name.to_ascii_lowercase();
                let desc_l = tool
                    .definition
                    .description
                    .as_deref()
                    .unwrap_or("")
                    .to_ascii_lowercase();
                tokens.iter().all(|tok| {
                    name_l.contains(tok)
                        || srv_l.contains(tok)
                        || orig_l.contains(tok)
                        || desc_l.contains(tok)
                })
            })
            .map(|tool| {
                let rank = match query_lower.as_deref() {
                    Some(q)
                        if tool.definition.name.eq_ignore_ascii_case(q)
                            || tool.original_name.eq_ignore_ascii_case(q) =>
                    {
                        0
                    }
                    Some(q)
                        if tool.original_name.to_ascii_lowercase().starts_with(q)
                            || tool.server.to_ascii_lowercase().starts_with(q) =>
                    {
                        1
                    }
                    _ => 2,
                };
                let summary = ToolSummary {
                    name: tool.definition.name.clone(),
                    server: tool.server.clone(),
                    original_name: tool.original_name.clone(),
                    read_only: read_only_tool(&state, tool),
                    description: tool
                        .definition
                        .description
                        .as_deref()
                        .map(|d| truncate_chars(d.trim(), MAX_SUMMARY_DESC_CHARS))
                        .filter(|d| !d.is_empty()),
                    parameters: tool
                        .definition
                        .parameters
                        .as_ref()
                        .map(compact_parameters_schema),
                };
                (rank, summary)
            })
            .collect();

        matches.sort_by(|(r1, a), (r2, b)| r1.cmp(r2).then_with(|| a.name.cmp(&b.name)));
        let total = matches.len();
        let cap = limit.clamp(1, 50);
        let page = matches.into_iter().take(cap).map(|(_, s)| s).collect();
        (total, page)
    }

    /// 将 `mcp_call` 传入的工具名（支持完整 `mcp__server__tool`，或 `server` + `original_name`，
    /// 或全局唯一的 `original_name`）解析为规范暴露名 `mcp__*`。
    pub fn resolve_tool_name(&self, tool: &str, server: Option<&str>) -> Option<String> {
        let tool = tool.trim();
        if tool.is_empty() {
            return None;
        }
        let state = self.lock_state();
        if let Some(entry) = state.tools.get(tool)
            && tool_available(&state, entry)
        {
            if let Some(srv) = server.map(str::trim).filter(|s| !s.is_empty())
                && entry.server != srv
            {
                return None;
            }
            return Some(entry.definition.name.clone());
        }
        if let Some(srv) = server.map(str::trim).filter(|s| !s.is_empty()) {
            let candidate = exposed_name(srv, tool);
            if let Some(entry) = state.tools.get(&candidate)
                && tool_available(&state, entry)
                && entry.server == srv
            {
                return Some(entry.definition.name.clone());
            }
            return state
                .tools
                .values()
                .find(|entry| {
                    tool_available(&state, entry)
                        && entry.server == srv
                        && entry.original_name == tool
                })
                .map(|entry| entry.definition.name.clone());
        }
        let mut matched = state
            .tools
            .values()
            .filter(|entry| tool_available(&state, entry) && entry.original_name == tool);
        let first = matched.next()?;
        if matched.next().is_some() {
            // 多服务同名工具时要求显式指定 server 或完整 mcp__ 名称，避免误调用。
            return None;
        }
        Some(first.definition.name.clone())
    }

    pub fn has_available_tool(&self, name: &str) -> bool {
        let state = self.lock_state();
        state
            .tools
            .get(name)
            .is_some_and(|tool| tool_available(&state, tool))
    }

    pub fn is_read_only(&self, name: &str) -> bool {
        let state = self.lock_state();
        state
            .tools
            .get(name)
            .is_some_and(|tool| read_only_tool(&state, tool))
    }

    pub fn definitions(&self, access: Access) -> Vec<FunctionObject> {
        let state = self.lock_state();
        state
            .tools
            .values()
            .filter(|tool| {
                tool_available(&state, tool)
                    && (access == Access::Full || read_only_tool(&state, tool))
            })
            .map(|tool| tool.definition.clone())
            .collect()
    }

    pub fn tool_origins(
        &self,
        access: Access,
    ) -> std::collections::BTreeMap<String, (String, String)> {
        let state = self.lock_state();
        state
            .tools
            .values()
            .filter(|tool| {
                tool_available(&state, tool)
                    && (access == Access::Full || read_only_tool(&state, tool))
            })
            .map(|tool| {
                (
                    tool.definition.name.clone(),
                    (tool.server.clone(), tool.original_name.clone()),
                )
            })
            .collect()
    }

    pub fn view(&self, access: Access) -> View {
        let state = self.lock_state();
        View {
            entries: state
                .directory
                .entries
                .iter()
                .map(|(name, entry)| {
                    let status = if !entry.enabled {
                        "disabled"
                    } else if entry.config.is_none() {
                        entry.status
                    } else if !entry
                        .config
                        .as_ref()
                        .is_some_and(|config| access.allows_server(config))
                    {
                        "suspended (read-only)"
                    } else if let Some(server) = state.servers.get(name) {
                        // 并行调用正在使用服务时视为可用，不阻塞管理查询。
                        match server.try_lock() {
                            Ok(server) => {
                                if server.restart_after_cancel {
                                    "not connected (cancelled)"
                                } else if server.client.stopped {
                                    "failed (connection closed)"
                                } else {
                                    "connected"
                                }
                            }
                            Err(TryLockError::Poisoned(_)) | Err(TryLockError::WouldBlock) => {
                                "connected"
                            }
                        }
                    } else {
                        entry.status
                    };
                    EntryView {
                        name: name.clone(),
                        source: entry.source.clone(),
                        path: entry.path.clone(),
                        status,
                    }
                })
                .collect(),
            diagnostics: state
                .directory
                .diagnostics
                .iter()
                .cloned()
                .chain(state.servers.iter().filter_map(|(name, server)| {
                    let missing = server
                        .read_only_tools
                        .iter()
                        .filter(|original| {
                            state
                                .tools
                                .get(&exposed_name(name, original))
                                .is_none_or(|tool| {
                                    tool.server != *name || tool.original_name != **original
                                })
                        })
                        .count();
                    (missing > 0).then(|| {
                        format!(
                            "MCP {name}: {missing} configured read-only tools were not discovered"
                        )
                    })
                }))
                .collect(),
        }
    }
}

// 服务可用性：`WouldBlock` 表示正被其他线程调用，视为可用；中毒沿用旧的可读语义。
fn server_available(server: &SharedServer) -> bool {
    match server.try_lock() {
        Ok(state) => !state.client.stopped || state.restart_after_cancel,
        Err(TryLockError::Poisoned(_)) | Err(TryLockError::WouldBlock) => true,
    }
}

fn tool_available(state: &State, tool: &RemoteTool) -> bool {
    state
        .servers
        .get(&tool.server)
        .is_some_and(server_available)
}

pub(super) fn read_only_tool(state: &State, tool: &RemoteTool) -> bool {
    state
        .servers
        .get(&tool.server)
        .is_some_and(|server| server.read_only_tools.contains(&tool.original_name))
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let taken: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{taken}…")
    } else {
        taken
    }
}

/// 精简参数 schema：保留 `required` 与顶层属性的类型/枚举/短说明，去除冗长嵌套文档。
fn compact_parameters_schema(schema: &Value) -> Value {
    let Some(obj) = schema.as_object() else {
        return schema.clone();
    };
    let mut out = Map::new();
    if let Some(required) = obj.get("required").and_then(Value::as_array)
        && !required.is_empty()
    {
        out.insert("required".into(), Value::Array(required.clone()));
    }
    if let Some(props) = obj.get("properties").and_then(Value::as_object) {
        let mut compact_props = Map::new();
        for (name, prop) in props {
            let mut summary = Map::new();
            if let Some(prop_obj) = prop.as_object() {
                if let Some(ty) = prop_obj.get("type") {
                    summary.insert("type".into(), ty.clone());
                }
                if let Some(en) = prop_obj.get("enum") {
                    summary.insert("enum".into(), en.clone());
                }
                if let Some(items_ty) = prop_obj.get("items").and_then(|i| i.get("type")) {
                    summary.insert("items".into(), json!({"type": items_ty}));
                }
                if let Some(desc) = prop_obj.get("description").and_then(Value::as_str) {
                    let trimmed = desc.trim();
                    if !trimmed.is_empty() {
                        summary.insert(
                            "description".into(),
                            Value::String(truncate_chars(trimmed, MAX_PROP_DESC_CHARS)),
                        );
                    }
                }
            }
            compact_props.insert(name.clone(), Value::Object(summary));
        }
        out.insert("properties".into(), Value::Object(compact_props));
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::{plan_server_config, plan_spawn_client};
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    #[test]
    fn search_and_resolve_filter_by_access_allowed_query_and_disambiguate_names() {
        let manager = McpManager::empty();
        let (client_a, _proc_a) = plan_spawn_client(&plan_server_config(&["-c", "true"]));
        let (client_b, _proc_b) = plan_spawn_client(&plan_server_config(&["-c", "true"]));
        {
            let mut state = manager.test_state();
            let mut cfg_a = plan_server_config(&["-c", "true"]);
            cfg_a.read_only_tools = BTreeSet::from(["lookup".to_owned(), "shared".to_owned()]);
            state.servers.insert(
                "docs".into(),
                SharedServer::new(crate::ServerState {
                    config: cfg_a,
                    cwd: PathBuf::from("."),
                    client: client_a,
                    restart_after_cancel: false,
                }),
            );
            state.servers.insert(
                "admin".into(),
                SharedServer::new(crate::ServerState {
                    config: plan_server_config(&["-c", "true"]),
                    cwd: PathBuf::from("."),
                    client: client_b,
                    restart_after_cancel: false,
                }),
            );
            let long_desc = "D".repeat(320);
            state.tools.insert(
                "mcp__docs__lookup".into(),
                RemoteTool {
                    server: "docs".into(),
                    original_name: "lookup".into(),
                    definition: FunctionObject {
                        name: "mcp__docs__lookup".into(),
                        description: Some(format!("Search API reference {long_desc}")),
                        parameters: Some(json!({
                            "type": "object",
                            "required": ["id"],
                            "properties": {
                                "id": {
                                    "type": "string",
                                    "description": "Library identifier",
                                    "examples": ["huge", "extra"]
                                }
                            }
                        })),
                        strict: Some(false),
                    },
                },
            );
            state.tools.insert(
                "mcp__docs__shared".into(),
                RemoteTool {
                    server: "docs".into(),
                    original_name: "shared".into(),
                    definition: FunctionObject {
                        name: "mcp__docs__shared".into(),
                        description: Some("Shared read helper".into()),
                        parameters: None,
                        strict: Some(false),
                    },
                },
            );
            state.tools.insert(
                "mcp__admin__shared".into(),
                RemoteTool {
                    server: "admin".into(),
                    original_name: "shared".into(),
                    definition: FunctionObject {
                        name: "mcp__admin__shared".into(),
                        description: Some("Dangerous write action".into()),
                        parameters: None,
                        strict: Some(false),
                    },
                },
            );
        }

        assert_eq!(manager.available_tool_count(Access::Full, |_| true), 3);
        assert_eq!(manager.available_tool_count(Access::ReadOnly, |_| true), 2);
        assert_eq!(
            manager.available_tool_count(Access::Full, |name| name.starts_with("mcp__docs__")),
            2
        );

        let (total, hits) =
            manager.search_tools(Access::ReadOnly, Some("reference"), None, 10, |_| true);
        assert_eq!(total, 1);
        assert_eq!(hits[0].name, "mcp__docs__lookup");
        assert!(hits[0].read_only);
        assert!(hits[0].description.as_deref().unwrap().ends_with('…'));
        // 精简 schema 保留 required 与属性类型/短说明，去除多余 examples 字段。
        let params = hits[0].parameters.as_ref().unwrap();
        assert_eq!(params["required"], json!(["id"]));
        assert_eq!(params["properties"]["id"]["type"], "string");
        assert!(params["properties"]["id"].get("examples").is_none());

        // 唯一原名可直接解析；跨服务同名必须给 server 或完整 mcp__ 名称。
        assert_eq!(
            manager.resolve_tool_name("lookup", None).as_deref(),
            Some("mcp__docs__lookup")
        );
        assert!(manager.resolve_tool_name("shared", None).is_none());
        assert_eq!(
            manager
                .resolve_tool_name("shared", Some("admin"))
                .as_deref(),
            Some("mcp__admin__shared")
        );
        assert_eq!(
            manager
                .resolve_tool_name("mcp__docs__shared", None)
                .as_deref(),
            Some("mcp__docs__shared")
        );
        let _ = manager.test_stop_servers();
    }
}
