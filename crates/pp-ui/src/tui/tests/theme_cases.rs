use super::*;
use pp_config::config::{IconMode, Theme, read_settings};
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

fn type_command(app: &mut App, text: &str) {
    let mut dirty = false;
    for ch in text.chars() {
        app.handle(key(KeyCode::Char(ch)), &mut dirty);
    }
    app.handle(key(KeyCode::Enter), &mut dirty);
}

#[test]
fn slash_themes_two_step_panel_previews_navigates_and_esc_restores() {
    let _guard = ThemeGuard::reset_default();
    let mut app = app();
    let mut dirty = false;

    app.insert_input("/themes".into());
    app.submit(PendingKind::Steering);
    assert!(app.themes_panel.is_some());
    assert!(app.needs_alternate());

    let step1 = draw(&mut app, 100, 32);
    assert!(step1.contains("Step 1/2: Icon Style"), "{step1}");
    assert!(
        step1.contains("Nerd Font") && step1.contains("ASCII"),
        "{step1}"
    );
    assert_eq!(
        crate::tui::themes::current(),
        (Theme::Default, IconMode::Nerd)
    );

    // Step 1 选择 ASCII 即实时预览图标风格。
    app.handle(key(KeyCode::Down), &mut dirty);
    assert_eq!(
        crate::tui::themes::current(),
        (Theme::Default, IconMode::Ascii)
    );

    // Enter 进入 Step 2：色彩主题。
    app.handle(key(KeyCode::Enter), &mut dirty);
    let step2 = draw(&mut app, 100, 32);
    assert!(step2.contains("Step 2/2: Color Theme"), "{step2}");
    assert!(step2.contains("Tokyo Night"), "{step2}");

    // Down 实时预览下一个主题，Backspace 返回 Step 1 仍保留已选预览。
    app.handle(key(KeyCode::Down), &mut dirty);
    assert_eq!(
        crate::tui::themes::current(),
        (Theme::TokyoNight, IconMode::Ascii)
    );
    app.handle(key(KeyCode::Backspace), &mut dirty);
    let back_to_step1 = draw(&mut app, 100, 32);
    assert!(
        back_to_step1.contains("Step 1/2: Icon Style"),
        "{back_to_step1}"
    );
    assert_eq!(
        crate::tui::themes::current(),
        (Theme::TokyoNight, IconMode::Ascii)
    );

    // Esc 取消并恢复打开面板前的 (Theme, IconMode)。
    app.handle(key(KeyCode::Esc), &mut dirty);
    assert!(app.themes_panel.is_none());
    assert_eq!(
        crate::tui::themes::current(),
        (Theme::Default, IconMode::Nerd)
    );
}

#[test]
fn slash_themes_confirm_persists_appearance_and_preserves_other_tui_settings() {
    let _guard = ThemeGuard::reset_default();
    let dir = tempfile::tempdir().unwrap();
    let settings_path = dir.path().join("settings.yml");
    std::fs::write(&settings_path, "tui:\n  max_fps: 30\n").unwrap();

    let mut app = app();
    app.settings_path = Some(settings_path.clone());
    app.conversation.progress(AgentEvent::ToolStarted {
        id: "r1".into(),
        name: "read".into(),
        arguments: r#"{"path":"src/main.rs"}"#.into(),
    });
    app.conversation.progress(AgentEvent::ToolFinished {
        id: "r1".into(),
        result: serde_json::json!({"ok":true,"source":"src/main.rs","lines_read":10}),
    });

    let mut dirty = false;
    type_command(&mut app, "/themes");
    assert!(app.themes_panel.is_some());

    // Step 1: Nerd -> Ascii -> Enter 进入 Step 2
    app.handle(key(KeyCode::Down), &mut dirty);
    app.handle(key(KeyCode::Enter), &mut dirty);
    // Step 2: Default -> TokyoNight -> Catppuccin -> Enter 确认保存
    app.handle(key(KeyCode::Down), &mut dirty);
    app.handle(key(KeyCode::Down), &mut dirty);
    app.handle(key(KeyCode::Enter), &mut dirty);

    let deadline = Instant::now() + Duration::from_secs(2);
    while app.themes_panel.is_some() && Instant::now() < deadline {
        app.receive();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        app.themes_panel.is_none(),
        "themes worker should finish saving"
    );
    assert_eq!(
        crate::tui::themes::current(),
        (Theme::Catppuccin, IconMode::Ascii)
    );

    let saved = read_settings(&settings_path).unwrap();
    assert_eq!(saved.tui.theme, Theme::Catppuccin);
    assert_eq!(saved.tui.icons, IconMode::Ascii);
    assert_eq!(saved.tui.max_fps, 30);

    let main_screen = draw(&mut app, 100, 30);
    assert!(main_screen.contains("+ Read"), "{main_screen}");
    assert!(main_screen.contains("> · @"), "{main_screen}");
    assert!(!main_screen.contains('\u{f00c}'), "{main_screen}");
}

#[test]
fn slash_themes_completion_and_direct_arguments_update_and_persist() {
    let _guard = ThemeGuard::reset_default();
    let dir = tempfile::tempdir().unwrap();
    let settings_path = dir.path().join("settings.yml");

    let mut app = app();
    app.settings_path = Some(settings_path.clone());
    let mut dirty = false;

    for ch in "/themes a".chars() {
        app.handle(key(KeyCode::Char(ch)), &mut dirty);
    }
    assert_eq!(
        menu_values(&app),
        vec![MenuAction::ThemeIcon(IconMode::Ascii)]
    );

    // Tab 补全 ascii，空格后列出全部 7 个主题颜色而不重复图标项。
    app.handle(key(KeyCode::Tab), &mut dirty);
    app.handle(key(KeyCode::Char(' ')), &mut dirty);
    let values = menu_values(&app);
    assert_eq!(values.len(), Theme::ALL.len());
    assert!(
        values
            .iter()
            .all(|v| matches!(v, MenuAction::ThemeColor(_)))
    );

    app.insert_input("dracula".into());
    app.submit(PendingKind::Steering);
    assert!(app.themes_panel.is_none());
    assert_eq!(
        crate::tui::themes::current(),
        (Theme::Dracula, IconMode::Ascii)
    );

    let deadline = Instant::now() + Duration::from_secs(2);
    while !settings_path.exists() && Instant::now() < deadline {
        app.receive();
        std::thread::sleep(Duration::from_millis(5));
    }
    let saved = read_settings(&settings_path).unwrap();
    assert_eq!(saved.tui.theme, Theme::Dracula);
    assert_eq!(saved.tui.icons, IconMode::Ascii);
}
