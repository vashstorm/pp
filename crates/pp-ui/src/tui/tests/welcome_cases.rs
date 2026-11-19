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
    assert!(wide.contains("Paper Plane"));
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

#[test]
fn welcome_widens_tips_card_on_wide_screens_and_keeps_logo_aligned() {
    let _guard = WelcomeThemeGuard::set(Theme::Default, IconMode::Nerd);
    let mut app = app();

    // 宽屏下卡片按 75% 终端宽自适应（下限 84，上限 120）。
    for (term_w, expected_card_w) in [(100, 84), (120, 90), (160, 120), (200, 120)] {
        let screen = draw(&mut app, term_w, 30);
        let rule_len = screen
            .lines()
            .filter_map(|line| {
                let trimmed = line.trim();
                (!trimmed.is_empty() && trimmed.chars().all(|ch| ch == '─'))
                    .then(|| trimmed.chars().count())
            })
            .next()
            .expect("tips 卡片应含水平边框");
        assert_eq!(
            rule_len, expected_card_w,
            "终端宽度 {term_w} 应渲染 {expected_card_w} 列卡片:\n{screen}"
        );
    }

    // 副标题比 WORDMARK 更短时，WORDMARK、Paper Plane 与版本号右列起始列仍对齐。
    let wide = draw(&mut app, 160, 30);
    let lines: Vec<&str> = wide.lines().collect();
    let wordmark_col = lines
        .iter()
        .find_map(|line| {
            line.find("██████    ██████")
                .map(|idx| line[..idx].chars().count())
        })
        .expect("wordmark");
    let tagline_col = lines
        .iter()
        .find_map(|line| {
            line.find("Paper Plane")
                .map(|idx| line[..idx].chars().count())
        })
        .expect("tagline");
    let version_col = lines
        .iter()
        .find_map(|line| {
            line.find(concat!("v", env!("CARGO_PKG_VERSION")))
                .map(|idx| line[..idx].chars().count())
        })
        .expect("version");
    assert_eq!(
        wordmark_col, tagline_col,
        "wordmark 与副标题应左对齐:\n{wide}"
    );
    assert_eq!(tagline_col, version_col, "副标题与版本号应左对齐:\n{wide}");

    // 品牌组合（logo + PP）可见外框在偶数终端宽下左右边距应完全相等。
    for term_w in [100_u16, 120, 160] {
        let screen = draw(&mut app, term_w, 30);
        let brand_rows: Vec<&str> = screen
            .lines()
            .skip_while(|l| l.trim().is_empty())
            .take(7)
            .collect();
        let min_left = brand_rows
            .iter()
            .map(|l| l.chars().take_while(|c| *c == ' ').count())
            .min()
            .unwrap();
        let max_right = brand_rows
            .iter()
            .map(|l| l.trim_end().chars().count())
            .max()
            .unwrap();
        let right_margin = usize::from(term_w) - max_right;
        assert_eq!(
            min_left, right_margin,
            "终端宽度 {term_w} 下品牌组合应左右居中（左 {min_left}，右 {right_margin}）:\n{screen}"
        );
        // 卡片中间竖线 `│` 必须落在顶部 logo 右缘与 `PP` 左缘的间隙内。
        let wordmark_left = brand_rows
            .iter()
            .find_map(|l| l.find("██████").map(|idx| l[..idx].chars().count()))
            .unwrap();
        let logo_right = brand_rows
            .iter()
            .take(5)
            .map(|l| {
                l.chars()
                    .take(wordmark_left)
                    .collect::<String>()
                    .trim_end()
                    .chars()
                    .count()
            })
            .max()
            .unwrap();
        let divider_col = screen
            .lines()
            .find(|l| l.contains("Session & workflow") && l.contains('│'))
            .and_then(|l| l.find('│').map(|idx| l[..idx].chars().count()))
            .unwrap();
        assert!(
            divider_col >= logo_right && divider_col < wordmark_left,
            "终端宽度 {term_w} 下竖线列 {divider_col} 应落在 logo 右缘 {logo_right} 与 PP 左缘 {wordmark_left} 之间:\n{screen}"
        );
    }
}
