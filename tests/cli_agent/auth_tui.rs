#[test]
fn auth_tui_without_models_opens_browser_masks_key_and_preserves_legacy_file() {
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::tempdir().unwrap();
    let browser_log = home.path().join("browser.log");
    let bin = home.path().join("bin"); std::fs::create_dir(&bin).unwrap();
    for program in ["open", "xdg-open"] {
        let path = bin.join(program);
        std::fs::write(&path, format!("#!/bin/sh\nprintf '%s\\n' \"$1\" >> '{}'\n", browser_log.display())).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let legacy = config_path(home.path(), "auth.json");
    std::fs::write(&legacy, "legacy-private-marker").unwrap();
    let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_pp"));
    command.cwd(home.path()); command.env("HOME", home.path());
    command.env("PATH", format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default()));
    let mut pty = Pty::spawn(command);
    pty.wait_for("无模型配置时保留输入入口", input_is_ready);
    pty.input("/login\r");
    pty.text("Select provider to login");
    pty.wait_for("授权状态完成加载", |screen| !screen.contents().contains("Loading authorization"));
    pty.input("\x1b[B\r");
    pty.text("Paste your Z.AI API key");
    pty.wait_for("浏览器自动打开", |_| std::fs::read_to_string(&browser_log).is_ok_and(|s| s.lines().count() == 1));
    pty.input("\x1b[200~private-api-key-marker\x1b[201~");
    pty.text("••••");
    assert!(!pty.parser.screen().hide_cursor());
    assert!(!String::from_utf8_lossy(&pty.raw).contains("private-api-key-marker"));
    pty.input("\x1bOQ");
    pty.wait_for("F2 再次打开链接", |_| std::fs::read_to_string(&browser_log).is_ok_and(|s| s.lines().count() == 2));
    assert!(std::fs::read_to_string(&browser_log).unwrap().lines().all(|url| url == "https://z.ai/manage-apikey/apikey-list"));
    pty.resize(10, 45); pty.resize(30, 100);
    pty.input("\x1b");
    pty.wait_for("取消授权恢复主界面", |screen| !screen.alternate_screen() && input_is_ready(screen));
    assert!(!config_path(home.path(), "auth.yml").exists());
    assert_eq!(std::fs::read_to_string(legacy).unwrap(), "legacy-private-marker");
    pty.input("/logout\r");
    pty.text("No providers are logged in.");
    pty.input("\x03"); pty.finish();
}

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
