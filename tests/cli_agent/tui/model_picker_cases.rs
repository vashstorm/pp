use super::*;

#[test]
fn tui_roles_configures_model_and_effort_without_switching_current_session() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    let models =
        json!({"providers":{"fixture":slash_provider(&server, "fixture", "completion", json!({
            "one":{"model":"vendor-one"}, "two":{"model":"vendor-two"}
        }))}})
        .to_string();
    std::fs::write(
        config_path(dir.path(), "settings.yml"),
        "default_model: fixture/one\n",
    )
    .unwrap();
    std::fs::write(config_path(dir.path(), "models.yml"), models).unwrap();
    let mut pty = Pty::start_with_existing_files(dir.path());
    pty.ready_model("fixture/one");
    pty.input("/Roles\r");
    pty.wait_for("独立 Roles 双栏面板", |screen| {
        let text = screen.contents();
        screen.alternate_screen()
            && [
                "Model roles",
                "All models",
                "DEFAULT",
                "FAST",
                "SLOW",
                "VISION",
                "PLAN",
                "REVIEW",
            ]
            .into_iter()
            .all(|label| text.contains(label))
    });
    assert_eq!(
        pty.parser.screen().cell(20, 50).unwrap().bgcolor(),
        vt100::Color::Rgb(40, 42, 54)
    );
    pty.resize(6, 30);
    pty.resize(30, 100);
    pty.input("\x1b[C\x1b[B\r");
    pty.text("Select a model for FAST");
    pty.input("two\r");
    pty.text("Choose an effort for this role");
    pty.input("\x1b[B\x1b[B\x1b[B\r");
    pty.text("Role configuration saved");
    let database = rusqlite::Connection::open_with_flags(
        config_path(dir.path(), "agent.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let read_fast_role = || {
        database
            .query_row(
                "SELECT enabled, model_reference, effort FROM model_roles WHERE role = 'fast'",
                [],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .unwrap()
    };
    assert_eq!(
        read_fast_role(),
        (1, "fixture/two".to_string(), "high".to_string())
    );
    pty.input(" ");
    pty.wait_for("取消选中并保存 Role", |screen| {
        screen.contents().contains("0/6")
    });
    assert_eq!(
        read_fast_role(),
        (0, "fixture/two".to_string(), "high".to_string())
    );
    pty.input("\x1b");
    pty.ready_model("fixture/one");
    no_more(&server);
    pty.input("\x03");
    pty.finish();
}

#[test]
fn tui_slash_model_picker_preserves_history() {
    let a = server();
    let b = server();
    let dir = tempfile::tempdir().unwrap();
    let models = json!({"providers": {
            "a": slash_provider(&a, "a", "completion", json!({
                "primary": {"model":"vendor-a"}})),
            "gateway": slash_provider(&b, "gateway", "completion", json!({
                "team/secondary": {"model":"vendor-b"}}))}})
    .to_string();
    std::fs::write(
        config_path(dir.path(), "settings.yml"),
        r#"{"default_model":"a/primary"}"#,
    )
    .unwrap();
    std::fs::write(config_path(dir.path(), "models.yml"), models).unwrap();
    let mut pty = Pty::start_with_existing_files(dir.path());
    pty.ready_model("a/primary");
    pty.input("remember picker alpha\r");
    let (request, body) = pty.request(&a);
    assert_eq!(body["model"], "vendor-a");
    let system = body["messages"][0].clone();
    final_reply(request, "picker alpha retained");
    pty.text("picker alpha retained");
    pty.completed();
    pty.ready_model("a/primary");

    // 用唯一前缀锁定模型命令；模糊匹配下 "/m" 也会命中 /compact。
    pty.input("/mo");
    pty.draft("/mo");
    pty.resize(4, 20);
    pty.resize(30, 100);
    pty.input("\t");
    pty.draft("/model");
    no_more(&a);
    no_more(&b);
    pty.input(" ");
    pty.wait_for("空格展开输入框上方模型菜单", |screen| {
        let text = screen.contents();
        let (cursor_row, cursor_col) = screen.cursor_position();
        text.contains("a/primary")
            && text.contains("gateway/team/secondary")
            && !text.contains("vendor-b")
            && !text.contains("[completion]")
            && text.find('').unwrap() < text.find("a/primary").unwrap()
            && !text.contains("❯ /model")
            && text.contains("a/primary  ")
            && !text.contains("(current)")
            && !text.contains("(default)")
            && !screen.hide_cursor()
            && cursor_col == 2
            && text
                .lines()
                .nth(usize::from(cursor_row))
                .is_some_and(|line| line.starts_with(''))
    });
    // 确认前检查首尾导航、翻页以及窗口缩放。
    pty.input("\x1b[H\x1b[5~\x1b[A\x1b[F\x1b[6~\x1b[B");
    pty.resize(4, 20);
    pty.resize(30, 100);
    pty.text("gateway/team/secondary");
    pty.input("\r");
    pty.ready_model("gateway/team/secondary");
    pty.text("picker alpha retained");
    no_more(&a);
    no_more(&b);

    pty.input("recall picker alpha\r");
    let (request, body) = pty.request(&b);
    assert_eq!(authorization(&request), "Bearer gateway-key");
    assert_eq!(body["model"], "vendor-b");
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 4);
    assert_eq!(messages[0], system);
    slash_assert_message(&messages[1], "user", "remember picker alpha");
    slash_assert_message(&messages[2], "assistant", "picker alpha retained");
    slash_assert_message(&messages[3], "user", "recall picker alpha");
    let history = messages.clone();
    final_reply(request, "picker beta retained");
    pty.text("picker beta retained");
    pty.completed();
    pty.ready_model("gateway/team/secondary");

    // 默认选中当前模型，直接确认不能误选第一项。
    pty.input("/model\r");
    pty.text("a/primary");
    pty.input("\r");
    pty.wait_for("current model confirmation closes picker", |screen| {
        let text = screen.contents();
        input_is_ready(screen)
            && text
                .lines()
                .last()
                .is_some_and(|line| line.contains("gateway/team/secondary"))
            && !text.contains("a/primary")
    });
    pty.draft("");
    no_more(&a);
    no_more(&b);

    pty.input("/model\r");
    pty.text("a/primary");
    pty.input("\x1b[A\x1b");
    pty.draft("");
    pty.ready_model("gateway/team/secondary");
    no_more(&a);
    no_more(&b);
    pty.input("/model a/primary\r");
    pty.ready_model("a/primary");
    pty.input("direct model recall\r");
    let (request, body) = pty.request(&a);
    assert_eq!(authorization(&request), "Bearer a-key");
    assert_eq!(body["model"], "vendor-a");
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(&messages[..history.len()], history.as_slice());
    slash_assert_message(&messages[4], "assistant", "picker beta retained");
    slash_assert_message(&messages[5], "user", "direct model recall");
    assert_eq!(messages.len(), 6);
    final_reply(request, "direct history retained");
    pty.text("direct history retained");
    pty.completed();
    pty.ready_model("a/primary");

    pty.input("/bogus\r");
    pty.text("unknown command; use /help");
    pty.draft("/bogus");
    no_more(&a);
    no_more(&b);
    pty.input("\x7f\x7f\x7f\x7f\x7f\x7f");
    pty.input("\x1b[200~/not-a-command\nordinary pasted task\x1b[201~");
    pty.draft("/not-a-command");
    pty.draft("ordinary pasted task");
    no_more(&a);
    no_more(&b);
    pty.input("\r");
    let (request, body) = pty.request(&a);
    let messages = body["messages"].as_array().unwrap();
    slash_assert_message(
        messages.last().unwrap(),
        "user",
        "/not-a-command\nordinary pasted task",
    );
    assert_eq!(messages.len(), 8);
    final_reply(request, "multiline slash task accepted");
    pty.text("multiline slash task accepted");
    // 模型命令要求任务收尾完成，仅收到正文不能作为处理栅栏。
    pty.completed();
    pty.ready_model("a/primary");

    pty.input("/model\r");
    pty.text("gateway/team/secondary");
    // 菜单不能拦截 Ctrl-C 或妨碍终端恢复。
    pty.input("\x03");
    pty.finish();
    no_more(&a);
    no_more(&b);
}

#[test]
fn tui_model_panel_search_focus_height_and_scroll_keep_query_out_of_history() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    let models: serde_json::Map<String, Value> = (0..25)
        .map(|index| {
            (
                format!("model-{index:02}"),
                json!({"model": format!("vendor-{index:02}")}),
            )
        })
        .collect();
    let config = json!({"providers": {
        "catalog": slash_provider(&server, "catalog", "completion", Value::Object(models))
    }});
    std::fs::write(config_path(dir.path(), "models.yml"), config.to_string()).unwrap();
    let mut pty = Pty::start_with_existing_files(dir.path());
    pty.ready_model("catalog/model-00");
    pty.input("/model ");
    pty.wait_for("独立搜索框聚焦且列表最多十一行", |screen| {
        let text = screen.contents();
        let (row, column) = screen.cursor_position();
        column == 2
            && !screen.hide_cursor()
            && text
                .lines()
                .nth(usize::from(row))
                .is_some_and(|line| line.starts_with(''))
            && text
                .lines()
                .filter(|line| {
                    line.trim_start_matches('❯')
                        .trim_start()
                        .starts_with("catalog/model-")
                })
                .count()
                == 11
            && !text.contains("❯ /model")
            && text.contains("catalog/model-10")
            && !text.contains("catalog/model-11")
    });
    let search_row = pty.parser.screen().cursor_position().0;
    pty.input("\x1b[F");
    pty.text("❯ catalog/model-24");
    pty.wait_for(
        "滚动后仍显示十一项并保留末项选择",
        |screen| {
            let text = screen.contents();
            text.lines()
                .filter(|line| {
                    line.trim_start_matches('❯')
                        .trim_start()
                        .starts_with("catalog/model-")
                })
                .count()
                == 11
                && text.contains("❯ catalog/model-24")
                && !text.lines().any(|line| {
                    line.trim_start_matches('❯')
                        .trim_start()
                        .starts_with("catalog/model-00")
                })
        },
    );
    pty.input("\x1b[200~MoDeL-17\x1b[201~");
    pty.wait_for("搜索仅更新独立输入框与模型候选", |screen| {
        let text = screen.contents();
        let (row, column) = screen.cursor_position();
        row == search_row
            && column == 10
            && text
                .lines()
                .nth(usize::from(row))
                .is_some_and(|line| line.trim_end() == " MoDeL-17")
            && text.contains("❯ catalog/model-17")
            && !text.contains("catalog/model-24")
            && text.lines().any(|line| line.trim_end() == "❯")
    });
    pty.input("\t");
    pty.text(" catalog/model-17");
    no_more(&server);
    pty.resize(6, 20);
    pty.resize(30, 100);
    pty.text(" catalog/model-17");
    assert_eq!(pty.parser.screen().cursor_position().0, search_row);
    pty.input("\r");
    pty.ready_model("catalog/model-17");
    pty.draft("");
    no_more(&server);
    pty.input("verify selected model\r");
    let (request, body) = pty.request(&server);
    assert_eq!(body["model"], "vendor-17");
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2);
    slash_assert_message(&messages[1], "user", "verify selected model");
    final_reply(request, "selection verified");
    pty.text("selection verified");
    pty.ready_model("catalog/model-17");
    pty.input("\x03");
    pty.finish();
    no_more(&server);
}
