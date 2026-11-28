use super::*;
use pp_config::config::ModelRole;
use pp_config::config::RoleAssignment;

#[test]
fn roles_command_favorites_and_saved_preferences() {
    let (mut app, requests, events) = plan_app();
    app.choices = favorite_model_choices();
    app.current_model = Some(app.choices[0].clone());
    app.favorite_models = vec![app.choices[3].reference.clone()];
    app.open_roles();
    let mut dirty = false;
    for code in [KeyCode::Down, KeyCode::Tab, KeyCode::Down] {
        app.handle(key(code), &mut dirty);
    }
    let selected = app.choices[0].reference.clone();
    app.handle(control('f'), &mut dirty);
    assert!(
        matches!(requests.try_recv(), Ok(WorkerCommand::ToggleFavoriteModel(reference)) if reference == selected)
    );
    assert!(app.favorite_models.len() == 1 && app.roles_panel.as_ref().unwrap().busy);
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
    assert!(text.contains(&format!("❯ {selected} ")) && text.contains("favorite-save-failed"));
    assert_eq!(app.current_model.as_ref().unwrap().reference, selected);
    assert!(app.model_roles.is_empty());

    let (mut hub, hub_req, _) = plan_app();
    hub.choices = favorite_model_choices();
    hub.current_model = Some(hub.choices[0].clone());
    hub.input = editor();
    hub.input.insert_str("/Roles");
    hub.submit(PendingKind::Steering);
    assert!(hub.roles_panel.is_none());
    hub.input = editor();
    hub.input.insert_str("/roles");
    hub.submit(PendingKind::Steering);
    assert!(hub.roles_panel.is_some() && hub.model_panel.is_none() && hub.needs_alternate());
    assert!(draw(&mut hub, 130, 30).contains("DEFAULT"));
    hub.handle(key(KeyCode::Esc), &mut false);
    assert!(hub.roles_panel.is_none() && hub_req.try_recv().is_err());
    hub.open_model_picker();
    assert!(hub.model_panel.is_some() && hub.roles_panel.is_none());

    let (mut saved, saved_req, saved_events) = plan_app();
    saved.choices = favorite_model_choices();
    saved.current_model = Some(saved.choices[0].clone());
    saved.open_roles();
    let assignment = RoleAssignment {
        fallback: None,
        enabled: true,
        model: saved.choices[1].reference.clone(),
        effort: ThinkLevel::High,
    };
    saved.apply_roles_action(crate::tui::roles::Action::Save(
        ModelRole::Fast,
        Some(assignment.clone()),
    ));
    assert!(matches!(
        saved_req.try_recv(),
        Ok(WorkerCommand::SaveModelRole(ModelRole::Fast, Some(_)))
    ));
    assert!(saved.model_roles.is_empty());
    saved_events
        .send(WorkerEvent::ModelRolesSaved(Ok((
            std::collections::BTreeMap::from([(ModelRole::Fast, assignment.clone())]),
            saved.choices[0].reference.clone(),
            ThinkLevel::None,
        ))))
        .unwrap();
    saved.receive();
    assert_eq!(saved.model_roles[&ModelRole::Fast], assignment);
    assert_eq!(
        saved.current_model.as_ref().unwrap().reference,
        saved.choices[0].reference
    );
    saved.apply_roles_action(crate::tui::roles::Action::Save(ModelRole::Fast, None));
    saved_req.try_recv().unwrap();
    saved_events
        .send(WorkerEvent::ModelRolesSaved(Err(anyhow::anyhow!(
            "role-save-failed"
        ))))
        .unwrap();
    saved.receive();
    assert_eq!(saved.model_roles[&ModelRole::Fast], assignment);
    assert!(!saved.roles_panel.as_ref().unwrap().busy);
    assert!(draw(&mut saved, 130, 30).contains("role-save-failed"));
}

#[test]
fn roles_model_switch_report_and_clear_override() {
    use pp_core::model::ModelSource;
    use pp_runtime::model_control::{Binding, Report, Service};
    let dir = tempfile::tempdir().unwrap();
    let settings = dir.path().join("settings.yml");
    let models = dir.path().join("models.yml");
    std::fs::write(&settings, "{}").unwrap();
    std::fs::write(
        &models,
        r#"{"providers":{"fixture":{"api_key":"fixture","models":{"one":{"model":"one"}}}}}"#,
    )
    .unwrap();
    let catalog = pp_config::config::ModelCatalog::from_files(&settings, &models).unwrap();
    let control = Service::start(pp_config::model_state::ModelState::shared(catalog)).unwrap();
    let binding = Binding::main(control.registry.clone());
    let (mut app, _requests, events) = plan_app();
    app.choices = favorite_model_choices();
    let old = app.choices[0].clone();
    let selected = app.choices[1].clone();
    app.current_model = Some(old.clone());
    app.current_source = Some(ModelSource::Default);
    app.model_control = Some(control);
    events
        .send(WorkerEvent::ModelState {
            revision: 0,
            model: old,
            effort: ThinkLevel::Low,
            source: ModelSource::Default,
        })
        .unwrap();
    binding.report(Report::Applied {
        agent: None,
        revision: 1,
        model: selected.clone(),
        effort: ThinkLevel::High,
        source: ModelSource::Override,
    });
    app.receive();
    assert_eq!(
        app.current_model.as_ref().unwrap().reference,
        selected.reference
    );
    assert_eq!(
        (app.think_level, app.current_source),
        (ThinkLevel::High, Some(ModelSource::Override))
    );
    events
        .send(WorkerEvent::ModelState {
            revision: 1,
            model: selected.clone(),
            effort: ThinkLevel::High,
            source: ModelSource::Override,
        })
        .unwrap();
    app.receive();
    assert_eq!(app.think_level, ThinkLevel::High);
    let screen = draw(&mut app, 130, 30);
    let status = screen.lines().last().unwrap();
    assert!(
        !status.contains("override") && status.contains("gateway/team/Beta"),
        "{screen}"
    );

    let (control, _binding, choices) = model_control_with(
        r#"{"providers":{"fixture":{"api_key":"fixture","models":{"one":{"model":"one"},"two":{"model":"two","thinking":{"efforts":["low","high"]}}}}}}"#,
    );
    let (mut ovr, requests, _events) = plan_app();
    ovr.model_control = Some(control);
    ovr.choices = choices;
    ovr.current_model = Some(ovr.choices[0].clone());
    ovr.default_think_level = ThinkLevel::High;
    assert!(ovr.request_model_switch("fixture/two".into()));
    let selection = wait_for_override(&mut ovr, "fixture/two");
    assert_eq!(selection.effort, ThinkLevel::Low);
    ovr.current_source = Some(ModelSource::Override);
    while let Ok(command) = requests.try_recv() {
        assert!(matches!(command, WorkerCommand::ApplyModelTargets));
    }

    ovr.open_roles();
    let screen = draw(&mut ovr, 140, 30);
    assert!(
        screen.contains("Override")
            && screen.contains("fixture/two")
            && screen.contains("Backspace clear override")
    );
    let mut dirty = false;
    ovr.handle(key(KeyCode::Tab), &mut dirty);
    ovr.handle(key(KeyCode::Backspace), &mut dirty);
    for _ in 0..400 {
        ovr.receive();
        if ovr.override_selection.is_none() && ovr.current_source.is_none() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(
        ovr.override_selection.is_none()
            && ovr.current_source.is_none()
            && ovr.pending_model.is_none()
    );
    assert!(
        std::iter::from_fn(|| requests.try_recv().ok())
            .any(|command| matches!(command, WorkerCommand::ApplyModelTargets))
    );
    ovr.handle(key(KeyCode::Backspace), &mut dirty);
    std::thread::sleep(std::time::Duration::from_millis(20));
    assert!(!ovr.poll_model_control() && ovr.override_selection.is_none());
}
