use super::*;
use crate::{
    config::{ModelRole, RoleAssignment},
    model_control::{Binding, Change, Report, Service},
};

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
        enabled: true,
        model: "other/two".into(),
        effort: ThinkLevel::Low,
    })
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
    assert!(main.take().is_none());
    assert!(
        service
            .registry
            .reports()
            .iter()
            .any(|report| matches!(report, Report::Failed { revision: 0, .. }))
    );
}
