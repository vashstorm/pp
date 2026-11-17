use super::*;
use pp_config::config::{IconMode, Theme};

struct WelcomeThemeGuard((Theme, IconMode));

impl WelcomeThemeGuard {
    fn set(theme: Theme, icons: IconMode) -> Self {
        let prev = themes::current();
        themes::set_active(theme, icons);
        Self(prev)
    }
}

impl Drop for WelcomeThemeGuard {
    fn drop(&mut self) {
        themes::set_active(self.0.0, self.0.1);
    }
}

#[test]
fn welcome_separates_shift_enter_from_description_across_breakpoints() {
    let _guard = WelcomeThemeGuard::set(Theme::Default, IconMode::Nerd);
    let mut app = app();

    for (width, height, expected_desc) in [
        (100, 30, "insert a newline"),
        (80, 24, "insert a newline"),
        (60, 30, "insert a newline"),
        (38, 30, "insert a newline"),
        (40, 12, "newline"),
    ] {
        let screen = draw(&mut app, width, height);
        assert!(
            screen.contains(&format!("Shift + Enter  {expected_desc}"))
                || screen.contains(&format!("Shift + Enter {expected_desc}")),
            "Shift + Enter 应与说明保留间隔 ({width}x{height}):\n{screen}"
        );
        assert!(
            !screen.contains(&format!("Shift + Enter{expected_desc}")),
            "Shift + Enter 与说明不得粘连 ({width}x{height})"
        );
    }
}

#[test]
fn welcome_responsive_breakpoints_center_hero_and_preserve_draft() {
    let _guard = WelcomeThemeGuard::set(Theme::Default, IconMode::Nerd);
    let mut app = app();
    app.input.insert_str("welcome draft");

    // 宽屏 100x30：视觉偏上居中，双栏卡片与全部 8 项入口完整展示。
    let wide = draw(&mut app, 100, 30);
    let wide_lines: Vec<&str> = wide.lines().collect();
    let logo_y = wide_lines
        .iter()
        .position(|line| line.contains("▄▄▄██▀▀██"))
        .expect("wide logo");
    assert!(logo_y >= 2, "宽屏品牌区应有顶部呼吸留白: {wide}");
    assert!(wide.contains("Terminal Coding Agent"));
    let card_top_y = wide_lines
        .iter()
        .position(|line| line.contains("Session & workflow"))
        .expect("card header");
    assert!(
        !wide_lines[logo_y..card_top_y]
            .iter()
            .any(|line| line.contains("fixture-cwd")),
        "logo 下方不应重复工作目录:\n{wide}"
    );
    assert!(
        wide_lines
            .iter()
            .any(|line| line.contains("Session & workflow")
                && line.contains("│")
                && line.contains("Keyboard")),
        "宽屏双栏卡片应包含两列标题与竖直分隔线:\n{wide}"
    );
    assert!(wide.contains("welcome draft"));

    // 80x24：两列文案仍完整单行展示。
    let std = draw(&mut app, 80, 24);
    assert!(std.contains("plan, then approve edits"), "{std}");
    assert!(std.contains("cancel the running task"), "{std}");

    // 60x30：退化为单列卡片并保留全部入口。
    let narrow = draw(&mut app, 60, 30);
    let session_y = narrow
        .lines()
        .position(|line| line.contains("Session & workflow"))
        .expect("narrow session");
    let keyboard_y = narrow
        .lines()
        .position(|line| line.contains("Keyboard"))
        .expect("narrow keyboard");
    assert!(session_y < keyboard_y);

    // 38x30：紧凑品牌 + 完整单列提示。
    let compact_tall = draw(&mut app, 38, 30);
    assert_eq!(compact_tall.lines().next().unwrap().trim(), "  PP");
    assert!(compact_tall.contains("Shift + Tab"));

    // 40x12：极矮屏幕保留紧凑品牌、最简提示与草稿。
    let compact_short = draw(&mut app, 40, 12);
    assert_eq!(compact_short.lines().next().unwrap().trim(), "  PP");
    assert!(compact_short.contains("/tips  All shortcuts"));
    assert!(compact_short.contains("welcome draft"));

    // 20x6：极小终端优先保留输入草稿。
    let tiny = draw(&mut app, 20, 6);
    assert!(tiny.contains("welcome draft"));
}

#[test]
fn welcome_uses_theme_palette_card_surface_and_ascii_icons() {
    let _guard = WelcomeThemeGuard::set(Theme::SolarizedLight, IconMode::Ascii);
    let mut app = app();
    let buffer = draw_buffer(&mut app, 100, 30);
    let rows: Vec<String> = (0..30)
        .map(|y| (0..100).map(|x| buffer[(x, y)].symbol()).collect())
        .collect();

    let cell_col = |y: usize, needle: &str| -> u16 {
        let row = &rows[y];
        let byte_idx = row.find(needle).unwrap();
        row[..byte_idx].chars().count() as u16
    };

    let card_row = rows
        .iter()
        .position(|row| row.contains("> Session & workflow") && row.contains("* Keyboard"))
        .expect("ascii section headers");
    let header_x = cell_col(card_row, "> Session & workflow");
    let light_palette = themes::palette();
    assert_eq!(
        buffer[(header_x, card_row as u16)].bg,
        light_palette.welcome_card_bg
    );
    assert_eq!(
        buffer[(header_x, card_row as u16)].fg,
        light_palette.welcome_first
    );

    let desc_row = rows
        .iter()
        .position(|row| row.contains("switch model, keep chat"))
        .expect("description row");
    let desc_x = cell_col(desc_row, "switch model, keep chat");
    assert_eq!(buffer[(desc_x, desc_row as u16)].fg, light_palette.activity);
    assert_eq!(
        buffer[(desc_x, desc_row as u16)].bg,
        light_palette.welcome_card_bg
    );

    let version_row = rows
        .iter()
        .position(|row| row.contains(concat!("v", env!("CARGO_PKG_VERSION"))))
        .expect("version row");
    let version_x = cell_col(version_row, concat!("v", env!("CARGO_PKG_VERSION")));
    assert_eq!(
        buffer[(version_x, version_row as u16)].fg,
        light_palette.welcome_muted
    );
}
