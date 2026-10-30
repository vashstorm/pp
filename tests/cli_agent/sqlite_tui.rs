#[test]
fn sqlite_input_history_survives_restart_with_original_text_and_pages() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let cwd_a = a.path().canonicalize().unwrap().to_string_lossy().into_owned();
    std::fs::write(config_path(home.path(), "settings.yml"), fixture_settings()).unwrap();
    std::fs::write(config_path(home.path(), "models.yml"), fixture_models(&server)).unwrap();
    let original = "sqlite 多行原文\n第二行 retained";
    let mut first = Pty::start_with_args(a.path(), home.path(), &[]);
    first.ready_model("fixture/primary");
    first.input(&format!("\x1b[200~{original}\x1b[201~\r"));
    let (request, body) = first.request(&server);
    assert_eq!(body["messages"].as_array().unwrap().last().unwrap()["content"], original);
    final_reply(request, "sqlite-input-accepted");
    first.text("sqlite-input-accepted");
    first.ready();
    first.input("/help\r");
    first.text("/quit");
    first.input("!printf sqlite-shell\r");
    bang_shell_wait_footer(&mut first, "SQLite shell 正常完成", "Timeout:");
    bang_shell_wait_done(&mut first);
    first.input("\x03");
    first.finish();

    let mut second = Pty::start_with_args(a.path(), home.path(), &[]);
    second.ready_model("fixture/primary");
    second.input("sqlite draft retained");
    second.draft("sqlite draft retained");
    second.input("\x1b[A");
    second.draft("printf sqlite-shell");
    second.input("\x1b[A");
    second.draft("/help");
    second.input("\x1b[A");
    second.wait_for("重启后恢复原始多行输入", |screen| {
        let rows: Vec<_> = screen.rows(0, screen.size().1).collect();
        rows.iter().rposition(|line| line.starts_with('❯')).is_some_and(|index| {
            rows[index].trim_start_matches('❯').trim() == "sqlite 多行原文"
                && rows.get(index + 1).is_some_and(|line| line.trim() == "第二行 retained")
        })
    });
    second.input("\x1b[B\x1b[B\x1b[B");
    second.draft("sqlite draft retained");
    second.input("\x03");
    second.finish();

    let path = config_path(home.path(), "history.db");
    let mut database = rusqlite::Connection::open(&path).unwrap();
    for text in [original, "/help", "!printf sqlite-shell"] {
        let events: i64 = database.query_row(
            "SELECT COUNT(*) FROM input_history WHERE cwd=?1 AND text=?2",
            rusqlite::params![cwd_a, text], |row| row.get(0),
        ).unwrap();
        assert_eq!(events, 1, "已接受输入必须只保存一次");
    }
    {
        let transaction = database.transaction().unwrap();
        for index in 0..130 {
            transaction.execute(
                "INSERT INTO input_history(cwd,text,created_at_ms) VALUES (?1,?2,?3)",
                rusqlite::params![cwd_a,format!("sqlite-page-{index}"),index],
            ).unwrap();
        }
        transaction.commit().unwrap();
    }
    drop(database);
    let mut paged = Pty::start_with_args(a.path(), home.path(), &[]);
    paged.ready_model("fixture/primary");
    paged.input("sqlite draft retained");
    paged.draft("sqlite draft retained");
    paged.input("\x1b[A");
    paged.draft("sqlite-page-129");
    paged.input(&"\x1b[A".repeat(99));
    paged.draft("sqlite-page-30");
    paged.input("\x1b[A");
    paged.draft("sqlite-page-29");
    paged.input(&"\x1b[B".repeat(101));
    paged.draft("sqlite draft retained");
    paged.input("\x03");
    paged.finish();

    let mut isolated = Pty::start_with_args(b.path(), home.path(), &[]);
    isolated.ready_model("fixture/primary");
    isolated.input("\x1b[A/help\r");
    isolated.text("/quit");
    isolated.draft("");
    assert!(!isolated.parser.screen().contents().contains("sqlite-page-"));
    assert!(!isolated.parser.screen().contents().contains("sqlite 多行原文"));
    isolated.input("\x03");
    isolated.finish();
    no_more(&server);
}
