use super::*;
use pp_runtime::user_input;
use pp_runtime::user_input::Part;

fn resource_app() -> (App, Receiver<WorkerCommand>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let mut app = App::new_with_skills(
        Err(anyhow::anyhow!("fixture")),
        pp_config::skills::Skills::default().shared(),
        commands::FileCommands::default(),
        dir.path(),
        None,
    )
    .unwrap();
    let (sender, receiver) = mpsc::channel();
    app.commands = Some(sender);
    app.state = State::Ready;
    (app, receiver, dir)
}
fn image() -> user_input::Image {
    let mut bytes = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(2, 3)
        .write_to(&mut bytes, image::ImageFormat::Png)
        .unwrap();
    user_input::image_bytes("fixture", bytes.get_ref()).unwrap()
}
fn settle(app: &mut App) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while app.resources.job.is_some() && Instant::now() < deadline {
        app.receive();
        thread::yield_now();
    }
    assert!(app.resources.job.is_none());
}

#[test]
fn resource_image_markers_submission_and_queue_snapshots() {
    let (mut app, _, _) = resource_app();
    app.insert_images(vec![image()]);
    assert_eq!(app.resources.active_images(&app.input_text()).len(), 1);
    let mut dirty = false;
    app.handle(key(KeyCode::Backspace), &mut dirty);
    assert_eq!(app.input_text(), "");
    assert!(app.resources.active_images(&app.input_text()).is_empty());
    app.handle(control('u'), &mut dirty);
    assert_eq!(app.input_text(), "\u{f03e} #1");
    assert_eq!(app.resources.active_images(&app.input_text()).len(), 1);
    app.handle(key(KeyCode::Backspace), &mut dirty);
    app.handle(Event::Paste("\u{f03e} #1".into()), &mut dirty);
    assert!(app.resources.active_images(&app.input_text()).is_empty());

    let (mut submit_app, commands, _) = resource_app();
    submit_app.insert_images(vec![image(), image()]);
    assert!(!submit_app.submit(PendingKind::Steering));
    settle(&mut submit_app);
    let WorkerCommand::RunInput { prepared, .. } = commands.try_recv().unwrap() else {
        panic!("structured input should be sent")
    };
    assert_eq!(
        prepared
            .parts
            .iter()
            .filter(|p| matches!(p, Part::Image(_)))
            .count(),
        2
    );
    assert_eq!(prepared.images.len(), 2);
    assert!(submit_app.input.is_empty());
    assert_eq!(submit_app.input_history, ["\u{f03e} #1\u{f03e} #2"]);
    submit_app.show_history(0);
    assert_eq!(
        submit_app
            .resources
            .active_images(&submit_app.input_text())
            .len(),
        2
    );

    for kind in [PendingKind::Steering, PendingKind::FollowUp] {
        let (mut app, _, dir) = resource_app();
        std::fs::write(dir.path().join("file"), "original").unwrap();
        app.state = State::Running;
        app.insert_input("@file ".into());
        app.insert_images(vec![image()]);
        app.submit(kind);
        settle(&mut app);
        std::fs::write(dir.path().join("file"), "changed").unwrap();
        let queued = app.input_queue.claim_next().unwrap().unwrap();
        assert_eq!(queued.kind, kind);
        let prepared = queued.prepared.as_ref().unwrap();
        assert!(
            prepared
                .parts
                .iter()
                .any(|p| matches!(p, Part::Text(t) if t.contains("original")))
        );
        assert!(prepared.has_images());
        let id = queued.id;
        app.input_queue.restore_front([queued].into()).unwrap();
        app.recall_pending(id);
        assert_eq!(app.resources.active_images(&app.input_text()).len(), 1);
    }
}

#[test]
fn resource_errors_paste_paths_plan_and_directory() {
    let (mut app, commands, dir) = resource_app();
    app.insert_input("@missing ".into());
    app.submit(PendingKind::Steering);
    settle(&mut app);
    assert_eq!(app.input_text(), "@missing ");
    assert!(commands.try_recv().is_err());
    std::fs::write(dir.path().join("file"), "body").unwrap();
    let mut dirty = false;
    app.clear_input();
    app.insert_input("@file ".into());
    app.submit(PendingKind::Steering);
    app.handle(key(KeyCode::Char('x')), &mut dirty);
    settle(&mut app);
    assert_eq!(app.input_text(), "@file x");
    assert!(commands.try_recv().is_err());
    app.clear_input();
    app.insert_input("@file ".into());
    app.submit(PendingKind::Steering);
    app.handle(key(KeyCode::Esc), &mut dirty);
    assert_eq!(app.input_text(), "@file ");
    assert!(app.resources.job.is_none());
    app.submit(PendingKind::Steering);
    app.open_model_picker();
    settle(&mut app);
    assert!(commands.try_recv().is_err());
    app.close_model_panel();
    assert_eq!(app.input_text(), "@file ");

    let (mut paste_app, _, dir) = resource_app();
    image::DynamicImage::new_rgb8(1, 1)
        .save(dir.path().join("my shot.png"))
        .unwrap();
    paste_app.handle(Event::Paste("my shot.png".into()), &mut dirty);
    settle(&mut paste_app);
    assert_eq!(paste_app.input_text(), "\u{f03e} #1");
    paste_app.handle(Event::Paste(" explain my shot.png".into()), &mut dirty);
    settle(&mut paste_app);
    assert_eq!(paste_app.input_text(), "\u{f03e} #1 explain my shot.png");
    assert_eq!(
        paste_app
            .resources
            .active_images(&paste_app.input_text())
            .len(),
        1
    );

    let (mut plan_app, plan_cmd, plan_dir) = resource_app();
    std::fs::write(plan_dir.path().join("file"), "plan body").unwrap();
    plan_app.insert_input("/plan 分析 @file ".into());
    plan_app.insert_images(vec![image()]);
    plan_app.submit(PendingKind::Steering);
    settle(&mut plan_app);
    assert!(matches!(
        plan_cmd.try_recv(),
        Ok(WorkerCommand::EnterPlan { .. })
    ));
    assert!(
        matches!(&plan_app.plan_after_enter, Some(PlanEnterAction::Prompt(_, Some(input))) if input.has_images())
    );
    assert!(plan_app.resources.ready.is_none());

    let (mut dir_app, scans, replies, dir_cmd) = file_picker_fixture();
    dir_app.handle(Event::Paste("@".into()), &mut false);
    scans.try_recv().unwrap();
    replies.send(Ok(vec!["folder/file.rs".into()])).unwrap();
    dir_app.receive();
    dir_app.handle(key(KeyCode::Right), &mut false);
    assert_eq!(dir_app.input_text(), "@folder/");
    assert!(dir_app.menu.is_some());
    dir_app.handle(key(KeyCode::Char(' ')), &mut false);
    assert_eq!(dir_app.input_text(), "@folder/ ");
    assert!(dir_app.file_query.is_none() && dir_app.menu.is_none());
    assert!(dir_cmd.try_recv().is_err());
}

#[test]
fn resource_plan_discussion_and_clipboard_guards() {
    let (mut app, commands, dir) = resource_app();
    std::fs::write(dir.path().join("file"), "discussion body").unwrap();
    app.workflow = ready_plan_workflow();
    app.receive_event_for_test(WorkerEvent::PlanRead(Ok(plan_review_fixture(
        PlanStatus::Ready,
    ))));
    app.handle(Event::Paste("分析 @file ".into()), &mut false);
    app.insert_images(vec![image()]);
    assert!(app.resource_input_available());
    app.handle(key(KeyCode::Enter), &mut false);
    settle(&mut app);
    let WorkerCommand::RunInput { prepared, .. } = commands.try_recv().unwrap() else {
        panic!("the plan discussion should carry structured input")
    };
    assert!(prepared.has_images());
    assert!(
        prepared
            .parts
            .iter()
            .any(|p| matches!(p, Part::Text(t) if t.contains("discussion body")))
    );
    assert!(app.plan_panel.is_none() && app.input.is_empty());

    app.input_queue
        .enqueue(PendingKind::FollowUp, "补充问题".into(), None, false)
        .unwrap();
    let mut workflow = app.workflow.clone();
    workflow.plan.as_mut().unwrap().status = PlanStatus::Draft;
    app.receive_event_for_test(WorkerEvent::Workflow(workflow));
    app.input_queue.close().unwrap();
    app.receive_event_for_test(WorkerEvent::completed(
        Instant::now(),
        Ok(RunOutcome::Completed),
    ));
    assert!(matches!(
        commands.try_recv().unwrap(),
        WorkerCommand::Run { prompt, .. } if prompt == "补充问题"
    ));

    app.input_queue.close().unwrap();
    app.receive_event_for_test(WorkerEvent::completed(
        Instant::now(),
        Ok(RunOutcome::Completed),
    ));
    assert!(matches!(commands.try_recv(), Ok(WorkerCommand::ReadPlan)));

    let (mut clip, clip_cmd, _) = resource_app();
    clip.insert_input("draft".into());
    clip.insert_images(vec![image()]);
    let screen = draw(&mut clip, 120, 30);
    assert!(screen.replace(' ', "").contains("\u{f03e}#1"), "{screen}");
    assert!(screen.contains("2x3") && !screen.contains("base64"));
    let (_sender, receiver) = mpsc::channel();
    clip.resources.job = Some(resources::Job {
        reply: receiver,
        cancellation: CancellationToken::new(),
        source: clip.input_text(),
        kind: PendingKind::Steering,
        cursor: clip.input.cursor(),
        replace_paste: false,
        prepare_after: None,
    });
    clip.submit(PendingKind::Steering);
    assert!(clip_cmd.try_recv().is_err());
    assert_eq!(clip.input_text(), "draft\u{f03e} #1");

    let (mut empty_clip, empty_cmd, _dir) = resource_app();
    empty_clip.insert_input("draft".into());
    empty_clip.insert_images(vec![image()]);
    empty_clip.input.move_cursor(CursorMove::Head);
    empty_clip.input.start_selection();
    empty_clip.input.move_cursor(CursorMove::End);
    let source = empty_clip.input_text();
    let selection = empty_clip.input.selection_range();
    let cursor = empty_clip.input.cursor();
    let screen = draw(&mut empty_clip, 120, 30);
    let (sender, receiver) = mpsc::channel();
    empty_clip.resources.job = Some(resources::Job {
        reply: receiver,
        cancellation: CancellationToken::new(),
        source: source.clone(),
        kind: PendingKind::Steering,
        cursor,
        replace_paste: false,
        prepare_after: None,
    });
    sender.send(Ok(resources::Output::Empty)).unwrap();
    assert!(empty_clip.receive_resources() && empty_clip.resources.job.is_none());
    assert_eq!(empty_clip.input_text(), source);
    assert_eq!(empty_clip.input.selection_range(), selection);
    assert_eq!(empty_clip.input.cursor(), cursor);
    assert_eq!(empty_clip.resources.active_images(&source).len(), 1);
    assert!(empty_cmd.try_recv().is_err());
    assert_eq!(draw(&mut empty_clip, 120, 30), screen);
}

#[test]
fn resource_atomic_edits_history_and_model_picker() {
    let (mut edited, _, _) = resource_app();
    edited.handle(Event::Paste("explain image.png".into()), &mut false);
    edited.handle(key(KeyCode::Char('!')), &mut false);
    settle(&mut edited);
    assert_eq!(edited.input_text(), "explain image.png!");

    let (mut picker, _, _) = resource_app();
    picker.remember_text("older".into());
    picker.insert_images(vec![image()]);
    picker.handle(key(KeyCode::Backspace), &mut false);
    picker.open_model_picker();
    picker.close_model_panel();
    picker.handle(control('u'), &mut false);
    assert_eq!(
        picker.resources.active_images(&picker.input_text()).len(),
        1
    );
    picker.handle(key(KeyCode::Backspace), &mut false);
    picker.navigate_history(true);
    assert_eq!(picker.input_text(), "older");
    picker.navigate_history(false);
    assert_eq!(picker.input_text(), "");
    picker.handle(control('u'), &mut false);
    assert_eq!(
        picker.resources.active_images(&picker.input_text()).len(),
        1
    );

    let (mut app, _, _) = resource_app();
    app.insert_input("中文 ".into());
    app.insert_images(vec![image()]);
    let marker = app.resources.images[0].marker.clone();
    let end = 3 + marker.chars().count();
    app.handle(Event::Paste(" tail".into()), &mut false);
    app.input.move_cursor(CursorMove::Jump(0, 3));
    app.handle(key(KeyCode::Right), &mut false);
    assert_eq!(app.input.cursor().1, end);
    app.handle(key(KeyCode::Left), &mut false);
    assert_eq!(app.input.cursor().1, 3);
    app.input.move_cursor(CursorMove::Jump(0, 4));
    app.input.start_selection();
    app.input.move_cursor(CursorMove::Jump(0, (end - 1) as u16));
    app.handle(Event::Paste("替换".into()), &mut false);
    assert_eq!(app.input_text(), "中文 替换 tail");
    assert!(app.resources.active_images(&app.input_text()).is_empty());
    app.handle(control('u'), &mut false);
    assert_eq!(app.input_text(), format!("中文 {marker} tail"));
    assert_eq!(app.resources.active_images(&app.input_text()).len(), 1);
    #[cfg(target_os = "macos")]
    {
        app.handle(control('r'), &mut false);
        assert_eq!(app.input_text(), "中文 替换 tail");
        assert!(app.resources.active_images(&app.input_text()).is_empty());
        app.handle(control('u'), &mut false);
    }
    app.input.move_cursor(CursorMove::Jump(0, end as u16));
    app.handle(control('w'), &mut false);
    assert_eq!(app.input_text(), "中文  tail");
    assert!(app.resources.active_images(&app.input_text()).is_empty());
    app.handle(control('u'), &mut false);
    assert_eq!(app.resources.active_images(&app.input_text()).len(), 1);
}

#[test]
fn resource_atomic_cut_legacy_recall_replacement_and_shell_prefix() {
    let (mut app, _, _) = resource_app();
    app.insert_images(vec![image()]);
    let first = app.resources.images[0].marker.clone();
    app.handle(Event::Paste("\n中间\n".into()), &mut false);
    app.insert_images(vec![image()]);
    let original = app.input_text();
    app.input.move_cursor(CursorMove::Jump(0, 1));
    app.input.start_selection();
    app.input.move_cursor(CursorMove::Jump(2, 2));
    app.handle(control('x'), &mut false);
    assert_eq!(app.input_text(), "");
    assert_eq!(app.input.yank_text(), original);
    assert!(app.resources.active_images(&app.input_text()).is_empty());
    app.handle(control('u'), &mut false);
    assert_eq!(app.input_text(), original);
    assert_eq!(app.resources.active_images(&app.input_text()).len(), 2);
    app.input.move_cursor(CursorMove::Jump(0, 0));
    app.handle(Event::Paste(format!("{first} ")), &mut false);
    let real = app.resources.bindings[&first].start;
    assert_eq!(real, first.len() + 1);
    app.remember_input();
    let index = app.input_history.len() - 1;
    app.clear_input();
    app.show_history(index);
    assert_eq!(app.resources.bindings[&first].start, real);
    crate::tui::attachment_edit::jump(&mut app.input, real);
    app.handle(key(KeyCode::Delete), &mut false);
    assert!(app.input_text().contains(&first));
    assert_eq!(app.resources.active_images(&app.input_text()).len(), 1);

    let (mut legacy, _, _) = resource_app();
    let mut old = image();
    old.marker = "[图片 9]".into();
    legacy.input_history.push("解释 [图片 9]".into());
    legacy.resources.history_images.insert(0, vec![old]);
    legacy.show_history(0);
    assert!(legacy.input_text().contains("\u{f03e} #9") && !legacy.input_text().contains("[图片"));
    legacy.insert_images(vec![image()]);
    assert!(legacy.input_text().ends_with("\u{f03e} #10"));
    legacy.handle(key(KeyCode::Backspace), &mut false);
    assert!(legacy.input_text().ends_with("\u{f03e} #9"));
    assert_eq!(
        legacy.resources.active_images(&legacy.input_text()).len(),
        1
    );
    let mut maximum = image();
    maximum.marker = format!("\u{f03e} #{}", u64::MAX);
    let source = legacy
        .resources
        .restore_input(vec![maximum.clone()], maximum.marker);
    legacy.input = editor();
    legacy.input.insert_str(source);
    legacy.insert_images(vec![image()]);
    assert_eq!(
        legacy.resources.active_images(&legacy.input_text()).len(),
        2
    );

    let (mut identical, _, _) = resource_app();
    identical.insert_images(vec![image()]);
    let marker = identical.input_text();
    identical.input.move_cursor(CursorMove::Head);
    identical.input.start_selection();
    identical.input.move_cursor(CursorMove::End);
    identical.handle(Event::Paste(marker.clone()), &mut false);
    assert_eq!(identical.input_text(), marker);
    assert!(
        identical
            .resources
            .active_images(&identical.input_text())
            .is_empty()
    );
    identical.handle(control('u'), &mut false);
    assert_eq!(
        identical
            .resources
            .active_images(&identical.input_text())
            .len(),
        1
    );
    for width in [1, 2, 6, 16, 80] {
        for height in [1, 3, 10, 30] {
            draw(&mut identical, width, height);
            assert_eq!(identical.input_text(), marker);
        }
    }

    let (mut shell, _, _) = resource_app();
    shell.insert_images(vec![image()]);
    assert_eq!(shell.raw_input_text(), "\u{f03e} #1");
    shell.handle(key(KeyCode::Home), &mut false);
    shell.handle(key(KeyCode::Char('!')), &mut false);
    assert!(shell.shell_mode);
    assert_eq!(shell.input_text(), "!\u{f03e} #1");
    assert_eq!(shell.raw_input_text(), "\u{f03e} #1");
    shell.handle(key(KeyCode::Right), &mut false);
    shell.handle(key(KeyCode::Left), &mut false);
    shell.handle(key(KeyCode::Delete), &mut false);
    assert_eq!(shell.raw_input_text(), "");
    assert!(
        shell
            .resources
            .active_images(&shell.raw_input_text())
            .is_empty()
    );
}

#[test]
fn resource_image_references_snapshot_stale_load_and_plan_prompt() {
    let (mut app, commands, dir) = resource_app();
    let path = dir.path().join("shot.png");
    image::DynamicImage::new_rgb8(3, 5).save(&path).unwrap();
    app.handle(Event::Paste("before @shot.png after ".into()), &mut false);
    settle(&mut app);
    assert_eq!(app.resources.active_images(&app.input_text()).len(), 1);
    assert!(app.input_text().contains("#1") && !app.input_text().contains("@shot.png"));
    std::fs::remove_file(&path).unwrap();
    app.submit(PendingKind::Steering);
    settle(&mut app);
    let WorkerCommand::RunInput { prepared, .. } = commands.try_recv().unwrap() else {
        panic!("the image snapshot should be sent")
    };
    assert!(prepared.has_images() && prepared.images[0].summary.contains("3 x 5"));
    image::DynamicImage::new_rgb8(3, 5).save(&path).unwrap();
    app.state = State::Ready;
    app.handle(Event::Paste("@shot.png ".into()), &mut false);
    app.handle(key(KeyCode::Char('!')), &mut false);
    settle(&mut app);
    assert_eq!(app.input_text(), "@shot.png !");
    assert!(app.resources.active_images(&app.input_text()).is_empty());

    let (mut plan_app, plan_cmd, plan_dir) = resource_app();
    image::DynamicImage::new_rgb8(3, 5)
        .save(plan_dir.path().join("shot.png"))
        .unwrap();
    plan_app.insert_input("/plan 分析 @shot.png".into());
    plan_app.submit(PendingKind::Steering);
    settle(&mut plan_app);
    assert!(matches!(
        plan_cmd.try_recv(),
        Ok(WorkerCommand::EnterPlan { .. })
    ));
    assert!(
        matches!(&plan_app.plan_after_enter, Some(PlanEnterAction::Prompt(_, Some(input))) if input.has_images())
    );
}
