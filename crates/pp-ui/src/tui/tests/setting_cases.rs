use super::*;
use crate::tui::settings;
use pp_config::config::{EditMode, IconMode, Theme, read_settings};
use std::time::{Duration, Instant};

struct ThemeGuard((Theme, IconMode));

impl ThemeGuard {
    fn reset_default() -> Self {
        let prev = crate::tui::themes::current();
        crate::tui::themes::set_active(Theme::Default, IconMode::Nerd);
        Self(prev)
    }
}

impl Drop for ThemeGuard {
    fn drop(&mut self) {
        crate::tui::themes::set_active(self.0.0, self.0.1);
    }
}

fn flush_settings(app: &mut App) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while app.settings_panel.as_ref().is_some_and(|panel| panel.busy) && Instant::now() < deadline {
        if !app.receive() {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    assert!(
        !app.settings_panel.as_ref().is_some_and(|panel| panel.busy),
        "timed out waiting for settings worker reply"
    );
}

fn wait_for_skills(app: &mut App) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while app.skills_pending && Instant::now() < deadline {
        if !app.receive() {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

#[test]
fn setting_aliases_and_tab_arguments_open_expected_tabs_and_complete() {
    let _guard = ThemeGuard::reset_default();
    let (mut app, _requests, _events) = plan_app();
    app.choices = favorite_model_choices();
    app.current_model = Some(app.choices[0].clone());

    for (cmd, expected_tab, marker) in [
        ("/setting", settings::Tab::General, "[1 General]"),
        ("/config", settings::Tab::General, "[1 General]"),
        ("/settings", settings::Tab::General, "[1 General]"),
        ("/setting roles", settings::Tab::Roles, "[2 Roles]"),
        ("/config themes", settings::Tab::Themes, "[3 Themes]"),
    ] {
        app.insert_input(cmd.into());
        app.submit(PendingKind::Steering);
        assert_eq!(app.active_settings_tab(), Some(expected_tab), "{cmd}");
        assert!(app.needs_alternate());
        let screen = draw(&mut app, 110, 30);
        assert!(screen.contains(marker), "{cmd}:\n{screen}");
        app.handle(key(KeyCode::Esc), &mut false);
        assert_eq!(app.active_settings_tab(), None, "{cmd}");
    }

    // Tab 补全支持 `/setting <tab>` 与 `/config <tab>`。
    app.insert_input("/config r".into());
    app.refresh_candidates(true);
    let actions: Vec<_> = active_menu(&app)
        .items()
        .iter()
        .map(|item| item.value)
        .collect();
    assert_eq!(actions, vec![MenuAction::SettingTab(settings::Tab::Roles)]);
    app.handle(key(KeyCode::Tab), &mut false);
    assert_eq!(app.input_text(), "/config roles");
}

#[test]
fn general_tab_persists_edits_guards_numeric_typing_and_updates_live_state() {
    let temp = tempfile::tempdir().unwrap();
    let settings_path = temp.path().join("settings.yml");
    let (mut app, _requests, _events) = plan_app();
    app.settings_path = Some(settings_path.clone());

    app.insert_input("/setting".into());
    app.submit(PendingKind::Steering);
    flush_settings(&mut app);
    let rendered = draw(&mut app, 110, 30);
    assert!(rendered.contains("Editing & Tools"), "{rendered}");
    assert!(rendered.contains("Read max lines"), "{rendered}");

    // 1. 首项为 `edit`，空格切换为 hashline 并写入磁盘。
    app.handle(key(KeyCode::Char(' ')), &mut false);
    flush_settings(&mut app);
    assert_eq!(
        read_settings(&settings_path).unwrap().edit,
        EditMode::Hashline
    );

    // 2. 移动到第 3 项 `read.maxLines`，进入内联数字编辑并输入包含 `2` `3` 的数字，不应误切标签页。
    app.handle(key(KeyCode::Down), &mut false);
    app.handle(key(KeyCode::Down), &mut false);
    app.handle(key(KeyCode::Enter), &mut false);
    for _ in 0..4 {
        app.handle(key(KeyCode::Backspace), &mut false);
    }
    for ch in ['2', '3', '4'] {
        app.handle(key(KeyCode::Char(ch)), &mut false);
    }
    assert_eq!(app.active_settings_tab(), Some(settings::Tab::General));
    app.handle(key(KeyCode::Enter), &mut false);
    flush_settings(&mut app);
    assert_eq!(read_settings(&settings_path).unwrap().read_max_lines, 234);

    // 3. 切到 `Display & System` 修改 `tui.maxFps` 与 `cost.currency`，验证运行期即时生效。
    app.handle(key(KeyCode::Tab), &mut false);
    for _ in 0..4 {
        app.handle(key(KeyCode::Down), &mut false);
    }
    app.handle(key(KeyCode::Tab), &mut false);
    app.handle(key(KeyCode::Enter), &mut false);
    for _ in 0..3 {
        app.handle(key(KeyCode::Backspace), &mut false);
    }
    for ch in ['4', '5'] {
        app.handle(key(KeyCode::Char(ch)), &mut false);
    }
    app.handle(key(KeyCode::Enter), &mut false);
    flush_settings(&mut app);
    assert_eq!(app.pending_max_fps, Some(45));

    app.handle(key(KeyCode::Down), &mut false);
    app.handle(key(KeyCode::Right), &mut false);
    flush_settings(&mut app);
    assert_ne!(app.cost_display.currency, "USD");
    assert_eq!(
        read_settings(&settings_path).unwrap().cost.currency,
        app.cost_display.currency
    );
}

#[test]
fn setting_tabs_switch_by_keys_click_guard_typing_and_revert_unsaved_theme_preview() {
    let _guard = ThemeGuard::reset_default();
    let (mut app, _requests, _events) = plan_app();
    app.choices = favorite_model_choices();
    app.current_model = Some(app.choices[0].clone());

    app.insert_input("/setting".into());
    app.submit(PendingKind::Steering);
    flush_settings(&mut app);
    assert_eq!(app.active_settings_tab(), Some(settings::Tab::General));

    // `]` 切到 Roles。
    app.handle(key(KeyCode::Char(']')), &mut false);
    assert_eq!(app.active_settings_tab(), Some(settings::Tab::Roles));

    // 在 Roles 打开模型搜索输入数字 `3` 时，不应误切到 Themes。
    app.handle(key(KeyCode::Right), &mut false);
    app.handle(key(KeyCode::Enter), &mut false);
    app.handle(key(KeyCode::Char('3')), &mut false);
    assert_eq!(app.active_settings_tab(), Some(settings::Tab::Roles));
    app.handle(key(KeyCode::Esc), &mut false);
    assert_eq!(app.active_settings_tab(), Some(settings::Tab::Roles));

    // 按 `3` 切到 Themes 并触发 Icon Style 预览变更（默认 Nerd -> Down 预览 ASCII）。
    app.handle(key(KeyCode::Char('3')), &mut false);
    assert_eq!(app.active_settings_tab(), Some(settings::Tab::Themes));
    let bar = draw(&mut app, 110, 30);
    assert!(bar.contains("[3 Themes]"), "{bar}");
    app.handle(key(KeyCode::Down), &mut false);
    assert_eq!(
        crate::tui::themes::current(),
        (Theme::Default, IconMode::Ascii)
    );

    // 点击顶部标签 `[1 General]` 离开 Themes：未保存的预览应回滚。
    let first_line = bar.lines().next().unwrap();
    let general_col = first_line.find("1 General").unwrap() as u16;
    let panel_top_y = bar
        .lines()
        .position(|line| line.contains("[3 Themes]"))
        .unwrap() as u16;
    app.handle(
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: general_col,
            row: panel_top_y,
            modifiers: KeyModifiers::NONE,
        }),
        &mut false,
    );
    assert_eq!(app.active_settings_tab(), Some(settings::Tab::General));
    assert_eq!(
        crate::tui::themes::current(),
        (Theme::Default, IconMode::Nerd)
    );

    // `[` 反向循环到 Skills，再 `Ctrl+Right` 回到 General。
    app.handle(key(KeyCode::Char('[')), &mut false);
    wait_for_skills(&mut app);
    assert_eq!(app.active_settings_tab(), Some(settings::Tab::Skills));

    app.handle(
        Event::Key(KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL)),
        &mut false,
    );
    assert_eq!(app.active_settings_tab(), Some(settings::Tab::General));
}
