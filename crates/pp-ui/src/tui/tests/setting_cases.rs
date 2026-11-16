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
        ("/setting", settings::Tab::General, "[General]"),
        ("/config", settings::Tab::General, "[General]"),
        ("/settings", settings::Tab::General, "[General]"),
        ("/setting roles", settings::Tab::Roles, "[Roles]"),
        ("/config themes", settings::Tab::Themes, "[Themes]"),
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

    // 合并后主菜单只展示 `/setting (config)`，不再单列 `/config`。
    app.insert_input("/".into());
    app.refresh_candidates(true);
    let top_labels: Vec<_> = active_menu(&app)
        .items()
        .iter()
        .map(|item| item.label.as_str())
        .collect();
    assert_eq!(
        top_labels
            .iter()
            .filter(|label| **label == "/setting (config)")
            .count(),
        1
    );
    assert!(!top_labels.contains(&"/setting"));
    assert!(!top_labels.contains(&"/config"));

    // 命令补全支持 `setting` 与 `config`（含前缀与模糊匹配），并补全为 `/setting`。
    for query in ["/set", "/setting", "/settings", "/conf", "/config", "/cfg"] {
        app.clear_input();
        app.insert_input(query.into());
        app.refresh_candidates(true);
        let labels: Vec<_> = active_menu(&app)
            .items()
            .iter()
            .map(|item| item.label.as_str())
            .collect();
        assert_eq!(labels, vec!["/setting (config)"], "{query}");
        app.handle(key(KeyCode::Tab), &mut false);
        assert_eq!(app.input_text(), "/setting", "{query}");
    }

    // `/con` 匹配 `/setting (config)` 与 `/context`，`/co` 同时匹配 `/compact`。
    for (query, expected) in [
        ("/con", vec!["/setting (config)", "/context"]),
        ("/co", vec!["/setting (config)", "/compact", "/context"]),
    ] {
        app.clear_input();
        app.insert_input(query.into());
        app.refresh_candidates(true);
        let co_labels: Vec<_> = active_menu(&app)
            .items()
            .iter()
            .map(|item| item.label.as_str())
            .collect();
        assert_eq!(co_labels, expected, "{query}");
    }

    // Tab 补全支持 `/settings <tab>`、`/setting <tab>` 与 `/config <tab>`。
    for (typed, expected) in [
        ("/settings r", "/settings roles"),
        ("/setting r", "/setting roles"),
        ("/config r", "/config roles"),
    ] {
        app.clear_input();
        app.insert_input(typed.into());
        app.refresh_candidates(true);
        let actions: Vec<_> = active_menu(&app)
            .items()
            .iter()
            .map(|item| item.value)
            .collect();
        assert_eq!(
            actions,
            vec![MenuAction::SettingTab(settings::Tab::Roles)],
            "{typed}"
        );
        app.handle(key(KeyCode::Tab), &mut false);
        assert_eq!(app.input_text(), expected, "{typed}");
    }
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
    app.handle(key(KeyCode::Char('+')), &mut false);
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

    // 数字与 `[/]` 不再切换标签页。
    for code in [
        KeyCode::Char('['),
        KeyCode::Char(']'),
        KeyCode::Char('1'),
        KeyCode::Char('2'),
        KeyCode::Char('3'),
        KeyCode::Char('4'),
    ] {
        app.handle(key(code), &mut false);
        assert_eq!(app.active_settings_tab(), Some(settings::Tab::General));
    }

    // `→` 切到 Roles。
    app.handle(key(KeyCode::Right), &mut false);
    assert_eq!(app.active_settings_tab(), Some(settings::Tab::Roles));

    // 在 Roles 打开模型搜索输入文本或按左右方向键时，不应误切标签页。
    app.handle(key(KeyCode::Tab), &mut false);
    app.handle(key(KeyCode::Enter), &mut false);
    app.handle(key(KeyCode::Char('3')), &mut false);
    app.handle(key(KeyCode::Right), &mut false);
    app.handle(key(KeyCode::Left), &mut false);
    assert_eq!(app.active_settings_tab(), Some(settings::Tab::Roles));
    app.handle(key(KeyCode::Esc), &mut false);
    assert_eq!(app.active_settings_tab(), Some(settings::Tab::Roles));

    // 按 `→` 切到 Themes 并触发 Icon Style 预览变更（默认 Nerd -> Down 预览 ASCII）。
    app.handle(key(KeyCode::Right), &mut false);
    assert_eq!(app.active_settings_tab(), Some(settings::Tab::Themes));
    let bar = draw(&mut app, 110, 30);
    assert!(bar.contains("[Themes]"), "{bar}");
    app.handle(key(KeyCode::Down), &mut false);
    assert_eq!(
        crate::tui::themes::current(),
        (Theme::Default, IconMode::Ascii)
    );

    // 点击顶部标签 `[General]` 离开 Themes：未保存的预览应回滚。
    let first_line = bar.lines().next().unwrap();
    let general_col = first_line.find("General").unwrap() as u16;
    let panel_top_y = bar
        .lines()
        .position(|line| line.contains("[Themes]"))
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

    // `←` 反向循环到 Skills，再 `→` 回到 General。
    app.handle(key(KeyCode::Left), &mut false);
    wait_for_skills(&mut app);
    assert_eq!(app.active_settings_tab(), Some(settings::Tab::Skills));

    app.handle(key(KeyCode::Right), &mut false);
    assert_eq!(app.active_settings_tab(), Some(settings::Tab::General));
}

#[test]
fn setting_tabs_keep_stable_geometry_palette_and_alternate_screen_across_switches() {
    let _guard = ThemeGuard::reset_default();
    let temp = tempfile::tempdir().unwrap();
    let settings_path = temp.path().join("settings.yml");
    let (mut app, _requests, _events) = plan_app();
    app.settings_path = Some(settings_path);
    app.choices = favorite_model_choices();
    app.current_model = Some(app.choices[0].clone());

    app.insert_input("/config".into());
    app.submit(PendingKind::Steering);
    flush_settings(&mut app);

    let mut screens = Vec::new();
    let mut buffers = Vec::new();
    for expected in [
        settings::Tab::General,
        settings::Tab::Roles,
        settings::Tab::Themes,
        settings::Tab::Skills,
    ] {
        if expected != settings::Tab::General {
            app.handle(key(KeyCode::Right), &mut false);
        }
        assert_eq!(app.active_settings_tab(), Some(expected));
        // 即使 Skills 后台回复尚未到达，也必须保持在备用屏幕，不闪回主屏。
        assert!(
            app.needs_alternate(),
            "{expected:?} should stay on alternate screen"
        );
        let buf = draw_buffer(&mut app, 110, 30);
        let screen = draw(&mut app, 110, 30);
        buffers.push((expected, buf));
        screens.push((expected, screen));
    }
    wait_for_skills(&mut app);
    let skills_loaded_screen = draw(&mut app, 110, 30);
    let skills_loaded_buf = draw_buffer(&mut app, 110, 30);

    // 切回 General 时应立即使用缓存快照渲染完整设置列表，不出现空白加载帧。
    app.handle(key(KeyCode::Right), &mut false);
    assert!(app.needs_alternate());
    let general_revisit = draw(&mut app, 110, 30);
    assert!(!general_revisit.contains("Loading settings.yml..."));
    assert!(general_revisit.contains("Edit mode"));
    flush_settings(&mut app);

    // 四个页签的顶部 Tab 文字起始列坐标完全不变，外框和第 2 行分隔线对齐。
    let tab_labels = ["General", "Roles", "Themes", "Skills"];
    let expected_cols: Vec<usize> = tab_labels
        .iter()
        .map(|label| screens[0].1.lines().next().unwrap().find(label).unwrap())
        .collect();
    for (tab, screen) in screens
        .iter()
        .map(|(t, s)| (*t, s.as_str()))
        .chain(std::iter::once((
            settings::Tab::Skills,
            skills_loaded_screen.as_str(),
        )))
    {
        let top_line = screen.lines().next().unwrap();
        let cols: Vec<usize> = tab_labels
            .iter()
            .map(|label| top_line.find(label).unwrap())
            .collect();
        assert_eq!(
            cols, expected_cols,
            "tab header shifted on {tab:?}: {top_line}"
        );
        let sep_line = screen.lines().nth(2).unwrap();
        assert!(
            sep_line.starts_with('│')
                && sep_line.ends_with('│')
                && sep_line[3..sep_line.len() - 3].chars().all(|c| c == '─'),
            "row 2 separator misaligned on {tab:?}: {sep_line}"
        );
    }

    // 四个页签使用一致的外框边框色与背景色。
    let expected_border_fg = buffers[0].1[(0, 5)].fg;
    for (tab, buf) in buffers
        .iter()
        .map(|(t, b)| (*t, b))
        .chain(std::iter::once((settings::Tab::Skills, &skills_loaded_buf)))
    {
        assert_eq!(
            buf[(0, 5)].fg,
            expected_border_fg,
            "border color mismatch on {tab:?}"
        );
    }

    // 四个页签的侧边栏竖线位于完全相同的列坐标。
    let general_pipe = screens[0].1.lines().nth(4).unwrap().find('│').unwrap();
    for (tab, screen) in screens
        .iter()
        .map(|(t, s)| (*t, s.as_str()))
        .chain(std::iter::once((
            settings::Tab::Skills,
            skills_loaded_screen.as_str(),
        )))
    {
        let pipe = screen.lines().nth(4).unwrap().find('│').unwrap();
        assert_eq!(
            pipe, general_pipe,
            "sidebar divider shifted on {tab:?}:\n{screen}"
        );
    }
}

#[test]
fn themes_and_skills_sidebars_switch_categories_and_toggle_by_mouse() {
    let _guard = ThemeGuard::reset_default();
    let (mut app, _requests, _events) = plan_app();

    // 1. Themes 页签：点击左侧 Color Theme 切换步骤，点击列表行预览。
    app.insert_input("/config themes".into());
    app.submit(PendingKind::Steering);
    let screen = draw(&mut app, 110, 30);
    assert!(screen.contains("Icon Style"));
    assert!(screen.contains("Color Theme"));

    let col_of = |screen: &str, row: u16, needle: &str| -> u16 {
        let line = screen.lines().nth(usize::from(row)).unwrap();
        let byte_idx = line.find(needle).unwrap();
        ratatui::text::Line::raw(&line[..byte_idx]).width() as u16
    };

    let color_row = screen
        .lines()
        .position(|line| line.contains("Color Theme"))
        .unwrap() as u16;
    let color_col = col_of(&screen, color_row, "Color Theme");
    app.handle(
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: color_col,
            row: color_row,
            modifiers: KeyModifiers::NONE,
        }),
        &mut false,
    );
    let theme_screen = draw(&mut app, 110, 30);
    assert!(theme_screen.contains("Step 2/2: Color Theme"));
    assert!(theme_screen.contains("Tokyo Night"));

    let tokyo_row = theme_screen
        .lines()
        .position(|line| line.contains("Tokyo Night"))
        .unwrap() as u16;
    let tokyo_col = col_of(&theme_screen, tokyo_row, "Tokyo Night");
    app.handle(
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: tokyo_col,
            row: tokyo_row,
            modifiers: KeyModifiers::NONE,
        }),
        &mut false,
    );
    assert_eq!(
        crate::tui::themes::current(),
        (Theme::TokyoNight, IconMode::Nerd)
    );

    // 2. Skills 页签：按 `→` 切到 Skills，点击左侧 Global management 切换分类，点击开关图标列启停技能。
    app.handle(key(KeyCode::Right), &mut false);
    wait_for_skills(&mut app);
    let mut snapshot = pp_config::skills::Skills::default();
    snapshot.entries.push(pp_config::skills::Skill {
        name: "rust".into(),
        description: "Rust skill".into(),
        path: "/skills/rust/SKILL.md".into(),
        enabled: true,
    });
    app.skill_panel = Some(skills::Panel::new(snapshot));
    let skills_screen = draw(&mut app, 110, 30);
    let global_row = skills_screen
        .lines()
        .position(|line| line.contains("Global management"))
        .unwrap() as u16;
    let global_col = col_of(&skills_screen, global_row, "Global management");
    app.handle(
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: global_col,
            row: global_row,
            modifiers: KeyModifiers::NONE,
        }),
        &mut false,
    );
    let global_screen = draw(&mut app, 110, 30);
    assert!(global_screen.contains("Skills — Global management"));
    let rust_row = global_screen
        .lines()
        .position(|line| line.contains("rust"))
        .unwrap() as u16;
    let switch_col = col_of(&global_screen, rust_row, "\u{f0765}");
    app.handle(
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: switch_col,
            row: rust_row,
            modifiers: KeyModifiers::NONE,
        }),
        &mut false,
    );
    assert!(!app.skill_panel.as_ref().unwrap().entries[0].enabled);
}
