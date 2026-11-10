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

fn changes_view(blocked: bool) -> ChangesView {
    let path = std::path::PathBuf::from("demo.txt");
    ChangesView {
        task_id: 1,
        revision: 2,
        files: vec![pp_runtime::tools::ChangedFile {
            sources: Vec::new(),
            path: path.clone(),
            diff: pp_core::events::FileDiff::Text {
                created: false,
                deleted: false,
                added: 1,
                removed: 1,
                hunks: vec![pp_core::events::DiffHunk {
                    lines: vec![
                        pp_core::events::DiffLine {
                            kind: pp_core::events::DiffLineKind::Removed,
                            old_line: Some(1),
                            new_line: None,
                            text: "user-local\n".into(),
                        },
                        pp_core::events::DiffLine {
                            kind: pp_core::events::DiffLineKind::Added,
                            old_line: None,
                            new_line: Some(1),
                            text: "agent-final\n".into(),
                        },
                    ],
                }],
            },
            undo: if blocked {
                Err("File changed outside this task; undo refused.".into())
            } else {
                Ok(UndoToken {
                    task_id: 1,
                    revision: 2,
                    path,
                })
            },
        }],
    }
}

fn changes_empty_view() -> ChangesView {
    ChangesView {
        task_id: 1,
        revision: 3,
        files: vec![],
    }
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
    app.ask_panel = Some(ask::Panel::new(pp_core::interaction::AskRequest {
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
            name: "read".into(),
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

/// 真实 `model_control` 控制线程 + 主 Agent 绑定：临时目录里单 provider 单模型，
/// 未写 `thinking` 因此允许全部档位。夹具只用于 `Change::Think`，不重载模型文件。
fn model_control() -> (
    pp_runtime::model_control::Service,
    pp_runtime::model_control::Binding,
) {
    let (control, binding, _) = model_control_with(
        r#"{"providers":{"fixture":{"api_key":"fixture","models":{"one":{"model":"one"}}}}}"#,
    );
    (control, binding)
}

/// 与 [`model_control`] 相同，但由调用方给出 `models.yml` 正文，并返回目录快照。
///
/// 夹具预置一个与配置默认不同的手动档位：新的 `ModelState` 中 `Change::Think`
/// 只修改已有的 `a` 槽，未登记覆盖时不会发布新 revision。测试关注提交/确认链路，
/// 因此这里先建立与真实使用一致的前提。
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

/// 轮询 UI 直到真实控制线程发布手动覆盖目标；返回已登记的 `a` 快照。
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

/// 先等到 `ThinkQueued` 再注入结果，避免异步控制线程与测试注入交错。
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

/// 夹具目录里的唯一模型：未写 `thinking` 因此允许全部档位。
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

/// 打开审阅面板并通过操作区第 4 项启动独立 reviewer；返回 ReviewPlan 命令。
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
mod changes_cases;
mod command_cases;
mod file_picker_cases;
mod hub_cases;
mod input_cases;
mod lsp_cases;
mod model_menu_cases;
mod pipes_cases;
mod plan_cases;
mod roles_cases;
mod streaming_panel_cases;
mod tree_session_cases;

mod todo_cases;

mod mcp_cases;

mod resource_cases;
