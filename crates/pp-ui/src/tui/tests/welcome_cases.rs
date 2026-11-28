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
            "Shift + Enter should keep a gap from its description ({width}x{height}):\n{screen}"
        );
        assert!(
            !screen.contains(&format!("Shift + Enter{expected_desc}")),
            "Shift + Enter must not run into its description ({width}x{height})"
        );
    }
}

#[test]
fn welcome_responsive_breakpoints_keep_top_gap_and_preserve_draft() {
    let _guard = WelcomeThemeGuard::set(Theme::Default, IconMode::Nerd);
    let mut app = app();
    app.input.insert_str("welcome draft");

    // Wide 100x30: the brand block is top-aligned with 2 rows of padding, and two columns show all 14 entries.
    let wide = draw(&mut app, 100, 30);
    let wide_lines: Vec<&str> = wide.lines().collect();
    let logo_y = wide_lines
        .iter()
        .position(|line| line.contains("▄▄▄██▀▀██"))
        .expect("wide logo");
    let brand_top_y = wide_lines
        .iter()
        .position(|line| line.contains('▄'))
        .expect("wide brand top");
    assert_eq!(
        brand_top_y, 2,
        "the brand block should be top-aligned with 2 rows of padding: {wide}"
    );
    assert!(wide.contains("Paper Plane"));
    let card_top_y = wide_lines
        .iter()
        .position(|line| line.contains("Session & workflow"))
        .expect("card header");
    assert!(
        !wide_lines[logo_y..card_top_y]
            .iter()
            .any(|line| line.contains("fixture-cwd")),
        "the working directory must not repeat below the logo:\n{wide}"
    );
    assert!(
        wide_lines
            .iter()
            .any(|line| line.contains("Session & workflow")
                && line.contains("│")
                && line.contains("Keyboard")),
        "the wide two-column card should have headings for both columns and a vertical divider:\n{wide}"
    );
    assert!(wide.contains("welcome draft"));

    // 80x24: both columns still show their text fully on one line.
    let std = draw(&mut app, 80, 24);
    assert!(std.contains("plan, then approve edits"), "{std}");
    assert!(std.contains("cancel the running task"), "{std}");

    // 60x30: the compact brand leaves room for a single-column card and keeps every entry.
    let narrow = draw(&mut app, 60, 30);
    assert_eq!(narrow.lines().next().unwrap().trim(), "\u{f1d9}  PP");
    let session_y = narrow
        .lines()
        .position(|line| line.contains("Session & workflow"))
        .expect("narrow session");
    let keyboard_y = narrow
        .lines()
        .position(|line| line.contains("Keyboard"))
        .expect("narrow keyboard");
    assert!(session_y < keyboard_y);

    // 38x30: compact brand plus the full single-column hints.
    let compact_tall = draw(&mut app, 38, 30);
    assert_eq!(compact_tall.lines().next().unwrap().trim(), "\u{f1d9}  PP");
    assert!(compact_tall.contains("Shift + Tab"));

    for screen in [&wide, &std, &narrow, &compact_tall] {
        for entry in [
            "/context",
            "/compact",
            "/tree",
            "complete commands/paths",
            "↑ / ↓",
            "Ctrl + O",
        ] {
            assert!(screen.contains(entry), "missing {entry}:\n{screen}");
        }
    }

    // 40x12: a very short screen keeps the compact brand, the minimal hints and the draft.
    let compact_short = draw(&mut app, 40, 12);
    assert_eq!(compact_short.lines().next().unwrap().trim(), "\u{f1d9}  PP");
    assert!(compact_short.contains("/tips  All shortcuts"));
    assert!(compact_short.contains("welcome draft"));

    // 20x6: a tiny terminal gives priority to the input draft.
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

    // On wide screens the card adapts to 75% of the terminal width (minimum 84, maximum 120).
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
            .expect("the tips card should have a horizontal border");
        assert_eq!(
            rule_len, expected_card_w,
            "terminal width {term_w} should render a {expected_card_w}-column card:\n{screen}"
        );
    }

    // When the tagline is shorter than WORDMARK, the right-column start of WORDMARK, Paper Plane and the version still line up.
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
        "the wordmark and the tagline should be left-aligned:\n{wide}"
    );
    assert_eq!(
        tagline_col, version_col,
        "the tagline and the version should be left-aligned:\n{wide}"
    );

    // At even terminal widths the visible brand block (logo + PP) has exactly equal left and right margins.
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
            "at terminal width {term_w} the brand block should be horizontally centered (left {min_left}, right {right_margin}):\n{screen}"
        );
        // The card's central divider `│` must sit inside the gap between the logo's right edge and `PP`'s left edge.
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
            "at terminal width {term_w} the divider column {divider_col} should fall between the logo right edge {logo_right} and the PP left edge {wordmark_left}:\n{screen}"
        );
    }
}
