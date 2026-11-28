//! MCP fixed two-tool proxy (`mcp_search` + `mcp_call`):
//! when the number of connected MCP tools exceeds the threshold (default >12), only these two fixed tools are exposed,
//! so dozens or hundreds of external schemas do not crowd the context and the `tools` array never changes during the session, keeping the Prompt Cache hit.
//!
//! `mcp_call` bypasses no gate: the target tool is resolved to its canonical `mcp__*` name before execution
//! and then authorized by exactly the same `AllowedTools`, `ToolPolicy`, `Access::ReadOnly` and `pp-permission` rules as a direct call.

use anyhow::{Context, Result, bail, ensure};
use async_openai::types::chat::FunctionObject;
use serde::Deserialize;
use serde_json::{Value, json};

pub(super) const MCP_SEARCH_TOOL: &str = "mcp_search";
pub(super) const MCP_CALL_TOOL: &str = "mcp_call";

const DEFAULT_SEARCH_LIMIT: usize = 15;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct McpSearchArgs {
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    server: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct McpCallArgs {
    tool: String,
    #[serde(default)]
    server: Option<String>,
    #[serde(default = "default_empty_object")]
    arguments: Value,
}

fn default_empty_object() -> Value {
    json!({})
}

pub(super) struct ResolvedMcpCall {
    pub(super) exposed_name: String,
    pub(super) inner_arguments: String,
}

pub(super) fn search_definition() -> FunctionObject {
    super::tool_def(
        MCP_SEARCH_TOOL,
        "Discover connected MCP tools by keyword or server name. Returns matching tool names (`mcp__<server>__<tool>`), short descriptions, read_only flags, and compact parameter schemas for use with `mcp_call`.",
        super::object_schema(
            json!({
                "query": {
                    "type": "string",
                    "description": "Space-separated keywords matching tool name, server, or description; omit to list available tools."
                },
                "server": {
                    "type": "string",
                    "description": "Optional MCP server name filter."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 50,
                    "description": "Max results (default 15, max 50)."
                }
            }),
            &[],
        ),
    )
}

pub(super) fn call_definition() -> FunctionObject {
    super::tool_def(
        MCP_CALL_TOOL,
        "Invoke a connected MCP tool discovered via `mcp_search`. Pass `tool` (full `mcp__<server>__<tool>` name, or original tool name with `server`) and an `arguments` object. Subject to normal MCP permission and read-only policies.",
        super::object_schema(
            json!({
                "tool": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Full `mcp__<server>__<tool>` name from `mcp_search`, or original tool name when paired with `server`."
                },
                "server": {
                    "type": "string",
                    "description": "Optional MCP server name when `tool` is an original (unprefixed) name."
                },
                "arguments": {
                    "type": "object",
                    "description": "JSON object of arguments for the target MCP tool (defaults to {})."
                }
            }),
            &["tool"],
        ),
    )
}

pub(super) fn resolve_call(
    mcp: &pp_mcp::McpManager,
    raw_arguments: &str,
) -> Result<ResolvedMcpCall> {
    let args: McpCallArgs =
        serde_json::from_str(raw_arguments).context("invalid mcp_call arguments")?;
    ensure!(
        !args.tool.trim().is_empty(),
        "mcp_call requires a non-empty `tool` name"
    );
    ensure!(
        args.arguments.is_object(),
        "mcp_call `arguments` must be a JSON object"
    );
    let Some(exposed_name) = mcp.resolve_tool_name(&args.tool, args.server.as_deref()) else {
        bail!(
            "unknown or ambiguous MCP tool `{}`; run mcp_search to list available tool names",
            args.tool
        );
    };
    Ok(ResolvedMcpCall {
        exposed_name,
        inner_arguments: args.arguments.to_string(),
    })
}

pub(super) fn run_search(
    mcp: &pp_mcp::McpManager,
    access: pp_mcp::Access,
    allowed: Option<&crate::AllowedTools>,
    raw_arguments: &str,
) -> Result<Value> {
    let args: McpSearchArgs = if raw_arguments.trim().is_empty() {
        McpSearchArgs {
            query: None,
            server: None,
            limit: None,
        }
    } else {
        serde_json::from_str(raw_arguments).context("invalid mcp_search arguments")?
    };
    let limit = args.limit.unwrap_or(DEFAULT_SEARCH_LIMIT);
    let (total, tools) = mcp.search_tools(
        access,
        args.query.as_deref(),
        args.server.as_deref(),
        limit,
        |name| allowed.is_none_or(|a| a.allows(name)),
    );
    Ok(json!({
        "ok": true,
        "total": total,
        "returned": tools.len(),
        "tools": tools,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{ToolPolicy, Tools};
    use pp_config::config::{
        EditMode, McpProxyMode, McpSettings, PermissionMode, PermissionPolicy, ToolsSettings,
    };
    use pp_core::cancellation::CancellationToken;
    use pp_core::plan::{WorkMode, WorkflowState};
    use std::collections::BTreeMap;
    use std::path::Path;
    use std::sync::Arc;

    fn connected_mcp_tools(dir: &Path, settings: McpSettings) -> Tools {
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/mcp_server.sh");
        let mcp_json = dir.join("mcp.json");
        std::fs::write(
            &mcp_json,
            json!({
                "mcpServers": {
                    "local": {
                        "command": "bash",
                        "args": [fixture],
                        "env": {"MCP_VALUE": "proxy-ok"},
                        "timeout": 5000,
                        "read_only_tools": ["echo"]
                    }
                }
            })
            .to_string(),
        )
        .unwrap();
        let manager = Arc::new(pp_mcp::McpManager::new_isolated(
            mcp_json,
            dir.to_path_buf(),
        ));
        manager
            .connect_enabled(&CancellationToken::new(), pp_mcp::Access::Full)
            .unwrap();
        let mut tools = Tools::with_mcp(dir.to_path_buf(), EditMode::Replace, manager).unwrap();
        tools.configure_mcp(settings);
        tools
    }

    #[test]
    fn mcp_proxy_switches_by_threshold_and_enforces_permission_allowed_and_readonly_gates() {
        let dir = tempfile::tempdir().unwrap();
        // The fixture server exposes 2 tools (echo / fail): at the default threshold 12 `mcp__*` is exposed directly, at threshold 1 the two-tool proxy switches on.
        let mut tools = connected_mcp_tools(dir.path(), McpSettings::default());
        let direct_names: Vec<String> = tools
            .all_definitions(WorkMode::Execute)
            .into_iter()
            .map(|d| d.name)
            .collect();
        assert!(direct_names.iter().any(|n| n == "mcp__local__echo"));
        assert!(!direct_names.iter().any(|n| n == MCP_SEARCH_TOOL));

        tools.configure_mcp(McpSettings {
            proxy: McpProxyMode::Auto,
            proxy_threshold: 1,
        });
        let proxy_names: Vec<String> = tools
            .all_definitions(WorkMode::Execute)
            .into_iter()
            .map(|d| d.name)
            .collect();
        assert!(proxy_names.iter().any(|n| n == MCP_SEARCH_TOOL));
        assert!(proxy_names.iter().any(|n| n == MCP_CALL_TOOL));
        assert!(!proxy_names.iter().any(|n| n.starts_with("mcp__")));

        let workflow = WorkflowState::default();
        let token = CancellationToken::new();
        let exec = |t: &Tools, name: &str, args: Value, policy: ToolPolicy<'_>| -> Value {
            let res = t.execute(name, &args.to_string(), "c1", policy, &token, &mut |_| {});
            assert!(res.stop_error.is_none());
            serde_json::from_str(&res.output).unwrap()
        };

        // 1. In Review mode mcp_search returns only the read-only tool echo and never leaks the non-read-only tool fail.
        let search_review = exec(&tools, MCP_SEARCH_TOOL, json!({}), ToolPolicy::Review);
        assert_eq!(search_review["ok"], true, "{search_review}");
        assert_eq!(search_review["total"], 1, "{search_review}");
        assert_eq!(search_review["tools"][0]["name"], "mcp__local__echo");

        // 2. In Review mode mcp_call allows the read-only echo and rejects the non-read-only fail.
        let ro_call = exec(
            &tools,
            MCP_CALL_TOOL,
            json!({"tool": "mcp__local__echo", "arguments": {"text": "hi"}}),
            ToolPolicy::Review,
        );
        assert_eq!(ro_call["ok"], true, "{ro_call}");
        assert_eq!(ro_call["result"]["content"][0]["text"], "proxy-ok");

        let denied_review = exec(
            &tools,
            MCP_CALL_TOOL,
            json!({"tool": "fail", "server": "local", "arguments": {}}),
            ToolPolicy::Review,
        );
        assert_eq!(denied_review["ok"], false, "{denied_review}");

        // 3. mcp_call strictly follows pp-permission rules (applied to the target's canonical name mcp__local__echo).
        tools
            .permission
            .select(Some(PermissionMode::WorkspaceWrite));
        tools.permission.set_config(ToolsSettings {
            permission: BTreeMap::from([("mcp__local__echo".to_owned(), PermissionPolicy::Deny)]),
            ..ToolsSettings::default()
        });
        let denied_perm = exec(
            &tools,
            MCP_CALL_TOOL,
            json!({"tool": "echo", "server": "local", "arguments": {"text": "blocked"}}),
            ToolPolicy::Execute {
                workflow: &workflow,
            },
        );
        assert_eq!(denied_perm["ok"], false, "{denied_perm}");

        // 4. mcp_call strictly follows AllowedTools filtering.
        tools.permission.set_config(ToolsSettings::default());
        tools.allowed_tools =
            Some(crate::AllowedTools::parse_cli(&["mcp__local__echo".to_owned()]).unwrap());
        let denied_allowed = exec(
            &tools,
            MCP_CALL_TOOL,
            json!({"tool": "mcp__local__fail", "arguments": {}}),
            ToolPolicy::Execute {
                workflow: &workflow,
            },
        );
        assert_eq!(denied_allowed["ok"], false, "{denied_allowed}");

        tools.mcp.shutdown().unwrap();
    }
}
