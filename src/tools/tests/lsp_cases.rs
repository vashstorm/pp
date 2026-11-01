use super::*;
use crate::config::ToolsSettings;
use crate::tools::approval::{ApprovalDecision, ApprovalHandler, ApprovalMode};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// 安装一个已经启用但不会自行启动服务器的 Registry：用户级配置为空对象，
/// 只合并内置定义，`lazy=true` 保证被拒绝的调用不会拉起任何进程。
fn tools_with_lsp(dir: &tempfile::TempDir) -> Tools {
    let mut tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
    let user = dir.path().join("lsp.json");
    fs::write(&user, "{}").unwrap();
    let registry = crate::lsp::Registry::new(
        dir.path().into(),
        Some(user),
        crate::lsp::Settings {
            enabled: true,
            lazy: true,
            ..Default::default()
        },
    );
    tools.install_lsp(registry);
    tools
}

fn run_lsp(tools: &Tools, arguments: Value, policy: ToolPolicy<'_>) -> Value {
    let execution = tools.execute(
        "lsp",
        &arguments.to_string(),
        "lsp-call",
        policy,
        &CancellationToken::new(),
        &mut |_| {},
    );
    assert!(
        execution.stop_error.is_none(),
        "unexpected cleanup failure: {:?}",
        execution.stop_error
    );
    serde_json::from_str(&execution.output).unwrap()
}

fn deny_handler(prompts: &Arc<AtomicUsize>) -> ApprovalHandler {
    let prompts = Arc::clone(prompts);
    Box::new(move |_request, _cancellation| {
        prompts.fetch_add(1, Ordering::SeqCst);
        Ok(ApprovalDecision::Deny)
    })
}

fn rename_arguments() -> Value {
    json!({
        "action":"rename","file":"src/lib.rs","line":1,
        "symbol":"value","new_name":"number","apply":false
    })
}

#[test]
fn lsp_schema_requires_an_installed_enabled_registry() {
    let dir = tempfile::tempdir().unwrap();
    let plain = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
    for mode in [WorkMode::Execute, WorkMode::Plan] {
        assert!(
            !plain
                .all_definitions(mode)
                .iter()
                .any(|definition| definition.name == "lsp"),
            "未安装 Registry 时不能暴露 lsp schema"
        );
    }
    assert!(
        !plain
            .review_definitions()
            .iter()
            .any(|definition| definition.name == "lsp")
    );

    let tools = tools_with_lsp(&dir);
    for mode in [WorkMode::Execute, WorkMode::Plan] {
        assert!(
            tools
                .all_definitions(mode)
                .iter()
                .any(|definition| definition.name == "lsp"),
            "{mode:?} 应暴露 lsp"
        );
    }
    assert!(
        tools
            .review_definitions()
            .iter()
            .any(|definition| definition.name == "lsp"),
        "Review 应暴露受限 lsp"
    );

    // enabled=false 时安装被忽略：没有 schema，也没有清理权。
    let disabled = crate::lsp::Registry::new(
        dir.path().into(),
        None,
        crate::lsp::Settings {
            enabled: false,
            ..Default::default()
        },
    );
    let mut disabled_tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
    disabled_tools.install_lsp(disabled);
    assert!(disabled_tools.lsp_registry().is_none());
    assert!(disabled_tools.lsp_cleanup().is_none());
    assert!(
        !disabled_tools
            .all_definitions(WorkMode::Execute)
            .iter()
            .any(|definition| definition.name == "lsp")
    );

    // 未安装/关闭时显式调用也只是普通工具失败，不会发现或启动服务器。
    let workflow = WorkflowState::default();
    let result = run_lsp(
        &plain,
        json!({"action":"status"}),
        ToolPolicy::Execute {
            workflow: &workflow,
        },
    );
    assert_eq!(result["ok"], false, "{result}");
}

#[test]
fn installed_registry_is_shared_without_cleanup_ownership() {
    let dir = tempfile::tempdir().unwrap();
    let owner = tools_with_lsp(&dir);
    assert!(owner.lsp_cleanup().is_some());

    let mut child = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
    child.install_shared_lsp(owner.lsp_registry().unwrap().clone());
    // 子 Agent 能看到工具并共享同一批客户端，但不会取得停止父 Registry 的句柄。
    assert!(child.lsp_registry().is_some());
    assert!(child.lsp_cleanup().is_none());
    assert!(
        child
            .all_definitions(WorkMode::Execute)
            .iter()
            .any(|definition| definition.name == "lsp")
    );
}

#[test]
fn review_policy_blocks_lsp_mutating_actions_without_starting_servers() {
    let dir = tempfile::tempdir().unwrap();
    let tools = tools_with_lsp(&dir);
    let cases = vec![
        json!({"action":"rename","file":"src/lib.rs","line":1,"symbol":"a","new_name":"b","apply":true}),
        json!({"action":"reload","file":"src/lib.rs"}),
        json!({"action":"request","query":"textDocument/hover"}),
        json!({"action":"diagnostics","file":"*"}),
        json!({"action":"bogus"}),
        json!({"unknown":"field"}),
        json!("not an object"),
    ];
    for arguments in cases {
        let result = run_lsp(&tools, arguments.clone(), ToolPolicy::Review);
        assert_eq!(result["ok"], false, "{arguments}: {result}");
        assert!(result.get("cancelled").is_none(), "{arguments}: {result}");
        assert!(
            result
                .get("error")
                .and_then(Value::as_str)
                .is_some_and(|error| !error.is_empty()),
            "{arguments}: {result}"
        );
    }
    // Review 拒绝必须发生在任何服务器启动或文件变更之前。
    assert!(tools.lsp_registry().unwrap().started().is_empty());
    assert!(!dir.path().join("src/main.rs").exists());
}

#[test]
fn plan_policy_blocks_lsp_mutating_actions_before_dispatch() {
    let dir = tempfile::tempdir().unwrap();
    let tools = tools_with_lsp(&dir);
    let document = plan_document();
    let cases = vec![
        json!({"action":"rename","file":"src/lib.rs","line":1,"symbol":"a","new_name":"b","apply":true}),
        json!({"action":"reload","file":"src/lib.rs"}),
        json!({"action":"request","query":"textDocument/hover"}),
        json!({"action":"diagnostics","file":"*"}),
    ];
    for arguments in cases {
        let result = run_lsp(
            &tools,
            arguments.clone(),
            ToolPolicy::Plan {
                document: &document,
                sealed: false,
                input: None,
            },
        );
        assert_eq!(result["ok"], false, "{arguments}: {result}");
    }
    assert!(tools.lsp_registry().unwrap().started().is_empty());
    assert!(!dir.path().join("src/main.rs").exists());
}

#[test]
fn forced_readonly_execute_rejects_mutating_actions_before_approval() {
    let dir = tempfile::tempdir().unwrap();
    let workflow = WorkflowState::default();
    let prompts = Arc::new(AtomicUsize::new(0));
    let mut tools = tools_with_lsp(&dir);
    tools.set_lsp_readonly(true);
    tools.approval.set_config(ToolsSettings {
        approval_mode: ApprovalMode::AlwaysAsk,
        ..Default::default()
    });
    tools.approval.set_handler(deny_handler(&prompts));

    let result = run_lsp(
        &tools,
        rename_arguments(),
        ToolPolicy::Execute {
            workflow: &workflow,
        },
    );
    assert_eq!(result["ok"], false, "{result}");
    // 只读强制先于审批：不会弹出审批，也不会被当成“用户拒绝”。
    let error = result["error"].as_str().unwrap_or_default();
    assert!(!error.contains("denied by the user"), "{result}");
    assert_eq!(prompts.load(Ordering::SeqCst), 0);
    assert!(tools.lsp_registry().unwrap().started().is_empty());
}

#[test]
fn execute_mutating_action_reaches_approval_with_exact_arguments() {
    let dir = tempfile::tempdir().unwrap();
    let workflow = WorkflowState::default();
    let prompts = Arc::new(AtomicUsize::new(0));
    let mut tools = tools_with_lsp(&dir);
    tools.approval.set_config(ToolsSettings {
        approval_mode: ApprovalMode::AlwaysAsk,
        ..Default::default()
    });
    tools.approval.set_handler(deny_handler(&prompts));

    // reload 是 Write 档：always-ask 下必须经过审批，审批发生在分发之前。
    let result = run_lsp(
        &tools,
        json!({"action":"reload"}),
        ToolPolicy::Execute {
            workflow: &workflow,
        },
    );
    assert_eq!(result["ok"], false, "{result}");
    assert!(
        result["error"]
            .as_str()
            .unwrap_or_default()
            .contains("lsp was denied by the user."),
        "{result}"
    );
    assert_eq!(prompts.load(Ordering::SeqCst), 1);
    assert!(tools.lsp_registry().unwrap().started().is_empty());
}

#[test]
fn execute_readonly_action_skips_approval_and_never_starts_when_unsupported() {
    let dir = tempfile::tempdir().unwrap();
    let workflow = WorkflowState::default();
    let prompts = Arc::new(AtomicUsize::new(0));
    let mut tools = tools_with_lsp(&dir);
    tools.approval.set_config(ToolsSettings {
        approval_mode: ApprovalMode::AlwaysAsk,
        ..Default::default()
    });
    tools.approval.set_handler(deny_handler(&prompts));

    // 无匹配服务器的扩展名：动作是只读档，跳过审批，由工具返回无服务器结果。
    let result = run_lsp(
        &tools,
        json!({"action":"hover","file":"notes.zzz","line":1}),
        ToolPolicy::Execute {
            workflow: &workflow,
        },
    );
    assert_eq!(prompts.load(Ordering::SeqCst), 0, "{result}");
    assert_eq!(result["ok"], false, "{result}");
    assert!(tools.lsp_registry().unwrap().started().is_empty());
}
