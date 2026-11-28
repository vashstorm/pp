// /resume boundary coverage (plan verification items 1, 4, and 5): listing
// placement/order/truncation, failure atomicity that preserves the current
// agent, interaction limits, and MCP resource handover after a cross-session
// switch. Everything runs against real PTYs, the loopback provider, and real
// files/locks; no production behavior is mocked. Helpers are prefixed
// `resume_boundary_` and no new `use` statements are introduced so sibling
// include files cannot collide.

fn resume_boundary_sessions_dir(home: &Path) -> std::path::PathBuf {
    home.join(".pp/agent/sessions")
}

fn resume_boundary_valid_id(id: &str) -> bool {
    let Some((time, random)) = id.rsplit_once('-') else {
        return false;
    };
    time.len() == 19
        && chrono::NaiveDateTime::parse_from_str(time, "%Y-%m-%dT%H-%M-%S").is_ok()
        && random.len() == 32
        && random
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn resume_boundary_session_ids(home: &Path) -> Vec<String> {
    let Ok(iter) = std::fs::read_dir(resume_boundary_sessions_dir(home)) else {
        return Vec::new();
    };
    let mut ids: Vec<String> = iter
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter_map(|name| name.strip_suffix(".jsonl").map(str::to_owned))
        .filter(|id| resume_boundary_valid_id(id))
        .collect();
    ids.sort();
    ids
}

fn resume_boundary_journal(home: &Path, id: &str) -> std::path::PathBuf {
    resume_boundary_sessions_dir(home).join(format!("{id}.jsonl"))
}

// Drive one real session (completion protocol) in the given cwd under the
// shared HOME and return its saved ID.
fn resume_boundary_create_session_with(
    server: &TestServer,
    home: &Path,
    cwd: &Path,
    settings: &str,
    models: &str,
    user_message: &str,
    answer: &str,
) -> String {
    crate::write_settings(home, settings).unwrap();
    std::fs::write(config_path(home, "models.yml"), models).unwrap();
    let before = resume_boundary_session_ids(home);
    let mut pty = Pty::start_with_args(cwd, home, &[]);
    pty.ready();
    pty.input(&format!("{user_message}\r"));
    let (request, _) = pty.request(server);
    final_reply(request, answer);
    pty.completed_after(answer);
    pty.input("/quit\r");
    pty.finish();
    no_more(server);
    let new: Vec<String> = resume_boundary_session_ids(home)
        .into_iter()
        .filter(|id| !before.contains(id))
        .collect();
    assert_eq!(
        new.len(),
        1,
        "expected exactly one new session, got {new:?}"
    );
    new.into_iter().next().unwrap()
}

fn resume_boundary_create_session(
    server: &TestServer,
    home: &Path,
    cwd: &Path,
    user_message: &str,
    answer: &str,
) -> String {
    resume_boundary_create_session_with(
        server,
        home,
        cwd,
        fixture_settings(),
        &fixture_models(server),
        user_message,
        answer,
    )
}

fn resume_boundary_set_modified(path: &Path, time: std::time::SystemTime) {
    let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    file.set_times(std::fs::FileTimes::new().set_modified(time))
        .unwrap();
}

fn resume_boundary_open(pty: &mut Pty) {
    pty.input("/resume\r");
    pty.wait_for("session panel opens in the conversation area", |screen| {
        screen.contents().contains("Resume session")
            && !screen.contents().contains("Loading saved sessions…")
    });
    pty.input("\x1b[C");
    pty.text("Resume session (all projects)");
}

fn resume_boundary_close(pty: &mut Pty) {
    pty.input("\x1b");
    pty.wait_for("Esc closes the session panel", |screen| {
        !screen.contents().contains("Resume session") && input_is_ready(screen)
    });
}

// 进程内 `/resume` 复用启动目录的 MCP 连接：不重新握手，也不替换服务进程。
fn resume_boundary_assert_mcp_reused(dir: &Path) {
    let requests = std::fs::read_to_string(dir.join("mcp-log")).unwrap();
    assert_eq!(
        requests
            .lines()
            .filter(|line| line.contains("\"method\":\"initialize\""))
            .count(),
        1,
        "resume 不应重新初始化 MCP 服务\n{requests}"
    );
    let pid: i32 = std::fs::read_to_string(dir.join("mcp-log.pid"))
        .unwrap()
        .parse()
        .unwrap();
    let pid = rustix::process::Pid::from_raw(pid).unwrap();
    assert!(
        rustix::process::test_kill_process(pid).is_ok(),
        "resume 应复用启动目录的 MCP 进程"
    );
}

fn resume_boundary_row(lines: &[String], needle: &str) -> usize {
    lines
        .iter()
        .position(|line| line.contains(needle))
        .unwrap_or_else(|| panic!("missing row {needle}: {lines:?}"))
}

#[test]
fn tui_slash_resume_pins_persist_reorder_search_and_report_save_failure() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let launch = tempfile::tempdir().unwrap();
    let older = resume_boundary_create_session(&server, home.path(), project.path(),
        "pin older session", "older answer");
    let newer = resume_boundary_create_session(&server, home.path(), project.path(),
        "pin newer session", "newer answer");
    let epoch = std::time::SystemTime::UNIX_EPOCH;
    resume_boundary_set_modified(&resume_boundary_journal(home.path(), &older), epoch + Duration::from_secs(1000));
    resume_boundary_set_modified(&resume_boundary_journal(home.path(), &newer), epoch + Duration::from_secs(2000));
    let original = std::fs::read(resume_boundary_journal(home.path(), &older)).unwrap();

    let mut pty = Pty::start_with_args(launch.path(), home.path(), &[]);
    pty.ready();
    resume_boundary_open(&mut pty);
    pty.wait_for("新会话默认排在旧会话之前", |screen| {
        let text = screen.contents();
        text.lines().any(|line| line.starts_with('›') && line.contains("pin newer session"))
            && text.contains("pin older session")
    });
    pty.input("\x1b[B");
    pty.wait_for("选中旧会话", |screen| screen.contents().lines()
        .any(|line| line.starts_with('›') && line.contains("pin older session")));

    // 写锁失败明确反馈，不提前更新图钉标记、不改变选择；解除锁后可重试。
    let blocker = rusqlite::Connection::open(home.path().join(".pp/agent/history.db")).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
    pty.input("\x10");
    pty.text("Could not save session pin; please retry.");
    assert!(pty.parser.screen().contents().lines().any(|line|
        line.starts_with('›') && line.contains("pin older session") && !line.contains('')));
    blocker.execute_batch("ROLLBACK").unwrap();
    pty.input("\x10");
    pty.wait_for("旧会话置顶并保留选中", |screen| {
        let rows: Vec<_> = screen.contents().lines().filter(|line| line.contains("pin older session") || line.contains("pin newer session")).map(str::to_owned).collect();
        rows.len() == 2 && rows[0].starts_with('›') && rows[0].contains(" pin older session")
    });
    // 置顶不会绕过检索；清空查询后再次按 pin 优先排序。
    pty.input("newer");
    pty.wait_for("查询排除不匹配的置顶会话", |screen| {
        let text = screen.contents();
        text.contains("> newer") && text.contains("pin newer session") && !text.contains("pin older session")
    });
    pty.input("\x15");
    pty.wait_for("清空查询仍保留置顶排序", |screen| screen.contents().lines()
        .any(|line| line.starts_with('›') && line.contains(" pin older session")));
    resume_boundary_close(&mut pty);
    pty.input("/quit\r");
    pty.finish();
    assert_eq!(std::fs::read(resume_boundary_journal(home.path(), &older)).unwrap(), original);
    assert_eq!(std::fs::metadata(resume_boundary_journal(home.path(), &older)).unwrap().modified().unwrap(), epoch + Duration::from_secs(1000));

    // 新进程在当前目录范围也安装持久偏好；取消 pin 后仍选中旧 ID。
    let mut pty = Pty::start_with_args_in_alt_screen(project.path(), home.path(), &["-r"]);
    pty.wait_for("重启后置顶仍在当前目录列表首位", |screen| screen.contents().lines()
        .any(|line| line.starts_with('›') && line.contains(" pin older session")));
    pty.input("\x10");
    pty.wait_for("取消置顶恢复时间排序并保留旧会话选择", |screen| {
        let text = screen.contents();
        let rows: Vec<_> = text.lines().filter(|line| line.contains("pin older session") || line.contains("pin newer session")).collect();
        rows.len() == 2 && rows[0].contains("pin newer session")
            && rows[1].starts_with('›') && rows[1].contains("pin older session") && !rows[1].contains('')
    });
    pty.input("\r");
    pty.wait_for("排序变化后恢复原选中的会话", |screen| {
        !screen.contents().contains("Resume session") && input_is_ready(screen)
            && tui_session_is_locked(home.path(), &older)
    });
    pty.input("/quit\r");
    pty.finish();
    no_more(&server);
}

#[test]
fn tui_slash_resume_locked_session_keeps_current_session_and_cwd() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    let saved = tempfile::tempdir().unwrap();
    let launch = tempfile::tempdir().unwrap();

    let locked_id = resume_boundary_create_session(
        &server,
        home.path(),
        saved.path(),
        "locked session marker",
        "locked answer",
    );
    let locked_journal = resume_boundary_journal(home.path(), &locked_id);
    resume_boundary_set_modified(
        &locked_journal,
        std::time::SystemTime::now() + Duration::from_secs(3600),
    );
    // 另一进程持锁：恢复必须失败且不破坏当前会话。
    let guard = std::fs::File::open(
        home.path().join(".pp/run/sessions").join(format!("{locked_id}.lock")),
    ).unwrap();
    rustix::fs::flock(&guard, rustix::fs::FlockOperation::NonBlockingLockExclusive).unwrap();

    let mut pty = Pty::start_with_args(launch.path(), home.path(), &[]);
    pty.ready();
    pty.input("locked test context\r");
    let (request, _) = pty.request(&server);
    final_reply(request, "locked test answer");
    pty.text("locked test answer");
    pty.ready();

    resume_boundary_open(&mut pty);
    pty.text("locked session marker");
    pty.input("\r");
    let red = vt100::Color::Rgb(224, 108, 117);
    pty.wait_for("持锁会话恢复失败显示错误弹窗", |screen| {
        screen.contents().contains("Failed to resume session:")
            && screen.contents().contains("Resume session (")
    });
    pty.text("Resume session");
    resume_boundary_close(&mut pty);
    pty.wait_for("关闭列表后主消息栏保留红框错误", |screen| {
        notification_bounds(screen, "Error", red).is_some()
    });

    // cwd 未切换：相对路径 write 工具仍落在 launch 目录；上下文保留。
    pty.input("after lock\r");
    let (request, body) = pty.request(&server);
    let body_text = serde_json::to_string(&body).unwrap();
    assert!(body_text.contains("locked test context"));
    assert!(body_text.contains("locked test answer"));
    assert!(body_text.contains("after lock"));
    calls_reply(
        request,
        json!([call(
            "w",
            "write",
            json!({"path":"lock-recovered.txt","content":"kept"})
        )]),
    );
    let (request, body) = pty.request(&server);
    let write_result = result(body["messages"].as_array().unwrap().last().unwrap(), "w");
    assert_eq!(write_result["bytes_written"], 4);
    final_reply(request, "lock recovered");
    pty.text("lock recovered");
    pty.ready();
    assert_eq!(
        std::fs::read_to_string(launch.path().join("lock-recovered.txt")).unwrap(),
        "kept"
    );
    pty.input("/quit\r");
    pty.finish();
    no_more(&server);
    drop(guard);
}

include!("resume_boundaries/navigation_cases.rs");
