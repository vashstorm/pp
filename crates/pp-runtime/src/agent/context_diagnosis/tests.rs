use super::*;
use crate::agent::persistence::ReplayRecord;
use async_openai::types::responses::{
    EasyInputContent, FunctionCallOutput, FunctionCallOutputItemParam, InputContent, InputItem,
    InputParam, Item, ReasoningItem, Role, SummaryPart,
};
use pp_config::config::{EditMode, ModelCatalog};
use pp_config::model_state::ModelState;
use pp_config::models::Protocol;
use pp_core::cancellation::CancellationToken;
use pp_core::context::TokenUsage;
use pp_core::events::AgentEvent;
use pp_tools::Tools;
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

fn category(report: &ContextReport, key: CategoryKey) -> &CategorySummary {
    report
        .categories
        .iter()
        .find(|c| c.key == key)
        .expect("missing category")
}

#[test]
fn empty_session_reports_zero_usage_and_empty_categories_without_side_effects() {
    for protocol in [Protocol::Response, Protocol::Completion] {
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

        let catalog = test_catalog(&cfg, protocol, Some(128_000));
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
        assert_eq!(
            report.thinking_in_context,
            matches!(protocol, Protocol::Response)
        );
        assert!(report.last_request.is_none());
        assert_eq!(report.session_output_tokens, 0);
        assert_eq!(report.categories.len(), 10);
        for cat in &report.categories {
            assert_eq!(cat.tokens, 0, "category {:?} must be 0", cat.key);
            assert_eq!(cat.bytes, 0, "category {:?} bytes must be 0", cat.key);
            assert_eq!(cat.item_count, 0, "category {:?} count must be 0", cat.key);
            assert!(
                cat.items.is_empty(),
                "category {:?} must have no items",
                cat.key
            );
        }

        // 只读诊断不创建会话 JSONL。
        assert_eq!(
            fs::read_dir(&sessions)
                .unwrap()
                .filter_map(Result::ok)
                .count(),
            0
        );
    }
}

#[test]
fn reset_clears_context_diagnosis_and_snapshot_to_zero() {
    for protocol in [Protocol::Response, Protocol::Completion] {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("cfg");
        let work = dir.path().join("work");
        let sessions = dir.path().join("sessions");
        fs::create_dir_all(&cfg).unwrap();
        fs::create_dir_all(&work).unwrap();
        fs::create_dir_all(&sessions).unwrap();
        fs::write(work.join("AGENTS.md"), "Workspace rule.").unwrap();

        let catalog = test_catalog(&cfg, protocol, Some(128_000));
        let mut agent = Agent::new(
            catalog,
            Tools::new(work.clone(), EditMode::Replace).unwrap(),
            sessions,
        )
        .unwrap();
        let snapshot = agent.observe_context();

        agent.refresh_instructions().unwrap();
        agent.append_user("hello");
        agent.last_request = Some(TokenUsage {
            input_tokens: 500,
            output_tokens: 120,
            cached_tokens: Some(100),
            cache_write_tokens: None,
        });
        agent.session_output_tokens = 120;
        agent
            .thinking_history
            .push(crate::agent::context_diagnosis::ThinkingEntry {
                thinking: "thought".into(),
                summary: String::new(),
            });

        let active = agent.diagnose_context().unwrap();
        assert!(!active.empty_session);
        assert!(active.active_tokens > 0);

        agent.reset();
        agent.publish_context_snapshot();

        let after_reset = agent.diagnose_context().unwrap();
        let snap_report = snapshot.read().unwrap().unwrap();
        for report in [&after_reset, snap_report.as_ref()] {
            assert!(report.empty_session);
            assert_eq!(report.active_tokens, 0);
            assert_eq!(report.thinking_tokens, 0);
            assert_eq!(report.thinking_turns, 0);
            assert!(report.last_request.is_none());
            assert_eq!(report.session_output_tokens, 0);
            assert_eq!(report.categories.len(), 10);
            for cat in &report.categories {
                assert_eq!(cat.tokens, 0);
                assert_eq!(cat.bytes, 0);
                assert_eq!(cat.item_count, 0);
                assert!(cat.items.is_empty());
            }
        }
    }
}

#[test]
fn non_empty_session_splits_ten_categories_and_excludes_unsent_agents_md() {
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
        pp_tools::ToolPolicy::Execute {
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

    // 注入一个内置之外的 MCP 工具到当前 session 的 tools 列表，验证 Built-in / Custom Tools 分离。
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
    assert_eq!(report.categories.len(), 10);

    let in_ctx_sum: u64 = report
        .categories
        .iter()
        .filter(|c| c.in_context)
        .map(|c| c.tokens)
        .sum();
    assert_eq!(in_ctx_sum, report.active_tokens);

    // Built-in Tools 与 Custom Tools (MCP) 严格拆分
    let builtin = category(&report, CategoryKey::BuiltInTools);
    assert!(
        builtin
            .items
            .iter()
            .any(|i| i.kind_label == "Built-in" && i.title == "read")
    );
    assert!(!builtin.items.iter().any(|i| i.kind_label == "MCP"));

    let custom = category(&report, CategoryKey::CustomTools);
    assert!(
        custom
            .items
            .iter()
            .any(|i| i.kind_label == "MCP" && i.title == "docs / search")
    );
    assert!(!custom.items.iter().any(|i| i.kind_label == "Built-in"));

    // 只统计已发送的 AGENTS.md，不统计未发送的 nested/AGENTS.md
    let instr = category(&report, CategoryKey::InstructionFiles);
    assert!(
        instr
            .items
            .iter()
            .any(|i| i.kind_label == "AGENTS.md" && i.title == "AGENTS.md")
    );
    assert!(!instr.items.iter().any(|i| i.title.contains("nested")));

    // Thinking 独立分类，不混入 UserMessages / AssistantMessages
    let thinking = category(&report, CategoryKey::Thinking);
    assert_eq!(thinking.items.len(), 1);
    assert!(
        thinking.items[0]
            .preview
            .contains("<encrypted_content: 4500 chars")
    );

    // User / Assistant / Tool Calls / Tool Output 四分法各自独立
    let user_msgs = category(&report, CategoryKey::UserMessages);
    assert!(
        user_msgs
            .items
            .iter()
            .any(|i| i.kind_label == "User" && i.preview.contains("base64 image data"))
    );
    assert!(user_msgs.items.iter().any(|i| i.kind_label == "Summary"));
    let asst_msgs = category(&report, CategoryKey::AssistantMessages);
    assert_eq!(asst_msgs.items.len(), 1);
    assert_eq!(asst_msgs.items[0].subtitle, "Done.");

    let calls = category(&report, CategoryKey::ToolCalls);
    assert_eq!(calls.items.len(), 1);
    assert!(calls.items[0].title.contains("read(skill://alpha)"));

    let outputs = category(&report, CategoryKey::ToolOutput);
    assert!(
        outputs
            .items
            .iter()
            .any(|i| i.kind_label == "Skill Body" && i.subtitle == "skill://alpha")
    );
}

#[test]
fn completion_thinking_tool_calls_and_resumed_session_usage_are_diagnosed() {
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

    // 模拟加载的项目规则事件 + 流式思考 + 带 tool_call 的助手回复 + 纯正文助手回复
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
                    content: Some("Let me check.".into()),
                    tool_calls: Some(vec![
                        async_openai::types::chat::ChatCompletionMessageToolCalls::Function(
                            async_openai::types::chat::ChatCompletionMessageToolCall {
                                id: "call_chat_1".into(),
                                function: async_openai::types::chat::FunctionCall {
                                    name: "read".into(),
                                    arguments: r#"{"path":"AGENTS.md"}"#.into(),
                                },
                            },
                        ),
                    ]),
                    ..Default::default()
                },
            ),
        );
        req.messages.push(
            async_openai::types::chat::ChatCompletionRequestMessage::Tool(
                async_openai::types::chat::ChatCompletionRequestToolMessage {
                    content: "Keep completion tests accurate.".into(),
                    tool_call_id: "call_chat_1".into(),
                },
            ),
        );
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
        database: stats_db.clone().into(),
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

    // 验证通过 Agent::restore 恢复后的 /context 统计（含 Thinking、AGENTS.md、ToolCalls、服务端 Usage）
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
    let thinking = category(&report, CategoryKey::Thinking);
    assert!(!thinking.in_context);
    assert!(
        thinking
            .items
            .iter()
            .any(|i| i.preview.contains("Step 1: inspect session."))
    );
    // Thinking 不在上下文中时，不计入 active_tokens 切分。
    let in_ctx_sum: u64 = report
        .categories
        .iter()
        .filter(|c| c.in_context)
        .map(|c| c.tokens)
        .sum();
    assert_eq!(in_ctx_sum, report.active_tokens);

    assert_eq!(category(&report, CategoryKey::ToolCalls).items.len(), 1);
    assert_eq!(
        category(&report, CategoryKey::AssistantMessages)
            .items
            .len(),
        2
    );
    assert_eq!(category(&report, CategoryKey::ToolOutput).items.len(), 1);

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
        category(&report, CategoryKey::InstructionFiles)
            .items
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

/// 追加一次工具调用 + 结果到原生历史（两种协议共用同一形状）。
fn push_tool_exchange(agent: &mut Agent, call_id: &str, path: &str, output: &str) {
    let arguments = format!(r#"{{"path":"{path}"}}"#);
    match &mut agent.session {
        Session::Completion(request) => {
            request.messages.push(
                async_openai::types::chat::ChatCompletionRequestMessage::Assistant(
                    async_openai::types::chat::ChatCompletionRequestAssistantMessage {
                        tool_calls: Some(vec![
                            async_openai::types::chat::ChatCompletionMessageToolCalls::Function(
                                async_openai::types::chat::ChatCompletionMessageToolCall {
                                    id: call_id.into(),
                                    function: async_openai::types::chat::FunctionCall {
                                        name: "read".into(),
                                        arguments,
                                    },
                                },
                            ),
                        ]),
                        ..Default::default()
                    },
                ),
            );
            request.messages.push(
                async_openai::types::chat::ChatCompletionRequestMessage::Tool(
                    async_openai::types::chat::ChatCompletionRequestToolMessage {
                        content: output.into(),
                        tool_call_id: call_id.into(),
                    },
                ),
            );
        }
        Session::Response(request) => {
            let InputParam::Items(items) = &mut request.input else {
                panic!("expected items");
            };
            items.push(InputItem::Item(Item::FunctionCall(
                async_openai::types::responses::FunctionToolCall {
                    arguments,
                    call_id: call_id.into(),
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
                    call_id: Some(call_id.into()),
                    output: FunctionCallOutput::Text(output.into()),
                    id: None,
                    status: None,
                    name: None,
                    namespace: None,
                    caller: None,
                },
            )));
        }
    }
}

fn in_context_tokens(report: &ContextReport) -> u64 {
    report
        .categories
        .iter()
        .filter(|category| category.in_context)
        .map(|category| category.tokens)
        .sum()
}

#[test]
fn json_body_preview_skips_parsing_above_the_preview_budget() {
    let within = format!(r#"{{"content":"{}"}}"#, "a".repeat(MAX_PREVIEW_BYTES - 32));
    assert!(within.len() < MAX_PREVIEW_BYTES);
    assert!(
        super::sections::json_body_preview(&within).contains("\n  \"content\""),
        "预算内的 JSON 正文仍按既有口径美化"
    );

    let over = format!(r#"{{"content":"{}"}}"#, "a".repeat(MAX_PREVIEW_BYTES));
    assert!(over.len() > MAX_PREVIEW_BYTES);
    let preview = super::sections::json_body_preview(&over);
    assert_eq!(preview, super::sections::oversized_preview(over.len()));
    assert!(!preview.contains('a'), "超大正文不再复制、解析或美化");
}

#[test]
fn oversized_tool_output_is_summarized_but_still_accounted() {
    for protocol in [Protocol::Response, Protocol::Completion] {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("cfg");
        let work = dir.path().join("work");
        let sessions = dir.path().join("sessions");
        fs::create_dir_all(&cfg).unwrap();
        fs::create_dir_all(&work).unwrap();
        fs::create_dir_all(&sessions).unwrap();
        fs::write(work.join("AGENTS.md"), "Keep diagnostics bounded.").unwrap();

        let catalog = test_catalog(&cfg, protocol, Some(200_000));
        let mut agent = Agent::new(
            catalog,
            Tools::new(work.clone(), EditMode::Replace).unwrap(),
            sessions,
        )
        .unwrap();
        agent.refresh_instructions().unwrap();
        agent.append_user("read the big file");

        let huge = format!(r#"{{"ok":true,"content":"{}"}}"#, "A".repeat(80_000));
        push_tool_exchange(&mut agent, "call_big", "big.txt", &huge);
        push_tool_exchange(
            &mut agent,
            "call_small",
            "small.txt",
            r#"{"ok":true,"content":"small"}"#,
        );

        let report = agent.diagnose_context().unwrap();
        assert_eq!(report.categories.len(), 10);
        let outputs = category(&report, CategoryKey::ToolOutput);
        let big = outputs
            .items
            .iter()
            .find(|item| item.subtitle == "big.txt")
            .expect("big tool output item");
        assert!(big.bytes > MAX_PREVIEW_BYTES as u64);
        assert_eq!(
            big.preview,
            super::sections::oversized_preview(huge.len()),
            "超过预算的工具输出只给字节摘要"
        );
        let small = outputs
            .items
            .iter()
            .find(|item| item.subtitle == "small.txt")
            .expect("small tool output item");
        assert!(
            small.preview.contains("\n  \"ok\": true"),
            "预算内的工具输出仍按既有口径美化：{}",
            small.preview
        );
        // 大正文按原始字节计入分类与总额，不因预览摘要而缩水。
        assert!(outputs.tokens > MAX_PREVIEW_BYTES as u64 / 3);
        assert_eq!(in_context_tokens(&report), report.active_tokens);
    }
}

#[test]
fn published_snapshot_reuses_cached_prefix_without_duplicating_items() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("cfg");
    let work = dir.path().join("work");
    let sessions = dir.path().join("sessions");
    fs::create_dir_all(&cfg).unwrap();
    fs::create_dir_all(&work).unwrap();
    fs::create_dir_all(&sessions).unwrap();
    fs::write(work.join("AGENTS.md"), "Keep diagnostics cached.").unwrap();

    let catalog = test_catalog(&cfg, Protocol::Response, Some(200_000));
    let mut agent = Agent::new(
        catalog,
        Tools::new(work.clone(), EditMode::Replace).unwrap(),
        sessions,
    )
    .unwrap();
    let snapshot = agent.observe_context();
    agent.refresh_instructions().unwrap();
    agent.append_user("first");
    push_tool_exchange(&mut agent, "call_1", "AGENTS.md", r#"{"ok":true,"rule":1}"#);

    agent.publish_context_snapshot();
    let first = snapshot_read(&snapshot);
    assert_eq!(category(&first, CategoryKey::UserMessages).item_count, 1);
    assert_eq!(category(&first, CategoryKey::ToolCalls).item_count, 1);
    let output_title = category(&first, CategoryKey::ToolOutput).items[0]
        .title
        .clone();
    assert!(
        output_title.contains("read"),
        "工具输出按前缀调用表标注工具名：{output_title}"
    );

    // 相同状态再次发布：复用缓存前缀，不能重复追加同一条消息的分类。
    agent.publish_context_snapshot();
    let repeat = snapshot_read(&snapshot);
    assert_eq!(repeat.categories.len(), 10);
    for (left, right) in first.categories.iter().zip(&repeat.categories) {
        assert_eq!(left.key, right.key);
        assert_eq!(left.item_count, right.item_count, "category {:?}", left.key);
        assert_eq!(left.bytes, right.bytes, "category {:?}", left.key);
        assert_eq!(left.tokens, right.tokens, "category {:?}", left.key);
    }
    assert_eq!(in_context_tokens(&repeat), repeat.active_tokens);

    // 追加尾部：只新增尾部消息的分类，前缀里的调用表仍被复用。
    agent.append_user("second");
    push_tool_exchange(&mut agent, "call_2", "AGENTS.md", r#"{"ok":true,"rule":2}"#);
    agent.publish_context_snapshot();
    let extended = snapshot_read(&snapshot);
    assert_eq!(category(&extended, CategoryKey::UserMessages).item_count, 2);
    assert_eq!(category(&extended, CategoryKey::ToolCalls).item_count, 2);
    assert_eq!(category(&extended, CategoryKey::ToolOutput).item_count, 2);
    assert_eq!(
        category(&extended, CategoryKey::ToolOutput).items[0].title,
        output_title
    );
    assert!(
        category(&extended, CategoryKey::ToolOutput).items[1]
            .title
            .contains("read")
    );
    assert_eq!(in_context_tokens(&extended), extended.active_tokens);

    // 工具表指纹变化（新增 MCP 工具）必须重算 Custom Tools。
    assert_eq!(category(&extended, CategoryKey::CustomTools).item_count, 0);
    let mut defs = agent.tools.all_definitions(agent.workflow.mode);
    defs.push(async_openai::types::chat::FunctionObject {
        name: "mcp__docs__search".into(),
        description: Some("Search docs via MCP".into()),
        parameters: Some(serde_json::json!({"type": "object", "properties": {}})),
        strict: Some(false),
    });
    agent.session.set_tools(defs);
    agent.publish_context_snapshot();
    let with_mcp = snapshot_read(&snapshot);
    assert_eq!(category(&with_mcp, CategoryKey::CustomTools).item_count, 1);
    assert_eq!(in_context_tokens(&with_mcp), with_mcp.active_tokens);
}

#[test]
fn reported_project_load_change_recomputes_instruction_section() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("cfg");
    let work = dir.path().join("work");
    let sessions = dir.path().join("sessions");
    fs::create_dir_all(&cfg).unwrap();
    fs::create_dir_all(work.join("nested")).unwrap();
    fs::create_dir_all(&sessions).unwrap();
    fs::write(work.join("AGENTS.md"), "Root sent rule.").unwrap();

    let catalog = test_catalog(&cfg, Protocol::Response, Some(200_000));
    let mut agent = Agent::new(
        catalog,
        Tools::new(work.clone(), EditMode::Replace).unwrap(),
        sessions,
    )
    .unwrap();
    let snapshot = agent.observe_context();
    agent.refresh_instructions().unwrap();
    agent.tools.project_request_sent();
    for doc in std::mem::take(&mut agent.pending_project_load) {
        agent
            .reported_project_load
            .insert(doc.path.display().to_string(), doc.content);
    }
    agent.append_user("first");

    agent.publish_context_snapshot();
    let first = snapshot_read(&snapshot);
    let sent = category(&first, CategoryKey::InstructionFiles).item_count;
    assert!(sent >= 1, "已发送的规则必须计入 Instruction Files");

    // 第二轮又发送一份规则：项目指令指纹变化必须重算该分区。
    let nested = work.join("nested/AGENTS.md");
    fs::write(&nested, "Nested rule.").unwrap();
    agent
        .reported_project_load
        .insert(nested.display().to_string(), "Nested rule.".into());
    agent.publish_context_snapshot();
    let second = snapshot_read(&snapshot);
    assert_eq!(
        category(&second, CategoryKey::InstructionFiles).item_count,
        sent + 1
    );
    assert!(
        category(&second, CategoryKey::InstructionFiles)
            .items
            .iter()
            .any(|item| item.title.ends_with("nested/AGENTS.md"))
    );
    assert_eq!(in_context_tokens(&second), second.active_tokens);
}

fn snapshot_read(snapshot: &Snapshot) -> ContextReport {
    snapshot
        .read()
        .expect("report published")
        .expect("diagnosis succeeded")
        .as_ref()
        .clone()
}
