use super::*;
use crate::model_control::Binding;
use crate::model_control::Change;
use crate::model_control::Report;
use crate::model_control::Service;
use pp_config::config::ModelRole;
use pp_config::config::RoleAssignment;

fn fixture(dir: &std::path::Path, protocol: &str) -> Agent {
    let mut agent = runtime_input_agent(dir, protocol, "127.0.0.1:1", "");
    let path = dir.join("models.yml");
    let mut models: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    models["providers"]["other"] = json!({"api_key":"fixture", "protocol": if protocol == "completion" {"response"} else {"completion"},
        "base_url":"http://127.0.0.1:1/v1", "models":{"two":{"model":"two"}}});
    std::fs::write(path, models.to_string()).unwrap();
    agent.reload_models().unwrap();
    seed_session(&mut agent, "committed context marker");
    agent
}
fn role() -> Option<RoleAssignment> {
    Some(RoleAssignment {
        fallback: None,
        enabled: true,
        model: "other/two".into(),
        effort: ThinkLevel::Low,
    })
}
fn role_with(model: &str, effort: ThinkLevel) -> Option<RoleAssignment> {
    Some(RoleAssignment {
        fallback: None,
        enabled: true,
        model: model.into(),
        effort,
    })
}

/// 请求体里的思考档位；两种协议的字段位置不同。
fn request_effort<'a>(protocol: &str, body: &'a Value) -> &'a Value {
    match protocol {
        "completion" => &body["reasoning_effort"],
        _ => &body["reasoning"]["effort"],
    }
}

/// 把 session 快照里唯一允许变化的顶层思考字段恢复成 before 的形态，于是整体比较
/// 等价于逐字节比较 `messages`/`input`、`tools`、`instructions`。
fn restore_effort(protocol: &str, after: &mut Value, before: &Value) {
    let key = match protocol {
        "completion" => "reasoning_effort",
        _ => "reasoning",
    };
    match before.get(key) {
        Some(value) => after[key] = value.clone(),
        None => {
            if let Value::Object(map) = after {
                map.remove(key);
            }
        }
    }
}

/// 与 `fixture` provider 同协议、同回环服务的另一个 provider，避免历史协议转换干扰。
fn add_other_provider(dir: &std::path::Path, protocol: &str, address: &str) {
    let path = dir.join("models.yml");
    let mut models: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    models["providers"]["other"] = json!({
        "api_key":"fixture-key",
        "protocol":protocol,
        "base_url":format!("http://{address}/v1"),
        "models":{"two":{"model":"two"}}
    });
    std::fs::write(path, models.to_string()).unwrap();
}

#[test]
fn runtime_model_roles_preserve_context_and_isolate_default_and_children() {
    for protocol in ["completion", "response"] {
        let main_dir = tempfile::tempdir().unwrap();
        let child_dir = tempfile::tempdir().unwrap();
        let inherited_dir = tempfile::tempdir().unwrap();
        let nested_dir = tempfile::tempdir().unwrap();
        let mut main = fixture(main_dir.path(), protocol);
        let mut child = fixture(child_dir.path(), protocol);
        let mut inherited = fixture(inherited_dir.path(), protocol);
        let mut nested = fixture(nested_dir.path(), protocol);
        let service = Service::start(main.model_catalog()).unwrap();
        let root = Binding::main(service.registry.clone());
        main.runtime_models = Some(root.clone());
        child.runtime_models = Some(root.child(Some(ModelRole::Fast), "a1".into()));
        inherited.runtime_models = Some(root.child(None, "a2".into()));
        nested.runtime_models = Some(
            child
                .runtime_models
                .as_ref()
                .unwrap()
                .child(Some(ModelRole::Fast), "a3".into()),
        );
        assert!(service.submit(Change::Role(ModelRole::Default, role())));
        service.flush();
        child.apply_runtime_models().unwrap();
        nested.apply_runtime_models().unwrap();
        assert_eq!(nested.current_model.reference, "fixture/one");
        inherited.apply_runtime_models().unwrap();
        assert_eq!(child.current_model.reference, "fixture/one");
        assert_eq!(inherited.current_model.reference, "fixture/one");
        main.apply_runtime_models().unwrap();
        assert_eq!(main.current_model.reference, "other/two");
        assert_eq!(main.think_level, ThinkLevel::Low);
        assert!(service.submit(Change::Role(ModelRole::Fast, role())));
        service.flush();
        child.apply_runtime_models().unwrap();
        assert_eq!(child.current_model.reference, "other/two");
        nested.apply_runtime_models().unwrap();
        assert_eq!(nested.current_model.reference, "other/two");
        inherited.apply_runtime_models().unwrap();
        assert_eq!(inherited.current_model.reference, "fixture/one");
        for agent in [&main, &child, &nested, &inherited] {
            assert!(
                serde_json::to_string(&agent.session)
                    .unwrap()
                    .contains("committed context marker")
            );
        }
        assert!(service.submit(Change::Role(ModelRole::Fast, None)));
        service.flush();
        child.apply_runtime_models().unwrap();
        assert_eq!(child.current_model.reference, "other/two");
    }
}

#[test]
fn runtime_model_latest_revision_wins_and_failed_revision_is_not_retried() {
    let dir = tempfile::tempdir().unwrap();
    let mut agent = fixture(dir.path(), "completion");
    let service = Service::start(agent.model_catalog()).unwrap();
    agent.runtime_models = Some(Binding::main(service.registry.clone()));
    assert!(service.submit(Change::Select("other/two".into())));
    assert!(service.submit(Change::Select("fixture/one".into())));
    service.flush();
    agent.apply_runtime_models().unwrap();
    assert_eq!(agent.current_model.reference, "fixture/one");
    let snapshot = serde_json::to_value(&agent.session).unwrap();
    let path = dir.path().join("models.yml");
    let mut models: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    models["providers"]["other"]["api_key"] = json!("invalid\nheader");
    std::fs::write(path, models.to_string()).unwrap();
    assert!(service.submit(Change::Select("other/two".into())));
    service.flush();
    service.registry.reports();
    agent.apply_runtime_models().unwrap();
    assert_eq!(agent.current_model.reference, "fixture/one");
    assert_eq!(serde_json::to_value(&agent.session).unwrap(), snapshot);
    assert!(
        service
            .registry
            .reports()
            .iter()
            .any(|report| matches!(report, Report::Failed { revision: 3, .. }))
    );
    agent.apply_runtime_models().unwrap();
    assert!(service.registry.reports().is_empty());
}

#[test]
fn runtime_model_rejects_unsupported_history_without_mutating_it() {
    use async_openai::types::responses as r;
    let dir = tempfile::tempdir().unwrap();
    let mut agent = fixture(dir.path(), "response");
    if let Session::Response(request) = &mut agent.session {
        let r::InputParam::Items(input) = &mut request.input else {
            panic!("原生历史应为条目列表")
        };
        input.push(r::InputItem::EasyMessage(r::EasyInputMessage {
            role: r::Role::User,
            content: r::EasyInputContent::ContentList(vec![r::InputContent::InputImage(
                r::InputImageContent::default(),
            )]),
            ..Default::default()
        }));
    }
    let before = serde_json::to_value(&agent.session).unwrap();
    let service = Service::start(agent.model_catalog()).unwrap();
    agent.runtime_models = Some(Binding::main(service.registry.clone()));
    assert!(service.submit(Change::Select("other/two".into())));
    service.flush();
    agent.apply_runtime_models().unwrap();
    assert_eq!(agent.current_model.reference, "fixture/one");
    assert_eq!(serde_json::to_value(&agent.session).unwrap(), before);
    assert!(
        service
            .registry
            .reports()
            .iter()
            .any(|report| matches!(report, Report::Failed { .. }))
    );
}

#[test]
fn runtime_model_role_save_failure_does_not_publish_target() {
    let dir = tempfile::tempdir().unwrap();
    let agent = fixture(dir.path(), "completion");
    let service = Service::start(agent.model_catalog()).unwrap();
    let mut main = Binding::main(service.registry.clone());
    std::fs::rename(
        dir.path().join("agent.db"),
        dir.path().join("saved-agent.db"),
    )
    .unwrap();
    std::fs::create_dir(dir.path().join("agent.db")).unwrap();
    assert!(service.submit(Change::Role(ModelRole::Default, role())));
    service.flush();
    assert!(main.take_targets().0.is_none());
    assert!(
        service
            .registry
            .reports()
            .iter()
            .any(|report| matches!(report, Report::Failed { revision: 0, .. }))
    );
}

#[test]
fn fallback_route_restores_primary_after_resume_and_manual_selection_clears_provenance() {
    let dir = tempfile::tempdir().unwrap();
    let mut agent = fixture(dir.path(), "completion");
    let assignment = RoleAssignment {
        enabled: true,
        model: "fixture/one".into(),
        effort: ThinkLevel::Low,
        fallback: Some(pp_config::config::RoleModel {
            model: "other/two".into(),
            effort: ThinkLevel::High,
        }),
    };
    let service = Service::start(agent.model_catalog()).unwrap();
    agent.runtime_models = Some(Binding::main(service.registry.clone()));
    assert!(service.submit(Change::Role(ModelRole::Default, Some(assignment))));
    service.flush();
    agent.apply_runtime_models().unwrap();
    assert!(agent.activate_fallback(&mut |_| {}).unwrap());
    assert_eq!(agent.current_model.reference, "other/two");
    assert!(!agent.activate_fallback(&mut |_| {}).unwrap());
    let catalog = agent.model_catalog();
    let id = agent.session_id().unwrap().to_owned();
    drop(agent);
    let sessions = dir.path().join("sessions");
    let restored = persistence::load(&sessions, &sessions.join(".locks"), Some(&id), true).unwrap();
    let mut agent = Agent::restore(
        catalog.clone(),
        Tools::new(dir.path().to_owned(), EditMode::Replace).unwrap(),
        restored,
    )
    .unwrap();
    agent.restore_role_primary(&mut |_| {}).unwrap();
    assert_eq!(agent.current_model.reference, "fixture/one");
    assert_eq!(agent.think_level, ThinkLevel::Low);
    assert!(
        serde_json::to_string(&agent.session)
            .unwrap()
            .contains("committed context marker")
    );
    agent.switch_model("fixture/one").unwrap();
    assert!(agent.role_route.is_none());
    drop(agent);
    let restored = persistence::load(&sessions, &sessions.join(".locks"), Some(&id), true).unwrap();
    assert!(restored.checkpoint.role_route.is_none());
}

#[test]
fn fallback_checkpoint_failure_keeps_model_and_native_history() {
    let dir = tempfile::tempdir().unwrap();
    let mut agent = fixture(dir.path(), "completion");
    agent.role_route = Some(pp_config::config::RoleRoute {
        role: ModelRole::Default,
        primary: pp_config::config::RoleModel {
            model: "fixture/one".into(),
            effort: agent.think_level,
        },
        fallback: Some(pp_config::config::RoleModel {
            model: "other/two".into(),
            effort: ThinkLevel::Low,
        }),
    });
    let before = serde_json::to_value(&agent.session).unwrap();
    let path = dir
        .path()
        .join("sessions")
        .join(format!("{}.jsonl", agent.session_id().unwrap()));
    agent.journal.use_read_only_file(&path);
    assert!(agent.activate_fallback(&mut |_| {}).is_err());
    assert_eq!(agent.current_model.reference, "fixture/one");
    assert_eq!(serde_json::to_value(&agent.session).unwrap(), before);
}

#[test]
fn runtime_binding_roles_route_main_children_and_custom_children() {
    let dir = tempfile::tempdir().unwrap();
    let agent = fixture(dir.path(), "completion");
    let service = Service::start(agent.model_catalog()).unwrap();
    let registry = service.registry.clone();

    // `Change::Select` 只写 main 槽：主 Agent 拿到的是 role 为 None 的手动目标。
    assert!(service.submit(Change::Select("other/two".into())));
    service.flush();
    let mut main = Binding::main(registry.clone());
    let (target, think) = main.take_targets();
    let target = target.expect("manual selection is visible to the main agent");
    assert_eq!(target.reference, "other/two");
    assert_eq!(target.role, None);
    assert!(think.is_none());

    // 角色目标写入 roles 槽：revision 更高时在主 Agent 的比较中胜出。
    assert!(service.submit(Change::Role(
        ModelRole::Default,
        role_with("fixture/one", ThinkLevel::High)
    )));
    service.flush();
    let (target, _) = main.take_targets();
    let target = target.expect("the newer role target wins for the main agent");
    assert_eq!(target.reference, "fixture/one");
    assert_eq!(target.role, Some(ModelRole::Default));
    assert_eq!(target.effort, Some(ThinkLevel::High));

    // 再来一次手动选择：revision 更高，Target.role 回到 None。
    assert!(service.submit(Change::Select("other/two".into())));
    service.flush();
    let (target, _) = main.take_targets();
    let target = target.expect("the newer manual selection wins again");
    assert_eq!(target.reference, "other/two");
    assert_eq!(target.role, None);
    assert_eq!(target.effort, None);

    // 每个断言用独立 Binding：seen 是每 Binding 独立的。
    let root = Binding::main(registry.clone());
    let mut default_child = root.child(Some(ModelRole::Default), "child-default".into());
    let mut fast_child = root.child(Some(ModelRole::Fast), "child-fast".into());
    let mut custom_child = root.child(None, "child-custom".into());

    // DEFAULT child 只消费 Default 角色目标，不消费 `Change::Select`。
    let (target, think) = default_child.take_targets();
    let target = target.expect("DEFAULT child follows the DEFAULT role");
    assert_eq!(target.reference, "fixture/one");
    assert_eq!(target.role, Some(ModelRole::Default));
    assert!(think.is_none());
    // 显式 FAST child 与自定义定义 child 此时都没有目标。
    assert!(fast_child.take_targets().0.is_none());
    let (target, think) = custom_child.take_targets();
    assert!(target.is_none() && think.is_none());

    // FAST 角色目标只给 FAST child。
    assert!(service.submit(Change::Role(
        ModelRole::Fast,
        role_with("other/two", ThinkLevel::Medium)
    )));
    service.flush();
    let (target, _) = fast_child.take_targets();
    let target = target.expect("FAST child follows the FAST role");
    assert_eq!(target.reference, "other/two");
    assert_eq!(target.role, Some(ModelRole::Fast));
    assert_eq!(target.effort, Some(ThinkLevel::Medium));
    assert!(default_child.take_targets().0.is_none());
    assert!(custom_child.take_targets().0.is_none());

    // think 目标只有主 Binding 拿到，且不会冒充 model 目标让 `pending()` 变真。
    assert!(service.submit(Change::Think(ThinkLevel::High)));
    service.flush();
    let (target, think) = main.take_targets();
    assert!(
        target.is_none(),
        "a think change must not publish a model target"
    );
    assert_eq!(
        think.expect("the main agent receives think targets").effort,
        ThinkLevel::High
    );
    let (target, think) = default_child.take_targets();
    assert!(target.is_none() && think.is_none());

    // `apply_think_target` 不得让 `pending()` 变真：只登记 think 槽时它仍为 false。
    let think_dir = tempfile::tempdir().unwrap();
    let think_agent = fixture(think_dir.path(), "completion");
    let think_only = Service::start(think_agent.model_catalog()).unwrap();
    let mut probe = Binding::main(think_only.registry.clone());
    assert!(think_only.submit(Change::Think(ThinkLevel::High)));
    think_only.flush();
    assert!(!probe.pending(), "pending() only tracks model targets");
    let (target, think) = probe.take_targets();
    assert!(target.is_none());
    assert_eq!(think.expect("think target").effort, ThinkLevel::High);
    // 同一 revision 不被重复领取。
    assert!(probe.take_targets().1.is_none());

    // 应用侧：`role_route` 随 `Target.role` 在 Some(Default) 与 None 间切换。
    let route_dir = tempfile::tempdir().unwrap();
    let mut agent = fixture(route_dir.path(), "completion");
    let routes = Service::start(agent.model_catalog()).unwrap();
    agent.runtime_models = Some(Binding::main(routes.registry.clone()));
    assert!(routes.submit(Change::Role(
        ModelRole::Default,
        role_with("fixture/one", ThinkLevel::Low)
    )));
    routes.flush();
    agent.apply_runtime_models().unwrap();
    assert_eq!(
        agent.role_route.as_ref().map(|route| route.role),
        Some(ModelRole::Default)
    );
    assert!(routes.submit(Change::Select("other/two".into())));
    routes.flush();
    agent.apply_runtime_models().unwrap();
    assert!(agent.role_route.is_none());

    // 同批领取时按 revision 顺序应用：model 较早、think 较晚，高 revision 的 effort 胜出。
    assert!(routes.submit(Change::Role(
        ModelRole::Default,
        role_with("other/two", ThinkLevel::Low)
    )));
    assert!(routes.submit(Change::Think(ThinkLevel::High)));
    routes.flush();
    agent.apply_runtime_models().unwrap();
    assert_eq!(agent.current_model.reference, "other/two");
    assert_eq!(agent.think_level, ThinkLevel::High);

    // think 较早、角色目标较晚：角色 effort 覆盖较早的手动档位。
    assert!(routes.submit(Change::Think(ThinkLevel::Low)));
    assert!(routes.submit(Change::Role(
        ModelRole::Default,
        role_with("fixture/one", ThinkLevel::Medium)
    )));
    routes.flush();
    agent.apply_runtime_models().unwrap();
    assert_eq!(agent.current_model.reference, "fixture/one");
    assert_eq!(agent.think_level, ThinkLevel::Medium);
}

#[test]
fn runtime_effort_change_keeps_request_prefix_identical() {
    for protocol in ["completion", "response"] {
        // 路径一：手动档位槽（`Change::Think`）。
        let dir = tempfile::tempdir().unwrap();
        let mut agent = fixture(dir.path(), protocol);
        let service = Service::start(agent.model_catalog()).unwrap();
        agent.runtime_models = Some(Binding::main(service.registry.clone()));
        let before = serde_json::to_value(&agent.session).unwrap();
        assert!(service.submit(Change::Think(ThinkLevel::High)));
        service.flush();
        service.registry.reports();
        agent.apply_runtime_models().unwrap();
        assert_eq!(agent.think_level, ThinkLevel::High);
        let mut after = serde_json::to_value(&agent.session).unwrap();
        // 会话快照是 `{protocol, request}`；前缀就是 `request` 内部的字段。
        assert_eq!(request_effort(protocol, &after["request"]), &json!("high"));
        restore_effort(protocol, &mut after["request"], &before["request"]);
        assert_eq!(
            after, before,
            "a manual effort change must not rewrite the request prefix"
        );

        // 路径二：角色只改 effort（model 不变），子 Agent 绑定 FAST。
        let dir = tempfile::tempdir().unwrap();
        let mut agent = fixture(dir.path(), protocol);
        let service = Service::start(agent.model_catalog()).unwrap();
        agent.runtime_models = Some(
            Binding::main(service.registry.clone())
                .child(Some(ModelRole::Fast), "cache-child".into()),
        );
        let before = serde_json::to_value(&agent.session).unwrap();
        let reference = agent.current_model.reference.clone();
        assert!(service.submit(Change::Role(
            ModelRole::Fast,
            role_with("fixture/one", ThinkLevel::High)
        )));
        service.flush();
        service.registry.reports();
        agent.apply_runtime_models().unwrap();
        assert_eq!(agent.current_model.reference, reference);
        assert_eq!(agent.think_level, ThinkLevel::High);
        let mut after = serde_json::to_value(&agent.session).unwrap();
        assert_eq!(request_effort(protocol, &after["request"]), &json!("high"));
        restore_effort(protocol, &mut after["request"], &before["request"]);
        assert_eq!(
            after, before,
            "a role effort-only change must not rewrite the request prefix"
        );
    }
}

#[test]
fn runtime_think_level_applies_to_next_request_mid_task() {
    for protocol in ["completion", "response"] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("marker.txt"), "marker body").unwrap();
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let mut agent =
            runtime_input_agent(dir.path(), protocol, &server.server_addr().to_string(), "");
        let (tx, rx) = std::sync::mpsc::channel::<Value>();
        let serving = protocol.to_owned();
        let server_thread = std::thread::spawn(move || {
            let mut request = server
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap()
                .unwrap();
            let body: Value = serde_json::from_reader(request.as_reader()).unwrap();
            tx.send(body).unwrap();
            tool_call_for(&serving, request, "read", r#"{"path":"marker.txt"}"#);
            let mut request = server
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap()
                .unwrap();
            let body: Value = serde_json::from_reader(request.as_reader()).unwrap();
            tx.send(body).unwrap();
            final_for(&serving, request, "marker read", true);
        });
        let service = Service::start(agent.model_catalog()).unwrap();
        agent.runtime_models = Some(Binding::main(service.registry.clone()));
        let mut submitted = false;
        let outcome = agent
            .run(
                "read the marker",
                &CancellationToken::new(),
                None,
                &mut |event| {
                    // 边界轮询已经过去：这次提交只能在下一请求前生效。
                    if matches!(event, AgentEvent::ModelRequestStarted) && !submitted {
                        submitted = true;
                        assert!(service.submit(Change::Think(ThinkLevel::High)));
                        service.flush();
                    }
                },
            )
            .unwrap();
        assert_eq!(outcome, RunOutcome::Completed);
        assert_eq!(agent.think_level(), ThinkLevel::High);
        let first = rx.recv().unwrap();
        let second = rx.recv().unwrap();
        assert_eq!(request_effort(protocol, &first), &json!("none"));
        assert_eq!(request_effort(protocol, &second), &json!("high"));
        if protocol == "response" {
            assert_eq!(second["reasoning"]["summary"], "concise");
        }
        server_thread.join().unwrap();
    }
}

#[test]
fn runtime_subagent_roles_switch_at_next_request_mid_run() {
    for (role, protocol) in [
        (ModelRole::Fast, "completion"),
        (ModelRole::Fast, "response"),
        (ModelRole::Slow, "completion"),
        (ModelRole::Slow, "response"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("marker.txt"), "child marker").unwrap();
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let address = server.server_addr().to_string();
        let mut agent = runtime_input_agent(dir.path(), protocol, &address, "");
        add_other_provider(dir.path(), protocol, &address);
        agent.reload_models().unwrap();
        agent
            .save_model_role(role, role_with("fixture/one", ThinkLevel::Low))
            .unwrap();
        let service = Service::start(agent.model_catalog()).unwrap();
        agent.runtime_models = Some(Binding::main(service.registry.clone()));
        agent.ensure_subagents().unwrap();
        let arguments = json!({
            "context": "role switch",
            "tasks": [{"role": role.name().to_lowercase(), "task": "inspect the marker"}]
        })
        .to_string();
        let binding = agent.subagents().unwrap();
        binding
            .execute("task", &arguments, true, &Default::default())
            .unwrap();

        // 第一轮：旧 model + 旧 effort；先提交角色更新再回工具调用，避免竞态。
        let mut request = server
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap()
            .unwrap();
        let body: Value = serde_json::from_reader(request.as_reader()).unwrap();
        assert_eq!(body["model"], "one");
        assert_eq!(request_effort(protocol, &body), &json!("low"));
        assert!(service.submit(Change::Role(role, role_with("other/two", ThinkLevel::High))));
        service.flush();
        tool_call_for(protocol, request, "read", r#"{"path":"marker.txt"}"#);

        // 第二轮：同一次运行里同时切到新 model 与新 effort。
        let mut request = server
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap()
            .unwrap();
        let body: Value = serde_json::from_reader(request.as_reader()).unwrap();
        assert_eq!(body["model"], "two");
        assert_eq!(request_effort(protocol, &body), &json!("high"));
        final_for(protocol, request, "child done", true);

        let result = agent.tools.execute(
            "wait",
            "{}",
            "wait-role-switch",
            crate::tools::ToolPolicy::Execute {
                workflow: &agent.workflow,
            },
            &Default::default(),
            &mut |_| {},
        );
        let result: Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(result["results"].as_array().unwrap().len(), 1);
        assert_eq!(result["pending"], false);
        assert_eq!(result["results"][0]["run"]["result"]["ok"], true);
        agent.stop_subagents().unwrap();
        assert!(
            service
                .registry
                .reports()
                .iter()
                .any(|report| matches!(report, Report::Applied { agent: Some(_), .. })),
            "the child reports its applied role target"
        );
    }
}

#[test]
fn runtime_think_failure_keeps_level_and_reports_once() {
    let dir = tempfile::tempdir().unwrap();
    let settings = dir.path().join("settings.yml");
    let models = dir.path().join("models.yml");
    std::fs::write(&settings, "rate_limit_retry_budget_seconds: 0\n").unwrap();
    std::fs::write(
        &models,
        json!({"providers":{"fixture":{
            "api_key":"fixture-key", "protocol":"completion",
            "base_url":"http://127.0.0.1:1/v1",
            "models":{"one":{"model":"one","thinking":{"efforts":["low"]}}}
        }}})
        .to_string(),
    )
    .unwrap();
    let mut agent = Agent::new(
        ModelCatalog::from_files(&settings, &models).unwrap(),
        Tools::new(dir.path().to_owned(), EditMode::Replace).unwrap(),
        dir.path().join("sessions"),
    )
    .unwrap();
    assert_eq!(agent.think_level, ThinkLevel::Low);
    let service = Service::start(agent.model_catalog()).unwrap();
    agent.runtime_models = Some(Binding::main(service.registry.clone()));
    let before = serde_json::to_value(&agent.session).unwrap();
    assert!(service.submit(Change::Think(ThinkLevel::Max)));
    service.flush();
    service.registry.reports();
    agent.apply_runtime_models().unwrap();
    assert_eq!(agent.think_level, ThinkLevel::Low);
    assert_eq!(serde_json::to_value(&agent.session).unwrap(), before);
    let reports = service.registry.reports();
    assert_eq!(
        reports
            .iter()
            .filter(|report| matches!(report, Report::ThinkFailed { .. }))
            .count(),
        1,
        "unsupported effort reports exactly once"
    );
    assert!(reports.iter().any(|report| matches!(
        report,
        Report::ThinkFailed {
            effort: ThinkLevel::Max,
            ..
        }
    )));
    // 失败的 revision 已被领取，不重试也不重复上报。
    agent.apply_runtime_models().unwrap();
    assert!(service.registry.reports().is_empty());
}
