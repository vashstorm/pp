//! Ask panel: multiple questions with single choice/multi choice/custom answers, and a
//! single Review submit.
//!
//! The Panel only owns interaction state and drawing; the request channel, answer
//! replies and transcript record blocks belong to `App` (src/tui.rs). All displayed text
//! is sanitized through `display_text`, while the raw answers always keep the
//! unmodified label/custom text.

use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Padding},
};
use ratatui_textarea::TextArea;

use super::dialog::Action;
use super::menu::{MenuEvent, MenuItem, SelectionMenu};
use super::transcript_style::{accent, activity, ask_bg, ask_border, ask_tab_bg, dim, removed};
use pp_core::interaction::AskAnswer;
use pp_core::interaction::AskArgs;
use pp_core::interaction::AskRequest;
use pp_core::text::display_text;

mod keys;
mod layout;
mod model;
mod render;
mod review;

pub(in crate::tui) use self::model::selection_icon;
use self::model::{hint_line, insert_custom_text, menu_items, menu_text};

const OTHER_LABEL: &str = "Other (type your own)";
const REQUIRED_MESSAGE: &str = "Answer this question before submitting.";
const EMPTY_MULTI_MESSAGE: &str = "No options selected";
const NOT_ANSWERED_MESSAGE: &str = "(not answered)";
const EDITOR_HINT: &str = "Enter accept · Ctrl + J newline · Esc discard";
const SINGLE_HINT: &str = "Space select · ↵ confirm · ↑/↓ move · Tab/←/→ question · Esc cancel";
const SINGLE_PAGE_HINT: &str = "Space select · ↵ confirm · ↑/↓ move · Esc cancel";
const MULTI_HINT: &str = "Space toggle · ↵ next · ↑/↓ move · Tab/←/→ question · Esc cancel";
const REVIEW_HINT: &str = "Enter submit · Tab questions · Esc cancel";
const COMPACT_HINT: &str = "Space select · Enter confirm · Tab next · Esc cancel";
const COMPACT_SINGLE_PAGE_HINT: &str = "Space select · Enter confirm · Esc cancel";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    Question(usize),
    Review,
}

/// Focus, selection, custom answer and list position of a single question.
struct QuestionState {
    selected: Vec<usize>,
    custom: String,
    menu: SelectionMenu<usize>,
}

impl QuestionState {
    fn new(question: &pp_core::interaction::AskQuestion) -> Self {
        let initial = question
            .recommended
            .filter(|index| *index < question.options.len())
            .unwrap_or(0);
        let menu =
            SelectionMenu::new(menu_items(question, &[], false), initial).with_accent(accent());
        Self {
            selected: Vec::new(),
            custom: String::new(),
            menu,
        }
    }
}

pub(super) struct Panel {
    questions: Arc<AskArgs>,
    states: Vec<QuestionState>,
    page: Page,
    editor: Option<(usize, TextArea<'static>)>,
    error: Option<String>,
    submitted: bool,
    review_scroll: usize,
    overlay: Option<Rect>,
    review: Option<(u16, Vec<Line<'static>>)>,
}

impl Panel {
    pub(super) fn new(request: AskRequest) -> Self {
        let args = request.args;
        let states = args.questions.iter().map(QuestionState::new).collect();
        Self {
            questions: args,
            states,
            page: Page::Question(0),
            editor: None,
            error: None,
            submitted: false,
            review_scroll: 0,
            overlay: None,
            review: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pp_core::interaction::AskOption;
    use pp_core::interaction::AskQuestion;
    use ratatui::{Terminal, backend::TestBackend};

    fn option(label: &str) -> AskOption {
        AskOption {
            label: label.into(),
            description: None,
            preview: None,
        }
    }

    fn question(id: &str, multi: bool, recommended: Option<usize>, labels: &[&str]) -> AskQuestion {
        AskQuestion {
            id: id.into(),
            question: format!("Choose for {id}?"),
            options: labels.iter().map(|label| option(label)).collect(),
            header: None,
            multi,
            recommended,
        }
    }

    fn panel(questions: Vec<AskQuestion>) -> Panel {
        Panel::new(AskRequest {
            call_id: "call-1".into(),
            args: Arc::new(AskArgs { questions }),
        })
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(code: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(code), KeyModifiers::CONTROL)
    }

    fn render(panel: &mut Panel, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| panel.draw(frame, Rect::new(0, 0, width, height)))
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn initial_focus_sanitization_and_custom_editor_answers() {
        {
            let panel_empty = self::panel(vec![question("q", true, None, &[])]);
            assert_eq!(panel_empty.states[0].menu.selected_index(), Some(0));

            let mut q = question("q", false, Some(1), &["Postgres", "Da\u{7f}ta", "C"]);
            q.options[1].description = Some("fast\u{1b}[31m".into());
            let mut panel = panel(vec![q]);
            assert_eq!(panel.states[0].menu.selected_index(), Some(1));
            assert!(panel.states[0].selected.is_empty());
            let items = panel.states[0].menu.items();
            assert!(!items[1].label.contains('\u{7f}'));
            assert!(!items[1].description.contains('\u{1b}'));
            panel.key(key(KeyCode::Up));
            let Action::Submit(answers) = panel.key(key(KeyCode::Enter)) else {
                panic!("single question single select must submit immediately");
            };
            assert_eq!(answers[0].selected_options, vec!["Postgres".to_string()]);
            assert_eq!(answers[0].custom_input, None);

            let mut tabs = self::panel(vec![question("q", false, None, &["a\tb\nc"])]);
            assert!(render(&mut tabs, 80, 24).contains("a   b c"));
            let Action::Submit(ans_tabs) = tabs.key(key(KeyCode::Enter)) else {
                panic!("expected selected answer");
            };
            assert_eq!(ans_tabs[0].selected_options, ["a\tb\nc"]);

            let mut ctrl_panel = self::panel(vec![question("q", false, None, &["A"])]);
            ctrl_panel.key(key(KeyCode::End));
            ctrl_panel.key(key(KeyCode::Enter));
            ctrl_panel.paste("bad\u{1b}[31mtail\r\nnext\rmiddle\r");
            assert!(!render(&mut ctrl_panel, 80, 24).contains('\u{1b}'));
            let Action::Submit(ctrl_ans) = ctrl_panel.key(key(KeyCode::Enter)) else {
                panic!("expected custom answer");
            };
            assert_eq!(
                ctrl_ans[0].custom_input.as_deref(),
                Some("bad\u{1b}[31mtail\r\nnext\rmiddle\r")
            );
            assert!(matches!(ctrl_panel.key(key(KeyCode::Enter)), Action::None));
        }

        {
            let mut p1 = panel(vec![question("q", false, Some(0), &["A", "B"])]);
            assert!(matches!(p1.paste("hello"), Action::None));
            assert!(p1.states[0].custom.is_empty());
            p1.key(key(KeyCode::End));
            assert!(matches!(p1.key(key(KeyCode::Enter)), Action::None));
            p1.paste("  ");
            assert!(matches!(p1.key(key(KeyCode::Enter)), Action::None));
            assert!(matches!(p1.key(key(KeyCode::Esc)), Action::None));
            assert!(p1.editor.is_none() && p1.states[0].custom.is_empty());
            p1.key(key(KeyCode::Enter));
            p1.paste("first line");
            p1.key(ctrl('j'));
            p1.paste("second");
            let Action::Submit(a1) = p1.key(key(KeyCode::Enter)) else {
                panic!("expected submit after custom answer");
            };
            assert_eq!(a1[0].custom_input, Some("first line\nsecond".to_string()));
            assert!(a1[0].selected_options.is_empty());

            let mut p2 = panel(vec![
                question("a", false, None, &["A", "B"]),
                question("b", true, None, &["x", "y"]),
            ]);
            p2.key(key(KeyCode::End));
            p2.key(key(KeyCode::Enter));
            p2.paste("custom");
            p2.key(key(KeyCode::Enter));
            assert_eq!(p2.page, Page::Question(1));
            p2.key(key(KeyCode::BackTab));
            p2.key(key(KeyCode::Home));
            assert!(matches!(p2.key(key(KeyCode::Char(' '))), Action::None));
            assert_eq!(p2.states[0].selected, vec![0]);
            assert!(p2.states[0].custom.is_empty());
            p2.key(key(KeyCode::Enter));
            p2.key(key(KeyCode::Enter));
            let Action::Submit(a2) = p2.key(key(KeyCode::Enter)) else {
                panic!("expected submit");
            };
            assert_eq!(a2[0].selected_options, vec!["A".to_string()]);
            assert_eq!(a2[0].custom_input, None);

            for paste in [false, true] {
                let mut p3 = panel(vec![question("q", false, None, &["SQLite"])]);
                p3.key(key(KeyCode::End));
                render(&mut p3, 80, 24);
                let before = p3.overlay;
                if paste {
                    p3.paste("answer");
                } else {
                    for ch in "answer".chars() {
                        p3.key(key(KeyCode::Char(ch)));
                    }
                }
                let screen = render(&mut p3, 80, 24);
                assert_eq!(p3.overlay, before);
                assert!(screen.contains("󰐽 answer") && !screen.contains("custom answer"));
                p3.key(ctrl('j'));
                p3.paste("second line");
                render(&mut p3, 80, 24);
                assert_eq!(p3.overlay.unwrap().height, before.unwrap().height + 1);
                assert_eq!(p3.overlay.unwrap().bottom(), before.unwrap().bottom());
                assert!(render(&mut p3, 40, 8).contains("second line"));
                p3.key(key(KeyCode::Esc));
                assert!(p3.states[0].custom.is_empty());
                p3.paste("accepted");
                let Action::Submit(ans) = p3.key(key(KeyCode::Enter)) else {
                    panic!("in-place input must submit");
                };
                assert_eq!(ans[0].custom_input.as_deref(), Some("accepted"));
            }
        }
    }

    #[test]
    fn multi_select_navigation_cancel_space_and_wheel() {
        {
            let mut p1 = panel(vec![
                question("checks", true, Some(0), &["unit", "lint", "fmt"]),
                question("db", false, Some(0), &["SQLite", "Postgres"]),
            ]);
            p1.key(key(KeyCode::End));
            p1.key(key(KeyCode::Up));
            p1.key(key(KeyCode::Char(' ')));
            p1.key(key(KeyCode::Home));
            p1.key(key(KeyCode::Char(' ')));
            assert_eq!(p1.states[0].selected, vec![0, 2]);
            p1.key(key(KeyCode::Tab));
            p1.key(key(KeyCode::Tab));
            assert_eq!(p1.page, Page::Review);
            assert!(matches!(p1.key(key(KeyCode::Enter)), Action::None));
            assert_eq!(p1.error.as_deref(), Some(REQUIRED_MESSAGE));
            assert_eq!(p1.page, Page::Question(1));
            assert!(matches!(p1.key(key(KeyCode::Enter)), Action::None));
            let Action::Submit(a1) = p1.key(key(KeyCode::Enter)) else {
                panic!("expected submit after all questions answered");
            };
            assert_eq!(
                a1[0].selected_options,
                vec!["unit".to_string(), "fmt".to_string()]
            );
            assert_eq!(a1[1].selected_options, vec!["SQLite".to_string()]);

            let mut p2 = panel(vec![question("checks", true, None, &["unit", "lint"])]);
            p2.key(key(KeyCode::Tab));
            let Action::Submit(a2) = p2.key(key(KeyCode::Enter)) else {
                panic!("empty multi selection must submit");
            };
            assert!(a2[0].selected_options.is_empty());

            let mut p3 = panel(vec![question("checks", true, Some(0), &["unit", "lint"])]);
            p3.key(key(KeyCode::Char(' ')));
            p3.key(key(KeyCode::End));
            p3.key(key(KeyCode::Enter));
            p3.paste("review again");
            p3.key(key(KeyCode::Enter));
            let Action::Submit(a3) = p3.key(key(KeyCode::Enter)) else {
                panic!("expected submit with selection and custom answer");
            };
            assert_eq!(a3[0].selected_options, vec!["unit".to_string()]);
            assert_eq!(a3[0].custom_input, Some("review again".to_string()));
        }

        {
            let mut picker = panel(vec![question("q", false, None, &["A"])]);
            picker.key(key(KeyCode::Tab));
            assert_eq!(picker.page, Page::Question(0));
            assert!(matches!(picker.key(key(KeyCode::Esc)), Action::Cancel));

            let mut review = panel(vec![question("q", true, None, &["A"])]);
            review.key(key(KeyCode::Tab));
            assert!(matches!(review.key(key(KeyCode::Esc)), Action::Cancel));

            let mut multi = panel(vec![
                question("a", false, None, &["A"]),
                question("b", false, None, &["B"]),
            ]);
            multi.key(key(KeyCode::Tab));
            assert_eq!(multi.page, Page::Question(1));
            multi.key(key(KeyCode::Right));
            assert_eq!(multi.page, Page::Review);
            multi.key(key(KeyCode::Tab));
            assert_eq!(multi.page, Page::Question(0));
            multi.key(key(KeyCode::Left));
            assert_eq!(multi.page, Page::Review);

            let mut space = panel(vec![question("q", false, Some(1), &["A", "B", "C"])]);
            render(&mut space, 40, 20);
            let area = space.overlay.expect("overlay cached after draw");
            assert!(space.wheel(false, area.x, area.bottom() - 1));
            assert_eq!(space.states[0].menu.selected_index(), Some(2));
            assert!(!space.wheel(true, area.right(), area.y));

            space.key(key(KeyCode::Up));
            assert!(matches!(space.key(key(KeyCode::Char(' '))), Action::None));
            assert_eq!(space.states[0].selected, [1]);
            let before = render(&mut space, 100, 24);
            assert!(before.contains("󰐾 B (Recommended)") && !before.contains("details"));
            assert!(matches!(space.key(ctrl('o')), Action::None));
            assert_eq!(render(&mut space, 100, 24), before);
            space.key(key(KeyCode::Home));
            space.key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL));
            space.key(KeyEvent::new_with_kind(
                KeyCode::Char(' '),
                KeyModifiers::NONE,
                KeyEventKind::Repeat,
            ));
            assert_eq!(space.states[0].selected, [1]);
            space.key(key(KeyCode::Char(' ')));
            let Action::Submit(ans) = space.key(key(KeyCode::Enter)) else {
                panic!("must submit the selected answer after Enter");
            };
            assert_eq!(ans[0].selected_options, ["A"]);
        }
    }

    #[test]
    fn picker_descriptions_tabs_highlighting_and_wrapping() {
        {
            let mut q = question("q", false, Some(0), &["A"]);
            q.options[0].description = Some("Option description".into());
            q.options[0].preview = Some("# Head\n\ntext".into());
            let mut p1 = panel(vec![q]);
            for (w, h) in [(40, 24), (24, 12)] {
                let screen = render(&mut p1, w, h);
                assert!(
                    screen.contains("󰐽 A") && !screen.contains("Head") && !screen.contains("text")
                );
                if w >= 40 {
                    assert!(screen.contains("Option description"));
                }
            }
            p1.key(key(KeyCode::Char(' ')));
            assert!(render(&mut p1, 40, 24).contains("󰐾 A"));

            let mut p2 = panel(vec![question("db", false, None, &["A", "B"])]);
            let s3 = render(&mut p2, 30, 3);
            assert!(s3.contains("\u{f02d7} Ask") && !s3.contains("db"));
            assert!(render(&mut p2, 30, 1).starts_with('A'));

            let mut sq = question("confirm_commit", false, None, &["yes", "no"]);
            sq.header = Some("Commit confirmation".into());
            sq.question = "Commit the changes?".into();
            let mut p3 = panel(vec![sq]);
            let screen = render(&mut p3, 80, 24);
            assert!(!screen.contains("confirm_commit") && !screen.contains("Tab/←/→"));
            assert_eq!(p3.overlay.unwrap().height, 8);
            let rows: Vec<_> = screen.lines().collect();
            let title = rows
                .iter()
                .position(|row| row.contains("╭ \u{f02d7} Ask"))
                .unwrap();
            assert!(rows[title + 1].contains("Commit the changes?"));
            assert!(rows[title + 2].contains("󰐽 yes"));

            let mut custom_only = panel(vec![question("custom", false, None, &[])]);
            let cs = render(&mut custom_only, 80, 24);
            assert_eq!(custom_only.overlay.unwrap().height, 6);
            assert!(cs.contains(OTHER_LABEL) && cs.contains("╭ \u{f02d7} Ask"));
        }

        {
            let mut first = question("first", false, None, &["A"]);
            first.header = Some(" 范围\t".into());
            let second = question("mode", true, None, &["B"]);
            let mut p = panel(vec![first, second]);
            for (page, label) in [(0, "范围"), (1, "mode"), (2, "Review")] {
                for width in [80, 18] {
                    let mut terminal = Terminal::new(TestBackend::new(width, 30)).unwrap();
                    terminal.draw(|frame| p.draw(frame, frame.area())).unwrap();
                    let buffer = terminal.backend().buffer();
                    let row = p.overlay.unwrap().y + 1;
                    // Trailing cells of a wide character are placeholders only and are not
                    // part of the displayed text.
                    let columns = (0..width).filter(|&x| {
                        x == 0 || Span::raw(buffer[(x - 1, row)].symbol()).width() <= 1
                    });
                    let highlighted: String = columns
                        .clone()
                        .filter(|&x| buffer[(x, row)].bg == ask_tab_bg())
                        .map(|x| buffer[(x, row)].symbol())
                        .collect();
                    assert_eq!(
                        highlighted,
                        format!(" {label} "),
                        "highlight must cover the active tab and one space each side"
                    );
                    for x in 2..width.saturating_sub(2) {
                        let cell = &buffer[(x, row)];
                        assert_ne!(
                            cell.bg,
                            accent(),
                            "tabs must not use the bright yellow background"
                        );
                        if cell.bg == ask_tab_bg() {
                            assert_eq!(cell.fg, activity());
                            assert!(!cell.modifier.contains(Modifier::BOLD));
                        }
                    }
                    if width == 80 {
                        let text: String = columns.map(|x| buffer[(x, row)].symbol()).collect();
                        assert!(
                            text.contains(" 范围   mode   Review "),
                            "tab labels must be separated by exactly three spaces: {text}"
                        );
                    }
                }
                assert_eq!(
                    p.page,
                    if page < 2 {
                        Page::Question(page)
                    } else {
                        Page::Review
                    }
                );
                p.key(key(KeyCode::Tab));
            }

            const LONG: &str = "请选择本次操作需要执行的完整检查范围，并确认后继续";
            let wrapped = |id: &str| {
                let mut question = question(id, false, None, &["A"]);
                question.question = LONG.into();
                question
            };

            // A single question has no Review tab, so the height depends only on the
            // wrapped prompt.
            let mut single = panel(vec![wrapped("first")]);
            let wide = render(&mut single, 80, 30);
            let wide_height = single.overlay.unwrap().height;
            // TestBackend keeps blank placeholder cells for double-width characters.
            assert!(wide.replace(' ', "").contains(LONG));
            let narrow = render(&mut single, 32, 30);
            assert!(
                single.overlay.unwrap().height > wide_height,
                "a wrapped prompt on a narrow screen must make the panel taller"
            );
            assert!(narrow.replace(' ', "").contains("请选择本次操作"));
            assert!(narrow.replace(' ', "").contains("确认后继续"));
            assert!(narrow.contains("󰐽 A"));

            // With several questions at the same height, switching tabs only swaps the
            // body.
            let mut pages = panel(vec![
                wrapped("first"),
                question("second", false, None, &["B"]),
            ]);
            let first = render(&mut pages, 80, 30);
            assert!(first.replace(' ', "").contains(LONG));
            pages.key(key(KeyCode::Tab));
            let s = render(&mut pages, 80, 30);
            assert!(
                s.contains("Choose for second?")
                    && !s.contains("请选择本次操作")
                    && s.contains("󰐽 B")
            );
        }
    }

    #[test]
    fn height_is_shared_and_controls_are_safe() {
        let mut long = question("long", false, None, &["A", "B", "C"]);
        for option in &mut long.options {
            option.description =
                Some("A detailed option description that wraps at narrow widths".into());
        }
        let mut panel = panel(vec![long, question("short", false, None, &["D"])]);

        // Wide screen: the tallest `long` page sets the shared height, and switching to
        // `short` or Review does not change it.
        render(&mut panel, 100, 40);
        let shared = panel.overlay.unwrap().height;
        assert_eq!(
            shared, 13,
            "prompt, three option names/descriptions, Other and the five-row frame"
        );
        panel.key(key(KeyCode::Tab));
        let screen = render(&mut panel, 100, 40);
        assert!(screen.contains("Choose for short?"));
        assert_eq!(
            panel.overlay.unwrap().height,
            shared,
            "switching tabs must not change the panel height"
        );

        // The validation error occupies its own row outside the shared height.
        panel.error = Some(REQUIRED_MESSAGE.into());
        let screen = render(&mut panel, 100, 40);
        assert_eq!(panel.overlay.unwrap().height, shared + 1);
        assert!(screen.contains(REQUIRED_MESSAGE));
        panel.error = None;
        panel.key(key(KeyCode::Tab));
        assert_eq!(panel.page, Page::Review);
        render(&mut panel, 100, 40);
        assert_eq!(
            panel.overlay.unwrap().height,
            shared,
            "the Review page keeps the tallest page height"
        );
        panel.key(key(KeyCode::BackTab));
        panel.key(key(KeyCode::BackTab));
        assert_eq!(panel.page, Page::Question(0));

        // Narrow screen: wrapped descriptions raise the shared height and short pages
        // follow it.
        render(&mut panel, 32, 40);
        let wrapped = panel.overlay.unwrap().height;
        assert_eq!(
            wrapped, 16,
            "wrapped descriptions must count toward the real height"
        );
        panel.key(key(KeyCode::Tab));
        render(&mut panel, 32, 40);
        assert_eq!(
            panel.overlay.unwrap().height,
            wrapped,
            "switching tabs on a narrow screen keeps the height too"
        );

        // Multi-line input on a short page is still laid out inside the shared height.
        panel.key(key(KeyCode::End));
        panel.key(key(KeyCode::Enter));
        panel.paste("one\ntwo\nthree");
        let screen = render(&mut panel, 32, 40);
        assert_eq!(
            panel.overlay.unwrap().height,
            wrapped,
            "three input lines on a short page must not exceed the shared height"
        );
        assert!(screen.contains("three"));
        panel.key(key(KeyCode::Esc));
        assert!(panel.states[1].custom.is_empty());

        // When the wrapped Review answers exceed the tallest question page, the panel
        // grows to its own height, capped at 70%.
        panel.key(key(KeyCode::BackTab));
        panel.key(key(KeyCode::End));
        panel.key(key(KeyCode::Enter));
        panel.paste(&"very long answer ".repeat(12));
        panel.key(key(KeyCode::Enter));
        panel.key(key(KeyCode::Tab));
        assert_eq!(panel.page, Page::Review);
        render(&mut panel, 32, 40);
        let review_height = panel.overlay.unwrap().height;
        assert!(
            review_height > wrapped,
            "the Review height must include wrapped answers"
        );
        render(&mut panel, 32, 12);
        assert!(
            panel.overlay.unwrap().height <= 8,
            "the panel must not exceed 70% of the screen height"
        );
    }
}
