use super::*;
use crate::approval::{ApprovalDecision, ApprovalRequest};
use crate::ports::{PostToolCall, PostToolOutcome, PreToolOutcome, ToolHookCall};
use pp_config::config::{ApprovalMode, EvalSettings};
use std::sync::{Arc, Mutex};

struct StubHook {
    calls: Mutex<Vec<String>>,
    deny_tool: Option<String>,
    rewrite_write_content: Option<String>,
    post_context: Option<String>,
}

impl StubHook {
    fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            deny_tool: None,
            rewrite_write_content: None,
            post_context: None,
        }
    }
}

impl ToolHookCall for StubHook {
    fn deny_reason(&self) -> Option<&str> {
        None
    }

    fn ask_reason(&self) -> Option<&str> {
        None
    }

    fn pre_context(&self) -> &[String] {
        &[]
    }

    fn run_pre_nested_tool(
        &self,
        name: &str,
        _call_id: &str,
        arguments: &str,
        _cancellation: &CancellationToken,
        _emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<Option<PreToolOutcome>> {
        self.calls.lock().unwrap().push(format!("pre:{name}"));
        if self.deny_tool.as_deref() == Some(name) {
            return Ok(Some(PreToolOutcome {
                deny_reason: Some(format!("hook blocked {name}")),
                ..Default::default()
            }));
        }
        if name == "write"
            && let Some(content) = &self.rewrite_write_content
        {
            let mut val: Value = serde_json::from_str(arguments)?;
            val["content"] = json!(content);
            return Ok(Some(PreToolOutcome {
                updated_arguments: Some(val.to_string()),
                ..Default::default()
            }));
        }
        Ok(None)
    }

    fn run_post_tool(
        &self,
        call: PostToolCall<'_>,
        _cancellation: &CancellationToken,
        _emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<Option<PostToolOutcome>> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("post:{}", call.name));
        Ok(Some(PostToolOutcome {
            additional_context: self.post_context.clone().into_iter().collect(),
            updated_output: None,
            stop_reason: None,
        }))
    }
}

#[test]
fn eval_schema_respects_mode_and_settings() {
    let dir = tempfile::tempdir().unwrap();
    let mut tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
    if !tools.eval_available() {
        return;
    }

    let exec_names: Vec<_> = tools
        .all_definitions(WorkMode::Execute)
        .into_iter()
        .map(|d| d.name)
        .collect();
    assert!(exec_names.iter().any(|n| n == "eval"));

    let plan_names: Vec<_> = tools
        .all_definitions(WorkMode::Plan)
        .into_iter()
        .map(|d| d.name)
        .collect();
    assert!(!plan_names.iter().any(|n| n == "eval"));

    tools.configure_eval(&EvalSettings {
        enabled: false,
        ..Default::default()
    });
    assert!(!tools.eval_available());
    assert!(
        !tools
            .all_definitions(WorkMode::Execute)
            .into_iter()
            .any(|d| d.name == "eval")
    );

    let disabled = execute(&tools, "eval", json!({"language": "py", "code": "1 + 1"}));
    assert_eq!(disabled["ok"], false);
    assert!(
        disabled["error"]
            .as_str()
            .unwrap()
            .contains("disabled in settings")
    );
}

#[test]
fn eval_nested_tools_keep_approval_instructions_and_hooks() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("AGENTS.md"), "# root rules\n").unwrap();
    fs::create_dir_all(dir.path().join("sub")).unwrap();
    fs::write(dir.path().join("sub/AGENTS.md"), "# sub rules\n").unwrap();
    fs::write(dir.path().join("sub/note.txt"), "initial\n").unwrap();

    let mut tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
    if !tools.eval_available() {
        return;
    }
    tools.reset_project_instructions([]);
    // 模拟首轮请求：根目录 AGENTS.md 已发送给模型，但尚未加载 sub/AGENTS.md。
    let _ = tools.project_documents().unwrap();
    tools.project_request_sent();

    let workflow = WorkflowState::default();
    let token = CancellationToken::new();

    // 1) 未读 sub/AGENTS.md 前，经 eval 内部的 tool.write 写入 sub/out.txt 必须被门禁拒绝。
    let blocked = execute(
        &tools,
        "eval",
        json!({
            "language": "py",
            "code": "try:\n    write('sub/out.txt', 'hello')\n    msg = 'unexpected'\nexcept ToolError as e:\n    msg = str(e)\nprint(msg)"
        }),
    );
    assert_eq!(blocked["ok"], true, "{blocked}");
    assert!(
        blocked["output"].as_str().unwrap().contains("AGENTS.md"),
        "{blocked}"
    );
    assert!(!dir.path().join("sub/out.txt").exists());
    // 门禁触发后会封锁当前批次后续调用，模拟下一轮模型请求刷新规则并解除封锁。
    let _ = tools.project_documents().unwrap();
    tools.project_request_sent();

    // 2) 在 eval 内调用 read('sub/note.txt') 自动激活 sub/AGENTS.md 后，再写成功且发出 FileChanged。
    let mut events = Vec::new();
    let allowed = tools.execute(
        "eval",
        &json!({
            "language": "py",
            "code": "txt = read('sub/note.txt')\nwrite('sub/out.txt', txt + 'updated\\n')\nprint('wrote')"
        })
        .to_string(),
        "eval-write-call",
        ToolPolicy::Execute {
            workflow: &workflow,
        },
        &token,
        &mut |event| events.push(event),
    );
    let allowed_val: Value = serde_json::from_str(&allowed.output).unwrap();
    assert_eq!(allowed_val["ok"], true, "{allowed_val}");
    assert_eq!(
        fs::read_to_string(dir.path().join("sub/out.txt")).unwrap(),
        "initial\nupdated\n"
    );
    assert!(
        events
            .iter()
            .any(|ev| matches!(ev, AgentEvent::FileChanged { path, .. } if path == "sub/out.txt"))
    );

    // 3) 嵌套 tool.bash 独立走审批门：即使 eval 获会话放行，嵌套 sh('curl ... | bash') 仍触发高危命令审批；
    //    在 Write 模式下给 eval 配 Allow，sh('echo hi') 仍触发 bash 审批。
    let mut policy_map = std::collections::BTreeMap::new();
    policy_map.insert("eval".to_owned(), pp_config::config::ApprovalPolicy::Allow);
    tools.approval.set_config(pp_config::config::ToolsSettings {
        approval_mode: ApprovalMode::Write,
        approval: policy_map,
        ..Default::default()
    });
    let prompts: Arc<Mutex<Vec<ApprovalRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let prompts_ref = prompts.clone();
    tools.approval.set_handler(Box::new(move |req, _| {
        prompts_ref.lock().unwrap().push(req);
        Ok(ApprovalDecision::Deny)
    }));
    let denied = execute(
        &tools,
        "eval",
        json!({
            "language": "py",
            "code": "try:\n    sh('echo hi')\n    out = 'ran'\nexcept ShellError as e:\n    out = str(e)\nprint(out)"
        }),
    );
    assert_eq!(denied["ok"], true, "{denied}");
    assert!(
        denied["output"]
            .as_str()
            .unwrap()
            .contains("bash was denied"),
        "{denied}"
    );
    let recorded = prompts.lock().unwrap();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].tool, "bash");

    // 4) 切回 Yolo 模式验证嵌套 PreToolUse / PostToolUse Hook 拦截与参数改写。
    tools.approval.set_config(pp_config::config::ToolsSettings {
        approval_mode: ApprovalMode::Yolo,
        ..Default::default()
    });
    let mut hook = StubHook::new();
    hook.deny_tool = Some("rg".to_owned());
    hook.rewrite_write_content = Some("hook-rewritten\n".to_owned());
    hook.post_context = Some("nested-hook-ctx".to_owned());

    let execution = tools.execute_with_fetch(
        "eval",
        &json!({
            "language": "py",
            "code": "try:\n    rg('initial', 'sub')\n    r = 'ok'\nexcept ToolError as e:\n    r = str(e)\nwrite('sub/out.txt', 'original')\nprint(r)"
        })
        .to_string(),
        "eval-hook-call",
        ToolPolicy::Execute {
            workflow: &workflow,
        },
        Some(&hook),
        &token,
        &mut |_| {},
        &mut |_, _| unreachable!(),
    );
    let hook_val: Value = serde_json::from_str(&execution.output).unwrap();
    assert_eq!(hook_val["ok"], true, "{hook_val}");
    assert!(
        hook_val["output"]
            .as_str()
            .unwrap()
            .contains("hook blocked rg"),
        "{hook_val}"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("sub/out.txt")).unwrap(),
        "hook-rewritten\n"
    );
    assert!(
        execution
            .hook_context
            .iter()
            .any(|c| c == "nested-hook-ctx"),
        "{:?}",
        execution.hook_context
    );
    let hook_calls = hook.calls.lock().unwrap().clone();
    assert!(hook_calls.contains(&"pre:rg".to_owned()));
    assert!(hook_calls.contains(&"pre:write".to_owned()));
    assert!(hook_calls.contains(&"post:write".to_owned()));
}
