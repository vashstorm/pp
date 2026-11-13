use super::*;
use crate::model_control::Binding;
use crate::model_control::Change;
use crate::model_control::Report;
use crate::model_control::Service;
use pp_config::config::ModelRole;
use pp_config::config::RoleAssignment;
use pp_config::config::RoleModel;
use pp_config::model_state::Selection;
use pp_core::model::ModelSource;
use pp_test_support::TestServer;
use std::sync::Arc;

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

/// 一次显式选择：完整模型引用 + 已解析档位。
fn selection(reference: &str, effort: ThinkLevel) -> Selection {
    Selection {
        reference: reference.into(),
        effort,
    }
}

/// 用进程级 `ModelState` 把用户输入解析成一次选择。
fn choose(agent: &Agent, reference: &str) -> Selection {
    agent
        .models()
        .resolve_reference(reference, Some(agent.think_level()))
        .unwrap_or_else(|error| panic!("resolving {reference}: {error:#}"))
}

/// 当前 Agent 的进程级模型状态启动唯一写者。
fn control(agent: &Agent) -> Service {
    Service::start(agent.models()).unwrap()
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

/// 启动期模型选择夹具：`fixture/one` 只支持 low/high、默认 low；当前模型先停在
/// `other/two`，让引用断言能区分“选中了哪个模型”。尚未发出任何请求。
fn select_model_agent(dir: &std::path::Path) -> Agent {
    let mut agent = runtime_input_agent(dir, "completion", "127.0.0.1:1", "");
    let path = dir.join("models.yml");
    let mut models: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    models["providers"]["fixture"]["models"]["one"]["thinking"] =
        json!({"efforts":["low", "high"], "defaultLevel":"low"});
    models["providers"]["other"] = json!({
        "api_key":"fixture",
        "protocol":"completion",
        "base_url":"http://127.0.0.1:1/v1",
        "models":{"two":{"model":"two"}}
    });
    std::fs::write(path, models.to_string()).unwrap();
    agent.reload_models().unwrap();
    agent.switch_model("other/two").unwrap();
    agent
}

#[test]
fn select_model_applies_requested_effort_and_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let mut agent = select_model_agent(dir.path());
    let sessions = dir.path().join("sessions");
    assert_eq!(agent.current_model().reference, "other/two");
    assert_eq!(agent.model_source(), ModelSource::Override);

    // 启动期尚未发出请求：选择模型不创建会话文件。
    let selection = agent.select_model("one:high").unwrap();
    assert_eq!(selection.reference, "fixture/one");
    assert_eq!(selection.effort, ThinkLevel::High);
    assert_eq!(agent.current_model().reference, "fixture/one");
    assert_eq!(agent.think_level(), ThinkLevel::High);
    assert_eq!(agent.model_source(), ModelSource::Override);
    assert!(agent.session_id().is_none());
    assert!(
        !sessions.exists(),
        "startup model selection must not create session files"
    );

    // 不支持的档位在 catalog 层失败：模型与档位都保持原值。
    let error = agent.select_model("one:medium").unwrap_err().to_string();
    assert!(error.contains("does not support effort medium"), "{error}");
    assert!(error.contains("low, high"), "{error}");
    assert_eq!(agent.current_model().reference, "fixture/one");
    assert_eq!(agent.think_level(), ThinkLevel::High);
    assert_eq!(agent.model_source(), ModelSource::Override);

    // 省略档位时用模型显式默认档，而不是沿用“保留当前档位”的语义。
    let selection = agent.select_model("one").unwrap();
    assert_eq!(selection.reference, "fixture/one");
    assert_eq!(selection.effort, ThinkLevel::Low);
    assert_eq!(agent.current_model().reference, "fixture/one");
    assert_eq!(agent.think_level(), ThinkLevel::Low);
    assert!(agent.session_id().is_none());
    // 选中结果与配置默认 `b` 完全一致：不保留覆盖，来源回到 Default。
    assert_eq!(agent.model_source(), ModelSource::Default);
    assert!(agent.models().override_selection().is_none());
}

#[test]
fn runtime_override_equal_to_default_clears_override_and_source() {
    let dir = tempfile::tempdir().unwrap();
    let mut agent = select_model_agent(dir.path());
    assert_eq!(agent.model_source(), ModelSource::Override);

    // 直接写与 `b` 相同的选择：不记录覆盖。
    let default = agent
        .models()
        .resolve_reference("fixture/one", Some(ThinkLevel::Low))
        .unwrap();
    assert!(!agent.models().set_override(default).unwrap());
    assert_eq!(agent.models().source(), ModelSource::Default);
    assert!(agent.models().override_selection().is_none());

    // 会话侧再切到同一选择：当前模型落到 `b`，来源保持 Default。
    agent.switch_model("fixture/one").unwrap();
    assert_eq!(agent.current_model.reference, "fixture/one");
    assert_eq!(agent.think_level, ThinkLevel::Low);
    assert_eq!(agent.model_source(), ModelSource::Default);
    assert!(agent.models().override_selection().is_none());
}

#[test]
fn runtime_save_default_keeps_override_and_resolve_follows_default() {
    let dir = tempfile::tempdir().unwrap();
    let mut agent = fixture(dir.path(), "completion");
    // 增加一个可切换的配置默认模型。
    let path = dir.path().join("models.yml");
    let mut models: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    models["providers"]["fixture"]["models"]["spare"] = json!({"model": "spare"});
    std::fs::write(path, models.to_string()).unwrap();
    agent.reload_models().unwrap();
    let service = control(&agent);
    agent.runtime_models = Some(Binding::main(service.registry.clone()));

    // 无覆盖：保存默认只改 `b`，运行中的会话不主动切换，但 resolve() 跟随。
    assert!(service.submit(Change::SaveDefault("fixture/spare".into())));
    service.flush();
    service.registry.reports();
    agent.apply_runtime_models().unwrap();
    assert_eq!(agent.current_model.reference, "fixture/one");
    assert_eq!(
        agent.models().default_selection().unwrap().reference,
        "fixture/spare"
    );
    assert_eq!(agent.models().resolve().unwrap().reference, "fixture/spare");

    // 覆盖会话：SaveDefault 与 DEFAULT 角色保存都不改变当前模型。
    assert!(service.submit(Change::Override(choose(&agent, "other/two"))));
    service.flush();
    agent.apply_runtime_models().unwrap();
    assert_eq!(agent.current_model.reference, "other/two");
    let override_selection = agent.models().override_selection().unwrap();

    assert!(service.submit(Change::SaveDefault("fixture/one".into())));
    service.flush();
    agent.apply_runtime_models().unwrap();
    assert_eq!(agent.current_model.reference, "other/two");
    assert_eq!(agent.models().resolve().unwrap(), override_selection);

    assert!(service.submit(Change::Role(
        ModelRole::Default,
        role_with("fixture/spare", ThinkLevel::High)
    )));
    service.flush();
    agent.apply_runtime_models().unwrap();
    assert_eq!(agent.current_model.reference, "other/two");
    assert_eq!(agent.model_source(), ModelSource::Override);
    assert_eq!(agent.models().resolve().unwrap(), override_selection);

    // 清除覆盖后按新的 `b` 生效。
    assert!(service.submit(Change::ClearOverride));
    service.flush();
    agent.apply_runtime_models().unwrap();
    assert_eq!(agent.current_model.reference, "fixture/spare");
    assert_eq!(agent.think_level, ThinkLevel::High);
    assert_eq!(agent.model_source(), ModelSource::Default);

    // 没有覆盖时 `b` 再变化：resolve() 立即跟随，运行中的会话等下个决议点。
    assert!(service.submit(Change::Role(ModelRole::Default, None)));
    service.flush();
    agent.apply_runtime_models().unwrap();
    assert_eq!(agent.models().resolve().unwrap().reference, "fixture/one");
    assert_eq!(agent.current_model.reference, "fixture/spare");
}

#[test]
fn runtime_default_role_change_keeps_sessions_and_roles_isolate_children() {
    for protocol in ["completion", "response"] {
        let main_dir = tempfile::tempdir().unwrap();
        let child_dir = tempfile::tempdir().unwrap();
        let inherited_dir = tempfile::tempdir().unwrap();
        let nested_dir = tempfile::tempdir().unwrap();
        let mut main = fixture(main_dir.path(), protocol);
        let mut child = fixture(child_dir.path(), protocol);
        let mut inherited = fixture(inherited_dir.path(), protocol);
        let mut nested = fixture(nested_dir.path(), protocol);
        let service = Service::start(main.models()).unwrap();
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

        // DEFAULT 角色只写配置默认 `b`：不向主会话或子 Agent 发布模型目标。
        assert!(service.submit(Change::Role(ModelRole::Default, role())));
        service.flush();
        for agent in [&mut main, &mut child, &mut inherited, &mut nested] {
            agent.apply_runtime_models().unwrap();
            assert_eq!(agent.current_model.reference, "fixture/one");
            assert_eq!(agent.model_source(), ModelSource::Default);
        }
        // `b` 已变化（下一次决议使用 other/two），但运行中的会话模型不变。
        assert_eq!(main.models().resolve().unwrap().reference, "other/two");
        assert_eq!(main.current_model.reference, "fixture/one");

        // FAST 角色的目标只到达 FAST 子 Agent（含其嵌套子 Agent）。
        assert!(service.submit(Change::Role(ModelRole::Fast, role())));
        service.flush();
        child.apply_runtime_models().unwrap();
        nested.apply_runtime_models().unwrap();
        inherited.apply_runtime_models().unwrap();
        main.apply_runtime_models().unwrap();
        assert_eq!(child.current_model.reference, "other/two");
        assert_eq!(child.think_level, ThinkLevel::Low);
        assert_eq!(child.model_source(), ModelSource::Role);
        assert_eq!(nested.current_model.reference, "other/two");
        assert_eq!(nested.think_level, ThinkLevel::Low);
        assert_eq!(inherited.current_model.reference, "fixture/one");
        assert_eq!(main.current_model.reference, "fixture/one");
        for agent in [&main, &child, &nested, &inherited] {
            assert!(
                serde_json::to_string(&agent.session)
                    .unwrap()
                    .contains("committed context marker")
            );
        }

        // 删除 FAST 角色只移除待应用槽：运行中的子 Agent 保持当前模型。
        assert!(service.submit(Change::Role(ModelRole::Fast, None)));
        service.flush();
        child.apply_runtime_models().unwrap();
        assert!(
            child
                .runtime_models
                .as_mut()
                .unwrap()
                .take_targets()
                .0
                .is_none()
        );
        assert_eq!(child.current_model.reference, "other/two");
    }
}

#[test]
fn runtime_model_latest_revision_wins_and_failed_revision_is_not_retried() {
    let dir = tempfile::tempdir().unwrap();
    let mut agent = fixture(dir.path(), "completion");
    let service = control(&agent);
    agent.runtime_models = Some(Binding::main(service.registry.clone()));
    assert!(service.submit(Change::Override(choose(&agent, "other/two"))));
    assert!(service.submit(Change::Override(selection("fixture/one", ThinkLevel::High))));
    service.flush();
    service.registry.reports();
    agent.apply_runtime_models().unwrap();
    assert_eq!(agent.current_model.reference, "fixture/one");
    assert_eq!(agent.think_level, ThinkLevel::High);
    assert_eq!(agent.model_source(), ModelSource::Override);

    // 应用失败（journal 只读）时保留当前模型与原生历史，并上报一次 Failed。
    let snapshot = serde_json::to_value(&agent.session).unwrap();
    let path = dir
        .path()
        .join("sessions")
        .join(format!("{}.jsonl", agent.session_id().unwrap()));
    agent.journal.use_read_only_file(&path);
    assert!(service.submit(Change::Override(choose(&agent, "other/two"))));
    service.flush();
    let queued = service.registry.reports();
    let revision = queued
        .iter()
        .find_map(|report| match report {
            Report::Queued { revision, .. } => Some(*revision),
            _ => None,
        })
        .expect("a manual override is queued before it is applied");
    agent.apply_runtime_models().unwrap();
    assert_eq!(agent.current_model.reference, "fixture/one");
    assert_eq!(serde_json::to_value(&agent.session).unwrap(), snapshot);
    assert!(
        service
            .registry
            .reports()
            .iter()
            .any(|report| matches!(report, Report::Failed { revision: failed, .. } if *failed == revision)),
        "the failed revision is reported with its own revision"
    );
    // 失败的 revision 已被领取，不重试也不重复上报。
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
    let service = control(&agent);
    agent.runtime_models = Some(Binding::main(service.registry.clone()));
    assert!(service.submit(Change::Override(choose(&agent, "other/two"))));
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
    let service = control(&agent);
    let mut fast =
        Binding::main(service.registry.clone()).child(Some(ModelRole::Fast), "fast".into());
    std::fs::rename(
        dir.path().join("agent.db"),
        dir.path().join("saved-agent.db"),
    )
    .unwrap();
    std::fs::create_dir(dir.path().join("agent.db")).unwrap();
    assert!(service.submit(Change::Role(ModelRole::Fast, role())));
    service.flush();
    assert!(fast.take_targets().0.is_none());
    assert!(
        service
            .registry
            .reports()
            .iter()
            .any(|report| matches!(report, Report::Failed { revision: 0, .. }))
    );
}

#[test]
fn runtime_override_survives_plan_enter_leave_and_restore() {
    let dir = tempfile::tempdir().unwrap();
    let mut agent = fixture(dir.path(), "completion");
    // 覆盖经运行控制通道（`Change::Override`）应用到主会话。
    let service = control(&agent);
    agent.runtime_models = Some(Binding::main(service.registry.clone()));
    assert!(service.submit(Change::Override(choose(&agent, "other/two"))));
    service.flush();
    agent.apply_runtime_models().unwrap();
    assert_eq!(agent.model_source(), ModelSource::Override);
    let expected = agent.current_model.reference.clone();
    let expected_level = agent.think_level;
    let models = agent.models();

    // 进入 Plan：手动覆盖优先，不套用 PLAN 角色。
    agent.enter_plan(&CancellationToken::new()).unwrap();
    assert_eq!(agent.current_model.reference, expected);
    assert_eq!(agent.think_level, expected_level);
    assert_eq!(agent.model_source(), ModelSource::Override);
    // 退出 Plan：按 resolve() 交接，仍是同一个覆盖模型。
    agent.leave_plan(&CancellationToken::new(), None).unwrap();
    assert_eq!(agent.current_model.reference, expected);
    assert_eq!(agent.think_level, expected_level);
    assert_eq!(agent.model_source(), ModelSource::Override);
    assert!(agent.role_route.is_none());

    // Agent::restore 也回到 resolve()：进程级覆盖在同一个 ModelState 上仍是原模型。
    let id = agent.session_id().unwrap().to_owned();
    drop(agent);
    let sessions = dir.path().join("sessions");
    let restored =
        persistence::load_test(&sessions, &sessions.join(".locks"), Some(&id), true).unwrap();
    let agent = Agent::restore(
        models,
        Tools::new(dir.path().to_owned(), EditMode::Replace).unwrap(),
        restored,
    )
    .unwrap();
    assert_eq!(agent.current_model.reference, expected);
    assert_eq!(agent.think_level, expected_level);
    assert_eq!(agent.model_source(), ModelSource::Override);
    assert!(agent.role_route.is_none());
    assert!(
        serde_json::to_string(&agent.session)
            .unwrap()
            .contains("committed context marker")
    );
}

#[test]
fn runtime_fallback_route_uses_default_role_and_returns_to_resolved() {
    let dir = tempfile::tempdir().unwrap();
    let mut agent = fixture(dir.path(), "completion");
    agent
        .save_model_role(
            ModelRole::Default,
            Some(RoleAssignment {
                enabled: true,
                model: "fixture/one".into(),
                effort: ThinkLevel::Low,
                fallback: Some(RoleModel {
                    model: "other/two".into(),
                    effort: ThinkLevel::High,
                }),
            }),
        )
        .unwrap();
    assert_eq!(agent.model_source(), ModelSource::Default);

    let mut events = Vec::new();
    assert!(
        agent
            .activate_fallback(&mut |event| events.push(event))
            .unwrap()
    );
    assert_eq!(agent.current_model.reference, "other/two");
    assert_eq!(agent.think_level, ThinkLevel::High);
    assert_eq!(agent.model_source(), ModelSource::Fallback);
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::ModelRouteChanged {
            model,
            effort: ThinkLevel::High,
            source: ModelSource::Fallback,
            ..
        } if model == "other/two"
    )));
    // 已处于 fallback：不再重复切换。
    assert!(!agent.activate_fallback(&mut |_| {}).unwrap());

    // 任务结束回到 resolve()（`b` = DEFAULT 角色 fixture/one / low）。
    agent.restore_resolved(&mut |_| {}).unwrap();
    assert_eq!(agent.current_model.reference, "fixture/one");
    assert_eq!(agent.think_level, ThinkLevel::Low);
    assert_eq!(agent.model_source(), ModelSource::Default);
    assert!(agent.role_route.is_none());
}

#[test]
fn runtime_restore_uses_resolved_model_and_drops_checkpoint_route() {
    let dir = tempfile::tempdir().unwrap();
    let mut agent = fixture(dir.path(), "completion");
    // 旧 checkpoint 的角色路由只作审计：恢复后的模型由 resolve() 决定。
    agent.role_route = Some(pp_config::config::RoleRoute {
        role: ModelRole::Default,
        primary: RoleModel {
            model: "fixture/one".into(),
            effort: ThinkLevel::Low,
        },
        fallback: Some(RoleModel {
            model: "other/two".into(),
            effort: ThinkLevel::High,
        }),
    });
    agent.checkpoint_route().unwrap();
    let models = agent.models();
    let id = agent.session_id().unwrap().to_owned();
    drop(agent);

    let sessions = dir.path().join("sessions");
    let locks = sessions.join(".locks");
    let saved = persistence::load_test(&sessions, &locks, Some(&id), true).unwrap();
    assert_eq!(
        saved.checkpoint.role_route.as_ref().map(|route| route.role),
        Some(ModelRole::Default)
    );
    let restored = Agent::restore(
        models,
        Tools::new(dir.path().to_owned(), EditMode::Replace).unwrap(),
        saved,
    )
    .unwrap();
    assert_eq!(restored.current_model.reference, "fixture/one");
    assert_eq!(restored.model_source(), ModelSource::Default);
    assert!(restored.role_route.is_none(), "恢复不还原 checkpoint 路由");
    drop(restored);

    let saved = persistence::load_test(&sessions, &locks, Some(&id), true).unwrap();
    assert!(saved.checkpoint.role_route.is_none(), "恢复后清理旧路由");
}

#[test]
fn fallback_checkpoint_failure_keeps_model_and_native_history() {
    let dir = tempfile::tempdir().unwrap();
    let mut agent = fixture(dir.path(), "completion");
    agent
        .save_model_role(
            ModelRole::Default,
            Some(RoleAssignment {
                enabled: true,
                model: "fixture/one".into(),
                effort: agent.think_level,
                fallback: Some(RoleModel {
                    model: "other/two".into(),
                    effort: ThinkLevel::Low,
                }),
            }),
        )
        .unwrap();
    let before = serde_json::to_value(&agent.session).unwrap();
    let path = dir
        .path()
        .join("sessions")
        .join(format!("{}.jsonl", agent.session_id().unwrap()));
    agent.journal.use_read_only_file(&path);
    assert!(agent.activate_fallback(&mut |_| {}).is_err());
    assert_eq!(agent.current_model.reference, "fixture/one");
    assert_eq!(agent.model_source(), ModelSource::Default);
    assert_eq!(serde_json::to_value(&agent.session).unwrap(), before);
}

#[test]
fn runtime_binding_roles_route_main_children_and_custom_children() {
    let dir = tempfile::tempdir().unwrap();
    let agent = fixture(dir.path(), "completion");
    let service = control(&agent);
    let registry = service.registry.clone();

    // `Change::Override` 只写 main 槽：主 Agent 拿到手动选择，子 Agent 一无所获。
    assert!(service.submit(Change::Override(selection("other/two", ThinkLevel::None))));
    service.flush();
    let mut main = Binding::main(registry.clone());
    let (target, think) = main.take_targets();
    let target = target.expect("manual selection is visible to the main agent");
    assert_eq!(
        target
            .selection
            .as_ref()
            .map(|selection| selection.reference.as_str()),
        Some("other/two")
    );
    assert_eq!(target.source, ModelSource::Override);
    assert!(think.is_none());

    // 每个断言用独立 Binding：seen 是每 Binding 独立的；子 Agent 只消费自己的角色槽。
    let mut default_child = main.child(Some(ModelRole::Default), "child-default".into());
    let mut fast_child = main.child(Some(ModelRole::Fast), "child-fast".into());
    let mut custom_child = main.child(None, "child-custom".into());
    assert!(
        default_child.take_targets().0.is_none(),
        "DEFAULT 不再是子 Agent 角色槽，也不继承手动覆盖"
    );
    assert!(
        fast_child.take_targets().0.is_none(),
        "FAST child must not inherit the main override"
    );
    assert!(custom_child.take_targets().0.is_none());

    // FAST 角色目标只写 roles[Fast]：主 Agent 与其它 child 都看不到。
    assert!(service.submit(Change::Role(
        ModelRole::Fast,
        role_with("other/two", ThinkLevel::Medium)
    )));
    service.flush();
    let (target, _) = fast_child.take_targets();
    let target = target.expect("FAST child follows the FAST role");
    let chosen = target.selection.expect("role target carries a selection");
    assert_eq!(chosen.reference, "other/two");
    assert_eq!(chosen.effort, ThinkLevel::Medium);
    assert_eq!(target.source, ModelSource::Role);
    assert!(
        main.take_targets().0.is_none(),
        "role targets never enter the main slot"
    );
    assert!(default_child.take_targets().0.is_none());
    assert!(custom_child.take_targets().0.is_none());

    // DEFAULT 角色即配置默认 `b`：只保存配置，不发布任何模型目标。
    assert!(service.submit(Change::Role(
        ModelRole::Default,
        role_with("fixture/one", ThinkLevel::High)
    )));
    service.flush();
    // 用同一个 main Binding：新建 Binding 会重新看到此前尚未过期的手动目标。
    assert!(
        main.take_targets().0.is_none(),
        "DEFAULT role only writes b; it must not publish a main target"
    );
    assert!(fast_child.take_targets().0.is_none());

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
    let think_only = control(&think_agent);
    let mut probe = Binding::main(think_only.registry.clone());
    assert!(think_only.submit(Change::Think(ThinkLevel::High)));
    think_only.flush();
    assert!(!probe.pending(), "pending() only tracks model targets");
    let (target, think) = probe.take_targets();
    assert!(target.is_none());
    assert_eq!(think.expect("think target").effort, ThinkLevel::High);
    // 同一 revision 不被重复领取。
    assert!(probe.take_targets().1.is_none());

    // 应用侧：主 Agent 不消费角色槽；覆盖目标切换后来源保持 Override。
    let route_dir = tempfile::tempdir().unwrap();
    let mut agent = fixture(route_dir.path(), "completion");
    let routes = control(&agent);
    agent.runtime_models = Some(Binding::main(routes.registry.clone()));
    assert!(routes.submit(Change::Role(
        ModelRole::Fast,
        role_with("other/two", ThinkLevel::Low)
    )));
    routes.flush();
    agent.apply_runtime_models().unwrap();
    assert_eq!(agent.current_model.reference, "fixture/one");
    assert!(agent.role_route.is_none());

    assert!(routes.submit(Change::Override(selection("other/two", ThinkLevel::Low))));
    routes.flush();
    agent.apply_runtime_models().unwrap();
    assert_eq!(agent.current_model.reference, "other/two");
    assert_eq!(agent.model_source(), ModelSource::Override);

    // 同批领取时按 revision 顺序应用：model 较早、think 较晚，高 revision 的 effort 胜出。
    assert!(routes.submit(Change::Override(selection("fixture/one", ThinkLevel::Low))));
    assert!(routes.submit(Change::Think(ThinkLevel::High)));
    routes.flush();
    agent.apply_runtime_models().unwrap();
    assert_eq!(agent.current_model.reference, "fixture/one");
    assert_eq!(agent.think_level, ThinkLevel::High);

    // think 较早、角色目标较晚：角色目标不进主槽，手动档位仍然生效。
    assert!(routes.submit(Change::Think(ThinkLevel::Low)));
    assert!(routes.submit(Change::Role(
        ModelRole::Fast,
        role_with("other/two", ThinkLevel::Medium)
    )));
    routes.flush();
    agent.apply_runtime_models().unwrap();
    assert_eq!(agent.current_model.reference, "fixture/one");
    assert_eq!(agent.think_level, ThinkLevel::Low);
}

#[test]
fn runtime_effort_change_keeps_request_prefix_identical() {
    for protocol in ["completion", "response"] {
        // 路径一：手动档位槽（`Change::Think`）。
        let dir = tempfile::tempdir().unwrap();
        let mut agent = fixture(dir.path(), protocol);
        let service = control(&agent);
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
        let service = control(&agent);
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
        let server = Arc::new(TestServer::start().unwrap());
        let mut agent = runtime_input_agent(dir.path(), protocol, &server.addr().to_string(), "");
        // 先发布一次配置写：`Change::Think` 的待应用 revision 来自 ModelState。
        agent.reload_models().unwrap();
        let (tx, rx) = std::sync::mpsc::channel::<Value>();
        let serving = protocol.to_owned();
        let worker = Arc::clone(&server);
        let server_thread = std::thread::spawn(move || {
            let request = worker
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap()
                .unwrap();
            let body: Value = request.json().unwrap();
            tx.send(body).unwrap();
            tool_call_for(&serving, request, "read", r#"{"path":"marker.txt"}"#);
            let request = worker
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap()
                .unwrap();
            let body: Value = request.json().unwrap();
            tx.send(body).unwrap();
            final_for(&serving, request, "marker read", true);
        });
        let service = control(&agent);
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
        let server = TestServer::start().unwrap();
        let address = server.addr().to_string();
        let mut agent = runtime_input_agent(dir.path(), protocol, &address, "");
        add_other_provider(dir.path(), protocol, &address);
        agent.reload_models().unwrap();
        agent
            .save_model_role(role, role_with("fixture/one", ThinkLevel::Low))
            .unwrap();
        let service = control(&agent);
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
        let request = server
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap()
            .unwrap();
        let body: Value = request.json().unwrap();
        assert_eq!(body["model"], "one");
        assert_eq!(request_effort(protocol, &body), &json!("low"));
        assert!(service.submit(Change::Role(role, role_with("other/two", ThinkLevel::High))));
        service.flush();
        tool_call_for(protocol, request, "read", r#"{"path":"marker.txt"}"#);

        // 第二轮：同一次运行里同时切到新 model 与新 effort。
        let request = server
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap()
            .unwrap();
        let body: Value = request.json().unwrap();
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
    // 先发布一次配置写：`Change::Think` 的待应用 revision 来自 ModelState。
    agent.reload_models().unwrap();
    let service = control(&agent);
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
