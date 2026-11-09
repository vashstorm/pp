use super::*;

fn wait_fixture_run(id: &str, status: &str) -> Run {
    Run {
        id: id.into(),
        agent_id: format!("agent-{id}"),
        owner: "Main".into(),
        status: status.into(),
        task: format!("work {id}"),
        result: (!matches!(status, "queued" | "running"))
            .then(|| json!({"ok":true,"output":format!("answer-{id}")})),
        consumed: false,
        duration_ms: 0,
        started_ms: now(),
        requests: 0,
        tokens: 0,
        cost: None,
    }
}

fn unified_wait(tools: &crate::tools::Tools, args: Value) -> Value {
    let result = tools.execute(
        "wait",
        &args.to_string(),
        "wait",
        crate::tools::ToolPolicy::Execute {
            workflow: &Default::default(),
        },
        &Default::default(),
        &mut |_| {},
    );
    serde_json::from_str(&result.output).unwrap()
}

#[test]
fn wait_subagent_reservations_acknowledge_only_after_commit_and_reject_other_owners() {
    let dir = tempfile::tempdir().unwrap();
    let binding = binding(dir.path(), TaskSettings::default());
    let mut tools =
        crate::tools::Tools::new(dir.path().into(), pp_config::config::EditMode::Replace).unwrap();
    tools.subagent = Some(binding.clone());
    let mut foreign = wait_fixture_run("foreign", "completed");
    foreign.owner = "other".into();
    {
        let mut state = lock(&binding.inner.state);
        state
            .disk
            .runs
            .insert("r1".into(), wait_fixture_run("r1", "completed"));
        state.disk.runs.insert("foreign".into(), foreign);
    }
    assert_eq!(
        unified_wait(&tools, json!({"run_ids":["r1","foreign"]}))["ok"],
        false
    );
    assert_eq!(binding.deliveries(&[]).len(), 1);
    let first = unified_wait(&tools, json!({"run_ids":["r1","r1"]}));
    assert_eq!(first["results"].as_array().unwrap().len(), 1);
    assert_eq!(binding.deliveries(&[]).len(), 1);
    assert_eq!(
        unified_wait(&tools, json!({"run_ids":["r1"]}))["results"],
        json!([])
    );
    tools.start_wait_batch();
    assert_eq!(
        unified_wait(&tools, json!({}))["results"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    // Registry 落盘失败也不提前提交内存中的 consumed 标记。
    let path = binding.directory().join("registry.json");
    fs::create_dir_all(binding.directory()).unwrap();
    if path.exists() {
        fs::remove_file(&path).unwrap();
    }
    fs::create_dir(&path).unwrap();
    assert!(tools.commit_wait_results().is_err());
    assert_eq!(binding.deliveries(&[]).len(), 1);
    fs::remove_dir(&path).unwrap();
    tools.commit_wait_results().unwrap();
    assert!(binding.deliveries(&[]).is_empty());
    assert_eq!(unified_wait(&tools, json!({}))["jobs"], json!([]));
    assert_eq!(
        unified_wait(&tools, json!({"run_ids":["r1"]}))["jobs"][0]["delivered"],
        true
    );
    assert!(!lock(&binding.inner.state).disk.runs["foreign"].consumed);
}

#[test]
fn wait_mixed_targets_wait_for_child_and_background_bash() {
    let dir = tempfile::tempdir().unwrap();
    let binding = binding(dir.path(), TaskSettings::default());
    let mut tools =
        crate::tools::Tools::new(dir.path().into(), pp_config::config::EditMode::Replace).unwrap();
    tools.subagent = Some(binding.clone());
    lock(&binding.inner.state)
        .disk
        .runs
        .insert("r1".into(), wait_fixture_run("r1", "running"));
    let workflow = pp_core::plan::WorkflowState::default();
    let policy = || crate::tools::ToolPolicy::Execute {
        workflow: &workflow,
    };
    tools.execute(
        "bash",
        r#"{"command":"while [ ! -f release ]; do sleep 0.01; done; printf mixed-bash","mode":"background"}"#,
        "bg",
        policy(),
        &Default::default(),
        &mut |_| {},
    );
    let mut updates = 0;
    let result = tools.execute(
        "wait",
        r#"{"timeout_ms":5000}"#,
        "wait",
        policy(),
        &Default::default(),
        &mut |event| {
            if let AgentEvent::ToolOutputSnapshot { output, .. } = event {
                updates += 1;
                let snapshot: Value = serde_json::from_str(&output).unwrap();
                assert_eq!(snapshot["jobs"].as_array().unwrap().len(), 2);
                if updates > 1 {
                    assert!(
                        snapshot["jobs"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|job| job["id"] == "r1" && job["status"] == "completed")
                    );
                    fs::write(dir.path().join("release"), "").unwrap();
                    return;
                }
                binding
                    .settle("r1", json!({"ok":true,"output":"child-mixed-result"}), 20)
                    .unwrap();
                // 新出现的 Run 不属于本次 wait 固定的目标集合。
                lock(&binding.inner.state)
                    .disk
                    .runs
                    .insert("r2".into(), wait_fixture_run("r2", "completed"));
            }
        },
    );
    assert!(updates >= 2);
    let result: Value = serde_json::from_str(&result.output).unwrap();
    assert_eq!(result["pending"], false);
    assert_eq!(result["jobs"].as_array().unwrap().len(), 2);
    assert_eq!(
        result["results"][0]["run"]["result"]["output"],
        "child-mixed-result"
    );
    assert_eq!(result["results"].as_array().unwrap().len(), 1);
    assert_eq!(result["background_results"][0]["output"], "mixed-bash");
    let only = unified_wait(&tools, json!({"run_ids":["r2"]}));
    assert_eq!(only["pending"], false);
    assert_eq!(only["jobs"].as_array().unwrap().len(), 1);
    let plan = tools.all_definitions(WorkMode::Plan);
    assert_eq!(plan.iter().filter(|d| d.name == "wait").count(), 1);
}

#[test]
fn wait_timeout_cancels_only_selected_runs_and_their_descendant_tokens() {
    let dir = tempfile::tempdir().unwrap();
    let binding = binding(dir.path(), TaskSettings::default());
    let mut tools =
        crate::tools::Tools::new(dir.path().into(), pp_config::config::EditMode::Replace).unwrap();
    tools.subagent = Some(binding.clone());
    let selected = CancellationToken::new();
    let descendant = selected.child_token();
    let unselected = CancellationToken::new();
    {
        let mut state = lock(&binding.inner.state);
        state
            .disk
            .runs
            .insert("r1".into(), wait_fixture_run("r1", "completed"));
        for (id, token) in [("r2", &selected), ("r3", &unselected)] {
            let run = wait_fixture_run(id, "running");
            let (sender, _) = mpsc::channel();
            state.live.insert(
                run.agent_id.clone(),
                Live {
                    sender,
                    input: InputQueue::default(),
                    cancellation: token.clone(),
                    generation: 1,
                },
            );
            state.disk.runs.insert(id.into(), run);
        }
    }
    let worker_binding = binding.clone();
    let worker_token = selected.clone();
    let worker = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !worker_token.is_cancelled() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        worker_binding
            .settle(
                "r2",
                json!({"ok":false,"cancelled":true,"partial_output":"selected-partial"}),
                15,
            )
            .unwrap();
    });
    let result = tools
        .run_wait_with_elapsed(
            r#"{"run_ids":["r1","r2"]}"#,
            "timeout",
            &Default::default(),
            &mut |_| panic!("超时必须进入回收路径"),
            || crate::tools::wait::TIMEOUT,
        )
        .unwrap();
    worker.join().unwrap();
    assert_eq!(result["timed_out"], true);
    assert_eq!(result["ok"], false);
    assert_eq!(result["pending"], false);
    assert_eq!(result["results"].as_array().unwrap().len(), 2);
    assert!(descendant.is_cancelled());
    assert!(!unselected.is_cancelled());
    assert_eq!(
        binding.wait_jobs(&["r3".into()]).unwrap()[0].status,
        "running"
    );
    // 取消完成不等于消费成功；原生历史提交前结果仍可再次交付。
    assert_eq!(binding.deliveries(&["r2".into()]).len(), 1);
    tools.commit_wait_results().unwrap();
    assert!(binding.deliveries(&["r1".into(), "r2".into()]).is_empty());
}

#[test]
fn wait_cleanup_failure_is_a_stop_error_and_does_not_reserve_results() {
    let dir = tempfile::tempdir().unwrap();
    let binding = binding(dir.path(), TaskSettings::default());
    let mut tools =
        crate::tools::Tools::new(dir.path().into(), pp_config::config::EditMode::Replace).unwrap();
    tools.subagent = Some(binding.clone());
    lock(&binding.inner.cleanup_errors).push(
        pp_core::cancellation::CleanupError(anyhow::anyhow!("fixture cleanup failure")).into(),
    );
    let workflow = Default::default();
    let token = CancellationToken::new();
    let start = tools.execute(
        "bash",
        r#"{"command":"sleep 30","mode":"background"}"#,
        "start",
        crate::tools::ToolPolicy::Execute {
            workflow: &workflow,
        },
        &token,
        &mut |_| {},
    );
    assert!(start.stop_error.is_none());
    let result = tools.execute(
        "wait",
        "{}",
        "wait",
        crate::tools::ToolPolicy::Execute {
            workflow: &workflow,
        },
        &token,
        &mut |event| {
            if matches!(event, AgentEvent::ToolOutputSnapshot { .. }) {
                token.cancel();
            }
        },
    );
    assert!(
        result
            .stop_error
            .unwrap()
            .is::<pp_core::cancellation::CleanupError>()
    );
    tools.start_wait_batch();
    let result = unified_wait(&tools, json!({}));
    assert_eq!(result["background_results"].as_array().unwrap().len(), 1);
}
fn binding(dir: &Path, settings: TaskSettings) -> Binding {
    let binding = Binding::root(
        RootOptions {
            dir: dir.join("state"),
            user: dir.join("user"),
            cwd: dir.to_owned(),
            model: "fixture/main".into(),
            thinking: ThinkLevel::None,
            settings,
            plan_mode: false,
        },
        Arc::new(|_, _| bail!("fixture factory")),
        Arc::new(|_| Ok(())),
    )
    .unwrap();
    binding.set_model_roles(BTreeMap::from([ModelRole::Default, ModelRole::Fast].map(
        |role| {
            (
                role,
                pp_config::config::RoleAssignment {
                    enabled: true,
                    model: "fixture/main".into(),
                    effort: ThinkLevel::None,
                    fallback: None,
                },
            )
        },
    )));
    binding
}

#[test]
fn child_bindings_cannot_spawn_subagents() {
    let dir = tempfile::tempdir().unwrap();
    let root = binding(
        dir.path(),
        TaskSettings {
            max_recursion_depth: 8,
            ..Default::default()
        },
    );
    let args = json!({"context":"no recursive dispatch","tasks":[{"role":"fast","task":"valid child work"}]})
        .to_string();
    for mode in [WorkMode::Execute, WorkMode::Plan] {
        assert!(
            root.definitions(mode)
                .iter()
                .any(|tool| tool.name == "task")
        );
        for depth in [1, 2] {
            let mut child = root.clone();
            child.depth = depth;
            child.owner = format!("a{depth}");
            child.readonly = false;
            child.spawns = vec!["*".into()];
            assert!(child.definitions(mode).is_empty());
            let before = {
                let state = lock(&root.inner.state);
                (state.disk.agents.clone(), state.disk.runs.clone())
            };
            let error = child
                .execute("task", &args, mode == WorkMode::Plan, &Default::default())
                .unwrap_err();
            assert_eq!(
                error.to_string(),
                "this agent is not allowed to spawn further subagents"
            );
            let state = lock(&root.inner.state);
            assert_eq!(
                serde_json::to_value((&state.disk.agents, &state.disk.runs)).unwrap(),
                serde_json::to_value(before).unwrap()
            );
        }
    }
    let disabled_dir = tempfile::tempdir().unwrap();
    let disabled = binding(
        disabled_dir.path(),
        TaskSettings {
            max_recursion_depth: 0,
            ..Default::default()
        },
    );
    for mode in [WorkMode::Execute, WorkMode::Plan] {
        assert!(disabled.definitions(mode).is_empty());
        assert!(
            disabled
                .execute("task", &args, mode == WorkMode::Plan, &Default::default())
                .is_err()
        );
    }
    let state = lock(&disabled.inner.state);
    assert!(state.disk.agents.is_empty());
    assert!(state.disk.runs.is_empty());
}

#[test]
fn model_roles_subagent_selection_controls_model_effort_permissions_and_route() {
    let dir = tempfile::tempdir().unwrap();
    let binding = binding(dir.path(), TaskSettings::default());
    binding.set_model_roles(std::collections::BTreeMap::from([
        (
            pp_config::config::ModelRole::Fast,
            pp_config::config::RoleAssignment {
                fallback: Some(pp_config::config::RoleModel {
                    model: "fixture/backup".into(),
                    effort: ThinkLevel::Medium,
                }),
                enabled: true,
                model: "fixture/fast".into(),
                effort: ThinkLevel::Low,
            },
        ),
        (
            pp_config::config::ModelRole::Review,
            pp_config::config::RoleAssignment {
                fallback: None,
                enabled: true,
                model: "fixture/review".into(),
                effort: ThinkLevel::High,
            },
        ),
        (
            pp_config::config::ModelRole::Explorer,
            pp_config::config::RoleAssignment {
                fallback: None,
                enabled: true,
                model: "fixture/explore".into(),
                effort: ThinkLevel::Low,
            },
        ),
    ]));
    let role_names = binding
        .definitions(WorkMode::Execute)
        .into_iter()
        .find(|def| def.name == "task")
        .and_then(|def| def.parameters)
        .and_then(|parameters| {
            parameters
                .pointer("/properties/tasks/items/properties/role/enum")
                .and_then(|value| value.as_array().cloned())
        })
        .expect("task schema");
    assert!(
        role_names.contains(&json!("explorer")),
        "启用 explorer 后必须出现在 task 角色枚举中：{role_names:?}"
    );
    binding
        .execute(
            "task",
            &json!({"context":"routing", "tasks":[
                {"role":"fast", "task":"find"},
                {"role":"review", "task":"review"},
                {"role":"fast", "readonly":true, "task":"quick review"},
                {"role":"explorer", "task":"map the callers"}
            ]})
            .to_string(),
            false,
            &Default::default(),
        )
        .unwrap();
    // 派发后修改配置不改变该批记录冻结的主备快照。
    binding.set_model_roles(BTreeMap::new());
    let state = lock(&binding.inner.state);
    for (id, model, effort) in [
        ("a1", "fixture/fast", ThinkLevel::Low),
        ("a2", "fixture/review", ThinkLevel::High),
        ("a3", "fixture/fast", ThinkLevel::Low),
        ("a4", "fixture/explore", ThinkLevel::Low),
    ] {
        assert_eq!(state.disk.agents[id].spec.model, model);
        let spec = &state.disk.agents[id].spec;
        assert_eq!(spec.thinking, effort);
        let route = spec.role_route.as_ref().unwrap();
        assert_eq!(route.primary.model, spec.model);
        assert_eq!(route.primary.effort, spec.thinking);
        assert_eq!(Some(route.role), spec.model_role);
        assert_eq!(spec.definition, route.role.name().to_lowercase());
    }
    assert!(!state.disk.agents["a1"].spec.readonly);
    assert!(state.disk.agents["a2"].spec.readonly);
    assert!(state.disk.agents["a3"].spec.readonly);
    assert!(state.disk.agents["a4"].spec.readonly);
    for (id, prompt) in [
        ("a1", catalog::builtin(ModelRole::Fast).prompt.as_str()),
        ("a2", catalog::builtin(ModelRole::Review).prompt.as_str()),
        ("a3", include_str!("../prompts/roles/read-only.md")),
        ("a4", catalog::builtin(ModelRole::Explorer).prompt.as_str()),
    ] {
        assert_eq!(state.disk.agents[id].spec.prompt, prompt.trim_end());
    }
    assert_eq!(
        state.disk.agents["a1"]
            .spec
            .role_route
            .as_ref()
            .unwrap()
            .fallback
            .as_ref()
            .unwrap()
            .model,
        "fixture/backup"
    );
    for id in ["a2", "a3", "a4"] {
        let spec = &state.disk.agents[id].spec;
        assert!(spec.spawns.is_empty());
        assert!(
            spec.tools
                .iter()
                .all(|name| matches!(name.as_str(), "read" | "rg" | "glob" | "lsp"))
        );
    }
}

#[test]
fn model_roles_default_and_plan_are_never_offered_or_dispatched_to_subagents() {
    let dir = tempfile::tempdir().unwrap();
    let binding = binding(dir.path(), TaskSettings::default());
    for enabled in [false, true] {
        for include_fast in [false, true] {
            let mut roles = BTreeMap::from([(
                pp_config::config::ModelRole::Plan,
                pp_config::config::RoleAssignment {
                    fallback: None,
                    enabled,
                    model: "fixture/planner-only-marker".into(),
                    effort: ThinkLevel::High,
                },
            )]);
            roles.insert(ModelRole::Default, roles[&ModelRole::Plan].clone());
            if include_fast {
                roles.insert(
                    pp_config::config::ModelRole::Fast,
                    pp_config::config::RoleAssignment {
                        fallback: None,
                        enabled: true,
                        model: "fixture/fast".into(),
                        effort: ThinkLevel::Low,
                    },
                );
            }
            binding.set_model_roles(roles);
            for mode in [WorkMode::Execute, WorkMode::Plan] {
                let task = binding
                    .definitions(mode)
                    .into_iter()
                    .find(|def| def.name == "task");
                if !include_fast {
                    assert!(task.is_none());
                } else {
                    let task = task.unwrap();
                    let parameters = task.parameters.unwrap();
                    assert_eq!(
                        parameters
                            .pointer("/properties/tasks/items/properties/role/enum")
                            .unwrap(),
                        &json!(["fast"])
                    );
                    assert!(!task.description.unwrap().contains("planner-only-marker"));
                }
                for role in ["default", "plan"] {
                    let error = binding
                        .execute(
                            "task",
                            &json!({"context":"test","tasks":[
                                {"role":role,"task":"reject reserved role"}
                            ]})
                            .to_string(),
                            mode == WorkMode::Plan,
                            &Default::default(),
                        )
                        .unwrap_err();
                    assert!(error.to_string().contains("reserved for the main agent"));
                }
                let state = lock(&binding.inner.state);
                assert!(state.disk.agents.is_empty());
                assert!(state.disk.runs.is_empty());
                assert_eq!(state.disk.next_id, 0);
            }
        }
    }
}

#[test]
fn model_roles_unselected_roles_are_absent_from_prompt_schema_and_rejected_before_dispatch() {
    let dir = tempfile::tempdir().unwrap();
    let binding = binding(dir.path(), TaskSettings::default());
    let inactive = pp_config::config::RoleAssignment {
        fallback: None,
        enabled: false,
        model: "fixture/disabled-model-marker".into(),
        effort: ThinkLevel::High,
    };
    binding.set_model_roles(std::collections::BTreeMap::from([
        (
            pp_config::config::ModelRole::Fast,
            pp_config::config::RoleAssignment {
                fallback: None,
                enabled: true,
                model: "fixture/fast".into(),
                effort: ThinkLevel::Low,
            },
        ),
        (pp_config::config::ModelRole::Review, inactive.clone()),
    ]));
    let task = binding
        .definitions(WorkMode::Execute)
        .into_iter()
        .find(|def| def.name == "task")
        .unwrap();
    let parameters = task.parameters.unwrap();
    assert_eq!(
        parameters
            .pointer("/properties/tasks/items/properties/role/enum")
            .unwrap(),
        &json!(["fast"])
    );
    assert!(
        !task
            .description
            .as_ref()
            .unwrap()
            .contains("disabled-model-marker")
    );
    assert!(
        !task
            .description
            .as_ref()
            .unwrap()
            .contains("review model role")
    );
    for role in ["review", "vision"] {
        assert!(binding.execute("task", &json!({"context":"test","tasks":[{"task":"valid","role":"fast"},{"task":"reject unselected role","role":role}]}).to_string(), false, &Default::default()).is_err());
    }
    assert!(lock(&binding.inner.state).disk.agents.is_empty());
    binding.set_model_roles(std::collections::BTreeMap::from([(
        pp_config::config::ModelRole::Review,
        inactive,
    )]));
    assert!(binding.definitions(WorkMode::Execute).is_empty());
    assert!(
        binding
            .execute(
                "task",
                &json!({"context":"test","tasks":[{"task":"missing role"}]}).to_string(),
                false,
                &Default::default()
            )
            .is_err()
    );
    assert!(lock(&binding.inner.state).disk.agents.is_empty());
}
#[test]
fn schema_checks_nested_required_additional_properties_and_combinators() {
    let schema = json!({"type":"object","properties":{"items":{"type":"array","items":{"type":"integer","minimum":1}}},"required":["items"],"additionalProperties":false});
    assert!(validate_output(&schema, &json!({"items":[1,2]})).is_ok());
    for value in [
        json!({}),
        json!({"items":[0]}),
        json!({"items":["x"]}),
        json!({"items":[],"extra":true}),
    ] {
        assert!(validate_output(&schema, &value).is_err());
    }
    assert!(validate_schema(&json!({"$ref":"other"})).is_err());
    assert!(
        validate_output(
            &json!({"oneOf":[{"type":"number"},{"type":"integer"}]}),
            &json!(1)
        )
        .is_err()
    );
}
#[test]
fn custom_agents_require_real_definitions_and_never_inherit_the_plan_model() {
    let dir = tempfile::tempdir().unwrap();
    let mut binding = binding(dir.path(), TaskSettings::default());
    binding.model = "fixture/planner".into();
    binding.allow_mcp(
        vec!["mcp__remote__echo".into(), "mcp__remote__fail".into()],
        vec!["mcp__remote__echo".into()],
    );
    let schema = binding.definitions(WorkMode::Execute)[0]
        .parameters
        .clone()
        .unwrap();
    assert!(
        schema
            .pointer("/properties/tasks/items/properties/agent")
            .is_none()
    );
    let project = dir.path().join(".pp/agents");
    fs::create_dir_all(&project).unwrap();
    fs::write(project.join("scout.md"), "---\nname: scout\ndescription: configured scout\ntools: [read, rg, glob, lsp, mcp__remote__echo, mcp__remote__fail]\n---\nConfigured investigation prompt.").unwrap();
    let definitions = binding.custom_roles();
    let scout = definitions.iter().find(|def| def.name == "scout").unwrap();
    assert!(scout.source.ends_with(".pp/agents/scout.md"));
    let schema = binding.definitions(WorkMode::Execute)[0]
        .parameters
        .clone()
        .unwrap();
    assert_eq!(
        schema
            .pointer("/properties/tasks/items/properties/role/enum")
            .unwrap(),
        &json!(["fast", "scout"])
    );
    for missing in ["task", "reviewer", "invented", ""] {
        assert!(
            binding
                .execute(
                    "task",
                    &json!({"context":"test","tasks":[{"role":missing,"task":"invalid"}]})
                        .to_string(),
                    false,
                    &Default::default()
                )
                .is_err()
        );
    }
    assert!(lock(&binding.inner.state).disk.agents.is_empty());
    binding
        .execute(
            "task",
            &json!({"context":"test","tasks":[{"role":"scout","task":"inspect"}]}).to_string(),
            true,
            &Default::default(),
        )
        .unwrap();
    let state = lock(&binding.inner.state);
    let spec = &state.disk.agents["a1"].spec;
    assert_eq!(spec.model, "fixture/main");
    assert_eq!(spec.model_role, None);
    assert!(spec.role_route.is_none());
    assert_eq!(spec.definition, "scout");
    assert_eq!(spec.prompt, "Configured investigation prompt.");
    assert!(spec.readonly);
    assert!(spec.tools.iter().any(|tool| tool == "mcp__remote__echo"));
    assert!(!spec.tools.iter().any(|tool| tool == "mcp__remote__fail"));
}

#[test]
fn custom_agent_discovery_preserves_project_priority_and_disabled_definitions() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join(".pp/agents");
    let user = dir.path().join("user/agents");
    fs::create_dir_all(&project).unwrap();
    fs::create_dir_all(&user).unwrap();
    for (path, label) in [(&project, "project"), (&user, "user")] {
        fs::write(
            path.join("analyst.md"),
            format!("---\nname: analyst\ndescription: {label}\ntools: []\n---\n{label} prompt"),
        )
        .unwrap();
    }
    fs::write(project.join("invalid.md"), "invalid definition").unwrap();
    let binding = binding(
        dir.path(),
        TaskSettings {
            disabled_agents: vec!["hidden".into()],
            ..Default::default()
        },
    );
    fs::write(
        project.join("hidden.md"),
        "---\nname: hidden\ndescription: hidden marker\n---\nHidden prompt",
    )
    .unwrap();
    for name in ["fast", "Default", "plan"] {
        fs::write(
            project.join(format!("{name}.md")),
            format!("---\nname: {name}\ndescription: reserved role\n---\nIgnored prompt"),
        )
        .unwrap();
    }
    let custom = binding.custom_roles();
    assert_eq!(custom.len(), 1);
    assert_eq!(custom[0].name, "analyst");
    assert_eq!(custom[0].description, "project");
    assert_eq!(custom[0].tools, Some(vec![]));
    let schema = binding.definitions(WorkMode::Execute)[0]
        .parameters
        .clone()
        .unwrap();
    assert_eq!(
        schema
            .pointer("/properties/tasks/items/properties/role/enum")
            .unwrap(),
        &json!(["fast", "analyst"])
    );
    assert!(
        binding
            .execute(
                "task",
                &json!({"context":"test","tasks":[{"role":"hidden","task":"reject"}]}).to_string(),
                false,
                &Default::default()
            )
            .is_err()
    );
}

#[test]
fn custom_role_uses_model_override_and_works_without_builtin_roles() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join(".pp/agents");
    fs::create_dir_all(&project).unwrap();
    fs::write(project.join("analyst.md"), "---\nname: analyst\ndescription: custom model marker\nmodel: fixture/definition\nthinking: high\ntools: [read, rg]\nspawns: []\noutput: {type: object}\n---\nCustom analysis prompt.").unwrap();
    let binding = binding(
        dir.path(),
        TaskSettings {
            agent_models: BTreeMap::from([
                ("analyst".into(), "fixture/override".into()),
                ("ghost".into(), "fixture/ghost".into()),
            ]),
            ..Default::default()
        },
    );
    binding.set_model_roles(BTreeMap::new());
    let schema = binding.definitions(WorkMode::Execute)[0]
        .parameters
        .clone()
        .unwrap();
    assert_eq!(
        schema
            .pointer("/properties/tasks/items/properties/role/enum")
            .unwrap(),
        &json!(["analyst"])
    );
    assert!(
        binding
            .execute(
                "task",
                &json!({"context":"test","tasks":[{"role":"ghost","task":"reject fabricated definition"}]}).to_string(),
                false,
                &Default::default(),
            )
            .is_err()
    );
    binding
        .execute(
            "task",
            &json!({"context":"test","tasks":[{"role":"analyst","task":"custom route"}]})
                .to_string(),
            false,
            &Default::default(),
        )
        .unwrap();
    let state = lock(&binding.inner.state);
    let custom = &state.disk.agents["a1"].spec;
    assert_eq!(custom.model, "fixture/override");
    assert_eq!(custom.thinking, ThinkLevel::High);
    assert_eq!(custom.model_role, None);
    assert!(custom.role_route.is_none());
    assert_eq!(custom.output_schema, Some(json!({"type":"object"})));
    assert_eq!(custom.prompt, "Custom analysis prompt.");
    assert_eq!(custom.tools, vec!["read", "rg"]);
}

#[test]
fn invalid_batch_has_no_dispatch_or_reserved_ids() {
    let dir = tempfile::tempdir().unwrap();
    let binding = binding(dir.path(), TaskSettings::default());
    for invalid in [
        json!({"task":"missing role"}),
        json!({"agent":"fast","task":"legacy selector"}),
        json!({"role":"missing","task":"unknown role"}),
        json!({"role":null,"task":"null role"}),
    ] {
        let result = binding.execute(
            "task",
            &json!({"context":"test","tasks":[{"role":"fast","task":"valid"},invalid]}).to_string(),
            false,
            &Default::default(),
        );
        assert!(result.is_err());
        let state = lock(&binding.inner.state);
        assert!(state.disk.agents.is_empty());
        assert!(state.disk.runs.is_empty());
        assert_eq!(state.disk.next_id, 0);
        assert_eq!(state.workers, 0);
    }
}
#[test]
fn delivery_is_inspectable_until_ack_and_scoped_to_owner() {
    let dir = tempfile::tempdir().unwrap();
    let binding = binding(dir.path(), TaskSettings::default());
    let run = Run {
        id: "r1".into(),
        agent_id: "a1".into(),
        owner: "Main".into(),
        status: "completed".into(),
        task: "test".into(),
        result: Some(json!({"ok":true,"output":"done"})),
        consumed: false,
        duration_ms: 0,
        started_ms: 0,
        requests: 1,
        tokens: 2,
        cost: None,
    };
    lock(&binding.inner.state)
        .disk
        .runs
        .insert("r1".into(), run);
    let result = binding.wait(&[], 0, &Default::default()).unwrap();
    assert!(result.to_string().contains("pp-subagent-result:r1"));
    assert_eq!(binding.deliveries(&[]).len(), 1);
    let mut other = binding.clone();
    other.owner = "other".into();
    assert!(other.deliveries(&[]).is_empty());
    assert!(other.wait(&["r1".into()], 0, &Default::default()).is_err());
    binding.acknowledge(&["r1".into()]).unwrap();
    assert!(binding.deliveries(&[]).is_empty());
}
#[test]
fn cancelled_permit_wait_does_not_leak_capacity() {
    let dir = tempfile::tempdir().unwrap();
    let mut binding = binding(
        dir.path(),
        TaskSettings {
            max_concurrency: 1,
            ..Default::default()
        },
    );
    binding.owner = "child".into();
    let permit = binding.permit(&Default::default()).unwrap();
    let token = CancellationToken::new();
    token.cancel();
    assert!(binding.permit(&token).is_err());
    drop(permit);
    assert!(binding.permit(&Default::default()).is_ok());
}

#[test]
fn run_permit_queues_initialization_and_releases_after_failure_or_cancellation() {
    let dir = tempfile::tempdir().unwrap();
    let binding = binding(
        dir.path(),
        TaskSettings {
            max_concurrency: 1,
            ..Default::default()
        },
    );
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let release_rx = Mutex::new(release_rx);
    *lock(&binding.inner.factory) = Some(Arc::new(move |record, _| {
        entered_tx.send(record.id.clone())?;
        lock(&release_rx).recv_timeout(Duration::from_secs(5))?;
        bail!("fixture initialization failure")
    }));
    binding
        .execute(
            "task",
            &json!({"context":"verify run permits", "tasks":[
                {"role":"fast","task":"first"}, {"role":"fast","task":"second"}, {"role":"fast","task":"third"}
            ]})
            .to_string(),
            false,
            &Default::default(),
        )
        .unwrap();
    let first = entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let (cancelled, next) = {
        let state = lock(&binding.inner.state);
        assert_eq!(binding.inner.semaphore.available_permits(), 0);
        assert_eq!(
            state
                .disk
                .agents
                .values()
                .filter(|a| a.status == "running")
                .count(),
            1
        );
        let queued = state
            .disk
            .runs
            .values()
            .filter(|r| r.status == "queued")
            .collect::<Vec<_>>();
        assert_eq!(queued.len(), 2);
        assert!(
            queued
                .iter()
                .all(|r| state.disk.agents[&r.agent_id].status == "queued")
        );
        (queued[0].id.clone(), queued[1].agent_id.clone())
    };
    binding
        .cancel_runs(std::slice::from_ref(&cancelled))
        .unwrap();
    binding.reap_runs(std::slice::from_ref(&cancelled)).unwrap();
    assert_eq!(
        lock(&binding.inner.state).disk.runs[&cancelled].status,
        "cancelled"
    );
    assert!(entered_rx.try_recv().is_err());
    release_tx.send(()).unwrap();
    assert_eq!(
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
        next
    );
    // 已有 worker 的后续 Run 同样排队，不能绕过当前占用的名额。
    let followup = binding.control(&first, "send", Some("follow-up")).unwrap();
    let followup = followup["run_id"].as_str().unwrap();
    {
        let state = lock(&binding.inner.state);
        assert_eq!(state.disk.runs[followup].status, "queued");
        assert_eq!(state.disk.agents[&first].status, "queued");
        let running = state
            .disk
            .runs
            .values()
            .find(|r| r.agent_id == next)
            .unwrap()
            .id
            .clone();
        drop(state);
        binding.cancel_runs(&[running]).unwrap();
    }
    release_tx.send(()).unwrap();
    assert_eq!(
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
        first
    );
    release_tx.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut state = lock(&binding.inner.state);
    while state
        .disk
        .runs
        .values()
        .any(|r| matches!(r.status.as_str(), "queued" | "running"))
    {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .expect("Run 未及时结束");
        state = binding
            .inner
            .changed
            .wait_timeout(state, remaining)
            .unwrap()
            .0;
    }
    assert_eq!(state.disk.runs[followup].status, "failed");
    assert_eq!(
        state
            .disk
            .runs
            .values()
            .filter(|r| r.status == "cancelled")
            .count(),
        2
    );
    drop(state);
    binding.shutdown().unwrap();
    assert_eq!(binding.inner.semaphore.available_permits(), 1);
}

#[test]
fn persisted_running_work_becomes_interrupted_without_restarting() {
    let dir = tempfile::tempdir().unwrap();
    let binding = binding(dir.path(), TaskSettings::default());
    let run = Run {
        id: "r1".into(),
        agent_id: "a1".into(),
        owner: "Main".into(),
        status: "running".into(),
        task: "test".into(),
        result: None,
        consumed: false,
        duration_ms: 0,
        started_ms: 0,
        requests: 0,
        tokens: 0,
        cost: None,
    };
    {
        let mut state = lock(&binding.inner.state);
        state.disk.runs.insert("r1".into(), run);
        binding.inner.save(&state).unwrap();
    }
    let restored = super::tests::binding(dir.path(), TaskSettings::default());
    assert_eq!(
        lock(&restored.inner.state).disk.runs["r1"].status,
        "interrupted"
    );
    assert_eq!(lock(&restored.inner.state).workers, 0);
}
#[test]
fn worktree_preserves_dirty_source_and_conflicts_retain_artifacts() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    fs::create_dir(&repo).unwrap();
    let git = |args: &[&str]| {
        let result = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(args)
            .output()
            .unwrap();
        assert!(result.status.success(), "{:?}", result);
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "fixture@example.com"]);
    git(&["config", "user.name", "Fixture"]);
    fs::write(repo.join("file"), "base\n").unwrap();
    git(&["add", "file"]);
    git(&["commit", "-qm", "base"]);
    fs::write(repo.join("file"), "dirty\n").unwrap();
    fs::write(repo.join("new"), [0, 1, 2, 255]).unwrap();
    let artifacts = dir.path().join("artifacts");
    fs::create_dir(&artifacts).unwrap();
    let target = workspace::prepare(&repo, &artifacts, "a1", &Default::default()).unwrap();
    assert_eq!(fs::read(target.join("file")).unwrap(), b"dirty\n");
    assert_eq!(fs::read(target.join("new")).unwrap(), [0, 1, 2, 255]);
    fs::write(target.join("file"), "child\n").unwrap();
    workspace::capture(&artifacts, "a1", true, &Default::default()).unwrap();
    assert_eq!(fs::read(repo.join("file")).unwrap(), b"child\n");
    fs::write(target.join("file"), "another\n").unwrap();
    fs::write(repo.join("file"), "user\n").unwrap();
    assert!(workspace::capture(&artifacts, "a1", true, &Default::default()).is_err());
    assert!(target.exists());
    assert!(artifacts.join("a1.patch").exists());
    assert_eq!(fs::read(repo.join("file")).unwrap(), b"user\n");
}

#[test]
fn worktree_captures_nested_repository_binary_and_deleted_files() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    fs::create_dir(&repo).unwrap();
    fn git(path: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(path)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    }
    fn init(path: &Path) {
        git(path, &["init", "-q"]);
        git(path, &["config", "user.email", "fixture@example.com"]);
        git(path, &["config", "user.name", "Fixture"]);
    }
    init(&repo);
    fs::write(repo.join("root"), "root\n").unwrap();
    git(&repo, &["add", "root"]);
    git(&repo, &["commit", "-qm", "root"]);
    let nested = repo.join("nested");
    fs::create_dir(&nested).unwrap();
    init(&nested);
    fs::write(nested.join("binary"), [0, 255, 1]).unwrap();
    fs::write(nested.join("deleted"), "remove").unwrap();
    git(&nested, &["add", "."]);
    git(&nested, &["commit", "-qm", "nested"]);
    let artifacts = dir.path().join("artifacts");
    let target = workspace::prepare(&repo, &artifacts, "a1", &Default::default()).unwrap();
    fs::write(target.join("nested/binary"), [0, 255, 9, 8]).unwrap();
    fs::remove_file(target.join("nested/deleted")).unwrap();
    let captured = workspace::capture(&artifacts, "a1", true, &Default::default()).unwrap();
    assert!(!captured.patches.is_empty());
    assert_eq!(fs::read(nested.join("binary")).unwrap(), [0, 255, 9, 8]);
    assert!(!nested.join("deleted").exists());
    assert!(
        workspace::capture(&artifacts, "a1", true, &Default::default())
            .unwrap()
            .patches
            .is_empty()
    );
}
#[test]
fn cancelled_and_unknown_waits_do_not_consume_other_owners_results() {
    let dir = tempfile::tempdir().unwrap();
    let binding = binding(dir.path(), Default::default());
    let token = CancellationToken::new();
    token.cancel();
    assert!(binding.wait(&[], 60_000, &token).is_err());
    assert!(
        binding
            .wait(&["missing".into()], 0, &Default::default())
            .is_err()
    );
}

#[test]
fn published_result_recovers_without_replaying_an_incomplete_registry_run() {
    let dir = tempfile::tempdir().unwrap();
    let binding = binding(dir.path(), Default::default());
    {
        let mut state = lock(&binding.inner.state);
        state.disk.runs.insert(
            "r1".into(),
            Run {
                id: "r1".into(),
                agent_id: "a1".into(),
                owner: "Main".into(),
                status: "running".into(),
                task: "task".into(),
                result: None,
                consumed: false,
                duration_ms: 0,
                started_ms: 0,
                requests: 0,
                tokens: 0,
                cost: None,
            },
        );
        binding.inner.save(&state).unwrap();
    }
    atomic_json(
        &binding.directory().join("r1.result.json"),
        &json!({"ok":true,"output":"durable result"}),
    )
    .unwrap();
    let restored = super::tests::binding(dir.path(), Default::default());
    let results = restored.deliveries(&[]);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].status, "completed");
    assert_eq!(
        results[0].result.as_ref().unwrap()["output"],
        "durable result"
    );
    assert_eq!(lock(&restored.inner.state).workers, 0);
}
#[test]
fn plan_mode_refuses_to_revive_a_writable_isolated_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let mut binding = binding(dir.path(), Default::default());
    Arc::get_mut(&mut binding.inner).unwrap().plan_mode = true;
    let spec = Spec {
        model_role: None,
        role_route: None,
        definition: "task".into(),
        prompt: String::new(),
        context: String::new(),
        model: "fixture/main".into(),
        thinking: ThinkLevel::None,
        tools: vec!["write".into()],
        spawns: vec![],
        output_schema: None,
        cwd: dir.path().into(),
        depth: 1,
        readonly: false,
        isolated: true,
        cost: None,
    };
    lock(&binding.inner.state).disk.agents.insert(
        "a1".into(),
        Record {
            id: "a1".into(),
            name: "isolated".into(),
            parent: "Main".into(),
            generation: 1,
            status: "parked".into(),
            spec,
            session_file: None,
            registered_ms: None,
            updated_ms: 0,
        },
    );
    assert!(binding.control("a1", "revive", None).is_err());
    assert_eq!(lock(&binding.inner.state).workers, 0);
}
