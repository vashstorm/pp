use super::*;
use crate::tools::Tools;
use async_openai::types::responses::{
    EasyInputContent, FunctionCallOutput, FunctionCallOutputItemParam, InputContent, InputItem,
    Item, ReasoningItem, Role, SummaryPart,
};
use pp_config::config::{EditMode, ModelCatalog};
use pp_config::model_state::ModelState;
use pp_config::models::Protocol;
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
fn empty_session_reports_prepared_baseline_agents_skills_and_system_parts_without_side_effects() {
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
    assert!(report.prepared_tokens > 0);
    assert_eq!(report.context_window, Some(128_000));
    assert_eq!(report.protocol, "response");
    assert!(matches!(
        report.compaction.status,
        CompactionStatus::NotEnoughHistory
    ));
    assert!(report.system_items.iter().any(|i| i.kind_label == "Base"));
    assert!(
        report
            .system_items
            .iter()
            .any(|i| i.kind_label == "Runtime")
    );
    assert!(
        report
            .system_items
            .iter()
            .any(|i| i.kind_label == "Append" && i.preview.contains("Extra append instruction."))
    );
    assert!(
        report
            .agent_md_items
            .iter()
            .any(|i| i.kind_label == "AGENTS.md" && i.title == "AGENTS.md")
    );
    assert!(
        report
            .skill_items
            .iter()
            .any(|i| i.kind_label == "Skill Index" && i.title == "alpha")
    );
    assert!(
        report
            .tool_items
            .iter()
            .any(|i| i.kind_label == "Built-in" && i.title == "read")
    );
    assert!(!report.largest_items.is_empty());
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
fn history_diagnosis_links_tools_skills_reasoning_images_and_compaction_preview() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("cfg");
    let work = dir.path().join("work");
    let sessions = dir.path().join("sessions");
    fs::create_dir_all(&cfg).unwrap();
    fs::create_dir_all(&work).unwrap();
    fs::create_dir_all(&sessions).unwrap();

    let catalog = test_catalog(&cfg, Protocol::Response, Some(100_000));
    let mut agent = Agent::new(
        catalog,
        Tools::new(work, EditMode::Replace).unwrap(),
        sessions,
    )
    .unwrap();
    agent.last_request = Some(TokenUsage {
        input_tokens: 9500,
        output_tokens: 420,
        cached_tokens: Some(7200),
        cache_write_tokens: Some(800),
    });
    agent.session_output_tokens = 1234;

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
    assert!(report.compaction.has_existing_summary);
    assert_eq!(report.compaction.total_exchanges, 2);
    assert!(matches!(
        report.compaction.status,
        CompactionStatus::Ready {
            compressible_exchanges: 1,
            retained_exchanges: 1,
            ..
        }
    ));

    let skill_result = report
        .history_items
        .iter()
        .find(|i| i.kind_label == "Skill Body")
        .expect("skill read tool result");
    assert_eq!(skill_result.subtitle, "skill://alpha");
    assert!(
        report
            .skill_items
            .iter()
            .any(|i| i.kind_label == "Skill Body" && i.title == "skill://alpha")
    );

    let reasoning_item = report
        .history_items
        .iter()
        .find(|i| i.kind_label == "Reasoning")
        .expect("reasoning item");
    assert!(
        reasoning_item
            .preview
            .contains("<encrypted_content: 4500 chars")
    );

    let user_img_item = report
        .history_items
        .iter()
        .find(|i| i.kind_label == "User" && i.preview.contains("base64 image data"))
        .expect("sanitized image preview");
    assert!(!user_img_item.preview.contains("AAAA"));

    assert_eq!(report.largest_items[0].kind_label, "Skill Body");
    let sum_categories: u64 = report.categories.iter().map(|c| c.tokens).sum();
    assert!(sum_categories > 0);
}
