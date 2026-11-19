// --- 执行交接复用 Plan 期 MCP 连接 ---

/// 把服务定义写入配置目录的 mcp.json；Agent 的 `catalog.mcp_path()` 就在
/// 同一目录，因此测试无需真实用户配置。
fn write_mcp_config(dir: &std::path::Path, servers: serde_json::Value) {
    std::fs::write(
        dir.join("mcp.json"),
        json!({ "mcpServers": servers }).to_string(),
    )
    .unwrap();
}

/// fixture 服务定义。每个服务用独立日志记录收到的 JSON-RPC 原文，
/// `initialize` 条数即真实握手次数。
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

/// 统计日志中已收到的 initialize 请求条数；文件缺失按 0 计。
fn initialize_count(log: &std::path::Path) -> usize {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter(|line| line.contains("\"method\":\"initialize\""))
        .count()
}

/// 轮询等待日志出现 `expected` 条 initialize 请求，用作“服务已开始握手”
/// 的取消栅栏；超时返回 false，不静默通过。
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

/// fixture 服务启动时把自身 PID 写入 `<日志>.pid`；信号 0 只探测进程是否
/// 存在，不向它递送信号，因此能区分“进程仍在运行”和“仅条目仍写 connected”。
fn assert_server_alive(log: &std::path::Path) {
    let pid_path = format!("{}.pid", log.display());
    let pid: i32 = std::fs::read_to_string(&pid_path)
        .unwrap_or_else(|error| panic!("missing {pid_path}: {error}"))
        .trim()
        .parse()
        .expect("fixture pid");
    // SAFETY: kill(pid, 0) 只做存在性探测，不递送任何信号。
    let alive = unsafe { libc::kill(pid, 0) } == 0;
    assert!(alive, "MCP fixture process {pid} is not alive");
}

/// 当前 manager 在该 access 下暴露的工具名。
fn mcp_tool_names(agent: &Agent, access: Access) -> Vec<String> {
    agent
        .tools
        .mcp
        .definitions(access)
        .into_iter()
        .map(|tool| tool.name)
        .collect()
}

/// `/mcp` 列表里该服务此刻的状态；未登记按 "missing" 计。
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

/// 先进入 Plan，再写入并加载新配置，覆盖 Plan 内的 /mcp reload。
fn enter_plan_with_mcp(agent: &mut Agent, dir: &std::path::Path, servers: serde_json::Value) {
    agent.enter_plan(&CancellationToken::new()).unwrap();
    write_mcp_config(dir, servers);
    agent
        .manage_mcp(&pp_mcp::Action::Reload, &CancellationToken::new())
        .unwrap();
}

/// 进入 Plan 后持有 Ready 计划并取得当前审批 token。
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
    // Execute 下先连接两个服务：各一次真实握手。
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

    // 进入 Plan 后复用两个进程，主会话仍可调用它们的工具。
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

    // Plan 运行读取计划正文，不触碰 MCP 连接。
    let token = ready_plan_token(&mut agent);
    assert_eq!(initialize_count(&local_log), 1);
    assert_eq!(initialize_count(&extra_log), 1);
    assert_server_alive(&local_log);
    assert_server_alive(&extra_log);

    // 退出 Plan 后沿用两个进程和完整工具表。
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

    // 再次进入 Plan 并批准执行：同一批连接贯穿审批，不重启服务。
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

    // Plan 期改写配置：交接既不重读配置，也不启动新进程。
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
    // 交接仍用旧连接与旧工具：新服务未出现，旧进程未重启。
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

    // 只有显式 /mcp reload 才加载新配置：旧服务停止，新服务首次握手。
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
    // Plan 内加载时尝试连接两个服务，单服务失败不影响其他服务。
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

    // 交接继续复用 Plan 期的健康连接，并保留失败状态。
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
    // 卡住的握手不能拖住测试：最坏情况下按 5s 截止时间失败。
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
    // 构造仍有待连接服务的状态，直接覆盖交接补连中的取消路径。
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
    // 补连按服务名序执行：extra 先连上，stuck 卡在 initialize，此时取消。
    let barrier = {
        let token = cancellation.clone();
        let log = stuck_log.clone();
        std::thread::spawn(move || {
            assert!(wait_for_initialize(&log, 1), "stuck 服务未开始握手");
            token.cancel();
        })
    };
    let error = agent
        .save_plan_and_leave(token, &InputQueue::default(), &cancellation)
        .err()
        .expect("交接被取消");
    barrier.join().unwrap();

    assert!(error.is::<pp_core::cancellation::Cancelled>(), "{error:#}");
    assert_eq!(agent.workflow.mode, WorkMode::Plan);
    assert!(agent.plan_cleanup_error.is_none());
    assert_eq!(initialize_count(&local_log), 1);
    assert_eq!(initialize_count(&extra_log), 1);
    assert_eq!(initialize_count(&stuck_log), 1);
    // 取消不回滚已建立的连接：extra 与 local 保持 connected；只有尚未完成
    // 握手的 stuck 以“未连接”收尾（它自己的进程在失败路径被停止）。
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

    // 会话日志改为只读：交接在 commit_plan_exit 处失败。
    let id = agent.session_id().unwrap().to_owned();
    let path = dir.path().join("sessions").join(format!("{id}.jsonl"));
    agent.journal.use_read_only_file(&path);
    let error = agent
        .save_plan_and_leave(token, &InputQueue::default(), &CancellationToken::new())
        .err()
        .expect("只读日志使交接失败");

    assert_eq!(agent.workflow.mode, WorkMode::Plan);
    // 失败来自会话持久化，而不是 MCP 连接。
    assert!(
        error.to_string().starts_with("persisting session "),
        "{error:#}"
    );
    assert!(agent.plan_cleanup_error.is_none());
    // 交接失败后，Plan 中已连接的 extra 与 local 仍可用。
    assert_eq!(initialize_count(&local_log), 1);
    assert_eq!(initialize_count(&extra_log), 1);
    assert_eq!(mcp_status(&agent, Access::Full, "extra"), "connected");
    assert_eq!(mcp_status(&agent, Access::Full, "local"), "connected");
    assert_server_alive(&local_log);
    assert_server_alive(&extra_log);
    let plan_tools = agent.session.tool_names();
    assert!(plan_tools.iter().any(|tool| tool == "mcp__extra__echo"));
}
