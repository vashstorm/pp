//! Instruction refresh behaviour: an unchanged round must skip rebuilding the tool table, the project
//! block and the system prompt, while every input change still reaches the very next round.
//!
//! Every case runs for both native protocols: the refresh key must behave identically for Chat Completions
//! and Responses sessions.
use super::*;

const PROTOCOLS: [&str; 2] = ["completion", "response"];

const PROJECT_RULES: &str =
    "# Project rules\n\n- FIXTURE-RULE: keep the fixture instruction body.\n";

/// Agent whose provider never receives a request: these cases only exercise prompt and tool-table refreshes.
///
/// The provider disables `prompt_cache_key` so two Agents in the same directory produce the same native
/// request bytes.
fn refresh_agent(dir: &std::path::Path, sessions: &str, protocol: &str) -> Agent {
    let settings = dir.join("settings.yml");
    std::fs::write(&settings, "rate_limit_retry_budget_seconds: 0\n").unwrap();
    let models = dir.join("models.yml");
    std::fs::write(
        &models,
        json!({"providers":{"fixture": {
            "api_key":"fixture-key",
            "protocol":protocol,
            "base_url":"http://127.0.0.1:1/v1",
            "prompt_cache_key": false,
            "models":{"one":{"model":"one"}}
        }}})
        .to_string(),
    )
    .unwrap();
    Agent::new(
        ModelCatalog::from_files(&settings, &models).unwrap(),
        Tools::new(dir.to_owned(), EditMode::Replace).unwrap(),
        dir.join(sessions),
    )
    .unwrap()
}

/// System prompt currently installed in the native request.
fn system_prompt(agent: &Agent) -> String {
    match &agent.session {
        Session::Completion(request) => match request.messages.first() {
            Some(ChatCompletionRequestMessage::System(message)) => match &message.content {
                async_openai::types::chat::ChatCompletionRequestSystemMessageContent::Text(
                    text,
                ) => text.clone(),
                _ => String::new(),
            },
            _ => String::new(),
        },
        Session::Response(request) => request.instructions.clone().unwrap_or_default(),
    }
}

/// Tool table the next request would send.
fn session_definitions(agent: &Agent) -> Vec<async_openai::types::chat::FunctionObject> {
    match &agent.session {
        Session::Completion(request) => request
            .tools
            .iter()
            .flatten()
            .map(|tool| match tool {
                ChatCompletionTools::Function(ChatCompletionTool { function }) => function.clone(),
                _ => panic!("the fixture only registers function tools"),
            })
            .collect(),
        Session::Response(request) => request
            .tools
            .iter()
            .flatten()
            .map(|tool| match tool {
                Tool::Function(function) => Session::function_from_response_tool(function),
                _ => panic!("the fixture only registers function tools"),
            })
            .collect(),
    }
}

fn has_tool(agent: &Agent, name: &str) -> bool {
    session_definitions(agent)
        .iter()
        .any(|definition| definition.name == name)
}

/// Developer messages appended after the system prompt, in order.
fn developer_messages(agent: &Agent) -> Vec<String> {
    match &agent.session {
        Session::Completion(request) => request
            .messages
            .iter()
            .filter_map(|message| {
                match message {
                ChatCompletionRequestMessage::Developer(message) => match &message.content {
                    async_openai::types::chat::ChatCompletionRequestDeveloperMessageContent::Text(
                        text,
                    ) => Some(text.clone()),
                    _ => None,
                },
                _ => None,
            }
            })
            .collect(),
        Session::Response(request) => match &request.input {
            InputParam::Items(items) => items
                .iter()
                .filter_map(|item| match item {
                    InputItem::EasyMessage(message) if message.role == Role::Developer => {
                        match &message.content {
                            async_openai::types::responses::EasyInputContent::Text(text) => {
                                Some(text.clone())
                            }
                            _ => None,
                        }
                    }
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        },
    }
}

/// Steady state of a running task: the session has history, the static project prefix is frozen and the
/// request boundary already published every loaded rule.
fn start_task(agent: &mut Agent) {
    super::append_user_to(&mut agent.session, "first task");
    let documents = agent.tools.project_documents().unwrap();
    agent.reported_project_load = documents
        .iter()
        .map(|document| {
            (
                document.path.display().to_string(),
                document.content.clone(),
            )
        })
        .collect();
    agent.pending_project_load.clear();
}

fn root_binding(dir: &std::path::Path) -> crate::subagent::Binding {
    let binding = crate::subagent::Binding::root(
        crate::subagent::RootOptions {
            dir: dir.join("subagents"),
            user: dir.join("user"),
            cwd: dir.to_owned(),
            model: "fixture/one".into(),
            thinking: pp_core::model::ThinkLevel::None,
            settings: pp_config::config::TaskSettings::default(),
            plan_mode: false,
        },
        std::sync::Arc::new(|_, _| anyhow::bail!("fixture spawn is disabled")),
        std::sync::Arc::new(|_| Ok(())),
    )
    .unwrap();
    binding.set_model_roles(std::collections::BTreeMap::from([(
        pp_config::config::ModelRole::Fast,
        pp_config::config::RoleAssignment {
            enabled: true,
            model: "fixture/one".into(),
            effort: pp_core::model::ThinkLevel::None,
            fallback: None,
        },
    )]));
    binding
}

/// Workspace-write sandbox with a fixed platform probe: no unit test starts a real sandbox process.
fn test_sandbox(dir: &std::path::Path) -> std::sync::Arc<pp_sandbox::Sandbox> {
    std::sync::Arc::new(pp_sandbox::Sandbox::with_probe_for_test(
        pp_config::config::SandboxSettings {
            mode: pp_config::config::SandboxMode::WorkspaceWrite,
            ..Default::default()
        },
        dir,
        None,
        None,
        pp_sandbox::ProbeResult::Available,
    ))
}

#[test]
fn unchanged_rounds_keep_the_prompt_surface_identical() {
    for protocol in PROTOCOLS {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), PROJECT_RULES).unwrap();
        let mut agent = refresh_agent(dir.path(), "sessions", protocol);
        // First round: the loaded project block is merged into the system prompt and frozen as the static prefix.
        assert!(agent.refresh_instructions_changed().unwrap(), "{protocol}");
        let system = system_prompt(&agent);
        let definitions = session_definitions(&agent);
        assert!(
            system.contains("FIXTURE-RULE"),
            "{protocol}: the fixture project rules must reach the system prompt"
        );
        assert_eq!(
            definitions,
            agent.tools.all_definitions(agent.workflow.mode),
            "{protocol}"
        );
        // The next round runs after a user message was accepted: the session has history, keeps the frozen
        // system prompt and must not append a project delta for rules that were already delivered.
        start_task(&mut agent);
        assert!(!agent.refresh_instructions_changed().unwrap(), "{protocol}");
        assert_eq!(system_prompt(&agent), system, "{protocol}");
        assert_eq!(session_definitions(&agent), definitions, "{protocol}");
        assert!(
            developer_messages(&agent).is_empty(),
            "{protocol}: an unchanged round must not append a project delta"
        );
        assert!(!agent.refresh_instructions_changed().unwrap(), "{protocol}");
        assert_eq!(system_prompt(&agent), system, "{protocol}");
        assert_eq!(session_definitions(&agent), definitions, "{protocol}");
        assert!(developer_messages(&agent).is_empty(), "{protocol}");
    }
}

#[test]
fn project_rule_changes_apply_once_and_removal_is_reported() {
    for protocol in PROTOCOLS {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("AGENTS.md");
        std::fs::write(
            &path,
            "# Project rules\n\n- FIXTURE-RULE-OLD: first body.\n",
        )
        .unwrap();
        let mut agent = refresh_agent(dir.path(), "sessions", protocol);
        assert!(agent.refresh_instructions_changed().unwrap(), "{protocol}");
        start_task(&mut agent);
        assert!(!agent.refresh_instructions_changed().unwrap(), "{protocol}");
        let system = system_prompt(&agent);
        assert!(system.contains("FIXTURE-RULE-OLD"), "{protocol}");

        // A changed rule body keeps the system prefix byte-identical and arrives once as a developer delta.
        std::fs::write(
            &path,
            "# Project rules\n\n- FIXTURE-RULE-NEW: second and longer body.\n",
        )
        .unwrap();
        assert!(agent.refresh_instructions_changed().unwrap(), "{protocol}");
        assert_eq!(system_prompt(&agent), system, "{protocol}");
        let deltas = developer_messages(&agent);
        assert_eq!(deltas.len(), 1, "{protocol}");
        assert!(
            deltas[0].contains("FIXTURE-RULE-NEW"),
            "{protocol}: {deltas:?}"
        );
        assert!(
            !deltas[0].contains("FIXTURE-RULE-OLD"),
            "{protocol}: {deltas:?}"
        );
        assert!(!agent.refresh_instructions_changed().unwrap(), "{protocol}");
        assert_eq!(
            developer_messages(&agent),
            deltas,
            "{protocol}: an unchanged round must not repeat the delta"
        );

        // Removal reports the file once and still leaves the system prefix alone.
        std::fs::remove_file(&path).unwrap();
        assert!(agent.refresh_instructions_changed().unwrap(), "{protocol}");
        assert_eq!(system_prompt(&agent), system, "{protocol}");
        let deltas = developer_messages(&agent);
        assert_eq!(deltas.len(), 2, "{protocol}");
        assert!(
            deltas[1].contains("Project instructions removed:") && deltas[1].contains("AGENTS.md"),
            "{protocol}: {deltas:?}"
        );
        assert!(!agent.refresh_instructions_changed().unwrap(), "{protocol}");
        assert_eq!(developer_messages(&agent), deltas, "{protocol}");
    }
}

/// One input of the instruction refresh: an optional preparation step, the mutation under test, and the
/// observable effect on the next request.
struct Shape {
    name: &'static str,
    prepare: fn(&mut Agent, &std::path::Path),
    change: fn(&mut Agent, &std::path::Path),
    tool: Option<(&'static str, bool)>,
    system: Option<&'static str>,
}

#[test]
fn instruction_input_changes_reach_the_next_round() {
    let shapes = [
        Shape {
            name: "web search settings",
            prepare: |_, _| {},
            change: |agent, _| {
                agent
                    .tools
                    .configure_web_search(pp_config::config::WebSearchSettings {
                        enabled: false,
                        ..Default::default()
                    });
            },
            tool: Some(("web_search", false)),
            system: None,
        },
        Shape {
            name: "allowed tools",
            prepare: |_, _| {},
            change: |agent, _| {
                agent.tools.set_allowed_tools(Some(
                    pp_tools::AllowedTools::parse_cli(&["Read".to_owned()]).unwrap(),
                ));
            },
            tool: Some(("bash", false)),
            system: None,
        },
        Shape {
            name: "tool limits",
            prepare: |_, _| {},
            change: |agent, _| {
                let mut limits = agent.tools.limits;
                limits.read_max_bytes = 4_096;
                agent.tools.set_limits(limits);
            },
            tool: None,
            system: None,
        },
        Shape {
            name: "ask handler",
            prepare: |_, _| {},
            change: |agent, _| {
                agent
                    .tools
                    .set_ask_handler(Some(std::sync::Arc::new(|_, _| Ok(Vec::new()))));
            },
            tool: Some(("ask", true)),
            system: None,
        },
        Shape {
            name: "yield schema",
            prepare: |_, _| {},
            change: |agent, _| {
                agent
                    .tools
                    .set_yield_schema(Some(json!({"type": "object"})));
            },
            tool: Some(("yield", true)),
            system: None,
        },
        Shape {
            name: "sandbox",
            prepare: |_, _| {},
            change: |agent, dir| agent.tools.set_sandbox(test_sandbox(dir)),
            tool: None,
            system: None,
        },
        Shape {
            name: "permission preset",
            prepare: |agent, dir| agent.tools.set_sandbox(test_sandbox(dir)),
            change: |agent, _| {
                agent
                    .tools
                    .permission
                    .select(Some(pp_config::config::PermissionMode::ReadOnly));
                agent.tools.invalidate_definitions();
            },
            tool: None,
            system: None,
        },
        Shape {
            name: "subagent role schema",
            prepare: |_, _| {},
            change: |agent, dir| {
                agent
                    .tools
                    .set_subagent(Some(std::sync::Arc::new(root_binding(dir))));
            },
            tool: Some(("task", true)),
            system: None,
        },
        Shape {
            name: "skills",
            prepare: |_, _| {},
            change: |agent, dir| {
                let skill = dir.join("skills/release-notes");
                std::fs::create_dir_all(&skill).unwrap();
                std::fs::write(
                    skill.join("SKILL.md"),
                    "---\nname: release-notes\ndescription: FIXTURE-SKILL-DESCRIPTION\n---\nBody.\n",
                )
                .unwrap();
                std::fs::write(
                    dir.join("settings.yml"),
                    format!(
                        "rate_limit_retry_budget_seconds: 0\nskill_dirs:\n  - {}\n",
                        dir.join("skills").display()
                    ),
                )
                .unwrap();
                agent
                    .tools
                    .set_skills(pp_config::skills::Skills::from_dir(dir).shared());
            },
            tool: None,
            system: Some("FIXTURE-SKILL-DESCRIPTION"),
        },
        Shape {
            name: "plan revision",
            prepare: |_, _| {},
            change: |agent, _| {
                let mut plan = pp_core::plan::PlanDocument::new().unwrap();
                plan.revision = 3;
                agent.workflow.plan = Some(plan);
            },
            tool: None,
            system: Some("Registered revision: 3"),
        },
        Shape {
            name: "work mode",
            prepare: |_, _| {},
            change: |agent, _| agent.workflow.mode = pp_core::plan::WorkMode::Plan,
            tool: Some(("plan_submit", true)),
            system: None,
        },
    ];
    for protocol in PROTOCOLS {
        for shape in &shapes {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("AGENTS.md"), PROJECT_RULES).unwrap();
            let mut agent = refresh_agent(dir.path(), "sessions", protocol);
            assert!(
                agent.refresh_instructions_changed().unwrap(),
                "{protocol} {}: the initial round installs the project block",
                shape.name
            );
            (shape.prepare)(&mut agent, dir.path());
            let _ = agent.refresh_instructions_changed().unwrap();
            start_task(&mut agent);
            assert!(
                !agent.refresh_instructions_changed().unwrap(),
                "{protocol} {}: the steady-state round must not report a change",
                shape.name
            );
            (shape.change)(&mut agent, dir.path());
            assert!(
                agent.refresh_instructions_changed().unwrap(),
                "{protocol} {}: the change must reach the next round",
                shape.name
            );
            // Whatever the shape, the native request must carry exactly the tool table the current tools
            // expose.
            assert_eq!(
                session_definitions(&agent),
                agent.tools.all_definitions(agent.workflow.mode),
                "{protocol} {}: the session must carry the refreshed tool table",
                shape.name
            );
            if let Some((name, expected)) = shape.tool {
                assert_eq!(
                    has_tool(&agent, name),
                    expected,
                    "{protocol} {}: unexpected presence of {name}",
                    shape.name
                );
            }
            if let Some(needle) = shape.system {
                assert!(
                    system_prompt(&agent).contains(needle),
                    "{protocol} {}: the refreshed system prompt must contain the new input",
                    shape.name
                );
            }
            assert!(
                !agent.refresh_instructions_changed().unwrap(),
                "{protocol} {}: the round after the change must be unchanged again",
                shape.name
            );
        }
    }
}

#[test]
fn session_replacements_force_the_full_refresh() {
    for protocol in PROTOCOLS {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), PROJECT_RULES).unwrap();
        let mut agent = refresh_agent(dir.path(), "sessions", protocol);
        assert!(agent.refresh_instructions_changed().unwrap(), "{protocol}");
        start_task(&mut agent);
        assert!(!agent.refresh_instructions_changed().unwrap(), "{protocol}");
        // A model or session replacement drops the key and installs a fresh native request: even with every
        // input unchanged, the next round must restore the tool table instead of trusting a stale key.
        agent.invalidate_instruction_refresh();
        agent.session.set_tools(Vec::new());
        assert!(agent.refresh_instructions_changed().unwrap(), "{protocol}");
        assert_eq!(
            session_definitions(&agent),
            agent.tools.all_definitions(agent.workflow.mode),
            "{protocol}"
        );
        assert!(!agent.refresh_instructions_changed().unwrap(), "{protocol}");
    }
}
