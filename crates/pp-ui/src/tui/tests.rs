use super::*;
use crossterm::event::{KeyEvent, MouseButton, MouseEvent};
use ratatui::{Terminal, backend::TestBackend, style::Modifier};

fn app() -> App {
    let mut app = App::new(Err(anyhow::anyhow!("fixture configuration unavailable"))).unwrap();
    app.conversation = app.conversation.fresh();
    app.show_welcome = true;
    app
}

fn draw_buffer(app: &mut App, width: u16, height: u16) -> ratatui::buffer::Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| {
            app.draw(
                frame,
                &StatusContext {
                    workdir: "fixture-cwd".into(),
                    ..StatusContext::default()
                },
            )
        })
        .unwrap();
    terminal.backend().buffer().clone()
}

fn draw(app: &mut App, width: u16, height: u16) -> String {
    let buffer = draw_buffer(app, width, height);
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn ask_fixture(
    app: &mut App,
) -> (
    mpsc::Receiver<Vec<pp_core::interaction::AskAnswer>>,
    CancellationToken,
) {
    let token = CancellationToken::new();
    app.state = State::Running;
    app.active_cancellation = Some(token.clone());
    app.conversation.begin_run();
    let args = pp_core::interaction::AskArgs::parse(r#"{"questions":[{"id":"choice","question":"Choose storage","options":[{"label":"SQLite"},{"label":"Postgres"}],"recommended":0}]}"#).unwrap();
    let (reply, answers) = mpsc::sync_channel(1);
    app.ask_panel = Some(dialog::Panel::new(pp_core::interaction::AskRequest {
        call_id: "ask-1".into(),
        args: std::sync::Arc::new(args),
    }));
    app.ask_reply = Some(reply);
    (answers, token)
}

fn folding_fixture(app: &mut App) {
    app.conversation = Transcript::with_read_preview(true);
    for id in ["first", "second"] {
        app.conversation.progress(AgentEvent::ToolStarted {
            id: id.into(),
            name: "custom".into(),
            arguments: format!(r#"{{"path":"{id}"}}"#),
        });
        app.conversation.progress(AgentEvent::ToolFinished {
                id: id.into(),
                result: serde_json::json!({"ok":true,"content":(0..20).map(|i| format!("{id}-{i:02}")).collect::<Vec<_>>().join("\n")}),
            });
    }
}

fn assert_notification(app: &mut App, color: Color) {
    let buffer = draw_buffer(app, 100, 30);
    assert!(
        buffer
            .content
            .iter()
            .any(|cell| cell.symbol() == "╭" && cell.fg == color)
    );
}

fn active_menu(app: &App) -> &SelectionMenu<MenuAction> {
    app.model_panel
        .as_ref()
        .map(|panel| &panel.menu)
        .or(app.menu.as_ref())
        .unwrap()
}

fn model_query(app: &App) -> &str {
    &app.model_panel.as_ref().unwrap().search.lines()[0]
}

fn menu_values(app: &App) -> Vec<MenuAction> {
    app.model_panel
        .as_ref()
        .map(|panel| &panel.menu)
        .or(app.menu.as_ref())
        .map_or_else(Vec::new, |menu| {
            menu.items().iter().map(|item| item.value).collect()
        })
}

type FilePickerFixture = (
    App,
    Receiver<String>,
    Sender<Result<Vec<String>>>,
    Receiver<WorkerCommand>,
);

fn file_picker_fixture() -> FilePickerFixture {
    let dir = tempfile::tempdir().unwrap();
    let mut app = App::new_with_skills(
        Err(anyhow::anyhow!("fixture")),
        pp_config::skills::Skills::default().shared(),
        commands::FileCommands::default(),
        dir.path(),
        None,
    )
    .unwrap();
    let (requests, scans) = mpsc::channel();
    let (replies, receiver) = mpsc::channel();
    app.file_worker = files::Worker {
        requests,
        replies: receiver,
    };
    let (sender, commands) = mpsc::channel();
    app.commands = Some(sender);
    app.state = State::Ready;
    app.conversation = app.conversation.fresh();
    (app, scans, replies, commands)
}

fn key(code: KeyCode) -> Event {
    Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn control(character: char) -> Event {
    Event::Key(KeyEvent::new(
        KeyCode::Char(character),
        KeyModifiers::CONTROL,
    ))
}

fn function_key(character: char) -> Event {
    Event::Key(KeyEvent::new(KeyCode::Char(character), FUNCTION_MODIFIER))
}

fn opposite_function_key(character: char) -> Event {
    let modifiers = if cfg!(target_os = "macos") {
        KeyModifiers::CONTROL
    } else {
        KeyModifiers::ALT
    };
    Event::Key(KeyEvent::new(KeyCode::Char(character), modifiers))
}

/// Real `model_control` control thread plus main Agent binding: one provider and one model in a temp directory,
/// with no `thinking` declared so every level is allowed. The fixture is only for `Change::Think` and does not reload model files.
fn model_control() -> (
    pp_runtime::model_control::Service,
    pp_runtime::model_control::Binding,
) {
    let (control, binding, _) = model_control_with(
        r#"{"providers":{"fixture":{"api_key":"fixture","models":{"one":{"model":"one"}}}}}"#,
    );
    (control, binding)
}

/// Same as [`model_control`] but with caller-supplied `models.yml` content, returning the catalog snapshot.
///
/// The fixture pre-sets a manual level different from the configured default: in a fresh `ModelState`, `Change::Think`
/// only updates an existing `a` slot and publishes no new revision when no override is registered. The test targets the submit/confirm path,
/// so it first establishes the same premise real usage has.
fn model_control_with(
    models: &str,
) -> (
    pp_runtime::model_control::Service,
    pp_runtime::model_control::Binding,
    Vec<ModelChoice>,
) {
    let dir = tempfile::tempdir().unwrap();
    let settings = dir.path().join("settings.yml");
    let models_path = dir.path().join("models.yml");
    std::fs::write(&settings, "{}").unwrap();
    std::fs::write(&models_path, models).unwrap();
    let catalog = pp_config::config::ModelCatalog::from_files(&settings, &models_path).unwrap();
    let choices = catalog.choices();
    let state = pp_config::model_state::ModelState::shared(catalog);
    if let Some(choice) = choices.first() {
        let effort = choice.thinking.levels().nth(1).unwrap_or(ThinkLevel::None);
        let _ = state.set_override(Selection {
            reference: choice.reference.clone(),
            effort,
        });
    }
    let control = pp_runtime::model_control::Service::start(state).unwrap();
    let binding = pp_runtime::model_control::Binding::main(control.registry.clone());
    (control, binding, choices)
}

/// Poll the UI until the real control thread publishes a manual override target; returns the registered `a` snapshot.
fn wait_for_override(app: &mut App, reference: &str) -> pp_config::model_state::Selection {
    for _ in 0..400 {
        app.receive();
        if let Some(selection) = app
            .override_selection
            .clone()
            .filter(|selection| selection.reference == reference)
        {
            return selection;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    panic!("model control published no override for {reference}");
}

/// Wait for `ThinkQueued` before injecting the result so the async control thread and the test injection do not interleave.
fn wait_for_queued_think(app: &mut App, level: ThinkLevel) -> u64 {
    for _ in 0..400 {
        app.receive();
        if let Some((revision, pending)) = app.pending_think
            && revision > 0
            && pending == level
        {
            return revision;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    panic!("model control published no queued effort");
}

/// The only model in the fixture directory: no `thinking` declared, so every level is allowed.
fn all_levels_choice() -> ModelChoice {
    ModelChoice {
        reference: "fixture/one".into(),
        model: "one".into(),
        protocol: pp_config::models::Protocol::Completion,
        thinking: Default::default(),
        context_window: None,
    }
}

fn favorite_model_choices() -> Vec<ModelChoice> {
    ["Alpha", "Beta", "Gamma", "Delta"]
        .into_iter()
        .map(|name| ModelChoice {
            reference: format!("gateway/team/{name}"),
            model: format!("vendor-{name}"),
            protocol: pp_config::models::Protocol::Completion,
            thinking: pp_config::models::ModelThinking::default(),
            context_window: None,
        })
        .collect()
}

fn file_commands_app(files: commands::FileCommands) -> (App, mpsc::Receiver<WorkerCommand>) {
    let shared = pp_config::skills::Skills::default().shared();
    let mut app = App::new_with_skills(
        Err(anyhow::anyhow!("fixture unavailable")),
        shared,
        files,
        Path::new("."),
        None,
    )
    .unwrap();
    app.conversation = app.conversation.fresh();
    app.show_welcome = true;
    let (sender, requests) = mpsc::channel();
    app.commands = Some(sender);
    app.state = State::Ready;
    (app, requests)
}

fn review_files() -> commands::FileCommands {
    commands::FileCommands {
        entries: vec![commands::FileCommand {
            name: "/review".into(),
            description: "Review source".into(),
            content: "Review $1 now.".into(),
        }],
        diagnostics: Vec::new(),
    }
}

fn plan_app() -> (
    App,
    mpsc::Receiver<WorkerCommand>,
    mpsc::Sender<WorkerEvent>,
) {
    let mut app = app();
    app.state = State::Ready;
    let (commands, requests) = mpsc::channel();
    let (events, replies) = mpsc::channel();
    app.commands = Some(commands);
    app.events = Some(replies);
    (app, requests, events)
}

fn ready_plan_workflow() -> WorkflowState {
    WorkflowState {
        mode: WorkMode::Plan,
        plan: Some(pp_core::plan::PlanDocument {
            id: "0123456789abcdef0123456789abcdef".into(),
            revision: 2,
            title: "Plan".into(),
            content: "# Plan\n\n- [ ] step one\n- [ ] step two\n".into(),
            status: PlanStatus::Ready,
            origin_prompt: "original task".into(),
            file_name: Some("create-canary.md".into()),
            location: pp_core::plan::PlanLocation::Plans,
            approved_tasks: None,
        }),
        ..Default::default()
    }
}

fn plan_review_fixture(status: PlanStatus) -> PlanReview {
    let document = pp_core::plan::PlanDocument {
        id: "0123456789abcdef0123456789abcdef".into(),
        revision: 2,
        title: "Plan".into(),
        content: "- [ ] step one\n- [ ] step two\n".into(),
        status,
        origin_prompt: "original task".into(),
        file_name: Some("create-canary.md".into()),
        location: pp_core::plan::PlanLocation::Plans,
        approved_tasks: None,
    };
    PlanReview {
        token: pp_core::plan::ReviewToken {
            session_id: "session".into(),
            plan_id: document.id.clone(),
            path: document.path().expect("fixture plan has a path"),
            revision: document.revision,
            content: document.content.clone(),
        },
        document,
        report: None,
    }
}

/// Open the review panel and start the standalone reviewer from action 4; returns the ReviewPlan command.
fn start_review(app: &mut App, requests: &mpsc::Receiver<WorkerCommand>) -> WorkerCommand {
    let mut review = plan_review_fixture(PlanStatus::Ready);
    review.report = app.workflow.review.clone();
    app.receive_event_for_test(WorkerEvent::PlanRead(Ok(review)));
    app.handle(key(KeyCode::Char('4')), &mut false);
    app.handle(key(KeyCode::Enter), &mut false);
    let command = requests.try_recv().unwrap();
    let WorkerCommand::ReviewPlan { token, .. } = &command else {
        panic!("expected ReviewPlan command");
    };
    assert_eq!(token.revision, 2);
    assert!(matches!(app.state, State::ReviewingPlan));
    assert!(app.plan_panel.is_none());
    assert!(app.review_transcript.is_some());
    command
}

mod ask_cases;
mod command_cases;
mod desktop_notification_cases;
mod dump_cases;
mod file_picker_cases;
mod hub_cases;
mod input_cases;
mod lsp_cases;
mod model_menu_cases;
mod plan_cases;
mod roles_cases;
mod setting_cases;
mod streaming_panel_cases;
mod tree_session_cases;

mod todo_cases;

mod mcp_cases;
mod mcp_tip_cases;

mod resource_cases;
mod theme_cases;
mod welcome_cases;
