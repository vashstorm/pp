use super::*;
use crate::tui::{desktop_notifications::Notice, worker_bridge::DialogOrigin};
use desktop_notifications::Kind;
use pp_config::config::DesktopNotifications;
use pp_core::interaction::{AskAnswer, AskArgs, AskRequest};
use std::sync::Arc;

fn take_notices(app: &mut App, mode: DesktopNotifications) -> Vec<Notice> {
    let mut notices = Vec::new();
    app.flush_desktop_notifications(mode, |notice| notices.push(notice));
    notices
}

fn kinds(notices: &[Notice]) -> Vec<Kind> {
    notices.iter().map(|notice| notice.kind).collect()
}

#[test]
fn task_notifications_use_current_focus_and_are_consumed_once() {
    use DesktopNotifications::{Always, Background, Off};

    let mut app = app();
    for (focus, mode, result, expected) in [
        (None, Background, Ok(RunOutcome::Completed), None),
        (
            None,
            Always,
            Ok(RunOutcome::Completed),
            Some(Kind::Completed),
        ),
        (Some(Event::FocusLost), Off, Ok(RunOutcome::Completed), None),
        (
            None,
            Background,
            Ok(RunOutcome::Completed),
            Some(Kind::Completed),
        ),
        (
            None,
            Background,
            Err(anyhow::anyhow!("fixture task error")),
            Some(Kind::Failed),
        ),
        (
            None,
            Background,
            Ok(RunOutcome::AwaitingPlanReview),
            Some(Kind::PlanReady),
        ),
        (None, Always, Ok(RunOutcome::Cancelled), None),
        (
            Some(Event::FocusGained),
            Background,
            Ok(RunOutcome::Completed),
            None,
        ),
        (
            None,
            Always,
            Ok(RunOutcome::Completed),
            Some(Kind::Completed),
        ),
        (
            Some(Event::FocusLost),
            Background,
            Ok(RunOutcome::Completed),
            Some(Kind::Completed),
        ),
    ] {
        app.conversation.begin_run();
        app.receive_event_for_test(WorkerEvent::completed(Instant::now(), result));
        if let Some(event) = focus {
            app.handle(event, &mut false);
        }
        assert_eq!(kinds(&take_notices(&mut app, mode)), expected.as_slice());
        assert!(take_notices(&mut app, Always).is_empty());
    }
}

fn offer_dialog(
    app: &mut App,
    origin: DialogOrigin,
    call_id: &str,
) -> (mpsc::Receiver<Vec<AskAnswer>>, CancellationToken) {
    let cancellation = CancellationToken::new();
    app.state = State::RunningTools;
    app.active_cancellation = Some(cancellation.clone());
    let arguments =
        r#"{"questions":[{"id":"q","question":"Fixture question","options":[{"label":"OK"}]}]}"#;
    app.conversation.progress(AgentEvent::ToolStarted {
        id: call_id.into(),
        name: "ask".into(),
        arguments: arguments.into(),
    });
    let (reply, answers) = mpsc::sync_channel(1);
    app.receive_event_for_test(WorkerEvent::Ask {
        origin,
        request: AskRequest {
            call_id: call_id.into(),
            args: Arc::new(AskArgs::parse(arguments).unwrap()),
        },
        cancellation: cancellation.clone(),
        reply,
    });
    (answers, cancellation)
}

#[test]
fn real_dialogs_notify_once_and_stop_being_deliverable_after_reply() {
    let mut app = app();
    app.handle(Event::FocusLost, &mut false);
    for (id, origin, expected) in [
        ("ask-main", DialogOrigin::Ask, Kind::Ask),
        (
            "permission-dialog",
            DialogOrigin::Permission,
            Kind::Permission,
        ),
        ("subagent-1-ask", DialogOrigin::Ask, Kind::Ask),
        (
            "subagent-1-permission",
            DialogOrigin::Permission,
            Kind::Permission,
        ),
    ] {
        let (answers, cancellation) = offer_dialog(&mut app, origin, id);
        let notices = take_notices(&mut app, DesktopNotifications::Background);
        assert_eq!(kinds(&notices), [expected]);
        assert!(notices[0].is_pending());
        app.handle(Event::FocusGained, &mut false);
        app.handle(Event::FocusLost, &mut false);
        assert!(take_notices(&mut app, DesktopNotifications::Background).is_empty());
        app.apply_ask_action(dialog::Action::Submit(vec![AskAnswer {
            id: "q".into(),
            selected_options: vec!["OK".into()],
            custom_input: None,
        }]));
        assert_eq!(answers.try_recv().unwrap()[0].selected_options, ["OK"]);
        assert!(!cancellation.is_cancelled());
        assert!(!notices[0].is_pending());
    }
}

#[test]
fn dialogs_discard_suppressed_and_cancelled_notices() {
    let mut app = app();
    app.handle(Event::FocusGained, &mut false);
    let (_answers, _) = offer_dialog(&mut app, DialogOrigin::Ask, "ask-foreground");
    assert!(take_notices(&mut app, DesktopNotifications::Background).is_empty());
    app.handle(Event::FocusLost, &mut false);
    assert!(take_notices(&mut app, DesktopNotifications::Background).is_empty());
    app.close_ask_on_exit();

    let (_answers, token) = offer_dialog(&mut app, DialogOrigin::Ask, "ask-cancelled");
    token.cancel();
    assert!(take_notices(&mut app, DesktopNotifications::Background).is_empty());
    app.close_ask_on_exit();

    let (_answers, _) = offer_dialog(&mut app, DialogOrigin::Permission, "permission-dialog");
    app.receive_event_for_test(WorkerEvent::DialogFinished {
        call_id: "permission-dialog".into(),
    });
    assert!(take_notices(&mut app, DesktopNotifications::Background).is_empty());
}

#[test]
fn restored_history_displays_an_answered_question_without_notifying() {
    use pp_runtime::agent::persistence::ReplayRecord;
    use serde_json::json;

    let mut app = app();
    app.handle(Event::FocusLost, &mut false);
    app.adopt_history(vec![
        ReplayRecord::User("Choose storage".into()),
        ReplayRecord::Event(AgentEvent::ToolStarted {
            id: "saved-ask".into(),
            name: "ask".into(),
            arguments: json!({"questions":[{"id":"q","question":"Saved question","options":[{"label":"SQLite"}]}]}).to_string(),
        }),
        ReplayRecord::Event(AgentEvent::ToolFinished {
            id: "saved-ask".into(),
            result: json!({"ok":true,"answers":[{"id":"q","selected_options":["SQLite"],"custom_input":null}]}),
        }),
        ReplayRecord::Event(AgentEvent::AssistantFinished {
            text: "Saved result".into(),
        }),
    ]);
    let screen = draw(&mut app, 100, 30);
    assert!(screen.contains("Saved question") && screen.contains("Saved result"));
    assert!(app.ask_panel.is_none());
    assert!(take_notices(&mut app, DesktopNotifications::Always).is_empty());
}

#[cfg(debug_assertions)]
#[test]
fn diagnostic_dialogs_and_permission_previews_are_silent() {
    let mut app = app();
    app.handle(Event::FocusLost, &mut false);
    let (_answers, _) = offer_dialog(&mut app, DialogOrigin::Mock, "mock-ask");
    assert!(app.ask_panel.is_some());
    assert!(take_notices(&mut app, DesktopNotifications::Always).is_empty());

    app.close_ask_on_exit();
    app.active_cancellation = None;
    app.state = State::Ready;
    app.submit_initial_command("/test-permission");
    assert!(app.ask_panel.is_some());
    assert!(take_notices(&mut app, DesktopNotifications::Always).is_empty());
}
