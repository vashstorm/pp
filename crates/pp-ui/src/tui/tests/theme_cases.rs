use super::*;
use pp_config::config::{IconMode, Theme, ToolStyle, read_settings};
use std::time::{Duration, Instant};

struct ThemeGuard(crate::tui::themes::Appearance);

impl ThemeGuard {
    fn reset_default() -> Self {
        let prev = crate::tui::themes::current_appearance();
        crate::tui::themes::set_active_appearance(
            Theme::Default,
            IconMode::Nerd,
            ToolStyle::Classic,
        );
        Self(prev)
    }
}

impl Drop for ThemeGuard {
    fn drop(&mut self) {
        crate::tui::themes::set_active_appearance(self.0.theme, self.0.icons, self.0.tools);
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
fn slash_themes_categories_preview_independently_and_esc_restores() {
    let _guard = ThemeGuard::reset_default();
    let mut app = app();
    let mut dirty = false;

    type_command(&mut app, "/themes");
    assert!(app.themes_panel.is_some());
    assert!(app.needs_alternate());
    let icons = draw(&mut app, 100, 32);
    assert!(icons.contains("Themes — Icon Style"), "{icons}");
    assert!(icons.contains("Nerd Font") && icons.contains("ASCII"));

    app.handle(key(KeyCode::Down), &mut dirty);
    assert_eq!(
        crate::tui::themes::current(),
        (Theme::Default, IconMode::Ascii)
    );

    // Navigating the category bar to the color page does not carry an unsaved icon preview into it.
    app.handle(key(KeyCode::Tab), &mut dirty);
    app.handle(key(KeyCode::Down), &mut dirty);
    app.handle(key(KeyCode::Enter), &mut dirty);
    let colors = draw(&mut app, 100, 32);
    assert!(colors.contains("Themes — Color Theme"), "{colors}");
    app.handle(key(KeyCode::Down), &mut dirty);
    assert_eq!(
        crate::tui::themes::current(),
        (Theme::TokyoNight, IconMode::Nerd)
    );

    // Returning to the icon category keeps its draft, and the color preview does not affect it.
    app.handle(key(KeyCode::Tab), &mut dirty);
    app.handle(key(KeyCode::Home), &mut dirty);
    app.handle(key(KeyCode::Enter), &mut dirty);
    let icons = draw(&mut app, 100, 32);
    assert!(icons.contains("Themes — Icon Style"), "{icons}");
    assert_eq!(
        crate::tui::themes::current(),
        (Theme::Default, IconMode::Ascii)
    );

    app.handle(key(KeyCode::Esc), &mut dirty);
    assert!(app.themes_panel.is_none());
    assert_eq!(
        crate::tui::themes::current(),
        (Theme::Default, IconMode::Nerd)
    );
}

fn wait_for_theme_save(app: &mut App) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while app.themes_panel.is_some() && Instant::now() < deadline {
        app.receive();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        app.themes_panel.is_none(),
        "themes worker should finish saving"
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

    // The icon category saves directly without entering the color category.
    app.handle(key(KeyCode::Down), &mut dirty);
    app.handle(key(KeyCode::Enter), &mut dirty);
    wait_for_theme_save(&mut app);
    let saved = read_settings(&settings_path).unwrap();
    assert_eq!(
        (saved.tui.theme, saved.tui.icons),
        (Theme::Default, IconMode::Ascii)
    );

    type_command(&mut app, "/themes");
    draw(&mut app, 100, 32);
    // The icon category previews Nerd, then the color category is selected; saving the color must not commit the icon draft.
    app.handle(key(KeyCode::Up), &mut dirty);
    app.handle(key(KeyCode::Tab), &mut dirty);
    app.handle(key(KeyCode::Down), &mut dirty);
    app.handle(key(KeyCode::Enter), &mut dirty);
    app.handle(key(KeyCode::Down), &mut dirty);
    app.handle(key(KeyCode::Down), &mut dirty);
    app.handle(key(KeyCode::Enter), &mut dirty);
    wait_for_theme_save(&mut app);
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
        vec![
            MenuAction::ThemeIcon(IconMode::Ascii),
            MenuAction::ThemeColor(Theme::AyuDark),
        ]
    );
    app.handle(key(KeyCode::Char('s')), &mut dirty);
    assert_eq!(
        menu_values(&app),
        vec![MenuAction::ThemeIcon(IconMode::Ascii)]
    );

    // Tab completes ascii, and after a space every theme color is listed without repeating the icon item.
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

    // Pass the tool card style directly and persist it, keeping the saved color and icon.
    type_command(&mut app, "/themes rail");
    assert!(app.themes_panel.is_none());
    assert_eq!(crate::tui::themes::current_tool_style(), ToolStyle::Rail);
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        app.receive();
        if read_settings(&settings_path).is_ok_and(|s| s.tui.tool_style == ToolStyle::Rail) {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let saved = read_settings(&settings_path).unwrap();
    assert_eq!(saved.tui.theme, Theme::Dracula);
    assert_eq!(saved.tui.icons, IconMode::Ascii);
    assert_eq!(saved.tui.tool_style, ToolStyle::Rail);
}

#[cfg(debug_assertions)]
#[test]
fn mock_test_snapshot_restores_current_source_and_override_selection() {
    use std::sync::{Arc, Mutex};

    let mut app = app();
    let selection = pp_config::model_state::Selection {
        reference: "fixture/override".into(),
        effort: pp_core::model::ThinkLevel::High,
    };
    app.current_source = Some(pp_core::model::ModelSource::Override);
    app.override_selection = Some(selection.clone());
    app.mock_saved = Some(app.take_mock_snapshot());
    app.install_mock_snapshot(pp_mock_ai::MockTestSnapshot {
        current_model: ModelChoice {
            reference: "mock/model".into(),
            model: "model".into(),
            protocol: pp_config::models::Protocol::Completion,
            thinking: pp_config::models::ModelThinking::default(),
            context_window: Some(8192),
        },
        choices: vec![],
        think_level: pp_core::model::ThinkLevel::Medium,
        default_think_level: pp_core::model::ThinkLevel::Medium,
        usage: pp_core::context::ContextUsage {
            estimated_tokens: 100,
            context_window: Some(8192),
            last_request: None,
            session_output_tokens: 0,
            auto_compact: false,
            threshold_percent: 80,
        },
        subagent_control: Arc::new(Mutex::new(None)),
    });
    assert_eq!(app.override_selection, None);
    app.finish_mock_test(
        Ok(pp_mock_ai::MockTestReport { calls: 0 }),
        Duration::from_millis(1),
        chrono::Local::now(),
    );
    assert_eq!(
        app.current_source,
        Some(pp_core::model::ModelSource::Override)
    );
    assert_eq!(app.override_selection, Some(selection));
}
