use super::*;
use crossterm::event::KeyEvent;
use ratatui::{Terminal, backend::TestBackend};

fn key(panel: &mut Panel, code: KeyCode) -> Action {
    panel.handle(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)))
}

fn app_of(theme: Theme, icons: IconMode, tools: ToolStyle) -> Appearance {
    Appearance {
        theme,
        icons,
        tools,
    }
}

fn render_screen(panel: &mut Panel, width: u16, height: u16) -> String {
    let a = panel.preview_appearance();
    set_active_appearance(a.theme, a.icons, a.tools);
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| panel.draw(frame, frame.area()))
        .unwrap();
    let buffer = terminal.backend().buffer();
    let result = (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    set_active_appearance(Theme::Default, IconMode::Nerd, ToolStyle::Classic);
    result
}

#[test]
fn categories_preview_and_save_independently() {
    for (category, expected) in [
        (
            Category::IconMode,
            app_of(Theme::Default, IconMode::Ascii, ToolStyle::Classic),
        ),
        (
            Category::Theme,
            app_of(Theme::TokyoNight, IconMode::Nerd, ToolStyle::Classic),
        ),
        (
            Category::Tools,
            app_of(Theme::Default, IconMode::Nerd, ToolStyle::Rail),
        ),
    ] {
        let mut panel = Panel::new(app_of(Theme::Default, IconMode::Nerd, ToolStyle::Classic));
        assert_eq!(
            key(&mut panel, KeyCode::Down),
            Action::Preview(app_of(Theme::Default, IconMode::Ascii, ToolStyle::Classic))
        );
        let icons = render_screen(&mut panel, 110, 32);
        assert!(icons.contains("Themes — Icon Style"), "{icons}");
        assert!(icons.contains("Nerd Font") && icons.contains("ASCII"));

        // Switching category previews only that category; the other two use their saved values.
        assert_eq!(
            panel.set_category(Category::Theme),
            Action::Preview(app_of(Theme::Default, IconMode::Nerd, ToolStyle::Classic))
        );
        assert_eq!(
            key(&mut panel, KeyCode::Down),
            Action::Preview(app_of(
                Theme::TokyoNight,
                IconMode::Nerd,
                ToolStyle::Classic
            ))
        );
        let colors = render_screen(&mut panel, 110, 32);
        assert!(colors.contains("Themes — Color Theme"), "{colors}");
        assert!(colors.contains("Tokyo Night") && colors.contains("Solarized Light"));
        for code in [KeyCode::Backspace, KeyCode::Char('h'), KeyCode::Char('l')] {
            assert_eq!(key(&mut panel, code), Action::None);
            assert_eq!(panel.category, Category::Theme);
        }

        // Switching to the Tools category previews independently and does not carry in other categories' unsaved drafts.
        assert_eq!(
            panel.set_category(Category::Tools),
            Action::Preview(app_of(Theme::Default, IconMode::Nerd, ToolStyle::Classic))
        );
        assert_eq!(
            key(&mut panel, KeyCode::Down),
            Action::Preview(app_of(Theme::Default, IconMode::Nerd, ToolStyle::Rail))
        );
        let tools_screen = render_screen(&mut panel, 110, 32);
        assert!(tools_screen.contains("Themes — Tools"), "{tools_screen}");
        assert!(tools_screen.contains("Classic") && tools_screen.contains("Ledger"));
        assert!(
            tools_screen.contains("Live Preview · Rail"),
            "{tools_screen}"
        );

        let restore = panel.set_category(category);
        match category {
            Category::IconMode => assert_eq!(
                restore,
                Action::Preview(app_of(Theme::Default, IconMode::Ascii, ToolStyle::Classic))
            ),
            Category::Theme => assert_eq!(
                restore,
                Action::Preview(app_of(
                    Theme::TokyoNight,
                    IconMode::Nerd,
                    ToolStyle::Classic
                ))
            ),
            Category::Tools => assert_eq!(restore, Action::None),
        }
        assert_eq!(key(&mut panel, KeyCode::Enter), Action::Save(expected));
        assert!(panel.busy);
        assert_eq!(panel.category, category);
        assert_eq!(
            key(&mut panel, KeyCode::Esc),
            Action::Cancel(app_of(Theme::Default, IconMode::Nerd, ToolStyle::Classic))
        );
    }
}

#[test]
fn preview_renders_aligned_cards_full_width_backgrounds_and_status_across_sizes() {
    let light = palette_for(Theme::SolarizedLight);
    for (w, h) in [(80, 24), (100, 30), (120, 36)] {
        let mut preview_ys = Vec::new();
        for (category, icons) in [
            (Category::IconMode, IconMode::Ascii),
            (Category::Theme, IconMode::Nerd),
        ] {
            let mut panel = Panel::new(app_of(Theme::SolarizedLight, icons, ToolStyle::Classic));
            let _ = panel.set_category(category);
            let a = panel.preview_appearance();
            set_active_appearance(a.theme, a.icons, a.tools);
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            terminal
                .draw(|frame| panel.draw(frame, frame.area()))
                .unwrap();
            let buffer = terminal.backend().buffer();

            let mut screen_rows = Vec::with_capacity(usize::from(h));
            for y in 0..h {
                let row: String = (0..w).map(|x| buffer[(x, y)].symbol()).collect();
                screen_rows.push(row);
                // Under a light theme no cell may fall back to the terminal's default Reset background.
                for x in 0..w {
                    assert_ne!(
                        buffer[(x, y)].bg,
                        ratatui::style::Color::Reset,
                        "cell ({x},{y}) fell back to Reset bg on {w}x{h} {category:?}"
                    );
                }
            }
            set_active_appearance(Theme::Default, IconMode::Nerd, ToolStyle::Classic);

            let screen = screen_rows.join("\n");
            assert!(screen.contains("Live Preview"), "{w}x{h}:\n{screen}");
            assert!(screen.contains("Solarized Light"), "{w}x{h}:\n{screen}");
            assert!(screen.contains("gpt-5"), "{w}x{h}:\n{screen}");

            // The user message bar, Edit card and bottom bar all fill their background across the viewport width, and the Edit corners line up.
            let preview_y = screen_rows
                .iter()
                .position(|r| r.contains("Live Preview"))
                .unwrap();
            let user_y = screen_rows
                .iter()
                .position(|r| r.contains("Apply solarized-light"))
                .unwrap() as u16;
            let edit_top_y = screen_rows
                .iter()
                .position(|r| r.contains("╭─") && r.contains("Edit:"))
                .unwrap() as u16;
            let edit_bot_y = screen_rows
                .iter()
                .position(|r| r.contains("╰─") && r.contains('╯'))
                .unwrap() as u16;
            let status_y = screen_rows
                .iter()
                .position(|r| r.contains("gpt-5") && r.contains("~/pp"))
                .unwrap() as u16;
            preview_ys.push((preview_y, user_y, edit_top_y, status_y));

            let col = |row: &str, ch: char| {
                row.find(ch)
                    .map(|byte| ratatui::text::Line::raw(&row[..byte]).width())
            };
            let left_col = col(&screen_rows[usize::from(edit_top_y)], '╭').unwrap();
            let right_col = col(&screen_rows[usize::from(edit_top_y)], '╮').unwrap();
            assert_eq!(
                col(&screen_rows[usize::from(edit_bot_y)], '╰'),
                Some(left_col)
            );
            assert_eq!(
                col(&screen_rows[usize::from(edit_bot_y)], '╯'),
                Some(right_col)
            );

            let user_bg_cols = (0..w)
                .filter(|&x| buffer[(x, user_y)].bg == light.user_bg)
                .count();
            let status_bg_cols = (0..w)
                .filter(|&x| buffer[(x, status_y)].bg == light.input_status_bg)
                .count();
            assert!(user_bg_cols >= usize::from(w) / 2, "{user_bg_cols}");
            assert_eq!(status_bg_cols, user_bg_cols);

            if icons == IconMode::Ascii {
                let preview_start = screen_rows
                    .iter()
                    .position(|r| r.contains("Live Preview"))
                    .unwrap();
                let preview_text = screen_rows[preview_start..=usize::from(status_y)].join("\n");
                assert!(
                    !preview_text
                        .chars()
                        .any(|ch| ('\u{e000}'..='\u{f8ff}').contains(&ch)
                            || ('\u{f0000}'..='\u{ffffd}').contains(&ch)),
                    "ASCII preview contained Nerd Font PUA glyph:\n{preview_text}"
                );
            }
        }
        assert_eq!(preview_ys[0], preview_ys[1], "preview jumped on {w}x{h}");
    }
}

#[test]
fn tools_category_renders_all_six_styles_with_live_preview_and_mouse_navigation() {
    for style in ToolStyle::ALL {
        for icons in IconMode::ALL {
            let mut panel = Panel::new(app_of(Theme::SolarizedLight, icons, style));
            let _ = panel.set_category(Category::Tools);
            let mut terminal = Terminal::new(TestBackend::new(110, 34)).unwrap();
            terminal
                .draw(|frame| panel.draw(frame, frame.area()))
                .unwrap();
            let buffer = terminal.backend().buffer();
            let mut rows = Vec::with_capacity(34);
            for y in 0..34 {
                rows.push(
                    (0..110)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>(),
                );
                for x in 0..110 {
                    assert_ne!(
                        buffer[(x, y)].bg,
                        ratatui::style::Color::Reset,
                        "Tools cell ({x},{y}) fell back to Reset bg for {style:?} {icons:?}"
                    );
                }
            }
            let screen = rows.join("\n");
            assert!(
                screen.contains(&format!("Live Preview · {}", style.label())),
                "{screen}"
            );
            assert!(screen.contains("themes.rs"), "{screen}");
            if icons == IconMode::Ascii {
                let preview_start = rows
                    .iter()
                    .position(|r| r.contains("Live Preview"))
                    .unwrap();
                let preview_and_list = rows[3..rows.len() - 1].join("\n");
                let preview_only = rows[preview_start..rows.len() - 1].join("\n");
                for slice in [preview_and_list, preview_only] {
                    assert!(
                        !slice
                            .chars()
                            .any(|ch| ('\u{e000}'..='\u{f8ff}').contains(&ch)
                                || ('\u{f0000}'..='\u{ffffd}').contains(&ch)),
                        "ASCII Tools screen contained Nerd Font PUA glyph:\n{slice}"
                    );
                }
            }
        }
    }

    // Clicking row 3 of the sidebar switches to the Tools category, and clicking a list row switches to that style.
    let mut panel = Panel::new(app_of(Theme::Default, IconMode::Nerd, ToolStyle::Classic));
    let _ = render_screen(&mut panel, 110, 32);
    assert_eq!(
        panel.handle(Event::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: panel.sidebar_area.x + 2,
            row: panel.sidebar_area.y + 2,
            modifiers: KeyModifiers::NONE,
        })),
        Action::None
    );
    assert_eq!(panel.category, Category::Tools);
    assert_eq!(
        panel.handle(Event::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: panel.list_area.x + 4,
            row: panel.list_area.y + 4,
            modifiers: KeyModifiers::NONE,
        })),
        Action::Preview(app_of(Theme::Default, IconMode::Nerd, ToolStyle::Hanging))
    );
}

#[test]
fn color_theme_list_caps_height_scrolls_and_renders_all_palettes() {
    let mut panel = Panel::new(app_of(Theme::Default, IconMode::Nerd, ToolStyle::Classic));
    let _ = panel.set_category(Category::Theme);

    // At a typical terminal height the list is capped by MAX_LIST_ROWS and scrolling turns on beyond it.
    let initial = render_screen(&mut panel, 110, 32);
    assert!(panel.list_area.height <= render::MAX_LIST_ROWS);
    assert_eq!(panel.list_area.height, render::MAX_LIST_ROWS);
    assert_eq!(panel.list_scroll, 0);
    assert!(initial.contains("Default"));
    assert!(!initial.contains("Night Owl"));
    assert!(initial.contains("Live Preview"));

    // PageDown pages and updates the selection and scroll window.
    assert_eq!(
        key(&mut panel, KeyCode::PageDown),
        Action::Preview(app_of(
            Theme::ALL[usize::from(render::MAX_LIST_ROWS)],
            IconMode::Nerd,
            ToolStyle::Classic,
        ))
    );
    let paged = render_screen(&mut panel, 110, 32);
    assert!(panel.list_scroll > 0);
    assert!(paged.contains(Theme::ALL[usize::from(render::MAX_LIST_ROWS)].label()));

    // End scrolls to the last theme, and clicking a visible row hits the theme at the scrolled offset.
    assert_eq!(
        key(&mut panel, KeyCode::End),
        Action::Preview(app_of(Theme::NightOwl, IconMode::Nerd, ToolStyle::Classic))
    );
    let at_end = render_screen(&mut panel, 110, 32);
    assert!(at_end.contains("Night Owl"), "{at_end}");
    assert!(!at_end.contains("Tokyo Night"), "{at_end}");
    assert_eq!(
        panel.list_scroll,
        Theme::ALL.len() - usize::from(render::MAX_LIST_ROWS)
    );

    let first_visible_row = panel.list_area.y;
    let first_visible_theme = Theme::ALL[panel.list_scroll];
    assert_eq!(
        panel.handle(Event::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: panel.list_area.x + 4,
            row: first_visible_row,
            modifiers: KeyModifiers::NONE,
        })),
        Action::Preview(app_of(
            first_visible_theme,
            IconMode::Nerd,
            ToolStyle::Classic
        ))
    );

    assert_eq!(
        key(&mut panel, KeyCode::PageUp),
        Action::Preview(app_of(
            Theme::ALL[panel
                .list_scroll
                .saturating_sub(usize::from(render::MAX_LIST_ROWS))],
            IconMode::Nerd,
            ToolStyle::Classic,
        ))
    );
    assert_eq!(
        key(&mut panel, KeyCode::Home),
        Action::Preview(app_of(Theme::Default, IconMode::Nerd, ToolStyle::Classic))
    );
    let back_top = render_screen(&mut panel, 110, 32);
    assert_eq!(panel.list_scroll, 0);
    assert!(back_top.contains("Default"));

    // All 20 themes render fully and no cell falls back to the Reset background.
    for theme in Theme::ALL {
        let mut themed = Panel::new(app_of(theme, IconMode::Nerd, ToolStyle::Classic));
        let _ = themed.set_category(Category::Theme);
        set_active_appearance(theme, IconMode::Nerd, ToolStyle::Classic);
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|frame| themed.draw(frame, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        for y in 0..30 {
            for x in 0..100 {
                assert_ne!(
                    buffer[(x, y)].bg,
                    ratatui::style::Color::Reset,
                    "cell ({x},{y}) fell back to Reset bg for theme {theme:?}"
                );
            }
        }
    }
    set_active_appearance(Theme::Default, IconMode::Nerd, ToolStyle::Classic);
}
