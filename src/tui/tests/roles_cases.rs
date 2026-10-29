use super::*;
use crate::config::{ModelRole, RoleAssignment};

#[test]
fn roles_command_opens_separate_hub_and_model_command_keeps_existing_picker() {
    let (mut app, requests, _) = plan_app();
    app.choices = favorite_model_choices();
    app.current_model = Some(app.choices[0].clone());
    for command in ["/Roles", "/roles"] {
        app.input = editor();
        app.input.insert_str(command);
        app.submit(PendingKind::Steering);
        assert!(app.roles_panel.is_some());
        assert!(app.model_panel.is_none());
        assert!(app.needs_alternate());
        assert!(draw(&mut app, 130, 30).contains("DEFAULT"));
        app.handle(key(KeyCode::Esc), &mut false);
        assert!(app.roles_panel.is_none());
    }
    assert!(requests.try_recv().is_err());
    app.open_model_picker();
    assert!(app.model_panel.is_some());
    assert!(app.roles_panel.is_none());
}

#[test]
fn roles_saved_snapshot_updates_preferences_only_and_failure_keeps_previous_role() {
    let (mut app, requests, events) = plan_app();
    app.choices = favorite_model_choices();
    app.current_model = Some(app.choices[0].clone());
    app.open_roles();
    let assignment = RoleAssignment {
        enabled: true,
        model: app.choices[1].reference.clone(),
        effort: ThinkLevel::High,
    };
    app.apply_roles_action(crate::tui::roles::Action::Save(
        ModelRole::Fast,
        Some(assignment.clone()),
    ));
    assert!(matches!(
        requests.try_recv(),
        Ok(WorkerCommand::SaveModelRole(ModelRole::Fast, Some(_)))
    ));
    assert!(app.model_roles.is_empty());
    events
        .send(WorkerEvent::ModelRolesSaved(Ok((
            std::collections::BTreeMap::from([(ModelRole::Fast, assignment.clone())]),
            app.choices[0].reference.clone(),
            ThinkLevel::None,
        ))))
        .unwrap();
    app.receive();
    assert_eq!(app.model_roles[&ModelRole::Fast], assignment);
    assert_eq!(
        app.current_model.as_ref().unwrap().reference,
        app.choices[0].reference
    );
    app.apply_roles_action(crate::tui::roles::Action::Save(ModelRole::Fast, None));
    requests.try_recv().unwrap();
    events
        .send(WorkerEvent::ModelRolesSaved(Err(anyhow::anyhow!(
            "role-save-failed"
        ))))
        .unwrap();
    app.receive();
    assert_eq!(app.model_roles[&ModelRole::Fast], assignment);
    assert!(!app.roles_panel.as_ref().unwrap().busy);
    app.notifications
        .tick(Instant::now() + Duration::from_millis(200));
    assert!(draw(&mut app, 130, 30).contains("role-save-failed"));
}
