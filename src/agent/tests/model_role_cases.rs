use super::*;
use crate::config::{ModelRole, RoleAssignment};
use crate::plan::WorkMode;

#[test]
fn model_roles_plan_applies_once_and_manual_model_and_effort_survive_continuation() {
    for protocol in ["completion", "response"] {
        let dir = tempfile::tempdir().unwrap();
        let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
        let path = dir.path().join("models.yml");
        let mut models: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        models["providers"]["fixture"]["models"]["planner"] = json!({"model":"planner"});
        std::fs::write(path, models.to_string()).unwrap();
        agent.reload_models().unwrap();
        agent
            .save_model_role(
                ModelRole::Plan,
                Some(RoleAssignment {
                    enabled: true,
                    model: "fixture/planner".into(),
                    effort: ThinkLevel::High,
                }),
            )
            .unwrap();
        seed_session(&mut agent, "planning context marker");
        agent.enter_plan(&CancellationToken::new()).unwrap();
        assert_eq!(agent.current_model.reference, "fixture/planner");
        assert_eq!(agent.think_level, ThinkLevel::High);
        assert_eq!(agent.workflow.mode, WorkMode::Plan);
        assert!(
            serde_json::to_string(&agent.session)
                .unwrap()
                .contains("planning context marker")
        );
        agent.switch_model("fixture/one").unwrap();
        agent.set_think_level(ThinkLevel::Low).unwrap();
        agent.enter_plan(&CancellationToken::new()).unwrap();
        assert_eq!(agent.current_model.reference, "fixture/one");
        assert_eq!(agent.think_level, ThinkLevel::Low);
        assert_eq!(
            agent.model_roles()[&ModelRole::Plan].model,
            "fixture/planner"
        );
    }
}

#[test]
fn model_roles_unavailable_plan_fails_before_changing_workflow_or_history() {
    let dir = tempfile::tempdir().unwrap();
    let mut agent = runtime_input_agent(
        dir.path(),
        "completion",
        "127.0.0.1:1",
        "model_roles: {plan: {model: removed/model, effort: high}}\n",
    );
    let session = serde_json::to_value(&agent.session).unwrap();
    assert!(agent.enter_plan(&CancellationToken::new()).is_err());
    assert_eq!(agent.workflow.mode, WorkMode::Execute);
    assert_eq!(serde_json::to_value(&agent.session).unwrap(), session);
}
