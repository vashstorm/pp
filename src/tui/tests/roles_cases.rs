use super::*;
use crate::config::{ModelRole, RoleAssignment};

#[test]
fn roles_favorites_commit_before_reorder_and_keep_focus_after_success_or_failure() {
    let (mut app, requests, events) = plan_app();
    app.choices = favorite_model_choices();
    app.current_model = Some(app.choices[0].clone());
    app.favorite_models = vec![app.choices[3].reference.clone()];
    app.open_roles();
    let mut dirty = false;
    for code in [KeyCode::Down, KeyCode::Right, KeyCode::Down] {
        app.handle(key(code), &mut dirty);
    }
    let selected = app.choices[0].reference.clone();
    app.handle(control('f'), &mut dirty);
    assert!(
        matches!(requests.try_recv(), Ok(WorkerCommand::ToggleFavoriteModel(reference)) if reference == selected)
    );
    assert_eq!(app.favorite_models.len(), 1);
    assert!(app.roles_panel.as_ref().unwrap().busy);
    for event in [control('f'), key(KeyCode::Enter)] {
        app.handle(event, &mut dirty);
    }
    assert!(requests.try_recv().is_err());
    events
        .send(WorkerEvent::FavoriteModelsSaved(Ok(vec![
            app.choices[3].reference.clone(),
            selected.clone(),
        ])))
        .unwrap();
    app.receive();
    assert!(!app.roles_panel.as_ref().unwrap().busy);
    let text = draw(&mut app, 140, 30);
    assert!(text.contains(&format!("❯ {selected} ")));
    assert!(text.find(&selected).unwrap() < text.find(&app.choices[3].reference).unwrap());
    app.handle(control('f'), &mut dirty);
    assert!(
        matches!(requests.try_recv(), Ok(WorkerCommand::ToggleFavoriteModel(reference)) if reference == selected)
    );
    events
        .send(WorkerEvent::FavoriteModelsSaved(Err(anyhow::anyhow!(
            "favorite-save-failed"
        ))))
        .unwrap();
    app.receive();
    assert_eq!(app.favorite_models.len(), 2);
    assert!(!app.roles_panel.as_ref().unwrap().busy);
    let text = draw(&mut app, 140, 30);
    assert!(text.contains(&format!("❯ {selected} ")));
    assert!(text.contains("favorite-save-failed"));
    assert_eq!(app.current_model.as_ref().unwrap().reference, selected);
    assert!(app.model_roles.is_empty());
}

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
        fallback: None,
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
    assert!(draw(&mut app, 130, 30).contains("role-save-failed"));
}
