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
    assert_eq!(app.input_text(), "[图片 1]");
    assert_eq!(app.resources.active_images(&app.input_text()).len(), 1);
    app.handle(key(KeyCode::Backspace), &mut dirty);
    app.handle(Event::Paste("[图片 1]".into()), &mut dirty);
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
    assert_eq!(app.input_history, ["[图片 1][图片 2]"]);
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
    assert_eq!(app.input_text(), "[图片 1]");
    app.handle(Event::Paste(" explain my shot.png".into()), &mut dirty);
    settle(&mut app);
    assert_eq!(app.input_text(), "[图片 1] explain my shot.png");
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
fn resource_directory_current_action_completes_without_submitting() {
    let (mut app, commands, _) = resource_app();
    app.insert_input("@folder/".into());
    app.file_query = files::query(&app.input);
    app.file_directory = "folder/".into();
    app.apply_menu_action(MenuAction::FileCurrent, true);
    assert_eq!(app.input_text(), "@folder/ ");
    assert!(app.file_query.is_none());
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
    assert!(screen.replace(' ', "").contains("[图片1]"), "{screen}");
    assert!(screen.contains("2 x 3"));
    assert!(!screen.contains("base64"));
    let (_sender, receiver) = mpsc::channel();
    app.resources.job = Some(resources::Job {
        reply: receiver,
        cancellation: CancellationToken::new(),
        source: app.input_text(),
        kind: PendingKind::Steering,
        cursor: app.input.cursor(),
        replace_paste: false,
    });
    app.submit(PendingKind::Steering);
    assert!(commands.try_recv().is_err());
    assert_eq!(app.input_text(), "draft[图片 1]");
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
