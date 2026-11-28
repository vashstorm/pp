// --- Execution handoff reuses the MCP connections from Plan ---

/// Write a server definition to mcp.json in the config directory; the Agent's
/// `catalog.mcp_path()` lives in that same directory, so the test needs no real user config.
fn write_mcp_config(dir: &std::path::Path, servers: serde_json::Value) {
    std::fs::write(
        dir.join("mcp.json"),
        json!({ "mcpServers": servers }).to_string(),
    )
    .unwrap();
}

/// fixture server definition. Each server logs the received JSON-RPC payloads to its own log,
/// so the number of `initialize` entries is the number of real handshakes.
fn fixture_server(log: &std::path::Path, read_only: bool, mode: Option<&str>) -> serde_json::Value {
    let mut env = json!({
        "MCP_LOG": log.to_str().unwrap(),
        "MCP_VALUE": "test",
    });
    if let Some(mode) = mode {
        env["MCP_MODE"] = json!(mode);
    }
    let mut server = json!({
        "command": "bash",
        "args": [format!(
            "{}/../../tests/fixtures/mcp_server.sh",
            env!("CARGO_MANIFEST_DIR")
        )],
        "env": env,
    });
    if read_only {
        server["read_only_tools"] = json!(["echo"]);
    }
    server
}

/// Count the initialize requests already received in the log; a missing file counts as 0.
fn initialize_count(log: &std::path::Path) -> usize {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter(|line| line.contains("\"method\":\"initialize\""))
        .count()
}

/// Poll until the log shows `expected` initialize requests, used as a cancellation barrier for
/// "the server started its handshake"; returns false on timeout instead of passing silently.
fn wait_for_initialize(log: &std::path::Path, expected: usize) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if initialize_count(log) >= expected {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    false
}

/// The fixture server writes its own PID to `<log>.pid` on startup; signal 0 only probes whether
/// the process exists without delivering a signal, so "the process is still running" can be
/// distinguished from "the entry merely says connected".
fn assert_server_alive(log: &std::path::Path) {
    let pid_path = format!("{}.pid", log.display());
    let pid: i32 = std::fs::read_to_string(&pid_path)
        .unwrap_or_else(|error| panic!("missing {pid_path}: {error}"))
        .trim()
        .parse()
        .expect("fixture pid");
    // SAFETY: kill(pid, 0) only probes existence and delivers no signal.
    let alive = unsafe { libc::kill(pid, 0) } == 0;
    assert!(alive, "MCP fixture process {pid} is not alive");
}

/// Tool names the current manager exposes under that access.
fn mcp_tool_names(agent: &Agent, access: Access) -> Vec<String> {
    agent
        .tools
        .mcp
        .definitions(access)
        .into_iter()
        .map(|tool| tool.name)
        .collect()
}

/// Current status of that server in the `/mcp` list; an unregistered entry counts as
/// "missing".
fn mcp_status(agent: &Agent, access: Access, name: &str) -> &'static str {
    agent
        .tools
        .mcp
        .view(access)
        .entries
        .into_iter()
        .find(|entry| entry.name == name)
        .map_or("missing", |entry| entry.status)
}

/// Enter Plan first, then write and load a new config, covering /mcp reload inside Plan.
fn enter_plan_with_mcp(agent: &mut Agent, dir: &std::path::Path, servers: serde_json::Value) {
    agent.enter_plan(&CancellationToken::new()).unwrap();
    write_mcp_config(dir, servers);
    agent
        .manage_mcp(&pp_mcp::Action::Reload, &CancellationToken::new())
        .unwrap();
}

/// Enter Plan, hold a Ready plan and obtain the current approval token.
fn ready_plan_token(agent: &mut Agent) -> ReviewToken {
    agent.workflow = plan_workflow(PLAN_CONTENT, ORIGIN, PlanStatus::Ready);
    write_plan_file(agent, PLAN_CONTENT);
    seed_session(agent, EXPLORATION);
    agent.read_plan().unwrap().token
}

#[test]
fn plan_exit_reuses_connected_mcp_service_without_reinitializing() {
    let dir = tempfile::tempdir().unwrap();
    let mut agent = runtime_input_agent(dir.path(), "completion", "127.0.0.1:1", "");
    let local_log = dir.path().join("local.log");
    let extra_log = dir.path().join("extra.log");
    // Connect two servers under Execute first: one real handshake each.
    write_mcp_config(
        dir.path(),
        json!({
            "extra": fixture_server(&extra_log, false, None),
            "local": fixture_server(&local_log, true, None),
        }),
    );
    agent
        .manage_mcp(&pp_mcp::Action::Reload, &CancellationToken::new())
        .unwrap();
    assert_eq!(initialize_count(&local_log), 1);
    assert_eq!(initialize_count(&extra_log), 1);
    assert_eq!(mcp_status(&agent, Access::Full, "local"), "connected");
    assert_eq!(mcp_status(&agent, Access::Full, "extra"), "connected");
    assert_server_alive(&local_log);
    assert_server_alive(&extra_log);

    // Reuse both processes after entering Plan; the main session can still call their tools.
    agent.enter_plan(&CancellationToken::new()).unwrap();
    assert_eq!(initialize_count(&local_log), 1);
    assert_eq!(initialize_count(&extra_log), 1);
    for name in ["local", "extra"] {
        assert!(
            agent
                .mcp_view()
                .entries
                .iter()
                .any(|entry| { entry.name == name && entry.status == "connected" })
        );
    }
    assert_server_alive(&local_log);
    assert_server_alive(&extra_log);
    let session_tools = agent.session.tool_names();
    for name in ["mcp__local__echo", "mcp__extra__echo"] {
        assert!(
            session_tools.iter().any(|tool| tool == name),
            "{session_tools:?}"
        );
    }

    // Running Plan reads the plan body and does not touch the MCP connections.
    let token = ready_plan_token(&mut agent);
    assert_eq!(initialize_count(&local_log), 1);
    assert_eq!(initialize_count(&extra_log), 1);
    assert_server_alive(&local_log);
    assert_server_alive(&extra_log);

    // After leaving Plan, both processes and the full tool list are reused.
    let preparation = agent
        .save_plan_and_leave(token, &InputQueue::default(), &CancellationToken::new())
        .unwrap();
    assert_eq!(preparation.workflow.mode, WorkMode::Execute);
    assert_eq!(initialize_count(&local_log), 1);
    assert_eq!(initialize_count(&extra_log), 1);
    assert_eq!(mcp_status(&agent, Access::Full, "local"), "connected");
    assert_eq!(mcp_status(&agent, Access::Full, "extra"), "connected");
    assert_server_alive(&local_log);
    assert_server_alive(&extra_log);
    let names = mcp_tool_names(&agent, Access::Full);
    assert!(names.contains(&"mcp__local__echo".to_owned()), "{names:?}");
    assert!(names.contains(&"mcp__extra__echo".to_owned()), "{names:?}");
    assert_eq!(
        session_tool_count(&agent),
        agent.tools.all_definitions(WorkMode::Execute).len()
    );

    // Enter Plan again and approve execution: the same batch of connections spans the
    // approval and no server is restarted.
    agent.enter_plan(&CancellationToken::new()).unwrap();
    let token = ready_plan_token(&mut agent);
    assert_eq!(initialize_count(&local_log), 1);
    assert_eq!(initialize_count(&extra_log), 1);
    let approval = agent
        .approve_plan(
            token,
            ExecutionContext::Keep,
            &InputQueue::default(),
            &CancellationToken::new(),
            &mut |_| {},
        )
        .unwrap();
    assert_eq!(approval.workflow.mode, WorkMode::Execute);
    assert_eq!(initialize_count(&local_log), 1);
    assert_eq!(initialize_count(&extra_log), 1);
    assert_eq!(mcp_status(&agent, Access::Full, "local"), "connected");
    assert_eq!(mcp_status(&agent, Access::Full, "extra"), "connected");
    assert_server_alive(&local_log);
    assert_server_alive(&extra_log);
    let names = mcp_tool_names(&agent, Access::Full);
    assert!(names.contains(&"mcp__local__echo".to_owned()), "{names:?}");
    assert!(names.contains(&"mcp__extra__echo".to_owned()), "{names:?}");
}

#[test]
fn plan_handoff_ignores_external_config_until_reload() {
    let dir = tempfile::tempdir().unwrap();
    let mut agent = runtime_input_agent(dir.path(), "completion", "127.0.0.1:1", "");
    let local_log = dir.path().join("local.log");
    let replacement_log = dir.path().join("replacement.log");
    enter_plan_with_mcp(
        &mut agent,
        dir.path(),
        json!({"local": fixture_server(&local_log, true, None)}),
    );
    assert_eq!(initialize_count(&local_log), 1);
    let token = ready_plan_token(&mut agent);

    // Config rewritten during Plan: the handoff neither rereads config nor starts new
    // processes.
    write_mcp_config(
        dir.path(),
        json!({"replacement": fixture_server(&replacement_log, true, None)}),
    );
    assert_eq!(initialize_count(&replacement_log), 0);
    let plan_tools = agent.session.tool_names();
    assert!(plan_tools.iter().any(|tool| tool == "mcp__local__echo"));
    assert!(
        !plan_tools
            .iter()
            .any(|tool| tool.starts_with("mcp__replacement__"))
    );

    let preparation = agent
        .save_plan_and_leave(token, &InputQueue::default(), &CancellationToken::new())
        .unwrap();
    assert_eq!(preparation.workflow.mode, WorkMode::Execute);
    // The handoff still uses the old connections and tools: the new server never appears and
    // the old processes are not restarted.
    assert_eq!(initialize_count(&local_log), 1);
    assert_eq!(initialize_count(&replacement_log), 0);
    assert_eq!(mcp_status(&agent, Access::Full, "local"), "connected");
    assert_eq!(mcp_status(&agent, Access::Full, "replacement"), "missing");
    assert_server_alive(&local_log);
    let names = mcp_tool_names(&agent, Access::Full);
    assert!(names.contains(&"mcp__local__echo".to_owned()), "{names:?}");
    assert!(
        !names.contains(&"mcp__replacement__echo".to_owned()),
        "{names:?}"
    );

    // Only an explicit /mcp reload loads the new config: the old server stops and the new one
    // performs its first handshake.
    agent
        .manage_mcp(&pp_mcp::Action::Reload, &CancellationToken::new())
        .unwrap();
    assert_eq!(initialize_count(&local_log), 1);
    assert_eq!(initialize_count(&replacement_log), 1);
    assert_eq!(mcp_status(&agent, Access::Full, "local"), "missing");
    assert_eq!(mcp_status(&agent, Access::Full, "replacement"), "connected");
    assert_server_alive(&replacement_log);
    let names = mcp_tool_names(&agent, Access::Full);
    assert!(
        names.contains(&"mcp__replacement__echo".to_owned()),
        "{names:?}"
    );
    assert!(!names.contains(&"mcp__local__echo".to_owned()), "{names:?}");
}

#[test]
fn plan_exit_isolates_supplementary_initialize_failure() {
    let dir = tempfile::tempdir().unwrap();
    let mut agent = runtime_input_agent(dir.path(), "completion", "127.0.0.1:1", "");
    let local_log = dir.path().join("local.log");
    let broken_log = dir.path().join("broken.log");
    enter_plan_with_mcp(
        &mut agent,
        dir.path(),
        json!({
            "local": fixture_server(&local_log, true, None),
            "broken": fixture_server(&broken_log, false, Some("init_error")),
        }),
    );
    // Loading inside Plan attempts both servers; a single server failure does not affect the
    // others.
    assert_eq!(initialize_count(&local_log), 1);
    assert_eq!(initialize_count(&broken_log), 1);
    assert_eq!(
        mcp_status(&agent, Access::Full, "broken"),
        "failed (initialization)"
    );

    let token = ready_plan_token(&mut agent);
    let preparation = agent
        .save_plan_and_leave(token, &InputQueue::default(), &CancellationToken::new())
        .unwrap();

    // The handoff keeps reusing Plan's healthy connection and preserves the failure state.
    assert_eq!(preparation.workflow.mode, WorkMode::Execute);
    assert_eq!(initialize_count(&local_log), 1);
    assert_eq!(initialize_count(&broken_log), 1);
    assert_eq!(mcp_status(&agent, Access::Full, "local"), "connected");
    assert_server_alive(&local_log);
    assert_eq!(
        mcp_status(&agent, Access::Full, "broken"),
        "failed (initialization)"
    );
    let names = mcp_tool_names(&agent, Access::Full);
    assert!(names.contains(&"mcp__local__echo".to_owned()), "{names:?}");
    assert!(
        !names.iter().any(|name| name.starts_with("mcp__broken__")),
        "{names:?}"
    );
}

#[test]
fn plan_exit_cancel_keeps_mcp_connections_intact() {
    let dir = tempfile::tempdir().unwrap();
    let mut agent = runtime_input_agent(dir.path(), "completion", "127.0.0.1:1", "");
    let local_log = dir.path().join("local.log");
    let extra_log = dir.path().join("extra.log");
    let stuck_log = dir.path().join("stuck.log");
    let mut stuck = fixture_server(&stuck_log, false, Some("timeout_init"));
    // A stuck handshake must not stall the test: the worst case fails at the 5s deadline.
    stuck["timeout"] = json!(5_000);
    agent.enter_plan(&CancellationToken::new()).unwrap();
    write_mcp_config(
        dir.path(),
        json!({
            "extra": fixture_server(&extra_log, false, None),
            "local": fixture_server(&local_log, true, None),
            "stuck": stuck,
        }),
    );
    // Build a state that still has a pending service, exercising the cancellation path of the
    // handoff reconnect directly.
    agent
        .tools
        .mcp
        .manage(
            &pp_mcp::Action::Reload,
            &CancellationToken::new(),
            Access::ReadOnly,
        )
        .unwrap();
    assert_eq!(initialize_count(&local_log), 1);

    let token = ready_plan_token(&mut agent);
    let cancellation = CancellationToken::new();
    // The reconnect runs in server-name order: extra connects first while stuck is blocked in
    // initialize, which is when we cancel.
    let barrier = {
        let token = cancellation.clone();
        let log = stuck_log.clone();
        std::thread::spawn(move || {
            assert!(wait_for_initialize(&log, 1), "stuck server never started its handshake");
            token.cancel();
        })
    };
    let error = agent
        .save_plan_and_leave(token, &InputQueue::default(), &cancellation)
        .err()
        .expect("handoff was cancelled");
    barrier.join().unwrap();

    assert!(error.is::<pp_core::cancellation::Cancelled>(), "{error:#}");
    assert_eq!(agent.workflow.mode, WorkMode::Plan);
    assert!(agent.plan_cleanup_error.is_none());
    assert_eq!(initialize_count(&local_log), 1);
    assert_eq!(initialize_count(&extra_log), 1);
    assert_eq!(initialize_count(&stuck_log), 1);
    // Cancellation does not roll back established connections: extra and local stay connected;
    // only stuck, whose handshake never completed, ends up "not connected" (its own process is
    // stopped on the failure path).
    assert_eq!(mcp_status(&agent, Access::Full, "extra"), "connected");
    assert_eq!(mcp_status(&agent, Access::Full, "stuck"), "not connected");
    assert_eq!(mcp_status(&agent, Access::Full, "local"), "connected");
    assert_server_alive(&local_log);
    assert_server_alive(&extra_log);
    let plan_tools = agent.tools.all_definitions(WorkMode::Plan);
    assert!(
        plan_tools
            .iter()
            .any(|tool| tool.name == "mcp__extra__echo")
    );
}

#[test]
fn plan_handoff_failure_keeps_mcp_connections_intact() {
    let dir = tempfile::tempdir().unwrap();
    let mut agent = runtime_input_agent(dir.path(), "completion", "127.0.0.1:1", "");
    let local_log = dir.path().join("local.log");
    let extra_log = dir.path().join("extra.log");
    enter_plan_with_mcp(
        &mut agent,
        dir.path(),
        json!({
            "extra": fixture_server(&extra_log, false, None),
            "local": fixture_server(&local_log, true, None),
        }),
    );
    let token = ready_plan_token(&mut agent);

    // The session journal becomes read-only: the handoff fails at commit_plan_exit.
    let id = agent.session_id().unwrap().to_owned();
    let path = dir.path().join("sessions").join(format!("{id}.jsonl"));
    agent.journal.use_read_only_file(&path);
    let error = agent
        .save_plan_and_leave(token, &InputQueue::default(), &CancellationToken::new())
        .err()
        .expect("read-only journal makes the handoff fail");

    assert_eq!(agent.workflow.mode, WorkMode::Plan);
    // The failure comes from session persistence, not from the MCP connections.
    assert!(
        error.to_string().starts_with("persisting session "),
        "{error:#}"
    );
    assert!(agent.plan_cleanup_error.is_none());
    // After the failed handoff, the extra and local servers connected during Plan remain
    // usable.
    assert_eq!(initialize_count(&local_log), 1);
    assert_eq!(initialize_count(&extra_log), 1);
    assert_eq!(mcp_status(&agent, Access::Full, "extra"), "connected");
    assert_eq!(mcp_status(&agent, Access::Full, "local"), "connected");
    assert_server_alive(&local_log);
    assert_server_alive(&extra_log);
    let plan_tools = agent.session.tool_names();
    assert!(plan_tools.iter().any(|tool| tool == "mcp__extra__echo"));
}
