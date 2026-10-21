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

// Short-lived notifications keep later screen assertions from racing the
// popup that a failed or successful resume renders above the input.
fn resume_boundary_quick_notifications(home: &Path) {
    std::fs::write(
        config_path(home, "settings.yml"),
        r#"{"default_model":"fixture/primary","notification_timeout_seconds":1}"#,
    )
    .unwrap();
}

// Drive one real session (completion protocol) in the given cwd under the
// shared HOME and return its saved ID.
fn resume_boundary_create_session_with(
    server: &Server,
    home: &Path,
    cwd: &Path,
    settings: &str,
    models: &str,
    user_message: &str,
    answer: &str,
) -> String {
    std::fs::write(config_path(home, "settings.yml"), settings).unwrap();
    std::fs::write(config_path(home, "models.yml"), models).unwrap();
    let before = resume_boundary_session_ids(home);
    let mut pty = Pty::start_with_args(cwd, home, &[]);
    pty.ready();
    pty.input(&format!("{user_message}\r"));
    let (request, _) = pty.request(server);
    final_reply(request, answer);
    pty.text(answer);
    pty.ready();
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
    server: &Server,
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

// A distinct but structurally valid session ID for crafted journal files.
fn resume_boundary_crafted_id(day: u32, hex: char) -> String {
    format!("2027-08-{day:02}T00-00-00-{}", hex.to_string().repeat(32))
}

// Copy a real journal under a new valid ID (the header is the only record
// that embeds the session ID), yielding an independent resumable session.
fn resume_boundary_clone_session(home: &Path, source: &str, id: &str) {
    let text = std::fs::read_to_string(resume_boundary_journal(home, source)).unwrap();
    let mut lines = text.lines();
    let mut header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    header["id"] = json!(id);
    let rest: Vec<&str> = lines.collect();
    let mut cloned = format!("{header}\n");
    if !rest.is_empty() {
        cloned.push_str(&rest.join("\n"));
        cloned.push('\n');
    }
    std::fs::write(resume_boundary_journal(home, id), cloned).unwrap();
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

fn resume_boundary_row(lines: &[String], needle: &str) -> usize {
    lines
        .iter()
        .position(|line| line.contains(needle))
        .unwrap_or_else(|| panic!("missing row {needle}: {lines:?}"))
}

#[test]
fn tui_slash_resume_lists_all_directories_sorted_by_mtime() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    let alpha = tempfile::tempdir().unwrap();
    let beta = tempfile::tempdir().unwrap();
    let launch = tempfile::tempdir().unwrap();

    let alpha_id = resume_boundary_create_session(
        &server,
        home.path(),
        alpha.path(),
        "project alpha",
        "alpha answer",
    );
    let beta_id = resume_boundary_create_session(
        &server,
        home.path(),
        beta.path(),
        "project beta",
        "beta answer",
    );

    // Fixed, distinct mtimes decide the order without sleeping.
    let alpha_time = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_609_459_200);
    let beta_time = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_612_136_400);
    let alpha_journal = resume_boundary_journal(home.path(), &alpha_id);
    let beta_journal = resume_boundary_journal(home.path(), &beta_id);
    resume_boundary_set_modified(&alpha_journal, alpha_time);
    resume_boundary_set_modified(&beta_journal, beta_time);
    let alpha_before = (
        std::fs::read(&alpha_journal).unwrap(),
        std::fs::metadata(&alpha_journal)
            .unwrap()
            .modified()
            .unwrap(),
    );
    let beta_before = (
        std::fs::read(&beta_journal).unwrap(),
        std::fs::metadata(&beta_journal)
            .unwrap()
            .modified()
            .unwrap(),
    );

    let mut pty = Pty::start_with_args(launch.path(), home.path(), &[]);
    pty.ready();
    resume_boundary_open(&mut pty);
    pty.wait_for("两个目录的会话摘要都列出", |screen| {
        screen.contents().contains("Resume session")
            && screen.contents().contains("project alpha")
            && screen.contents().contains("project beta")
    });
    // 键盘提示行在面板中。
    pty.text("↵ select");
    pty.text("Esc cancel");

    let lines: Vec<String> = pty
        .parser
        .screen()
        .contents()
        .lines()
        .map(str::to_owned)
        .collect();
    let title_row = resume_boundary_row(&lines, "Resume session");
    let search_row = lines.iter().position(|line| line.starts_with('>')).unwrap();
    let beta_row = resume_boundary_row(&lines, "project beta");
    let alpha_row = resume_boundary_row(&lines, "project alpha");
    assert!(title_row < search_row && search_row < beta_row && beta_row < alpha_row);
    assert_eq!(alpha_row - beta_row, 4, "每项包含标题、预览、元数据及空行");
    assert!(lines[beta_row + 1].contains("beta answer"));
    assert!(lines[alpha_row + 1].contains("alpha answer"));
    assert!(lines[beta_row + 2].contains("ago") && lines[beta_row + 2].contains("✓ done"));
    assert!(lines[alpha_row + 2].contains("ago") && lines[alpha_row + 2].contains("✓ done"));
    assert!(!lines.iter().any(|line| line.contains("fixture-model")), "全屏面板隐藏编辑器和状态栏");

    // 列表是只读的：没有模型请求。
    no_more(&server);

    // Esc 取消：欢迎页与空闲输入框恢复。
    pty.input("\x1b");
    pty.wait_for("Esc 关闭面板回到欢迎页", |screen| {
        !screen.contents().contains("Resume session")
            && screen.contents().contains("█▀▀█")
            && input_is_ready(screen)
    });
    no_more(&server);

    pty.input("/quit\r");
    pty.finish();
    no_more(&server);

    // 文件字节与 mtime 不变；launch 会话没有新建日志。
    assert_eq!(
        std::fs::read(&alpha_journal).unwrap(),
        alpha_before.0,
        "alpha journal bytes changed"
    );
    assert_eq!(
        std::fs::metadata(&alpha_journal)
            .unwrap()
            .modified()
            .unwrap(),
        alpha_before.1,
        "alpha journal mtime changed"
    );
    assert_eq!(std::fs::read(&beta_journal).unwrap(), beta_before.0);
    assert_eq!(
        std::fs::metadata(&beta_journal)
            .unwrap()
            .modified()
            .unwrap(),
        beta_before.1
    );
    let mut expected = vec![alpha_id.clone(), beta_id.clone()];
    expected.sort();
    assert_eq!(resume_boundary_session_ids(home.path()), expected);
}

#[test]
fn tui_slash_resume_orders_equal_mtime_sessions_by_id_descending() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let launch = tempfile::tempdir().unwrap();

    let alpha_id = resume_boundary_create_session(
        &server,
        home.path(),
        project.path(),
        "tie alpha input",
        "tie alpha answer",
    );
    let beta_id = resume_boundary_create_session(
        &server,
        home.path(),
        project.path(),
        "tie beta input",
        "tie beta answer",
    );
    let tie_time = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_609_459_200);
    resume_boundary_set_modified(&resume_boundary_journal(home.path(), &alpha_id), tie_time);
    resume_boundary_set_modified(&resume_boundary_journal(home.path(), &beta_id), tie_time);

    let (first_summary, second_summary) = if alpha_id > beta_id {
        ("tie alpha input", "tie beta input")
    } else {
        ("tie beta input", "tie alpha input")
    };

    let mut pty = Pty::start_with_args(launch.path(), home.path(), &[]);
    pty.ready();
    resume_boundary_open(&mut pty);
    pty.wait_for("两条摘要都列出", |screen| {
        screen.contents().contains("tie alpha input")
            && screen.contents().contains("tie beta input")
    });
    let lines: Vec<String> = pty
        .parser
        .screen()
        .contents()
        .lines()
        .map(str::to_owned)
        .collect();
    let first_row = resume_boundary_row(&lines, first_summary);
    let second_row = resume_boundary_row(&lines, second_summary);
    assert!(
        first_row < second_row,
        "equal mtime must fall back to ID descending:\n{:?}",
        lines
    );
    assert!(lines[first_row + 2].contains("ago"));
    assert_eq!(lines[first_row + 2].trim_start_matches(' ').split("  ·").next(),
        lines[second_row + 2].trim_start_matches(' ').split("  ·").next());
    resume_boundary_close(&mut pty);
    pty.input("/quit\r");
    pty.finish();
    no_more(&server);
}

#[test]
fn tui_slash_resume_truncates_long_first_user_message_summary() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let launch = tempfile::tempdir().unwrap();
    std::fs::write(
        config_path(home.path(), "settings.yml"),
        fixture_settings(),
    )
    .unwrap();
    std::fs::write(
        config_path(home.path(), "models.yml"),
        fixture_models(&server),
    )
    .unwrap();
    let before = resume_boundary_session_ids(home.path());

    // 多行、连续空白与超长中文：摘要折叠空白并截断到 80 个字符加省略号。
    let message = format!("首行\n\n接续  {}\n尾部标记词", "端".repeat(90));
    let mut saved = Pty::start_with_args(project.path(), home.path(), &[]);
    saved.ready();
    saved.input(&format!("\x1b[200~{message}\x1b[201~"));
    saved.input("\r");
    let (request, _) = saved.request(&server);
    final_reply(request, "truncation answer");
    saved.text("truncation answer");
    saved.ready();
    saved.input("/quit\r");
    saved.finish();
    no_more(&server);
    let new: Vec<String> = resume_boundary_session_ids(home.path())
        .into_iter()
        .filter(|id| !before.contains(id))
        .collect();
    assert_eq!(
        new.len(),
        1,
        "expected exactly one new session, got {new:?}"
    );

    let expected_summary = format!("首行 接续 {}…", "端".repeat(74));
    let mut pty = Pty::start_with_args(launch.path(), home.path(), &[]);
    pty.ready();
    // 宽屏保证整行摘要可见，截断由列表逻辑而非窄屏裁切决定。
    pty.resize(40, 400);
    resume_boundary_open(&mut pty);
    pty.wait_for("折叠并截断后的摘要", |screen| {
        screen.contents().contains(&expected_summary)
    });
    let text = pty.parser.screen().contents();
    assert!(
        !text.contains("尾部标记词"),
        "81st char onward must not leak"
    );
    assert!(
        !text.contains(&"端".repeat(81)),
        "summary must stop at 80 characters"
    );
    resume_boundary_close(&mut pty);
    pty.input("/quit\r");
    pty.finish();
    no_more(&server);
}

#[test]
fn tui_slash_resume_empty_state_without_sessions_directory() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    let launch = tempfile::tempdir().unwrap();
    std::fs::write(
        config_path(home.path(), "settings.yml"),
        fixture_settings(),
    )
    .unwrap();
    std::fs::write(
        config_path(home.path(), "models.yml"),
        fixture_models(&server),
    )
    .unwrap();

    let mut pty = Pty::start_with_args(launch.path(), home.path(), &[]);
    pty.ready();
    resume_boundary_open(&mut pty);
    pty.text("No saved sessions found.");
    pty.input("\x1b");
    pty.wait_for("Esc 关闭面板回到欢迎页", |screen| {
        !screen.contents().contains("Resume session")
            && screen.contents().contains("█▀▀█")
            && input_is_ready(screen)
    });
    no_more(&server);
    pty.input("/quit\r");
    pty.finish();
    no_more(&server);
}

#[test]
fn tui_slash_resume_ignores_invalid_session_entries() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let launch = tempfile::tempdir().unwrap();

    let clean_id = resume_boundary_create_session(
        &server,
        home.path(),
        project.path(),
        "clean valid session",
        "clean answer",
    );
    let half_id = resume_boundary_create_session(
        &server,
        home.path(),
        project.path(),
        "half tail session",
        "half answer",
    );
    let dir = resume_boundary_sessions_dir(home.path());

    // 无换行的最后半条记录：不影响列出。
    {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(resume_boundary_journal(home.path(), &half_id))
            .unwrap();
        file.write_all(b"{\"type\":\"custom\",\"customType\":\"pp.native_append\",\"data\":")
            .unwrap();
    }
    // 非法 ID、错误后缀、坏头、目录与符号链接都被忽略。
    std::fs::write(dir.join("not-a-valid-id.jsonl"), "garbage\n").unwrap();
    std::fs::write(dir.join("ignored-suffix.txt"), "garbage\n").unwrap();
    std::fs::write(
        dir.join(format!("{}.jsonl", resume_boundary_crafted_id(1, 'c'))),
        "not a session header\n",
    )
    .unwrap();
    std::fs::create_dir(dir.join(format!("{}.jsonl", resume_boundary_crafted_id(2, 'd')))).unwrap();
    std::os::unix::fs::symlink(
        resume_boundary_journal(home.path(), &clean_id),
        dir.join(format!("{}.jsonl", resume_boundary_crafted_id(3, 'e'))),
    )
    .unwrap();
    // 只选干净会话：设为最新，默认选中第一项。
    resume_boundary_set_modified(
        &resume_boundary_journal(home.path(), &clean_id),
        std::time::SystemTime::now() + Duration::from_secs(3600),
    );
    resume_boundary_quick_notifications(home.path());

    let mut pty = Pty::start_with_args(launch.path(), home.path(), &[]);
    pty.ready();
    resume_boundary_open(&mut pty);
    pty.wait_for(
        "两条有效摘要都在，坏文件不拖垮列表",
        |screen| {
            screen.contents().contains("clean valid session")
                && screen.contents().contains("half tail session")
                && !screen.contents().contains("No saved sessions found.")
        },
    );
    pty.input("\r");
    pty.wait_for("恢复干净会话", |screen| {
        screen.contents().contains("Resumed session:")
            && screen.contents().contains("clean valid session")
            && screen.contents().contains("clean answer")
    });
    pty.ready();

    // 恢复的会话可继续对话。
    pty.input("after mixed\r");
    let (request, body) = pty.request(&server);
    let body = serde_json::to_string(&body).unwrap();
    assert!(body.contains("clean valid session"));
    assert!(body.contains("after mixed"));
    final_reply(request, "mixed recovered");
    pty.text("mixed recovered");
    pty.ready();
    pty.input("/quit\r");
    pty.finish();
    no_more(&server);

    // 无效文件未被清理，干净日志继续追加。
    assert!(dir.join("not-a-valid-id.jsonl").exists());
    assert!(dir.join("ignored-suffix.txt").exists());
    let journal = std::fs::read_to_string(resume_boundary_journal(home.path(), &clean_id)).unwrap();
    assert!(journal.contains("after mixed"));
    let _ = half_id;
}

#[test]
fn tui_slash_resume_corrupted_native_record_keeps_current_session() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    let saved = tempfile::tempdir().unwrap();
    let launch = tempfile::tempdir().unwrap();

    let broken_id = resume_boundary_create_session(
        &server,
        home.path(),
        saved.path(),
        "corrupt session marker",
        "corrupt answer",
    );
    let broken_journal = resume_boundary_journal(home.path(), &broken_id);
    // 首条用户消息之后的完整坏行：列表不预检，恢复时由 load 拒绝。
    {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&broken_journal)
            .unwrap();
        file.write_all(b"this line is not json\n").unwrap();
    }
    resume_boundary_set_modified(
        &broken_journal,
        std::time::SystemTime::now() + Duration::from_secs(3600),
    );
    let before = std::fs::read(&broken_journal).unwrap();
    resume_boundary_quick_notifications(home.path());

    let mut pty = Pty::start_with_args(launch.path(), home.path(), &[]);
    pty.ready();
    pty.input("current context marker\r");
    let (request, _) = pty.request(&server);
    final_reply(request, "current context answer");
    pty.text("current context answer");
    pty.ready();

    resume_boundary_open(&mut pty);
    pty.text("corrupt session marker");
    pty.input("\r");
    let red = vt100::Color::Rgb(224, 108, 117);
    pty.wait_for("恢复失败显示错误弹窗", |screen| {
        notification_bounds(screen, "Error", red).is_some()
            && !screen.contents().contains("Resumed session:")
    });
    // 失败保留列表，可取消重选。
    pty.text("Resume session");
    resume_boundary_close(&mut pty);
    pty.wait_for("错误弹窗淡出", |screen| {
        notification_bounds(screen, "Error", red).is_none()
    });

    // 取消后旧对话完整，只出现一次。
    let text = pty.parser.screen().contents();
    assert_eq!(text.matches("current context marker").count(), 1);
    assert_eq!(text.matches("current context answer").count(), 1);

    // 旧会话可继续发起下一轮，上下文保留。
    pty.input("after corrupt failure\r");
    let (request, body) = pty.request(&server);
    let body = serde_json::to_string(&body).unwrap();
    assert!(body.contains("current context marker"));
    assert!(body.contains("current context answer"));
    assert!(body.contains("after corrupt failure"));
    final_reply(request, "corrupt recovered");
    pty.text("corrupt recovered");
    pty.ready();
    pty.input("/quit\r");
    pty.finish();
    no_more(&server);
    // 坏文件未被触碰。
    assert_eq!(std::fs::read(&broken_journal).unwrap(), before);
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
    let guard = std::fs::File::open(&locked_journal).unwrap();
    rustix::fs::flock(&guard, rustix::fs::FlockOperation::NonBlockingLockExclusive).unwrap();
    resume_boundary_quick_notifications(home.path());

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
        notification_bounds(screen, "Error", red).is_some()
            && !screen.contents().contains("Resumed session:")
    });
    pty.text("Resume session");
    resume_boundary_close(&mut pty);
    pty.wait_for("错误弹窗淡出", |screen| {
        notification_bounds(screen, "Error", red).is_none()
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

#[test]
fn tui_slash_resume_missing_saved_cwd_still_restores_history() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    let launch = tempfile::tempdir().unwrap();

    let gone_id = {
        let saved = tempfile::tempdir().unwrap();
        let id = resume_boundary_create_session(
            &server,
            home.path(),
            saved.path(),
            "gone cwd marker",
            "gone answer",
        );
        drop(saved);
        id
    };
    resume_boundary_set_modified(
        &resume_boundary_journal(home.path(), &gone_id),
        std::time::SystemTime::now() + Duration::from_secs(3600),
    );
    resume_boundary_quick_notifications(home.path());

    let mut pty = Pty::start_with_args(launch.path(), home.path(), &[]);
    pty.ready();
    resume_boundary_open(&mut pty);
    pty.text("gone cwd marker");
    pty.input("\r");
    resume_history_wait_resumed(&mut pty, &gone_id, &["gone cwd marker", "gone answer"]);

    // 原目录已删除不影响恢复，下一轮仍携带旧对话。
    pty.input("still here\r");
    let (request, body) = pty.request(&server);
    let body = serde_json::to_string(&body).unwrap();
    assert!(body.contains("still here"));
    assert!(body.contains("gone cwd marker"));
    assert!(body.contains("gone answer"));
    final_reply(request, "still answered");
    pty.text("still answered");
    pty.ready();
    pty.input("/quit\r");
    pty.finish();
    no_more(&server);
}

#[test]
fn tui_slash_resume_selecting_current_session_is_a_noop() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    let launch = tempfile::tempdir().unwrap();
    resume_boundary_quick_notifications(home.path());
    std::fs::write(
        config_path(home.path(), "models.yml"),
        fixture_models(&server),
    )
    .unwrap();

    let mut pty = Pty::start_with_args(launch.path(), home.path(), &[]);
    pty.ready();
    pty.input("current session marker\r");
    let (request, _) = pty.request(&server);
    final_reply(request, "current session answer");
    pty.text("current session answer");
    pty.ready();
    let ids = resume_boundary_session_ids(home.path());
    assert_eq!(ids.len(), 1, "expected exactly one session: {ids:?}");
    let id = ids[0].clone();

    resume_boundary_open(&mut pty);
    // 当前会话最新，默认选中第一项；选择它只关闭列表。
    pty.text("current session marker");
    pty.input("\r");
    pty.wait_for("面板关闭且对话保持原样", |screen| {
        !screen.contents().contains("Resume session")
            && screen.contents().contains("current session marker")
            && screen.contents().contains("current session answer")
    });
    let text = pty.parser.screen().contents();
    assert!(
        !text.contains("Resumed session:"),
        "current-session selection must not announce a resume"
    );
    assert_eq!(text.matches("current session marker").count(), 1);
    no_more(&server);

    // 输入历史保留（两条已提交输入，最旧的是首条消息）。
    pty.input("\x1b[A\x1b[A");
    pty.draft("current session marker");
    pty.input("\x01\x0b");
    pty.draft("");

    // 后续输入追加到原会话。
    pty.input("current follow up\r");
    let (request, body) = pty.request(&server);
    let body = serde_json::to_string(&body).unwrap();
    assert!(body.contains("current session marker"));
    assert!(body.contains("current session answer"));
    assert!(body.contains("current follow up"));
    final_reply(request, "current follow answer");
    pty.text("current follow answer");
    pty.ready();
    pty.input("/quit\r");
    pty.finish();
    no_more(&server);
    assert_eq!(resume_boundary_session_ids(home.path()), [id.as_str()]);
    let journal = std::fs::read_to_string(resume_boundary_journal(home.path(), &id)).unwrap();
    assert!(journal.contains("current follow up"));
}

#[test]
fn tui_slash_resume_interrupted_session_replays_partial_text_once() {
    for protocol in ["completion", "response"] {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let path = api_path(protocol);
        resume_boundary_quick_notifications(home.path());
        std::fs::write(
            config_path(home.path(), "models.yml"),
            response_models(&server, Some(protocol)),
        )
        .unwrap();

        // 传输完成但协议轮次中断：保留部分展示文字，无原生提交。
        let mut interrupted = Pty::start_with_existing_files(home.path());
        interrupted.ready();
        interrupted.input("interrupted resume input\r");
        let (request, _) = receive_at(&server, path);
        let mut stream = SseReply::new(request);
        streaming_delta(&mut stream, protocol, "partial-resume-marker");
        interrupted.text("partial-resume-marker");
        drop(interrupted);
        drop(stream);
        let ids = resume_boundary_session_ids(home.path());
        assert_eq!(ids.len(), 1, "expected exactly one session: {ids:?}");

        let mut pty = Pty::start_with_args(home.path(), home.path(), &[]);
        pty.ready();
        resume_boundary_open(&mut pty);
        pty.text("interrupted resume input");
        pty.input("\r");
        let red = vt100::Color::Rgb(224, 108, 117);
        pty.wait_for("中断会话恢复显示错误弹窗", |screen| {
            notification_bounds(screen, "Error", red).is_some()
                && screen
                    .contents()
                    .contains("Previous request was interrupted.")
        });
        pty.ready();
        pty.wait_for("错误弹窗淡出", |screen| {
            notification_bounds(screen, "Error", red).is_none()
        });
        let text = pty.parser.screen().contents();
        assert_eq!(text.matches("interrupted resume input").count(), 1);
        assert_eq!(text.matches("partial-resume-marker").count(), 1);
        no_more(&server);

        // 未提交的 turn 不进入后续原生请求。
        pty.input("after interrupt\r");
        let (request, body) = pty.request_at(&server, path);
        let body = serde_json::to_string(&body).unwrap();
        assert!(body.contains("interrupted resume input"));
        assert!(body.contains("after interrupt"));
        assert!(
            !body.contains("partial-resume-marker"),
            "partial display text must not become model context"
        );
        if protocol == "completion" {
            final_reply(request, "post interrupt answer");
        } else {
            response_final(request, "post interrupt answer");
        }
        pty.text("post interrupt answer");
        pty.ready();
        pty.input("/quit\r");
        pty.finish();
        no_more(&server);
    }
}

#[test]
fn tui_slash_resume_navigates_beyond_viewport_and_survives_resize() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let launch = tempfile::tempdir().unwrap();

    let base_id = resume_boundary_create_session(
        &server,
        home.path(),
        project.path(),
        "nav session marker",
        "nav answer",
    );
    let clones: Vec<String> = (1..=5)
        .map(|day| resume_boundary_crafted_id(day, '1'))
        .collect();
    for id in &clones {
        resume_boundary_clone_session(home.path(), &base_id, id);
    }
    // 屏幕顺序（新→旧）：c5, c4, c3, c2, c1, base。
    let hour = Duration::from_secs(3600);
    let base_time = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_609_459_200);
    resume_boundary_set_modified(&resume_boundary_journal(home.path(), &base_id), base_time);
    for (index, id) in clones.iter().enumerate() {
        resume_boundary_set_modified(
            &resume_boundary_journal(home.path(), id),
            base_time + hour * (index as u32 + 1),
        );
    }

    let mut pty = Pty::start_with_args(launch.path(), home.path(), &[]);
    pty.ready();
    // 小视口使 6 项列表超过一页。
    pty.resize(10, 100);
    resume_boundary_open(&mut pty);
    pty.wait_for("小视口下的会话面板", |screen| {
        screen.contents().contains("Resume session")
            && screen.contents().contains("nav session marker")
    });
    // PgDn 翻页后 End 选到最后一项（最旧的 base）。
    pty.input("\x1b[6~");
    pty.input("\x1b[F");
    pty.input("\r");
    pty.wait_for("恢复最旧会话", |screen| {
        let text = screen.contents();
        text.contains("Resumed session:")
            && text
                .split_whitespace()
                .collect::<String>()
                .contains(&base_id)
    });
    pty.ready();
    no_more(&server);

    // 重新打开：End 到最后一项（当前会话），放大窗口后选择必须保留，
    // 再按一次 Up 应选中倒数第二项 c1 而不是重置到第一项。
    pty.input("/resume\r");
    pty.text("No sessions in current folder.");
    pty.input("\x1b[C");
    pty.wait_for("面板重新打开", |screen| {
        screen.contents().contains("Resume session")
            && screen.contents().contains("nav session marker")
    });
    pty.input("\x1b[F");
    pty.resize(30, 100);
    pty.text("Resume session");
    pty.input("\x1b[A");
    pty.input("\r");
    let second_oldest = clones[0].clone();
    pty.wait_for("resize 后恢复倒数第二项", |screen| {
        let text = screen.contents();
        text.contains("Resumed session:")
            && text
                .split_whitespace()
                .collect::<String>()
                .contains(&second_oldest)
    });
    pty.ready();
    pty.input("/quit\r");
    pty.finish();
    no_more(&server);
}

#[test]
fn tui_slash_resume_while_busy_does_not_switch_or_request() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    let launch = tempfile::tempdir().unwrap();
    std::fs::write(
        config_path(home.path(), "settings.yml"),
        fixture_settings(),
    )
    .unwrap();
    std::fs::write(
        config_path(home.path(), "models.yml"),
        fixture_models(&server),
    )
    .unwrap();

    let mut pty = Pty::start_with_args(launch.path(), home.path(), &[]);
    pty.ready();
    pty.input("busy resume context\r");
    let (request, _) = pty.request(&server);
    // 模型运行中提交 /resume：不打开面板、不切换。
    pty.input("/resume\r");
    // A subsequent paste must reach the editor, fencing processing of Enter.
    // If /resume opened a modal panel, this paste would be consumed instead.
    pty.input("\x1b[200~ after-busy-submit\x1b[201~");
    pty.draft("/resume after-busy-submit");
    assert!(
        !pty.parser.screen().contents().contains("Resume session"),
        "panel must not open while the model is running\n{}",
        pty.diagnostics()
    );
    no_more(&server);
    final_reply(request, "busy resume answer");
    pty.text("busy resume answer");
    pty.ready();
    // 空闲后同一命令可用。
    pty.input("\x01\x0b");
    pty.draft("");
    resume_boundary_open(&mut pty);
    resume_boundary_close(&mut pty);
    pty.input("/quit\r");
    pty.finish();
    no_more(&server);
}

#[test]
fn tui_slash_resume_mcp_servers_reap_after_cross_session_exit() {
    for phase in ["idle", "model", "tool"] {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let launch = tempfile::tempdir().unwrap();
        let mut settings = super::mcp::settings("call_timeout");
        settings["default_model"] = json!("fixture/primary");
        settings["notification_timeout_seconds"] = json!(1);
        settings["mcp_servers"]["local"]["timeout_seconds"] = json!(30);
        resume_boundary_create_session_with(
            &server,
            home.path(),
            project.path(),
            &settings.to_string(),
            &fixture_models(&server),
            "mcp project marker",
            "mcp project answer",
        );

        let mut pty = Pty::start_with_args(launch.path(), home.path(), &[]);
        pty.ready();
        resume_boundary_open(&mut pty);
        pty.text("mcp project marker");
        pty.input("\r");
        pty.wait_for("跨目录恢复保存会话", |screen| {
            screen.contents().contains("Resumed session:")
                && screen.contents().contains("mcp project marker")
                && screen.contents().contains("mcp project answer")
        });
        pty.ready();

        let mut pending = None;
        if phase != "idle" {
            pty.input("mcp phase\r");
            let (request, _) = pty.request(&server);
            if phase == "tool" {
                calls_reply(
                    request,
                    json!([call("pending", "mcp__local__echo", json!({"text":"hello"}))]),
                );
                pty.text("mcp__local__echo");
            } else {
                pending = Some(request);
            }
        }
        pty.input("\u{3}");
        pty.finish();
        // 恢复沿用当前目录的 MCP 连接，退出后进程全部回收。
        super::mcp::assert_stopped(project.path());
        super::mcp::assert_stopped(launch.path());
        drop(pending);
    }
}

#[test]
fn tui_slash_resume_filters_current_folder_searches_and_deletes_saved_session() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    let saved = tempfile::tempdir().unwrap();
    let launch = tempfile::tempdir().unwrap();
    let id = resume_boundary_create_session(&server, home.path(), saved.path(),
        "可删除的旧会话", "saved preview marker");
    let journal = resume_boundary_journal(home.path(), &id);
    let original = std::fs::read(&journal).unwrap();
    resume_boundary_quick_notifications(home.path());
    let mut pty = Pty::start_with_args(launch.path(), home.path(), &[]);
    pty.ready();
    pty.input("current scope marker\r");
    let (request, _) = pty.request(&server);
    final_reply(request, "current scope answer");
    pty.text("current scope answer");
    pty.ready();
    let current_id = resume_boundary_session_ids(home.path()).into_iter().find(|candidate| candidate != &id).unwrap();

    pty.input("/resume\r");
    pty.wait_for("当前目录仅显示当前会话和状态", |screen| {
        let text = screen.contents();
        text.contains("Resume session (current folder)") && text.contains("current scope marker")
            && text.contains("current  ·  ✓ done") && !text.contains("saved preview marker")
    });
    pty.input("\x1b[3~");
    pty.input("\r");
    pty.wait_for("当前会话不能删除且仍可选中关闭", |screen| {
        !screen.contents().contains("Resume session") && input_is_ready(screen)
    });
    assert!(resume_boundary_journal(home.path(), &current_id).exists());

    pty.input("/resume\r");
    pty.text("current  ·  ✓ done");
    pty.input("\x1b[C");
    pty.text("saved preview marker");
    pty.input("\x1b[200~PREVIEW marker\x1b[201~");
    pty.wait_for("按回答预览搜索，大小写不敏感且多个词均需命中", |screen| {
        let text = screen.contents();
        text.contains("> PREVIEW marker") && text.contains("可删除的旧会话")
            && !text.contains("current scope marker")
    });
    pty.input("\x1b[3~");
    pty.text("Delete selected session?");
    pty.input("\x1b");
    pty.wait_for("取消删除保留查询和条目", |screen| {
        let text = screen.contents();
        !text.contains("Delete selected session?") && text.contains("可删除的旧会话")
            && text.contains("> PREVIEW marker")
    });
    assert_eq!(std::fs::read(&journal).unwrap(), original);
    pty.input("\x1b[3~");
    pty.text("Delete selected session?");
    pty.input("y");
    pty.text("No matching sessions.");
    assert!(!journal.exists());
    no_more(&server);
    resume_boundary_close(&mut pty);
    pty.text("current scope answer");
    pty.input("still in current session\r");
    let (request, body) = pty.request(&server);
    assert!(body.to_string().contains("current scope marker"));
    final_reply(request, "current session preserved");
    pty.text("current session preserved");
    pty.ready();
    pty.input("/quit\r");
    pty.finish();
    assert_eq!(resume_boundary_session_ids(home.path()), [current_id]);
}
