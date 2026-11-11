use super::*;
use pp_config::config::ModelRole;
use pp_config::config::RoleAssignment;

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

#[test]
fn model_switch_report_is_not_overwritten_by_an_older_worker_snapshot() {
    use pp_core::model::ModelSource;
    use pp_runtime::model_control::Binding;
    use pp_runtime::model_control::Report;
    use pp_runtime::model_control::Service;
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
    // worker 快照已排队，控制线程的新结果在 UI 下一次 receive 前到达。
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
        source: ModelSource::Override,
    });
    app.receive();
    assert_eq!(
        app.current_model.as_ref().unwrap().reference,
        selected.reference
    );
    assert_eq!(app.current_source, Some(ModelSource::Override));
    // 应用后的 worker 快照补齐 effort，并保持来源一致。
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
    // 手动覆盖不再尾随状态栏；显示仍保持新模型。
    let status = screen.lines().last().unwrap();
    assert!(
        !status.contains("override") && status.contains("gateway/team/Beta"),
        "{screen}"
    );
}

#[test]
fn roles_backspace_clears_override_and_no_override_never_submits_clear() {
    use pp_core::model::ModelSource;
    let (control, _binding, choices) = model_control_with(
        r#"{"providers":{"fixture":{"api_key":"fixture","models":{"one":{"model":"one"},"two":{"model":"two","thinking":{"efforts":["low","high"]}}}}}}"#,
    );
    let (mut app, requests, _events) = plan_app();
    app.model_control = Some(control);
    app.choices = choices;
    app.current_model = Some(app.choices[0].clone());
    app.default_think_level = ThinkLevel::High;
    // ⌥P 的映射由 model_menu_cases 覆盖；这里直接走同一条提交路径登记覆盖。
    assert!(app.request_model_switch("fixture/two".into()));
    let selection = wait_for_override(&mut app, "fixture/two");
    // 目标档位沿既有策略：two 无显式 defaultLevel，当前档位 off 与全局默认都不在
    // 它允许的 low/high 内，因此落到该模型的首个允许档位 Low。
    assert_eq!(selection.effort, ThinkLevel::Low);
    app.current_source = Some(ModelSource::Override);
    // 覆盖路径不直接发 worker 命令，只补发待应用目标的叫醒包。
    while let Ok(command) = requests.try_recv() {
        assert!(matches!(command, WorkerCommand::ApplyModelTargets));
    }

    app.open_roles();
    let screen = draw(&mut app, 140, 30);
    assert!(screen.contains("Override"), "{screen}");
    assert!(screen.contains("fixture/two"), "{screen}");
    assert!(screen.contains("Backspace clear override"), "{screen}");

    let mut dirty = false;
    app.handle(key(KeyCode::Right), &mut dirty);
    app.handle(key(KeyCode::Backspace), &mut dirty);
    for _ in 0..400 {
        app.receive();
        if app.override_selection.is_none() && app.current_source.is_none() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(app.override_selection.is_none());
    assert_eq!(app.current_source, None);
    assert!(app.pending_model.is_none());
    // 关键回归护栏：清除覆盖后必须叫醒 worker 把会话切回 `resolve()`，
    // 否则 registry 里 `selection: None` 的目标永远不会被应用，agent 停在旧模型。
    assert!(
        std::iter::from_fn(|| requests.try_recv().ok())
            .any(|command| matches!(command, WorkerCommand::ApplyModelTargets)),
        "清除覆盖后应补发 ApplyModelTargets"
    );

    // 无覆盖时 Backspace 不产生任何新报告（面板不提交 ClearOverride）。
    app.handle(key(KeyCode::Backspace), &mut dirty);
    std::thread::sleep(std::time::Duration::from_millis(20));
    assert!(!app.poll_model_control());
    assert!(app.override_selection.is_none());
}
