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
        ("a3", include_str!("../../prompts/roles/read-only.md")),
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
                .all(|name| matches!(name.as_str(), "read" | "rg" | "glob" | "web_search" | "lsp"))
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
                    let item_props = parameters
                        .pointer("/properties/tasks/items/properties")
                        .unwrap();
                    assert_eq!(
                        item_props.get("isolated").is_some(),
                        mode == WorkMode::Execute
                    );
                    assert_eq!(
                        item_props.get("readonly").is_some(),
                        mode == WorkMode::Execute
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
