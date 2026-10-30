use super::auth::*;
use crate::auth::Provider;
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::{Terminal, backend::TestBackend};

fn key(code: KeyCode) -> Event {
    Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn render(panel: &mut Panel, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| panel.draw(frame, frame.area()))
        .unwrap();
    terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect()
}

#[test]
fn auth_panel_matches_provider_status_and_logout_scope() {
    let mut panel = Panel::new(false);
    panel.listed(vec![Provider::OpenaiCodex, Provider::Zai]);
    let screen = render(&mut panel, 120, 30);
    assert!(screen.contains("Select provider to login"));
    assert!(screen.contains("ChatGPT Plus/Pro (Codex Subscription) ● logged in (login)"));
    assert!(screen.contains("Z.AI (GLM Coding Plan) ● logged in (api key)"));
    assert!(screen.contains('╭') && screen.contains('╰'));
    for (width, height) in [(20, 6), (1, 1), (80, 20)] {
        render(&mut panel, width, height);
    }
    assert!(matches!(panel.handle(key(KeyCode::Enter)), Action::None));
    let screen = render(&mut panel, 120, 30);
    assert!(screen.contains("Log in again") && screen.contains("Retry model discovery"));
    panel.handle(key(KeyCode::Down));
    assert!(matches!(
        panel.handle(key(KeyCode::Enter)),
        Action::Discover(Provider::OpenaiCodex)
    ));
    assert!(matches!(panel.handle(key(KeyCode::Esc)), Action::Cancel));
    assert!(panel.closing);

    let mut logout = Panel::new(true);
    logout.listed(vec![Provider::GithubCopilot]);
    let screen = render(&mut logout, 100, 20);
    assert!(screen.contains("Select provider to logout") && screen.contains("GitHub Copilot"));
    assert!(!screen.contains("ChatGPT") && !screen.contains("Kimi Code"));
    assert!(matches!(
        logout.handle(key(KeyCode::Enter)),
        Action::Logout(Provider::GithubCopilot)
    ));
    let mut empty = Panel::new(true);
    empty.listed(Vec::new());
    assert!(render(&mut empty, 100, 20).contains("No providers are logged in."));
}

#[test]
fn auth_key_panel_masks_paste_and_routes_links_without_leaking_input() {
    let mut panel = Panel::new(false);
    panel.listed(Vec::new());
    panel.handle(key(KeyCode::Down));
    let url = match panel.handle(key(KeyCode::Enter)) {
        Action::Open(url) => url,
        _ => panic!("必须打开密钥管理页"),
    };
    assert_eq!(url, Provider::Zai.key_url().unwrap());
    panel.handle(Event::Paste("private-key-marker".into()));
    let screen = render(&mut panel, 100, 30);
    assert!(!screen.contains("private-key-marker"));
    assert!(screen.contains('•') && screen.contains("Paste your Z.AI API key"));
    assert!(matches!(panel.handle(key(KeyCode::F(2))), Action::Open(value) if value == url));
    assert!(
        matches!(panel.handle(key(KeyCode::Enter)), Action::Login(Provider::Zai, Some(value)) if value == "private-key-marker")
    );
    assert!(!render(&mut panel, 100, 30).contains("private-key-marker"));
}

#[test]
fn auth_commandcode_key_panel_masks_input_and_submits_to_correct_provider() {
    let mut panel = Panel::new(false);
    panel.listed(Vec::new());
    assert!(render(&mut panel, 100, 30).contains("Command Code"));
    panel.handle(key(KeyCode::End));
    let url = match panel.handle(key(KeyCode::Enter)) {
        Action::Open(url) => url,
        _ => panic!("必须打开 Command Code 控制台"),
    };
    assert_eq!(url, Provider::CommandCode.key_url().unwrap());
    assert!(matches!(panel.handle(key(KeyCode::Enter)), Action::None));
    panel.handle(Event::Paste("user_private-commandcode-key".into()));
    let screen = render(&mut panel, 100, 30);
    assert!(screen.contains("Paste your Command Code API key"));
    assert!(screen.contains("user_...") && screen.contains('•'));
    assert!(!screen.contains("user_private-commandcode-key"));
    assert!(matches!(
        panel.handle(key(KeyCode::Enter)),
        Action::Login(Provider::CommandCode, Some(value)) if value == "user_private-commandcode-key"
    ));
    assert!(!render(&mut panel, 100, 30).contains("user_private-commandcode-key"));

    let mut logout = Panel::new(true);
    logout.listed(vec![Provider::CommandCode]);
    assert!(render(&mut logout, 100, 30).contains("Command Code ● logged in (api key)"));
    assert!(matches!(
        logout.handle(key(KeyCode::Enter)),
        Action::Logout(Provider::CommandCode)
    ));
}

#[test]
fn auth_panel_mouse_selects_and_opens_wrapped_urls_during_wait() {
    let mut panel = Panel::new(false);
    panel.listed(Vec::new());
    render(&mut panel, 100, 30);
    // 面板顶边 y=1，首条项目位于 y=2。
    let click = |x, y| {
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        })
    };
    assert!(matches!(
        panel.handle(click(5, 2)),
        Action::Login(Provider::OpenaiCodex, None)
    ));
    panel.progress(crate::auth::service::Progress::Verification {
        url: "https://auth.openai.com/long-authorization-page".into(),
        code: "ABC-123".into(),
    });
    let screen = render(&mut panel, 30, 20);
    assert!(screen.contains("ABC-123"));
    assert!(
        matches!(panel.handle(click(5, 3)), Action::Open(url) if url.starts_with("https://auth.openai.com/"))
    );
}

#[test]
fn auth_first_login_bootstraps_agent_without_restarting_ui_or_losing_draft() {
    use super::{App, State};
    let dir = tempfile::tempdir().unwrap();
    let global = dir.path().join("agent");
    std::fs::create_dir(&global).unwrap();
    let token = crate::cancellation::CancellationToken::new();
    crate::auth::service::commit_login(
        &global,
        Provider::Zai,
        crate::auth::Credential::api_key("fixture-private".into()),
        &token,
    )
    .unwrap();
    let store = crate::auth::Store::load(&global.join("agent.db")).unwrap();
    let mut catalog = crate::auth::models::Catalog::default();
    catalog.providers.insert(
        "zai".into(),
        crate::auth::models::Entry {
            auth: Provider::Zai,
            generation: store.credentials[&Provider::Zai].generation.clone(),
            protocol: Provider::Zai.protocol(),
            models: std::collections::BTreeMap::from([(
                "remote".into(),
                crate::auth::models::Model {
                    protocol: None,
                    model: "remote".into(),
                    context_window: None,
                },
            )]),
        },
    );
    catalog.save(&global.join("models-sub.yml")).unwrap();
    let factory = crate::agent::Bootstrap {
        dir: global,
        cwd: dir.path().into(),
        skills: crate::skills::Skills::default().shared(),
        prompt: crate::system_prompt::Snapshot::bundled(),
        approval: None,
    };
    let mut app = App::new(Err(anyhow::anyhow!("No models configured"))).unwrap();
    app.bootstrap = Some(factory);
    let (worker, requests, replies) = Worker::fixture();
    app.auth_worker = worker;
    app.input.insert_str("existing draft");
    assert!(app.open_auth(false));
    assert!(matches!(requests.recv().unwrap().0, Request::List));
    replies
        .send(Reply::Listed(Ok(vec![Provider::Zai])))
        .ok()
        .unwrap();
    app.receive();
    replies
        .send(Reply::Updated(Ok(crate::auth::service::Update {
            logged: vec![Provider::Zai],
            message: "Discovered 1 model".into(),
        })))
        .ok()
        .unwrap();
    app.receive();
    let factory = match requests.recv().unwrap().0 {
        Request::Bootstrap(factory) => factory,
        _ => panic!("首次授权必须初始化 Agent"),
    };
    replies
        .send(Reply::Bootstrapped(factory.build().map(Box::new)))
        .ok()
        .unwrap();
    app.receive();
    assert!(matches!(app.state, State::Ready));
    assert_eq!(app.current_model.as_ref().unwrap().reference, "zai/remote");
    assert_eq!(app.input.lines(), &["existing draft"]);
    app.apply_auth_action(Action::Close);
    assert!(app.auth_panel.is_none());
    assert!(app.input_history.is_empty());
}
