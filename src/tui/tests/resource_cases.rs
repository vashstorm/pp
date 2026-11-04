use super::*;
use crate::user_input::{self, Part};

fn resource_app() -> (App, Receiver<WorkerCommand>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let mut app = App::new_with_skills(
        Err(anyhow::anyhow!("fixture")),
        crate::skills::Skills::default().shared(),
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
fn resource_images_delete_undo_and_manual_marker_never_forges_attachment() {
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
}

#[test]
fn resource_submission_sends_one_prepared_message_and_history_recalls_image() {
    let (mut app, commands, _) = resource_app();
    app.insert_images(vec![image(), image()]);
    assert!(!app.submit(PendingKind::Steering));
    settle(&mut app);
    let WorkerCommand::RunInput { prepared, .. } = commands.try_recv().unwrap() else {
        panic!("应发送结构化输入")
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
    assert!(app.input.is_empty());
    assert_eq!(app.input_history, ["\u{f03e} #1\u{f03e} #2"]);
    app.show_history(0);
    assert_eq!(app.resources.active_images(&app.input_text()).len(), 2);
}

#[test]
fn resource_steering_and_followup_keep_submit_time_snapshot_and_recall() {
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
fn resource_failure_cancel_edit_and_modal_never_overwrite_draft() {
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
}

#[test]
fn resource_image_paste_paths_and_mixed_text_use_distinct_paths() {
    let (mut app, _, dir) = resource_app();
    image::DynamicImage::new_rgb8(1, 1)
        .save(dir.path().join("my shot.png"))
        .unwrap();
    let mut dirty = false;
    app.handle(Event::Paste("my shot.png".into()), &mut dirty);
    settle(&mut app);
    assert_eq!(app.input_text(), "\u{f03e} #1");
    app.handle(Event::Paste(" explain my shot.png".into()), &mut dirty);
    settle(&mut app);
    assert_eq!(app.input_text(), "\u{f03e} #1 explain my shot.png");
    assert_eq!(app.resources.active_images(&app.input_text()).len(), 1);
}

#[test]
fn resource_plan_command_prepares_before_transition_and_carries_snapshot() {
    let (mut app, commands, dir) = resource_app();
    std::fs::write(dir.path().join("file"), "plan body").unwrap();
    app.insert_input("/plan 分析 @file ".into());
    app.insert_images(vec![image()]);
    app.submit(PendingKind::Steering);
    settle(&mut app);
    assert!(matches!(
        commands.try_recv(),
        Ok(WorkerCommand::EnterPlan { .. })
    ));
    assert!(
        matches!(&app.plan_after_enter, Some(PlanEnterAction::Prompt(_, Some(input))) if input.has_images())
    );
    assert!(app.resources.ready.is_none());
}

#[test]
fn resource_directory_space_completes_without_submitting() {
    let (mut app, scans, replies, commands) = file_picker_fixture();
    app.handle(Event::Paste("@".into()), &mut false);
    scans.try_recv().unwrap();
    replies.send(Ok(vec!["folder/file.rs".into()])).unwrap();
    app.receive();
    app.handle(key(KeyCode::Right), &mut false);
    assert_eq!(app.input_text(), "@folder/");
    assert!(app.menu.is_some());
    app.handle(key(KeyCode::Char(' ')), &mut false);
    assert_eq!(app.input_text(), "@folder/ ");
    assert!(app.file_query.is_none());
    assert!(app.menu.is_none());
    assert!(commands.try_recv().is_err());
}

#[test]
fn resource_plan_discussion_uses_prepared_images_and_text() {
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
        panic!("计划讨论应携带结构化输入")
    };
    assert!(prepared.has_images());
    assert!(
        prepared
            .parts
            .iter()
            .any(|p| matches!(p, Part::Text(t) if t.contains("discussion body")))
    );
    assert!(app.plan_panel.is_none());
    assert!(app.input.is_empty());
}

#[test]
fn resource_clipboard_pending_blocks_submit_and_image_summary_is_visible() {
    let (mut app, commands, _) = resource_app();
    app.insert_input("draft".into());
    app.insert_images(vec![image()]);
    let screen = draw(&mut app, 120, 30);
    assert!(screen.replace(' ', "").contains("\u{f03e}#1"), "{screen}");
    assert!(screen.contains("2x3"));
    assert!(!screen.contains("base64"));
    let (_sender, receiver) = mpsc::channel();
    app.resources.job = Some(resources::Job {
        reply: receiver,
        cancellation: CancellationToken::new(),
        source: app.input_text(),
        kind: PendingKind::Steering,
        cursor: app.input.cursor(),
        replace_paste: false,
        prepare_after: None,
    });
    app.submit(PendingKind::Steering);
    assert!(commands.try_recv().is_err());
    assert_eq!(app.input_text(), "draft\u{f03e} #1");
}

#[test]
fn resource_edit_during_possible_image_paste_keeps_original_text() {
    let (mut app, _, _) = resource_app();
    app.handle(Event::Paste("explain image.png".into()), &mut false);
    app.handle(key(KeyCode::Char('!')), &mut false);
    settle(&mut app);
    assert_eq!(app.input_text(), "explain image.png!");
}

#[test]
fn resource_model_picker_and_history_restore_attachment_undo_bindings() {
    let (mut app, _, _) = resource_app();
    app.remember_text("older".into());
    app.insert_images(vec![image()]);
    app.handle(key(KeyCode::Backspace), &mut false);
    app.open_model_picker();
    app.close_model_panel();
    app.handle(control('u'), &mut false);
    assert_eq!(app.resources.active_images(&app.input_text()).len(), 1);
    app.handle(key(KeyCode::Backspace), &mut false);
    app.navigate_history(true);
    assert_eq!(app.input_text(), "older");
    app.navigate_history(false);
    assert_eq!(app.input_text(), "");
    app.handle(control('u'), &mut false);
    assert_eq!(app.resources.active_images(&app.input_text()).len(), 1);
}

#[test]
fn resource_atomic_navigation_partial_selection_word_delete_and_undo() {
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
fn resource_atomic_multiline_cut_and_forged_duplicate_do_not_rebind() {
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
}

#[test]
fn resource_image_references_freeze_snapshot_keep_order_and_cancel_stale_loads() {
    let (mut app, commands, dir) = resource_app();
    let path = dir.path().join("shot.png");
    image::DynamicImage::new_rgb8(3, 5).save(&path).unwrap();
    app.handle(Event::Paste("before @shot.png after ".into()), &mut false);
    settle(&mut app);
    assert_eq!(app.resources.active_images(&app.input_text()).len(), 1);
    assert!(app.input_text().contains("#1"));
    assert!(!app.input_text().contains("@shot.png"));
    std::fs::remove_file(&path).unwrap();
    app.submit(PendingKind::Steering);
    settle(&mut app);
    let WorkerCommand::RunInput { prepared, .. } = commands.try_recv().unwrap() else {
        panic!("应发送图片快照")
    };
    assert!(prepared.has_images());
    assert!(prepared.images[0].summary.contains("3 x 5"));
    image::DynamicImage::new_rgb8(3, 5).save(&path).unwrap();
    app.state = State::Ready;
    app.handle(Event::Paste("@shot.png ".into()), &mut false);
    app.handle(key(KeyCode::Char('!')), &mut false);
    settle(&mut app);
    assert_eq!(app.input_text(), "@shot.png !");
    assert!(app.resources.active_images(&app.input_text()).is_empty());
}

#[test]
fn resource_legacy_attachment_recall_uses_new_marker_and_two_digit_ids_are_atomic() {
    let (mut app, _, _) = resource_app();
    let mut old = image();
    old.marker = "[图片 9]".into();
    app.input_history.push("解释 [图片 9]".into());
    app.resources.history_images.insert(0, vec![old]);
    app.show_history(0);
    assert!(app.input_text().contains("\u{f03e} #9"));
    assert!(!app.input_text().contains("[图片"));
    app.insert_images(vec![image()]);
    assert!(app.input_text().ends_with("\u{f03e} #10"));
    app.handle(key(KeyCode::Backspace), &mut false);
    assert!(app.input_text().ends_with("\u{f03e} #9"));
    assert_eq!(app.resources.active_images(&app.input_text()).len(), 1);
    let mut maximum = image();
    maximum.marker = format!("\u{f03e} #{}", u64::MAX);
    let source = app
        .resources
        .restore_input(vec![maximum.clone()], maximum.marker);
    app.input = editor();
    app.input.insert_str(source);
    app.insert_images(vec![image()]);
    assert_eq!(app.resources.active_images(&app.input_text()).len(), 2);
}

#[test]
fn resource_identical_text_replacement_drops_binding_and_tiny_layouts_preserve_input() {
    let (mut app, _, _) = resource_app();
    app.insert_images(vec![image()]);
    let marker = app.input_text();
    app.input.move_cursor(CursorMove::Head);
    app.input.start_selection();
    app.input.move_cursor(CursorMove::End);
    app.handle(Event::Paste(marker.clone()), &mut false);
    assert_eq!(app.input_text(), marker);
    assert!(app.resources.active_images(&app.input_text()).is_empty());
    app.handle(control('u'), &mut false);
    assert_eq!(app.resources.active_images(&app.input_text()).len(), 1);
    for width in [1, 2, 6, 16, 80] {
        for height in [1, 3, 10, 30] {
            draw(&mut app, width, height);
            assert_eq!(app.input_text(), marker);
        }
    }
}

#[test]
fn resource_plan_image_reference_without_delimiter_prepares_snapshot_before_transition() {
    let (mut app, commands, dir) = resource_app();
    image::DynamicImage::new_rgb8(3, 5)
        .save(dir.path().join("shot.png"))
        .unwrap();
    app.insert_input("/plan 分析 @shot.png".into());
    app.submit(PendingKind::Steering);
    settle(&mut app);
    assert!(matches!(
        commands.try_recv(),
        Ok(WorkerCommand::EnterPlan { .. })
    ));
    assert!(
        matches!(&app.plan_after_enter, Some(PlanEnterAction::Prompt(_, Some(input))) if input.has_images())
    );
}
