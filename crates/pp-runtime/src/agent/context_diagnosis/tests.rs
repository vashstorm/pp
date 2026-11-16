use super::*;
use crate::agent::persistence::ReplayRecord;
use crate::tools::Tools;
use async_openai::types::responses::{
    EasyInputContent, FunctionCallOutput, FunctionCallOutputItemParam, InputContent, InputItem,
    Item, ReasoningItem, Role, SummaryPart,
};
use pp_config::config::{EditMode, ModelCatalog};
use pp_config::model_state::ModelState;
use pp_config::models::Protocol;
use pp_core::cancellation::CancellationToken;
use pp_core::events::AgentEvent;
use std::fs;

fn test_catalog(
    dir: &std::path::Path,
    protocol: Protocol,
    context_window: Option<u64>,
) -> ModelCatalog {
    let proto = match protocol {
        Protocol::Completion => "completion",
        Protocol::Response => "response",
    };
    let window = context_window
        .map(|w| format!("        context_window: {w}\n"))
        .unwrap_or_default();
    fs::write(
        dir.join("models.yml"),
        format!(
            "providers:\n  test:\n    api_key: test-key\n    base_url: http://127.0.0.1:1/v1\n    protocol: {proto}\n    models:\n      demo:\n        model: demo-model\n{window}"
        ),
    )
    .unwrap();
    ModelCatalog::from_files(&dir.join("settings.yml"), &dir.join("models.yml")).unwrap()
}

#[test]
fn empty_session_reports_zero_stats_without_prefill_or_side_effects() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("cfg");
    let work = dir.path().join("work");
    let sessions = dir.path().join("sessions");
    fs::create_dir_all(&cfg).unwrap();
    fs::create_dir_all(work.join(".pp")).unwrap();
    fs::create_dir_all(&sessions).unwrap();

    fs::write(work.join("AGENTS.md"), "Workspace rule: keep tests fast.").unwrap();
    fs::write(
        work.join(".pp/APPEND_SYSTEM.md"),
        "Extra append instruction.",
    )
    .unwrap();
    fs::write(cfg.join("settings.yml"), "skill_dirs:\n  - skills\n").unwrap();
    let skill_dir = cfg.join("skills/alpha");
    fs::create_dir_all(&skill_dir).unwrap();
    fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: alpha\ndescription: Alpha helper\n---\n\nFull alpha body.",
    )
    .unwrap();

    let catalog = test_catalog(&cfg, Protocol::Response, Some(128_000));
    let mut tools = Tools::new(work.clone(), EditMode::Replace).unwrap();
    let skills = pp_config::skills::Skills::from_dir(&cfg).shared();
    tools.skills = skills;
    let prompt_source =
        crate::system_prompt::Snapshot::discover(&work, &cfg, None, None, None).unwrap();

    let agent = Agent::new_with_prompt(
        ModelState::shared(catalog),
        tools,
        sessions.clone(),
        sessions.join(".locks"),
        prompt_source,
        sessions.join(".stats/stats.db"),
    )
    .unwrap();

    let report = agent.diagnose_context().unwrap();
    assert!(report.empty_session);
    assert_eq!(report.active_tokens, 0);
    assert_eq!(report.thinking_tokens, 0);
    assert_eq!(report.thinking_turns, 0);
    assert!(report.categories.is_empty());
    assert!(report.system_items.is_empty());
    assert!(report.agent_md_items.is_empty());
    assert!(report.skill_items.is_empty());
    assert!(report.tool_items.is_empty());
    assert!(report.mcp_items.is_empty());
    assert!(report.message_items.is_empty());
    assert!(report.thinking_items.is_empty());
    assert!(report.tool_result_items.is_empty());
    // 只读诊断不创建会话 JSONL。
    assert_eq!(
        fs::read_dir(&sessions)
            .unwrap()
            .filter_map(Result::ok)
            .count(),
        0
    );
}

#[test]
fn non_empty_session_splits_tools_mcp_thinking_and_excludes_unsent_agents_md() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("cfg");
    let work = dir.path().join("work");
    let sessions = dir.path().join("sessions");
    fs::create_dir_all(&cfg).unwrap();
    fs::create_dir_all(work.join("nested")).unwrap();
    fs::create_dir_all(&sessions).unwrap();

    fs::write(work.join("AGENTS.md"), "Root sent rule.").unwrap();
    // 激活但尚未随请求发送的 nested/AGENTS.md 不应计入真实统计。
    fs::write(work.join("nested/AGENTS.md"), "Unsent nested rule.").unwrap();

    let catalog = test_catalog(&cfg, Protocol::Response, Some(100_000));
    let mut agent = Agent::new(
        catalog,
        Tools::new(work.clone(), EditMode::Replace).unwrap(),
        sessions,
    )
    .unwrap();
    agent.refresh_instructions().unwrap();
    agent.tools.project_request_sent();
    for doc in std::mem::take(&mut agent.pending_project_load) {
        agent
            .reported_project_load
            .insert(doc.path.display().to_string(), doc.content);
    }

    // 在发送后才通过 read 激活 nested 目录（模拟读了子目录文件但尚未发送下一轮请求）。
    fs::write(work.join("nested/file.txt"), "hello").unwrap();
    let read_res = agent.tools.execute(
        "read",
        r#"{"path":"nested/file.txt"}"#,
        "call_nested",
        crate::tools::ToolPolicy::Execute {
            workflow: &agent.workflow,
        },
        &CancellationToken::new(),
        &mut |_| {},
    );
    assert!(read_res.stop_error.is_none());

    agent.last_request = Some(TokenUsage {
        input_tokens: 9500,
        output_tokens: 420,
        cached_tokens: Some(7200),
        cache_write_tokens: Some(800),
    });
    agent.session_output_tokens = 1234;

    // 注入一个内置之外的 MCP 工具到当前 session 的 tools 列表，验证 Tools / MCP 分离。
    let mut defs = agent.tools.all_definitions(agent.workflow.mode);
    defs.push(async_openai::types::chat::FunctionObject {
        name: "mcp__docs__search".into(),
        description: Some("Search docs via MCP".into()),
        parameters: Some(serde_json::json!({"type": "object", "properties": {}})),
        strict: Some(false),
    });
    agent.session.set_tools(defs);

    let huge_skill_body = "S".repeat(15000);
    let encrypted = "E".repeat(4500);
    let image_b64 = format!("data:image/png;base64,{}", "A".repeat(4000));

    let Session::Response(request) = &mut agent.session else {
        panic!("expected response session");
    };
    let InputParam::Items(items) = &mut request.input else {
        panic!("expected items");
    };
    items.push(InputItem::EasyMessage(
        async_openai::types::responses::EasyInputMessage {
            role: Role::User,
            content: "[Context summary]\nEarlier context summary.\n[/Context summary]".into(),
            ..Default::default()
        },
    ));
    items.push(InputItem::EasyMessage(
        async_openai::types::responses::EasyInputMessage {
            role: Role::User,
            content: EasyInputContent::ContentList(vec![
                InputContent::InputText(async_openai::types::responses::InputTextContent {
                    text: "Read the alpha skill".into(),
                    prompt_cache_breakpoint: None,
                }),
                InputContent::InputImage(async_openai::types::responses::InputImageContent {
                    image_url: Some(image_b64),
                    ..Default::default()
                }),
            ]),
            ..Default::default()
        },
    ));
    items.push(InputItem::Item(Item::Reasoning(ReasoningItem {
        id: Some("rs_1".into()),
        summary: vec![SummaryPart::SummaryText(
            async_openai::types::responses::SummaryTextContent {
                text: "Thinking step".into(),
            },
        )],
        content: None,
        encrypted_content: Some(encrypted),
        status: None,
    })));
    items.push(InputItem::Item(Item::FunctionCall(
        async_openai::types::responses::FunctionToolCall {
            arguments: r#"{"path":"skill://alpha"}"#.into(),
            call_id: "call_1".into(),
            name: "read".into(),
            id: None,
            status: None,
            namespace: None,
            caller: None,
            r#async: None,
        },
    )));
    items.push(InputItem::Item(Item::FunctionCallOutput(
        FunctionCallOutputItemParam {
            call_id: Some("call_1".into()),
            output: FunctionCallOutput::Text(format!(
                r#"{{"ok":true,"uri":"skill://alpha","content":"{huge_skill_body}"}}"#
            )),
            id: None,
            status: None,
            name: None,
            namespace: None,
            caller: None,
        },
    )));
    items.push(InputItem::EasyMessage(
        async_openai::types::responses::EasyInputMessage {
            role: Role::User,
            content: "Follow up turn".into(),
            ..Default::default()
        },
    ));
    items.push(InputItem::EasyMessage(
        async_openai::types::responses::EasyInputMessage {
            role: Role::Assistant,
            content: "Done.".into(),
            ..Default::default()
        },
    ));

    let report = agent.diagnose_context().unwrap();
    assert!(!report.empty_session);
    assert!(report.active_tokens > 0);
    assert_eq!(report.last_request.unwrap().cached_tokens, Some(7200));
    assert_eq!(report.session_output_tokens, 1234);
    assert!(report.thinking_in_context);
    assert!(report.thinking_tokens > 0);
    assert_eq!(report.thinking_turns, 1);

    assert!(matches!(
        report.compaction.status,
        CompactionStatus::Ready {
            compressible_exchanges: 1,
            retained_exchanges: 1,
            ..
        }
    ));

    // Built-in Tools 与 MCP 严格拆分
    assert!(
        report
            .tool_items
            .iter()
            .any(|i| i.kind_label == "Built-in" && i.title == "read")
    );
    assert!(!report.tool_items.iter().any(|i| i.kind_label == "MCP"));
    assert!(
        report
            .mcp_items
            .iter()
            .any(|i| i.kind_label == "MCP" && i.title == "docs / search")
    );
    assert!(!report.mcp_items.iter().any(|i| i.kind_label == "Built-in"));

    // 只统计已发送的 AGENTS.md，不统计未发送的 nested/AGENTS.md
    assert!(
        report
            .agent_md_items
            .iter()
            .any(|i| i.kind_label == "AGENTS.md" && i.title == "AGENTS.md")
    );
    assert!(
        !report
            .agent_md_items
            .iter()
            .any(|i| i.title.contains("nested"))
    );

    // Thinking 独立分类，不混入 message_items
    assert_eq!(report.thinking_items.len(), 2); // Summary Row + Turn 1
    assert!(
        report.thinking_items[1]
            .preview
            .contains("<encrypted_content: 4500 chars")
    );
    assert!(
        !report
            .message_items
            .iter()
            .any(|i| i.kind_label == "Reasoning" || i.kind_label == "Thinking")
    );

    // Tool Results 与 Messages 分离
    assert!(
        report
            .tool_result_items
            .iter()
            .any(|i| i.kind_label == "Skill Body" && i.subtitle == "skill://alpha")
    );
    assert!(
        report
            .skill_items
            .iter()
            .any(|i| i.kind_label == "Skill Body" && i.title == "skill://alpha")
    );
    assert!(
        report
            .message_items
            .iter()
            .any(|i| i.kind_label == "User" && i.preview.contains("base64 image data"))
    );
    assert_eq!(report.categories.len(), 8);
}

#[test]
fn completion_thinking_and_resumed_session_usage_are_diagnosed() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("cfg");
    let work = dir.path().join("work");
    let sessions = dir.path().join("sessions");
    fs::create_dir_all(&cfg).unwrap();
    fs::create_dir_all(&work).unwrap();
    fs::create_dir_all(&sessions).unwrap();

    fs::write(work.join("AGENTS.md"), "Keep completion tests accurate.").unwrap();

    let catalog = test_catalog(&cfg, Protocol::Completion, Some(64_000));
    let mut agent = Agent::new(
        catalog.clone(),
        Tools::new(work.clone(), EditMode::Replace).unwrap(),
        sessions.clone(),
    )
    .unwrap();
    agent.refresh_instructions().unwrap();
    let system = agent.session_system_text().unwrap().to_owned();
    agent.append_user("Explain thinking stats");
    let metadata = crate::agent::persistence::Metadata {
        role_route: None,
        cwd: &work,
        model_reference: "test/demo",
        think_level: ThinkLevel::Medium,
        edit_mode: EditMode::Replace,
        tool_limits: agent.tools.limits,
        base_system: &agent.base_system,
        prompt_source: agent.prompt_source.as_ref(),
    };

    // 构造一条真实的 Completion 历史并持久化到 JSONL + stats.db
    agent
        .journal
        .sent(
            &metadata,
            (&agent.session).into(),
            &agent.workflow,
            Some("Explain thinking stats"),
            None,
        )
        .unwrap();
    let session_id = agent.session_id().unwrap().to_owned();
    let session_file = sessions.join(format!("{session_id}.jsonl"));

    // 模拟加载的项目规则事件 + 流式思考 + 助手回复
    agent.journal.event(&AgentEvent::ProjectInstructionsLoaded {
        path: work.join("AGENTS.md").display().to_string(),
        content: "Keep completion tests accurate.".into(),
    });
    agent.journal.event(&AgentEvent::AssistantStarted);
    agent.journal.event(&AgentEvent::ThinkingDelta {
        text: "Step 1: inspect session.\nStep 2: aggregate thinking.".into(),
    });
    agent.journal.event(&AgentEvent::AssistantFinished {
        text: "Here is the answer.".into(),
    });
    if let Session::Completion(req) = &mut agent.session {
        req.messages.push(
            async_openai::types::chat::ChatCompletionRequestMessage::Assistant(
                async_openai::types::chat::ChatCompletionRequestAssistantMessage {
                    content: Some("Here is the answer.".into()),
                    ..Default::default()
                },
            ),
        );
    }
    agent.journal.commit((&agent.session).into()).unwrap();
    drop(agent);

    // 写入一条对应的服务端请求用量到 stats.db
    let stats_db = sessions.join(".stats/stats.db");
    let ctx = pp_stats::RequestContext {
        model_reference: "test/demo".into(),
        database: stats_db.clone(),
        cwd: work.clone(),
        purpose: "task",
        session_file: Some(session_file),
        unlinked_requests: None,
        first_request: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        skills: Vec::new(),
    };
    let mut obs = pp_stats::Observation::new();
    obs.usage = Some(TokenUsage {
        input_tokens: 1200,
        output_tokens: 340,
        cached_tokens: Some(600),
        cache_write_tokens: None,
    });
    pp_stats::save(
        &ctx,
        "demo-model",
        "chat-completions",
        obs,
        std::time::Instant::now(),
        &Ok(()),
    );

    // 验证通过 Agent::restore 恢复后的 /context 统计（含 Thinking、AGENTS.md、服务端 Usage）
    let restored = crate::agent::persistence::load(
        &sessions,
        &sessions.join(".locks"),
        Some(&session_id),
        true,
        &stats_db,
    )
    .unwrap();
    let restored_agent = Agent::restore(
        ModelState::shared(catalog),
        Tools::new(work, EditMode::Replace).unwrap(),
        restored,
    )
    .unwrap();
    assert_eq!(restored_agent.session_system_text().unwrap(), system);

    let report = restored_agent.diagnose_context().unwrap();
    assert!(!report.empty_session);
    assert!(!report.thinking_in_context);
    assert_eq!(report.thinking_turns, 1);
    assert!(report.thinking_tokens > 0);
    assert!(
        report
            .thinking_items
            .iter()
            .any(|i| i.preview.contains("Step 1: inspect session."))
    );
    assert_eq!(
        report.last_request,
        Some(TokenUsage {
            input_tokens: 1200,
            output_tokens: 340,
            cached_tokens: Some(600),
            cache_write_tokens: None,
        })
    );
    assert_eq!(report.session_output_tokens, 340);
    assert!(
        report
            .agent_md_items
            .iter()
            .any(|i| i.kind_label == "AGENTS.md" && i.title == "AGENTS.md")
    );

    // 验证显式提取函数也能正确忽略失败重试的思考
    let records = vec![
        ReplayRecord::Event(AgentEvent::AssistantStarted),
        ReplayRecord::Event(AgentEvent::ThinkingDelta {
            text: "failed attempt thought".into(),
        }),
        ReplayRecord::Event(AgentEvent::ModelAttemptFailed {
            attempt: 1,
            model: "demo".into(),
            error: "503".into(),
        }),
        ReplayRecord::Event(AgentEvent::AssistantStarted),
        ReplayRecord::Event(AgentEvent::ThinkingDelta {
            text: "successful thought".into(),
        }),
        ReplayRecord::Event(AgentEvent::AssistantFinished { text: "ok".into() }),
    ];
    let extracted = extract_thinking_from_replay(&records);
    assert_eq!(extracted.len(), 1);
    assert_eq!(extracted[0].thinking, "successful thought");
    let _ = CancellationToken::new();
}
