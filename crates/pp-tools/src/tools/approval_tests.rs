use super::*;
use pp_config::config::{ApprovalMode, ApprovalPolicy, ToolsSettings};
use pp_core::cancellation::CancellationToken;
use std::sync::{Arc, Mutex};

fn gate(mode: ApprovalMode, policies: &[(&str, ApprovalPolicy)]) -> Gate {
    let mut gate = Gate::new();
    gate.set_config(ToolsSettings {
        approval_mode: Some(mode),
        approval: policies
            .iter()
            .map(|(name, policy)| (name.to_string(), *policy))
            .collect(),
        ..Default::default()
    });
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
fn lsp_read_actions_stay_read_tier_and_skip_always_ask_prompt() {
    let gate = gate(ApprovalMode::AlwaysAsk, &[]);
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
    // 单文件诊断是只读动作；显式 timeout 不改变分档。
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

#[test]
fn lsp_write_actions_are_write_tier_even_for_preview() {
    // write 模式放行 Write 档、只拦截 Exec 档：写入动作不是 Exec 档。
    let write = gate(ApprovalMode::Write, &[]);
    for action in ["reload", "request"] {
        enforce(&write, "lsp", &lsp_write(action)).unwrap();
    }
    // always-ask 只放行 Read 档：写入动作（含 apply=false 预览）必须询问，
    // 无交互 handler 时 fail-closed。
    let always = gate(ApprovalMode::AlwaysAsk, &[]);
    for action in ["reload", "request"] {
        enforce(&always, "lsp", &lsp_write(action)).unwrap_err();
    }
}

#[test]
fn lsp_invalid_arguments_are_conservatively_write() {
    // 无效参数既不能降成 Read 档，也不能跳过审批：按 Write 处理。
    let write = gate(ApprovalMode::Write, &[]);
    for arguments in [
        "{broken",
        "{}",
        r#"{"action":"diagnostics"}"#,
        r#"{"action":"bogus"}"#,
        r#"{"action":42}"#,
        r#"{"action":"rename","unexpected":true}"#,
    ] {
        enforce(&write, "lsp", arguments).unwrap();
        assert_eq!(tier_of_arguments("lsp", arguments), Tier::Write);
    }
    let always = gate(ApprovalMode::AlwaysAsk, &[]);
    for arguments in ["{broken", "{}", r#"{"action":"bogus"}"#] {
        enforce(&always, "lsp", arguments).unwrap_err();
    }
}

#[test]
fn lsp_policy_overrides_action_tiers() {
    // 显式 tools.approval.lsp 优先于动作分档：deny 拦只读动作。
    let denied = gate(ApprovalMode::Write, &[("lsp", ApprovalPolicy::Deny)]);
    enforce(&denied, "lsp", &lsp_read("definition")).unwrap_err();
    // allow 旁路写入动作的审批。
    let allowed = gate(ApprovalMode::AlwaysAsk, &[("lsp", ApprovalPolicy::Allow)]);
    enforce(&allowed, "lsp", &lsp_write("rename")).unwrap();
    // prompt 强制每次询问，优先于动作分档。
    let prompted = gate(ApprovalMode::Write, &[("lsp", ApprovalPolicy::Prompt)]);
    enforce(&prompted, "lsp", &lsp_read("hover")).unwrap_err();
}

#[test]
fn lsp_write_action_approval_reaches_handler_for_every_call() {
    let mut gate = gate(ApprovalMode::AlwaysAsk, &[]);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&seen);
    gate.set_handler(Box::new(move |request, _cancellation| {
        recorded.lock().unwrap().push(request.session_grant);
        Ok(ApprovalDecision::Approve)
    }));
    enforce(&gate, "lsp", &lsp_write("reload")).unwrap();
    enforce(&gate, "lsp", &lsp_write("reload")).unwrap();
    // 只读动作不进入审批，handler 不被调用。
    enforce(&gate, "lsp", &lsp_read("hover")).unwrap();
    assert_eq!(*seen.lock().unwrap(), vec![true, true]);
}

#[test]
fn tier_mapping_covers_builtin_mcp_and_unknown() {
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
