#[test]
fn auth_tui_logout_refreshes_models_and_blocks_current_authorization() {
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::tempdir().unwrap();
    let auth_path = config_path(home.path(), "auth.yml");
    let sub_path = config_path(home.path(), "models-sub.yml");
    std::fs::write(&auth_path, "credentials:\n  zai:\n    generation: fixture\n    access_token: private-key-marker\n").unwrap();
    std::fs::write(&sub_path, "providers:\n  zai:\n    auth: zai\n    generation: fixture\n    protocol: completion\n    models:\n      remote:\n        model: remote\n").unwrap();
    for path in [&auth_path, &sub_path] { std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap(); }
    let mut pty = Pty::start_with_existing_files(home.path());
    pty.ready_model("zai/remote");
    pty.input("/logout\r");
    pty.text("Z.AI (GLM Coding Plan) ● logged in (api key)");
    pty.input("\r");
    pty.text("Logged out locally.");
    let database = rusqlite::Connection::open_with_flags(
        config_path(home.path(), "agent.db"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ).unwrap();
    let remaining: i64 = database.query_row("SELECT COUNT(*) FROM auth_credentials", [], |row| row.get(0)).unwrap();
    assert_eq!(remaining, 0);
    assert!(!auth_path.exists());
    pty.input("\x1b"); pty.ready_model("zai/remote");
    pty.input("/model\r");
    pty.text("No matching models");
    pty.input("\x1b");
    pty.wait_for("模型面板退出后再输入任务", |screen| !screen.alternate_screen() && input_is_ready(screen));
    pty.input("authorization must block this request\r");
    pty.text("is not signed in");
    assert!(!String::from_utf8_lossy(&pty.raw).contains("private-key-marker"));
    pty.input("\x03"); pty.finish();
}
