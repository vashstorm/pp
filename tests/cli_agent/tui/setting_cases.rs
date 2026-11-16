use super::*;
use pp_config::config::{EditMode, IconMode, Theme, read_settings};

#[test]
fn slow_tui_setting_tabs_persist_general_and_revert_unsaved_theme() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    let mut pty = Pty::start(&server, home.path());
    pty.ready();
    let settings_path = config_path(home.path(), "settings.yml");

    // 1. `/setting` 打开 `[General]`，切换 `edit: replace -> hashline` 并持久化到 settings.yml。
    pty.input("/setting\r");
    pty.text("[General]");
    pty.text("Editing & Tools");
    pty.text("edit");
    pty.input(" ");
    pty.wait_for("edit 更新为 hashline 并写入磁盘", |_| {
        read_settings(&settings_path).is_ok_and(|settings| settings.edit == EditMode::Hashline)
    });

    // 2. 按 `→` 依次切到 Roles、Themes 并预览 ASCII（未保存），`←` 切回 General 后主题预览自动回滚。
    pty.input("\x1b[C");
    pty.text("[Roles]");
    pty.input("\x1b[C");
    pty.text("[Themes]");
    pty.text("Step 1/2: Icon Style");
    pty.input("\x1b[B");
    pty.text("ASCII");
    pty.input("\x1b[D");
    pty.text("[Roles]");
    pty.input("\x1b[D");
    pty.text("[General]");
    let saved = read_settings(&settings_path).unwrap();
    assert_eq!(
        (saved.tui.theme, saved.tui.icons),
        (Theme::Default, IconMode::Nerd)
    );

    // 3. 按 `←` 循环到 Skills，按 `Esc` 关闭统一 Setting 面板，再用 `/config` 别名验证重启后配置保留。
    pty.input("\x1b[D");
    pty.text("[Skills]");
    pty.input("\x1b");
    pty.ready();
    pty.input("\x03");
    pty.finish();

    let mut restarted = Pty::start_with_existing_files(home.path());
    restarted.ready();
    restarted.input("/config\r");
    restarted.text("[General]");
    restarted.text("hashline");
    restarted.input("\x1b");
    restarted.ready();
    // 已移除的 `/subagents` 保留草稿且不打开面板。
    restarted.input("/subagents\r");
    restarted.text("/subagents");
    assert!(!restarted.contents().contains("Agent Hub"));
    restarted.input("\x03");
    restarted.finish();
    no_more(&server);
}
