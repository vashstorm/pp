use super::*;
use pp_config::config::{PermissionMode, PermissionPolicy, ToolsSettings};
use pp_core::cancellation::CancellationToken;
use std::sync::{Arc, Mutex};

fn gate(mode: PermissionMode, policies: &[(&str, PermissionPolicy)]) -> Gate {
    let gate = Gate::new();
    gate.set_config(ToolsSettings {
        permission: policies
            .iter()
            .map(|(name, policy)| (name.to_string(), *policy))
            .collect(),
        ..Default::default()
    });
    gate.select(Some(mode));
    gate
}

fn enforce(gate: &Gate, name: &str, arguments: &str) -> anyhow::Result<()> {
    gate.enforce(
        name,
        arguments,
        tier_of_arguments(name, arguments),
        None,
        &CancellationToken::new(),
    )
}

fn lsp(arguments: serde_json::Value) -> String {
    arguments.to_string()
}

fn lsp_read(action: &str) -> String {
    match action {
        "status" | "capabilities" => lsp(serde_json::json!({ "action": action })),
        _ => lsp(serde_json::json!({ "action": action, "file": "src/lib.rs", "line": 1 })),
    }
}

fn lsp_write(action: &str) -> String {
    match action {
        "rename" => lsp(serde_json::json!({
            "action":"rename","file":"src/lib.rs","line":1,
            "symbol":"value","new_name":"number","apply":false
        })),
        "rename_file" => lsp(serde_json::json!({
            "action":"rename_file","file":"src/lib.rs","new_name":"src/main.rs"
        })),
        "code_actions" => lsp(serde_json::json!({
            "action":"code_actions","file":"src/lib.rs","line":1
        })),
        "reload" => lsp(serde_json::json!({
            "action":"reload","file":"src/lib.rs"
        })),
        "request" => lsp(serde_json::json!({
            "action":"request","query":"rust-analyzer/syntaxTree"
        })),
        other => panic!("not a mutating lsp action: {other}"),
    }
}

#[test]
fn lsp_action_tier_classification_and_policy_overrides() {
    {
        let gate = gate(PermissionMode::ReadOnly, &[]);
        for action in [
            "definition",
            "type_definition",
            "implementation",
            "references",
            "hover",
            "symbols",
            "status",
            "capabilities",
        ] {
            enforce(&gate, "lsp", &lsp_read(action)).unwrap();
        }
        enforce(
            &gate,
            "lsp",
            &lsp(serde_json::json!({
                "action":"diagnostics","file":"src/lib.rs","timeout":60
            })),
        )
        .unwrap();
        assert_eq!(
            tier_of_arguments("lsp", &lsp_read("definition")),
            Tier::Read
        );
    }

    {
        let workspace_write = gate(PermissionMode::WorkspaceWrite, &[]);
        for action in ["reload", "request"] {
            enforce(&workspace_write, "lsp", &lsp_write(action)).unwrap_err();
        }
        let read_only = gate(PermissionMode::ReadOnly, &[]);
        for action in ["reload", "request"] {
            enforce(&read_only, "lsp", &lsp_write(action)).unwrap_err();
        }
    }

    {
        let yolo = gate(PermissionMode::DangerFullAccess, &[]);
        for arguments in [
            "{broken",
            "{}",
            r#"{"action":"diagnostics"}"#,
            r#"{"action":"bogus"}"#,
            r#"{"action":42}"#,
            r#"{"action":"rename","unexpected":true}"#,
        ] {
            enforce(&yolo, "lsp", arguments).unwrap();
            assert_eq!(tier_of_arguments("lsp", arguments), Tier::Write);
        }
        let read_only = gate(PermissionMode::ReadOnly, &[]);
        for arguments in ["{broken", "{}", r#"{"action":"bogus"}"#] {
            enforce(&read_only, "lsp", arguments).unwrap_err();
        }
    }

    {
        let denied = gate(
            PermissionMode::WorkspaceWrite,
            &[("lsp", PermissionPolicy::Deny)],
        );
        enforce(&denied, "lsp", &lsp_read("definition")).unwrap_err();
        let allowed = gate(
            PermissionMode::ReadOnly,
            &[("lsp", PermissionPolicy::Allow)],
        );
        enforce(&allowed, "lsp", &lsp_write("rename")).unwrap();
        let prompted = gate(
            PermissionMode::WorkspaceWrite,
            &[("lsp", PermissionPolicy::Prompt)],
        );
        enforce(&prompted, "lsp", &lsp_read("hover")).unwrap_err();
    }
}

#[test]
fn lsp_write_action_prompts_and_tool_tier_mapping() {
    {
        let mut gate = gate(PermissionMode::ReadOnly, &[]);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&seen);
        gate.set_handler(Box::new(move |request, _cancellation| {
            recorded.lock().unwrap().push(request.session_grant);
            Ok(PermissionDecision::Approve)
        }));
        enforce(&gate, "lsp", &lsp_write("reload")).unwrap();
        enforce(&gate, "lsp", &lsp_write("reload")).unwrap();
        enforce(&gate, "lsp", &lsp_read("hover")).unwrap();
        assert_eq!(*seen.lock().unwrap(), vec![true, true]);
    }

    assert_eq!(tier_of("read"), Tier::Read);
    assert_eq!(tier_of("rg"), Tier::Read);
    assert_eq!(tier_of("glob"), Tier::Read);
    assert_eq!(tier_of("todo"), Tier::Read);
    assert_eq!(tier_of("ask"), Tier::Read);
    assert_eq!(tier_of("edit"), Tier::Write);
    assert_eq!(tier_of("write"), Tier::Write);
    assert_eq!(tier_of("bash"), Tier::Exec);
    assert_eq!(tier_of("bash_task"), Tier::Exec);
    assert_eq!(tier_of("mcp__anything__tool"), Tier::Write);
    assert_eq!(tier_of("mcp__"), Tier::Write);
    assert_eq!(tier_of("unknown_tool"), Tier::Exec);
}
