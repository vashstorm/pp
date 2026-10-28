use super::*;

#[test]
fn tree_double_escape_deadline_and_reset_are_deterministic() {
    let now = Instant::now();
    let mut tap = EscapeTap::default();
    assert!(!tap.press(now));
    assert!(tap.press(now + Duration::from_millis(500)));
    assert!(!tap.press(now + Duration::from_secs(1)));
    assert!(!tap.press(now + Duration::from_millis(1501)));
    tap.reset();
    assert!(!tap.press(now + Duration::from_millis(1502)));
}

#[test]
fn tree_empty_or_failed_scan_keeps_startup_editor_on_main_screen() {
    for failed in [false, true] {
        let mut app = app();
        app.state = State::Ready;
        app.show_welcome = true;
        let (commands, requests) = mpsc::channel();
        let (events, replies) = mpsc::channel();
        app.commands = Some(commands);
        app.events = Some(replies);
        let before = draw_buffer(&mut app, 100, 30);
        let mut dirty = false;
        app.handle(key(KeyCode::Esc), &mut dirty);
        app.handle(key(KeyCode::Esc), &mut dirty);
        assert!(matches!(requests.try_recv(), Ok(WorkerCommand::ReadTree)));
        assert!(!app.needs_alternate());
        // 主屏幕与历史浏览的绘制路径都不能暂时替换输入框。
        for native in [false, true] {
            app.native_render = native;
            assert_eq!(draw_buffer(&mut app, 100, 30), before);
        }
        events
            .send(WorkerEvent::TreeRead(if failed {
                Err(anyhow::anyhow!("fixture tree read failed"))
            } else {
                Ok(crate::agent::persistence::tree::View {
                    tree: Default::default(),
                    records: vec![],
                })
            }))
            .unwrap();
        app.receive();
        assert!(matches!(app.state, State::Ready));
        assert!(app.tree_panel.is_none());
        assert!(!app.needs_alternate());
        let after = draw_buffer(&mut app, 100, 30);
        for y in 26..30 {
            for x in 0..100 {
                assert_eq!(after[(x, y)], before[(x, y)]);
            }
        }
    }
}

#[test]
fn tree_double_escape_cancelled_scan_ignores_late_result_and_keeps_draft() {
    let mut app = app();
    app.state = State::Ready;
    let (commands, requests) = mpsc::channel();
    let (events, replies) = mpsc::channel();
    app.commands = Some(commands);
    app.events = Some(replies);
    let mut dirty = false;
    app.handle(key(KeyCode::Esc), &mut dirty);
    app.handle(key(KeyCode::Esc), &mut dirty);
    assert!(matches!(requests.try_recv(), Ok(WorkerCommand::ReadTree)));
    assert!(app.tree_panel.is_some());
    app.handle(key(KeyCode::Esc), &mut dirty);
    assert!(app.tree_panel.is_none());
    assert!(matches!(app.state, State::ReadingTree));
    events
        .send(WorkerEvent::TreeRead(Ok(
            crate::agent::persistence::tree::View {
                tree: crate::agent::persistence::tree::Tree::fixture(
                    &[(None, crate::agent::persistence::tree::Kind::User, "seed")],
                    Some(0),
                ),
                records: vec![vec![crate::agent::persistence::ReplayRecord::User(
                    "seed".into(),
                )]],
            },
        )))
        .unwrap();
    app.receive();
    assert!(app.tree_panel.is_none());
    app.input.insert_str("保留草稿");
    app.handle(key(KeyCode::Esc), &mut dirty);
    app.handle(key(KeyCode::Esc), &mut dirty);
    assert!(requests.try_recv().is_err());
    assert_eq!(app.input.lines(), &["保留草稿"]);
    app.clear_input();
    app.handle(key(KeyCode::Esc), &mut dirty);
    app.handle(Event::Paste("".into()), &mut dirty);
    app.handle(key(KeyCode::Esc), &mut dirty);
    assert!(requests.try_recv().is_err());
    let token = CancellationToken::new();
    app.active_cancellation = Some(token.clone());
    app.state = State::Running;
    app.handle(key(KeyCode::Esc), &mut dirty);
    app.handle(key(KeyCode::Esc), &mut dirty);
    assert!(token.is_cancelled());
    assert!(requests.try_recv().is_err());
}

#[test]
fn tree_inline_preview_keeps_history_viewport_draft_and_paste_isolated() {
    use crate::agent::persistence::{
        ReplayRecord,
        tree::{Kind, Tree, View},
    };
    let mut app = app();
    app.state = State::Ready;
    for i in 0..40 {
        app.append("You", &format!("original message {i}"));
    }
    app.input.insert_str("existing draft");
    draw(&mut app, 100, 30);
    app.conversation.page_up();
    let before = draw(&mut app, 100, 30);
    let history = app.input_history.clone();
    let (commands, requests) = mpsc::channel();
    let (events, replies) = mpsc::channel();
    app.commands = Some(commands);
    app.events = Some(replies);
    app.open_tree();
    assert!(matches!(requests.try_recv(), Ok(WorkerCommand::ReadTree)));
    let loading = draw(&mut app, 100, 30);
    assert_eq!(loading, before);
    assert!(!app.needs_alternate());
    events
        .send(WorkerEvent::TreeRead(Ok(View {
            tree: Tree::fixture(
                &[
                    (None, Kind::User, "preview only"),
                    (Some(0), Kind::Assistant, "branch answer"),
                ],
                Some(1),
            ),
            records: vec![
                vec![ReplayRecord::User("preview only".into())],
                vec![
                    ReplayRecord::Event(AgentEvent::AssistantStarted),
                    ReplayRecord::Event(AgentEvent::AssistantFinished {
                        text: "branch answer".into(),
                    }),
                ],
            ],
        })))
        .unwrap();
    app.receive();
    assert!(app.needs_alternate());
    let preview = draw(&mut app, 100, 30);
    assert!(preview.contains("preview only") && preview.contains("branch answer"));
    assert!(!preview.contains("existing draft"));
    let mut dirty = false;
    for event in [
        key(KeyCode::Up),
        key(KeyCode::Char('f')),
        Event::Paste("must not search or edit".into()),
        key(KeyCode::PageDown),
    ] {
        app.handle(event, &mut dirty);
    }
    assert_eq!(app.input.lines(), &["existing draft"]);
    assert_eq!(app.input_history, history);
    app.handle(key(KeyCode::Esc), &mut dirty);
    assert!(app.tree_panel.is_none());
    assert_eq!(draw(&mut app, 100, 30), before);
    assert!(requests.try_recv().is_err());
}

#[test]
fn tree_navigation_error_preserves_transcript_draft_and_panel() {
    let mut app = app();
    app.state = State::NavigatingTree;
    app.append("You", "original branch");
    app.input.insert_str("existing draft");
    app.tree_panel = Some(tree::Panel::new());
    app.tree_panel.as_mut().unwrap().pending = true;
    let (events, replies) = mpsc::channel();
    app.events = Some(replies);
    events
        .send(WorkerEvent::TreeNavigated(Err(anyhow::anyhow!(
            "cannot switch model: unsupported history item"
        ))))
        .unwrap();
    app.receive();
    assert!(matches!(app.state, State::Ready));
    assert_eq!(app.input.lines(), &["existing draft"]);
    assert!(!app.tree_panel.as_ref().unwrap().pending);
    app.tree_panel = None;
    assert!(draw(&mut app, 100, 30).contains("original branch"));
}

#[test]
fn resume_cancelled_scan_never_reopens_panel_and_keeps_worker_busy_until_reply() {
    let mut app = app();
    app.state = State::Ready;
    app.append("You", "original conversation");
    let original = draw(&mut app, 100, 30);
    let (commands, requests) = mpsc::channel();
    let (events, replies) = mpsc::channel();
    app.commands = Some(commands);
    app.events = Some(replies);
    let mut dirty = false;
    app.handle(Event::Paste("/resume".into()), &mut dirty);
    app.handle(key(KeyCode::Enter), &mut dirty);
    assert!(matches!(
        requests.try_recv(),
        Ok(WorkerCommand::ListSessions)
    ));
    assert!(draw(&mut app, 100, 30).contains("Loading saved sessions…"));
    app.handle(Event::Paste("must not enter editor".into()), &mut dirty);
    app.handle(function_key('p'), &mut dirty);
    app.handle(key(KeyCode::F(1)), &mut dirty);
    app.handle(key(KeyCode::Esc), &mut dirty);
    assert!(draw(&mut app, 100, 30).contains("original conversation"));
    app.handle(Event::Paste("next prompt".into()), &mut dirty);
    app.handle(key(KeyCode::Enter), &mut dirty);
    assert!(matches!(requests.try_recv(), Err(TryRecvError::Empty)));
    assert_eq!(app.input.lines(), &["next prompt"]);
    events
        .send(WorkerEvent::SessionsListed(Ok(
            crate::agent::persistence::SessionListing {
                sessions: vec![crate::agent::persistence::SessionSummary {
                    id: "late".into(),
                    cwd: "/tmp".into(),
                    updated_at: std::time::SystemTime::now(),
                    summary: "late scan result".into(),
                    preview: String::new(),
                    bytes: 0,
                    done: false,
                }],
                diagnostics: Vec::new(),
            },
        )))
        .unwrap();
    app.receive();
    let screen = draw(&mut app, 100, 30);
    assert!(screen.contains("original conversation"));
    assert!(!screen.contains("Resume session"));
    assert!(!screen.contains("late scan result"));
    assert_eq!(screen.lines().last(), original.lines().last());
    app.handle(key(KeyCode::Enter), &mut dirty);
    assert!(
        matches!(requests.try_recv(), Ok(WorkerCommand::Run { prompt, .. }) if prompt == "next prompt")
    );
}

#[test]
fn notification_overlay_preserves_welcome_draft_and_cursor() {
    let mut app = app();
    app.show_welcome = true;
    app.input.insert_str("草稿🙂");
    let cursor = app.input.cursor();
    let before = draw(&mut app, 100, 30);
    let now = Instant::now();
    app.notifications
        .show(Level::Error, "temporary error".into(), now);
    app.notifications.tick(now + Duration::from_millis(200));
    let shown = draw(&mut app, 100, 30);
    assert!(shown.contains("temporary error"));
    assert!(app.show_welcome);
    assert_eq!(app.input.cursor(), cursor);
    assert_eq!(app.input.lines(), &["草稿🙂"]);
    assert_eq!(shown.lines().last(), before.lines().last());
    assert!(app.notifications.tick(now + Duration::from_millis(10000)));
    assert_eq!(draw(&mut app, 100, 30), before);
}
