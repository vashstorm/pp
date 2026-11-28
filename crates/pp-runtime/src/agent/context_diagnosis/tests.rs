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
fn empty_session_and_reset_clear_context_diagnosis() {
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

        // A read-only diagnosis creates no session JSONL.
        assert_eq!(
            fs::read_dir(&sessions)
                .unwrap()
                .filter_map(Result::ok)
                .count(),
            0
        );
    }

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
            reasoning_tokens: None,
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
    // A nested/AGENTS.md that is active but not yet sent with a request must not count toward real statistics.
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

    // Activate the nested directory through read only after sending (simulating reading a subdirectory file without sending the next request).
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
        reasoning_tokens: None,
    });
    agent.session_output_tokens = 1234;

    // Inject a non-built-in MCP tool into the current session's tools list to verify the Built-in / Custom Tools split.
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

    // Built-in Tools and Custom Tools (MCP) are split strictly
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

    // Count only sent AGENTS.md files, not the unsent nested/AGENTS.md
    let instr = category(&report, CategoryKey::InstructionFiles);
    assert!(
        instr
            .items
            .iter()
            .any(|i| i.kind_label == "AGENTS.md" && i.title == "AGENTS.md")
    );
    assert!(!instr.items.iter().any(|i| i.title.contains("nested")));

    // Thinking is a separate category and does not mix into UserMessages / AssistantMessages
    let thinking = category(&report, CategoryKey::Thinking);
    assert_eq!(thinking.items.len(), 1);
    assert!(
        thinking.items[0]
            .preview
            .contains("<encrypted_content: 4500 chars")
    );

    // User / Assistant / Tool Calls / Tool Output are four independent categories
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

    // Build a real Completion history and persist it to JSONL + stats.db
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

    // Simulate a loaded project rule event + streamed thinking + an assistant reply with tool_call + a plain-text assistant reply
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

    // Write one matching server request usage row to stats.db
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
        reasoning_tokens: Some(90),
    });
    pp_stats::save(
        &ctx,
        "demo-model",
        "chat-completions",
        obs,
        std::time::Instant::now(),
        &Ok(()),
    );

    // Verify /context statistics after an Agent::restore (including Thinking, AGENTS.md, ToolCalls, server Usage)
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
    // When Thinking is not in context, it is excluded from the active_tokens split.
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
            reasoning_tokens: Some(90),
        })
    );
    assert_eq!(report.session_output_tokens, 340);
    assert!(
        category(&report, CategoryKey::InstructionFiles)
            .items
            .iter()
            .any(|i| i.kind_label == "AGENTS.md" && i.title == "AGENTS.md")
    );

    // Verify the explicit extraction function also ignores thinking from failed retries
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

/// Append one tool call + result to the native history (both protocols share the same shape).
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
fn context_diagnosis_preview_budget_cache_and_instructions() {
    {
        let within = format!(r#"{{"content":"{}"}}"#, "a".repeat(MAX_PREVIEW_BYTES - 32));
        assert!(within.len() < MAX_PREVIEW_BYTES);
        assert!(
            super::sections::json_body_preview(&within).contains("\n  \"content\""),
            "a JSON body within the budget is still pretty-printed as before"
        );

        let over = format!(r#"{{"content":"{}"}}"#, "a".repeat(MAX_PREVIEW_BYTES));
        assert!(over.len() > MAX_PREVIEW_BYTES);
        let preview = super::sections::json_body_preview(&over);
        assert_eq!(preview, super::sections::oversized_preview(over.len()));
        assert!(
            !preview.contains('a'),
            "an oversized body is no longer copied, parsed or pretty-printed"
        );
    }

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
            "tool output beyond the budget only gets a byte summary"
        );
        let small = outputs
            .items
            .iter()
            .find(|item| item.subtitle == "small.txt")
            .expect("small tool output item");
        assert!(
            small.preview.contains("\n  \"ok\": true"),
            "tool output within the budget is still pretty-printed as before: {}",
            small.preview
        );
        // Large bodies count into categories and totals by raw bytes and do not shrink because of the preview summary.
        assert!(outputs.tokens > MAX_PREVIEW_BYTES as u64 / 3);
        assert_eq!(in_context_tokens(&report), report.active_tokens);
    }

    {
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
            "tool output is labelled with the tool name from the prefix call table: {output_title}"
        );

        // Publishing the same state again: the cached prefix is reused and the same message's classification is not appended twice.
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

        // Append to the tail: only the new tail message is classified and the call table in the prefix is still reused.
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

        // A tool table fingerprint change (a new MCP tool) must recompute Custom Tools.
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

    {
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
        assert!(sent >= 1, "sent rules must count toward Instruction Files");

        // A second rule is sent in the second round: a project instruction fingerprint change must recompute that section.
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
}

fn snapshot_read(snapshot: &Snapshot) -> ContextReport {
    snapshot
        .read()
        .expect("report published")
        .expect("diagnosis succeeded")
        .as_ref()
        .clone()
}

/// Every consumer-visible field of every category and item, used to compare two reports field by field.
type ReportDigest = Vec<(
    CategoryKey,
    u64,
    u64,
    usize,
    bool,
    Vec<(String, String, String, u64, u64, String)>,
)>;

fn report_digest(report: &ContextReport) -> ReportDigest {
    report
        .categories
        .iter()
        .map(|cat| {
            (
                cat.key,
                cat.tokens,
                cat.bytes,
                cat.item_count,
                cat.in_context,
                cat.items
                    .iter()
                    .map(|item| {
                        (
                            item.kind_label.clone(),
                            item.title.clone(),
                            item.subtitle.clone(),
                            item.tokens,
                            item.bytes,
                            item.preview.clone(),
                        )
                    })
                    .collect(),
            )
        })
        .collect()
}

/// Push one native thinking item so the `thinking` classification has work to cache.
fn push_thinking(agent: &mut Agent, protocol: Protocol) {
    match protocol {
        Protocol::Response => match &mut agent.session {
            Session::Response(request) => {
                let InputParam::Items(items) = &mut request.input else {
                    panic!("expected items");
                };
                items.push(InputItem::Item(Item::Reasoning(ReasoningItem {
                    id: Some("rs_cache".into()),
                    summary: vec![SummaryPart::SummaryText(
                        async_openai::types::responses::SummaryTextContent {
                            text: "Cached thinking step".into(),
                        },
                    )],
                    content: None,
                    encrypted_content: Some("E".repeat(2048)),
                    status: None,
                })));
            }
            Session::Completion(_) => panic!("expected response session"),
        },
        Protocol::Completion => {
            agent.thinking_history.push(ThinkingEntry {
                thinking: "Cached thinking step".into(),
                summary: String::new(),
            });
        }
    }
}

/// Keep only a prefix of the native history, the way compaction and tree navigation replace it wholesale.
fn truncate_history(agent: &mut Agent, keep: usize) {
    match &mut agent.session {
        Session::Completion(request) => request.messages.truncate(keep.max(1)),
        Session::Response(request) => {
            if let InputParam::Items(items) = &mut request.input {
                items.truncate(keep);
            }
        }
    }
    let retained = agent.thinking_history.len().min(keep);
    crate::agent::context_diagnosis::trim_for_retained_turns(&mut agent.thinking_history, retained);
}

fn assert_published_matches_full(agent: &Agent, snapshot: &Snapshot, step: &str) {
    let published = snapshot
        .read()
        .unwrap_or_else(|| panic!("report published at {step}"))
        .unwrap_or_else(|error| panic!("diagnosis failed at {step}: {error}"));
    let full = agent
        .diagnose_context()
        .unwrap_or_else(|error| panic!("full diagnosis failed at {step}: {error}"));
    assert!(
        report_digest(&published) == report_digest(&full),
        "the cache-hit publish must equal a full re-diagnosis at {step}"
    );
}

/// Diagnostics are published repeatedly from a warm cache; each publish must reproduce a full `diagnose_context()`
/// field by field, both native protocols and with or without thinking records, and a report published earlier must not
/// change when the session grows afterwards.
#[test]
fn context_diagnosis_cached_publish_matches_full_diagnosis() {
    for protocol in [Protocol::Response, Protocol::Completion] {
        for with_thinking in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let cfg = dir.path().join("cfg");
            let work = dir.path().join("work");
            let sessions = dir.path().join("sessions");
            fs::create_dir_all(&cfg).unwrap();
            fs::create_dir_all(&work).unwrap();
            fs::create_dir_all(&sessions).unwrap();
            fs::write(work.join("AGENTS.md"), "Keep cached reports identical.").unwrap();

            let catalog = test_catalog(&cfg, protocol, Some(1_000_000));
            let mut agent = Agent::new(
                catalog,
                Tools::new(work.clone(), EditMode::Replace).unwrap(),
                sessions,
            )
            .unwrap();
            let snapshot = agent.observe_context();
            agent.refresh_instructions().unwrap();
            agent.append_user("compare cached and full reports");
            if with_thinking {
                push_thinking(&mut agent, protocol);
            }

            // Forty tool outputs just under the live preview budget keep multi-megabyte previews materialized,
            // so the shared-item invariant is exercised with the largest bodies the panel can actually show.
            let big = format!(r#"{{"ok":true,"content":"{}"}}"#, "x".repeat(63 * 1024));
            for index in 0..40 {
                push_tool_exchange(&mut agent, &format!("call_big_{index}"), "big.txt", &big);
            }
            for index in 0..10 {
                push_tool_exchange(
                    &mut agent,
                    &format!("call_small_{index}"),
                    "small.txt",
                    r#"{"ok":true,"content":"small"}"#,
                );
            }

            agent.publish_context_snapshot();
            assert_published_matches_full(&agent, &snapshot, "first publish");
            let published_early = snapshot.read().unwrap().unwrap();
            let early_digest = report_digest(&published_early);
            assert_eq!(early_digest.len(), 10);
            assert_eq!(
                category(&published_early, CategoryKey::ToolOutput).item_count,
                50
            );

            // Same state published again: the cached prefix is reused, the report is unchanged.
            agent.publish_context_snapshot();
            assert_published_matches_full(&agent, &snapshot, "repeated publish");
            assert!(
                report_digest(&published_early) == early_digest,
                "a published report must not change when the same state is published again"
            );

            // Appended tool exchanges extend the tail; the report handed out earlier keeps its content.
            push_tool_exchange(
                &mut agent,
                "call_tail_1",
                "tail.txt",
                r#"{"ok":true,"content":"tail"}"#,
            );
            push_tool_exchange(
                &mut agent,
                "call_tail_2",
                "tail.txt",
                r#"{"ok":true,"content":"tail"}"#,
            );
            agent.publish_context_snapshot();
            assert_published_matches_full(&agent, &snapshot, "appended tool exchange");
            assert_eq!(
                category(&snapshot_read(&snapshot), CategoryKey::ToolOutput).item_count,
                52
            );
            assert!(
                report_digest(&published_early) == early_digest,
                "a report published before the append must keep its items"
            );

            // A wholesale history replacement (compaction / tree navigation) drops the cached prefix.
            truncate_history(&mut agent, 4);
            agent.publish_context_snapshot();
            assert_published_matches_full(&agent, &snapshot, "replaced history");
            assert!(
                report_digest(&published_early) == early_digest,
                "a report published before a history replacement must keep its items"
            );

            // A tool-table switch must recompute the tool sections against the full diagnosis.
            let mut defs = agent.tools.all_definitions(agent.workflow.mode);
            defs.push(async_openai::types::chat::FunctionObject {
                name: "mcp__docs__search".into(),
                description: Some("Search docs via MCP".into()),
                parameters: Some(serde_json::json!({"type": "object", "properties": {}})),
                strict: Some(false),
            });
            agent.session.set_tools(defs);
            agent.publish_context_snapshot();
            assert_published_matches_full(&agent, &snapshot, "tool table switch");
            assert_eq!(
                category(&snapshot_read(&snapshot), CategoryKey::CustomTools).item_count,
                1
            );
            assert!(
                category(&published_early, CategoryKey::CustomTools)
                    .items
                    .is_empty(),
                "the tool table of an earlier report must not be rewritten"
            );
        }
    }
}
