use super::*;

#[test]
fn agent_hub_shortcut_opens_over_completion_and_preserves_draft() {
    let mut app = app();
    app.input.insert_str("/ag");
    app.refresh_candidates(true);
    assert!(app.menu.is_some());
    let toggle = Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::ALT));
    app.handle(toggle.clone(), &mut false);
    assert!(app.subagent_panel.is_some());
    assert!(app.menu.is_none());
    assert!(draw(&mut app, 120, 30).contains("Agent Hub"));
    assert_eq!(app.input_text(), "/ag");
    app.handle(toggle, &mut false);
    assert!(app.subagent_panel.is_none());
    assert_eq!(app.input_text(), "/ag");
}

#[test]
fn agent_hub_shortcut_only_accepts_option_a_press() {
    use crate::tui::app::hub::is_hub_toggle;
    for modifiers in [
        KeyModifiers::NONE,
        KeyModifiers::CONTROL,
        KeyModifiers::ALT | KeyModifiers::CONTROL,
    ] {
        assert!(!is_hub_toggle(&Event::Key(KeyEvent::new(
            KeyCode::Char('a'),
            modifiers,
        ))));
    }
    for kind in [KeyEventKind::Repeat, KeyEventKind::Release] {
        assert!(!is_hub_toggle(&Event::Key(KeyEvent::new_with_kind(
            KeyCode::Char('a'),
            KeyModifiers::ALT,
            kind,
        ))));
    }
    let mut app = app();
    app.input.insert_str("keep draft");
    app.handle(control('a'), &mut false);
    assert!(app.subagent_panel.is_none());
    assert_eq!(app.input.cursor().1, 0);
    assert_eq!(app.input_text(), "keep draft");
}

fn wait_until(mut ready: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while std::time::Instant::now() < deadline {
        if ready() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    panic!("timed out waiting for hub condition");
}

fn seed_subagent_root(
    dir: &std::path::Path,
    tokens: u64,
    with_agent: bool,
) -> pp_runtime::subagent::Binding {
    use std::io::Write;
    std::fs::create_dir_all(dir).unwrap();
    let registry = if with_agent {
        serde_json::json!({
            "version": 1,
            "agents": {
                "a1": {
                    "id": "a1",
                    "name": "worker-1",
                    "parent": "Main",
                    "generation": 1,
                    "status": "parked",
                    "spec": {
                        "definition": "fast",
                        "prompt": "",
                        "context": "",
                        "model": "fixture-model",
                        "thinking": "none",
                        "tools": ["read", "bash"],
                        "spawns": [],
                        "output_schema": null,
                        "cwd": ".",
                        "depth": 1,
                        "readonly": false,
                        "isolated": false
                    },
                    "session_file": null,
                    "registered_ms": 1000,
                    "updated_ms": 2000
                }
            },
            "runs": {
                "r1": {
                    "id": "r1",
                    "agent_id": "a1",
                    "owner": "Main",
                    "status": "completed",
                    "task": "audit workspace",
                    "result": { "ok": true, "output": "done" },
                    "consumed": true,
                    "duration_ms": 500,
                    "started_ms": 1000,
                    "requests": 1,
                    "tokens": tokens,
                    "cost": 0.01
                }
            },
            "next_id": 1
        })
    } else {
        serde_json::json!({
            "version": 1,
            "agents": {},
            "runs": {},
            "next_id": 0
        })
    };
    std::fs::write(dir.join("registry.json"), registry.to_string()).unwrap();
    if with_agent {
        let mut log = std::fs::File::create(dir.join("a1.events.jsonl")).unwrap();
        let events = [
            serde_json::json!({"agent_id":"a1","run_id":"r1","generation":1,"timestamp_ms":1000,"hub":{"kind":"run_state","status":"running"}}),
            serde_json::json!({"agent_id":"a1","run_id":"r1","generation":1,"timestamp_ms":1010,"event":{"tool_started":{"id":"t1","name":"read","arguments":"{\"path\":\"a.rs\"}"}}}),
            serde_json::json!({"agent_id":"a1","run_id":"r1","generation":1,"timestamp_ms":1020,"event":{"tool_finished":{"id":"t1","result":{"ok":true}}}}),
            serde_json::json!({"agent_id":"a1","run_id":"r1","generation":1,"timestamp_ms":1030,"event":{"tool_started":{"id":"t2","name":"bash","arguments":"{\"command\":\"ls\"}"}}}),
            serde_json::json!({"agent_id":"a1","run_id":"r1","generation":1,"timestamp_ms":1040,"event":{"tool_output":{"id":"t2","stream":"stdout","text":"ignored-output"}}}),
            serde_json::json!({"agent_id":"a1","run_id":"r1","generation":1,"timestamp_ms":1050,"event":{"tool_finished":{"id":"t2","result":{"ok":true}}}}),
            serde_json::json!({"agent_id":"a1","run_id":"r1","generation":1,"timestamp_ms":1100,"event":{"model_request_finished":{"elapsed_ms":50,"usage":{"input_tokens":tokens,"output_tokens":0},"success":true}}}),
            serde_json::json!({"agent_id":"a1","run_id":"r1","generation":1,"timestamp_ms":1500,"hub":{"kind":"run_state","status":"completed"}}),
        ];
        for event in events {
            writeln!(log, "{event}").unwrap();
        }
    }
    pp_runtime::subagent::Binding::fixture_root(dir).unwrap()
}

#[test]
fn hub_sync_root_and_subagents_ensured_avoid_duplicate_bind_and_reset_projection_on_root_change() {
    let dir1 = tempfile::tempdir().unwrap();
    let dir2 = tempfile::tempdir().unwrap();
    let root1 = seed_subagent_root(dir1.path(), 1200, true);
    let root2 = seed_subagent_root(dir2.path(), 0, false);

    let mut app = app();
    *app.subagent_control.lock().unwrap() = Some(root1);
    // hub_tick 先于 SubagentsEnsured 看到新根：只触发一次 Bind。
    app.hub_tick();
    let epoch_after_sync = app.hub_root_epoch;
    assert_eq!(epoch_after_sync, 1);
    wait_until(|| {
        app.hub_tick();
        app.hub_projection.agent_ready("a1")
    });

    app.open_hub();
    assert_eq!(app.hub_root_epoch, epoch_after_sync);
    // 迟到的 SubagentsEnsured 对同一根不得重绑，也不会让 tokens 翻倍。
    app.subagents_ensured(Ok(()));
    assert_eq!(app.hub_root_epoch, epoch_after_sync);
    let screen = draw(&mut app, 130, 32);
    assert!(screen.contains("2 tools"), "{screen}");
    assert!(screen.contains("1K tok"), "{screen}");
    assert!(!screen.contains("2K tok"), "{screen}");
    assert!(!screen.contains("· loading"), "{screen}");

    // 开着 Hub 换到新根（无子 Agent）：必须清空旧投影并重新附着新空根。
    *app.subagent_control.lock().unwrap() = Some(root2);
    app.hub_tick();
    assert!(app.hub_attached);
    wait_until(|| {
        app.hub_tick();
        draw(&mut app, 130, 32).contains("No subagents yet")
    });
    let empty_screen = draw(&mut app, 130, 32);
    assert!(!empty_screen.contains("worker-1"), "{empty_screen}");

    // 关闭 Hub 后若回到无子 Agent 的空根：不保持附着。
    let dir3 = tempfile::tempdir().unwrap();
    let root3 = seed_subagent_root(dir3.path(), 0, false);
    app.close_hub();
    *app.subagent_control.lock().unwrap() = Some(root3);
    app.hub_tick();
    assert!(!app.hub_attached);
}

#[test]
fn hub_preloads_existing_subagents_and_reuses_projection_within_root() {
    let empty_dir = tempfile::tempdir().unwrap();
    let full_dir = tempfile::tempdir().unwrap();
    let empty_root = seed_subagent_root(empty_dir.path(), 0, false);
    let full_root = seed_subagent_root(full_dir.path(), 3000, true);

    let mut app = app();
    *app.subagent_control.lock().unwrap() = Some(empty_root);
    app.hub_tick();
    assert!(app.hub_worker.is_none(), "无子 Agent 的会话不启动观察线程");

    // 切换到包含历史子 Agent 的会话（例如 /resume）：未打开 Hub 也会后台静默预加载。
    *app.subagent_control.lock().unwrap() = Some(full_root);
    app.hub_tick();
    assert!(app.hub_worker.is_some());
    assert!(app.subagent_panel.is_none());
    wait_until(|| {
        app.hub_tick();
        app.hub_projection.agent_ready("a1")
    });

    // 首帧打开 Hub 直接看到完整终态，无 loading 占位。
    app.open_hub();
    let epoch = app.hub_root_epoch;
    let first_open = draw(&mut app, 130, 32);
    assert!(first_open.contains("2 tools"), "{first_open}");
    assert!(first_open.contains("Recent activity"), "{first_open}");
    assert!(first_open.contains("Read"), "{first_open}");
    assert!(!first_open.contains("Loading activity"), "{first_open}");
    assert!(!first_open.contains("— tools"), "{first_open}");

    // 同根内关闭再打开：复用投影与 epoch，不重扫。
    app.close_hub();
    assert!(app.subagent_panel.is_none());
    app.open_hub();
    assert_eq!(app.hub_root_epoch, epoch);
    let reopened = draw(&mut app, 130, 32);
    assert!(reopened.contains("2 tools"), "{reopened}");
    assert!(!reopened.contains("— tools"), "{reopened}");
}

#[test]
fn hub_shows_stable_placeholders_while_loading_and_switches_on_batch() {
    use crate::tui::subagents::{Drafts, Panel, Reply};
    use pp_core::events::AgentEvent;
    use pp_core::model::ThinkLevel;
    use pp_runtime::subagent::{
        HubAgent, HubSnapshot, ObservedEntry, ObservedRecord, OverviewPage, Record, Run, Spec,
    };
    use ratatui::{Terminal, backend::TestBackend};
    use std::path::PathBuf;

    let mut panel = Panel::default();
    let mut drafts = Drafts::default();
    let snapshot = HubSnapshot {
        output_previews: vec![],
        pipeline_previews: vec![],
        shell_previews: vec![],
        agents: vec![HubAgent {
            record: Record {
                id: "a1".into(),
                name: "a1-name".into(),
                parent: "Main".into(),
                generation: 1,
                status: "parked".into(),
                spec: Spec {
                    model_role: None,
                    role_route: None,
                    definition: "fast".into(),
                    prompt: String::new(),
                    context: String::new(),
                    model: "fixture-model".into(),
                    thinking: ThinkLevel::None,
                    tools: vec![],
                    spawns: vec![],
                    output_schema: None,
                    cwd: PathBuf::from("."),
                    depth: 1,
                    readonly: false,
                    isolated: false,
                    cost: None,
                },
                session_file: None,
                registered_ms: None,
                updated_ms: 100,
            },
            model_change: None,
        }],
        runs: vec![Run {
            id: "r1".into(),
            agent_id: "a1".into(),
            owner: "Main".into(),
            status: "completed".into(),
            task: "work".into(),
            result: None,
            consumed: true,
            duration_ms: 10,
            started_ms: 10,
            requests: 1,
            tokens: 0,
            cost: None,
        }],
        warnings: vec![],
    };
    panel.apply(Reply::Snapshot {
        epoch: 1,
        snapshot: Box::new(snapshot),
    });

    let render = |panel: &mut Panel, drafts: &mut Drafts| -> String {
        let mut terminal = Terminal::new(TestBackend::new(130, 32)).unwrap();
        terminal
            .draw(|frame| panel.draw(frame, frame.area(), drafts, &Default::default()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..32)
            .map(|y| {
                (0..130)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    let loading_screen = render(&mut panel, &mut drafts);
    assert!(loading_screen.contains("— tools"), "{loading_screen}");
    assert!(
        loading_screen.contains("Loading activity…"),
        "{loading_screen}"
    );
    assert!(!loading_screen.contains("0 tools"), "{loading_screen}");

    panel.apply(Reply::ActivityBatch {
        epoch: 1,
        pages: vec![(
            "a1".into(),
            OverviewPage {
                entries: vec![ObservedEntry {
                    start_offset: 0,
                    record: ObservedRecord {
                        root_id: None,
                        agent_id: "a1".into(),
                        run_id: Some("r1".into()),
                        generation: 1,
                        sequence: Some(1),
                        timestamp_ms: Some(1_000),
                        event: Some(AgentEvent::ToolStarted {
                            id: "t1".into(),
                            name: "read".into(),
                            arguments: r#"{"path":"main.rs"}"#.into(),
                        }),
                        hub: None,
                    },
                }],
                last_activity_ms: Some(1_000),
                cursor: None,
                reset: false,
                has_more: false,
                warnings: vec![],
            },
        )],
    });
    let ready_screen = render(&mut panel, &mut drafts);
    assert!(ready_screen.contains("1 tools"), "{ready_screen}");
    assert!(!ready_screen.contains("— tools"), "{ready_screen}");
    assert!(
        !ready_screen.contains("Loading activity…"),
        "{ready_screen}"
    );
}
