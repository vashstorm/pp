//! 提问面板：多题单选／多选／自定义回答与 Review 一次提交。
//!
//! Panel 只维护交互状态与绘制；请求通道、答案回传与 transcript 记录块由
//! `App`（src/tui.rs）负责。所有展示文本经过 `display_text` 清洗，原始答案
//! 始终使用未经改写的 label／custom 文本。

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

use super::menu::{MenuEvent, MenuItem, SelectionMenu};
use super::transcript_style::ACCENT;
use super::transcript_style::ACTIVITY;
use super::transcript_style::ASK_BG;
use super::transcript_style::ASK_BORDER;
use super::transcript_style::ASK_TAB_BG;
use super::transcript_style::DIM;
use super::transcript_style::REMOVED;
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

#[derive(Debug)]
pub(super) enum Action {
    None,
    Submit(Vec<AskAnswer>),
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    Question(usize),
    Review,
}

/// 每题独立的焦点、选集、custom 答案与列表位置。
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
            SelectionMenu::new(menu_items(question, &[], false), initial).with_accent(ACCENT);
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
        let AskRequest { args, .. } = request;
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
    fn initial_focus_recommended_without_preselection() {
        let panel = panel(vec![question("q", false, Some(1), &["A", "B", "C"])]);
        assert_eq!(panel.states[0].menu.selected_index(), Some(1));
        assert!(panel.states[0].selected.is_empty());
        assert!(panel.states[0].custom.is_empty());
        let empty = self::panel(vec![question("q", true, None, &[])]);
        assert_eq!(empty.states[0].menu.selected_index(), Some(0));
    }

    #[test]
    fn recommended_badge_is_display_only_and_labels_sanitized() {
        let mut q = question("q", false, Some(0), &["Postgres", "Da\u{7f}ta"]);
        q.options[1].description = Some("fast\u{1b}[31m".into());
        let mut panel = panel(vec![q]);
        let items = panel.states[0].menu.items();
        assert!(!items[1].label.contains('\u{7f}'));
        assert!(!items[1].description.contains('\u{1b}'));
        let action = panel.key(key(KeyCode::Enter));
        let Action::Submit(answers) = action else {
            panic!("single question single select must submit immediately");
        };
        assert_eq!(answers.len(), 1);
        assert_eq!(answers[0].id, "q");
        assert_eq!(answers[0].selected_options, vec!["Postgres".to_string()]);
        assert_eq!(answers[0].custom_input, None);
    }

    #[test]
    fn single_select_custom_editor_submit_with_multiline() {
        let mut panel = panel(vec![question("q", false, Some(0), &["A", "B"])]);
        panel.key(key(KeyCode::End));
        assert!(matches!(panel.key(key(KeyCode::Enter)), Action::None));
        assert!(panel.editor.is_some());
        panel.paste("  ");
        assert!(matches!(panel.key(key(KeyCode::Enter)), Action::None));
        assert!(panel.editor.is_some());
        assert!(matches!(panel.key(key(KeyCode::Esc)), Action::None));
        assert!(panel.editor.is_none());
        assert!(panel.states[0].custom.is_empty());
        panel.key(key(KeyCode::Enter));
        assert!(panel.editor.is_some());
        panel.paste("第一行");
        panel.key(ctrl('j'));
        panel.paste("second");
        let action = panel.key(key(KeyCode::Enter));
        let Action::Submit(answers) = action else {
            panic!("expected submit after custom answer");
        };
        assert_eq!(answers[0].custom_input, Some("第一行\nsecond".to_string()));
        assert!(answers[0].selected_options.is_empty());
    }

    #[test]
    fn multi_toggle_order_then_required_answer_locates_question() {
        let mut panel = panel(vec![
            question("checks", true, Some(0), &["unit", "lint", "fmt"]),
            question("db", false, Some(0), &["SQLite", "Postgres"]),
        ]);
        panel.key(key(KeyCode::End));
        panel.key(key(KeyCode::Up));
        panel.key(key(KeyCode::Char(' ')));
        panel.key(key(KeyCode::Home));
        panel.key(key(KeyCode::Char(' ')));
        assert_eq!(panel.states[0].selected, vec![0, 2]);
        panel.key(key(KeyCode::Tab));
        panel.key(key(KeyCode::Tab));
        assert_eq!(panel.page, Page::Review);
        assert!(matches!(panel.key(key(KeyCode::Enter)), Action::None));
        assert_eq!(panel.error.as_deref(), Some(REQUIRED_MESSAGE));
        assert_eq!(panel.page, Page::Question(1));
        assert!(matches!(panel.key(key(KeyCode::Enter)), Action::None));
        assert_eq!(panel.page, Page::Review);
        let action = panel.key(key(KeyCode::Enter));
        let Action::Submit(answers) = action else {
            panic!("expected submit after all questions answered");
        };
        assert_eq!(answers[0].id, "checks");
        assert_eq!(
            answers[0].selected_options,
            vec!["unit".to_string(), "fmt".to_string()]
        );
        assert_eq!(answers[0].custom_input, None);
        assert_eq!(answers[1].selected_options, vec!["SQLite".to_string()]);
    }

    #[test]
    fn multi_empty_selection_is_submittable() {
        let mut panel = panel(vec![question("checks", true, None, &["unit", "lint"])]);
        panel.key(key(KeyCode::Tab));
        assert_eq!(panel.page, Page::Review);
        let action = panel.key(key(KeyCode::Enter));
        let Action::Submit(answers) = action else {
            panic!("empty multi selection must submit");
        };
        assert!(answers[0].selected_options.is_empty());
        assert_eq!(answers[0].custom_input, None);
    }

    #[test]
    fn multi_custom_coexists_with_selection() {
        let mut panel = panel(vec![question("checks", true, Some(0), &["unit", "lint"])]);
        panel.key(key(KeyCode::Char(' ')));
        panel.key(key(KeyCode::End));
        panel.key(key(KeyCode::Enter));
        panel.paste("再检查中文");
        panel.key(key(KeyCode::Enter));
        assert_eq!(panel.page, Page::Review);
        let action = panel.key(key(KeyCode::Enter));
        let Action::Submit(answers) = action else {
            panic!("expected submit with selection and custom answer");
        };
        assert_eq!(answers[0].selected_options, vec!["unit".to_string()]);
        assert_eq!(answers[0].custom_input, Some("再检查中文".to_string()));
    }

    #[test]
    fn single_select_option_clears_previous_custom() {
        let mut panel = panel(vec![
            question("a", false, None, &["A", "B"]),
            question("b", true, None, &["x", "y"]),
        ]);
        panel.key(key(KeyCode::End));
        panel.key(key(KeyCode::Enter));
        panel.paste("custom");
        panel.key(key(KeyCode::Enter));
        assert_eq!(panel.page, Page::Question(1));
        panel.key(key(KeyCode::BackTab));
        assert_eq!(panel.page, Page::Question(0));
        panel.key(key(KeyCode::Home));
        assert!(matches!(panel.key(key(KeyCode::Char(' '))), Action::None));
        assert_eq!(panel.page, Page::Question(0));
        assert_eq!(panel.states[0].selected, vec![0]);
        assert!(panel.states[0].custom.is_empty());
        panel.key(key(KeyCode::Enter));
        panel.key(key(KeyCode::Enter));
        assert_eq!(panel.page, Page::Review);
        let action = panel.key(key(KeyCode::Enter));
        let Action::Submit(answers) = action else {
            panic!("expected submit");
        };
        assert_eq!(answers[0].selected_options, vec!["A".to_string()]);
        assert_eq!(answers[0].custom_input, None);
    }

    #[test]
    fn esc_cancels_from_picker_and_review() {
        let mut picker = panel(vec![question("q", false, None, &["A"])]);
        assert!(matches!(picker.key(key(KeyCode::Esc)), Action::Cancel));
        let mut review = panel(vec![question("q", true, None, &["A"])]);
        review.key(key(KeyCode::Tab));
        assert!(matches!(review.key(key(KeyCode::Esc)), Action::Cancel));
    }

    #[test]
    fn tab_and_arrows_cycle_pages() {
        let mut panel = panel(vec![
            question("a", false, None, &["A"]),
            question("b", false, None, &["B"]),
        ]);
        panel.key(key(KeyCode::Tab));
        assert_eq!(panel.page, Page::Question(1));
        panel.key(key(KeyCode::Right));
        assert_eq!(panel.page, Page::Review);
        panel.key(key(KeyCode::Tab));
        assert_eq!(panel.page, Page::Question(0));
        panel.key(key(KeyCode::Left));
        assert_eq!(panel.page, Page::Review);
    }

    #[test]
    fn single_single_select_question_has_no_review_page() {
        let mut panel = panel(vec![question("a", false, None, &["A"])]);
        panel.key(key(KeyCode::Tab));
        assert_eq!(panel.page, Page::Question(0));
    }

    #[test]
    fn single_space_selects_without_submitting_and_details_key_is_ignored() {
        let mut panel = panel(vec![question("q", false, Some(1), &["A", "B"])]);
        assert!(matches!(panel.key(key(KeyCode::Char(' '))), Action::None));
        assert_eq!(panel.page, Page::Question(0));
        assert_eq!(panel.states[0].selected, [1]);
        assert!(!panel.submitted);
        let before = render(&mut panel, 100, 24);
        assert!(before.contains("󰐾 B (Recommended)"));
        assert!(!before.contains("details"));
        assert!(matches!(panel.key(ctrl('o')), Action::None));
        assert_eq!(render(&mut panel, 100, 24), before);
        panel.key(key(KeyCode::Home));
        panel.key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL));
        panel.key(KeyEvent::new_with_kind(
            KeyCode::Char(' '),
            KeyModifiers::NONE,
            KeyEventKind::Repeat,
        ));
        assert_eq!(panel.states[0].selected, [1]);
        panel.key(key(KeyCode::Char(' ')));
        assert_eq!(panel.states[0].selected, [0]);
        assert!(!panel.submitted);
        let Action::Submit(answers) = panel.key(key(KeyCode::Enter)) else {
            panic!("按 Enter 后应提交已选答案");
        };
        assert_eq!(answers[0].selected_options, ["A"]);
    }

    #[test]
    fn wheel_navigates_inside_overlay_only() {
        let mut panel = panel(vec![question("q", false, Some(0), &["A", "B", "C"])]);
        render(&mut panel, 40, 20);
        let area = panel.overlay.expect("overlay cached after draw");
        assert!(panel.wheel(false, area.x, area.bottom() - 1));
        assert_eq!(panel.states[0].menu.selected_index(), Some(1));
        assert!(!panel.wheel(true, area.right(), area.y));
        assert!(!panel.wheel(true, area.x, area.bottom()));
        assert_eq!(panel.states[0].menu.selected_index(), Some(1));
    }

    #[test]
    fn picker_shows_descriptions_and_keeps_previews_in_messages() {
        let mut q = question("q", false, Some(0), &["A"]);
        q.options[0].description = Some("Option description".into());
        q.options[0].preview = Some("# Head\n\ntext".into());
        let mut panel = panel(vec![q]);
        for (width, height) in [(40, 24), (24, 12), (40, 24)] {
            let screen = render(&mut panel, width, height);
            assert!(screen.contains("󰐽 A"), "应显示选项：{screen}");
            assert!(screen.contains("Choose for q?"));
            if width >= 40 {
                assert!(screen.contains("Option description"));
            }
            assert!(!screen.contains("Head"));
            assert!(!screen.contains("text"));
        }
        panel.key(key(KeyCode::Char(' ')));
        let screen = render(&mut panel, 40, 24);
        assert!(screen.contains("󰐾 A"));
    }

    #[test]
    fn compact_mode_renders_without_border() {
        let mut panel = panel(vec![question("db", false, None, &["A", "B"])]);
        let screen = render(&mut panel, 30, 3);
        assert!(screen.contains("Ask"), "紧凑模式应显示 Ask 标题");
        assert!(!screen.contains("db"), "紧凑模式不应显示内部题目 ID");
        let screen = render(&mut panel, 30, 1);
        assert!(
            screen.starts_with('A'),
            "focused option lost on one-row screen: {screen}"
        );
    }

    #[test]
    fn option_display_expands_tabs_without_rewriting_answer_labels() {
        let mut panel = panel(vec![question("q", false, None, &["a\tb\nc"])]);
        let screen = render(&mut panel, 80, 24);
        assert!(
            screen.contains("a   b c"),
            "menu lost visible label whitespace: {screen}"
        );
        let Action::Submit(answers) = panel.key(key(KeyCode::Enter)) else {
            panic!("expected selected answer")
        };
        assert_eq!(answers[0].selected_options, ["a\tb\nc"]);
    }

    #[test]
    fn paste_without_editor_is_ignored() {
        let mut panel = panel(vec![question("q", false, None, &["A"])]);
        assert!(matches!(panel.paste("hello"), Action::None));
        assert!(panel.states[0].custom.is_empty());
    }

    #[test]
    fn custom_option_accepts_direct_typing_and_paste_without_leaving_list() {
        for paste in [false, true] {
            let mut panel = panel(vec![question("q", false, None, &["SQLite"])]);
            panel.key(key(KeyCode::End));
            render(&mut panel, 80, 24);
            let before = panel.overlay;
            if paste {
                panel.paste("answer");
            } else {
                for character in "answer".chars() {
                    panel.key(key(KeyCode::Char(character)));
                }
            }
            let screen = render(&mut panel, 80, 24);
            assert_eq!(panel.overlay, before, "进入 Other 不应改变外框位置和高度");
            assert!(screen.contains("󰐽 SQLite"));
            assert!(screen.contains("󰐽 answer"));
            assert!(!screen.contains("custom answer"));
            panel.key(ctrl('j'));
            panel.paste("second line");
            render(&mut panel, 80, 24);
            let after = panel.overlay.unwrap();
            let before = before.unwrap();
            assert_eq!(after.height, before.height + 1, "新增输入行应扩展面板");
            assert_eq!(after.bottom(), before.bottom(), "面板底部应保持锚定");
            let compact = render(&mut panel, 40, 8);
            assert!(
                compact.contains("second line"),
                "紧凑屏幕也应显示正在编辑的答案：{compact}"
            );
            panel.key(key(KeyCode::Esc));
            assert!(panel.states[0].custom.is_empty());
            assert!(render(&mut panel, 80, 24).contains(OTHER_LABEL));
            panel.paste("accepted");
            let Action::Submit(answers) = panel.key(key(KeyCode::Enter)) else {
                panic!("原位输入应可提交");
            };
            assert_eq!(answers[0].custom_input.as_deref(), Some("accepted"));
        }
    }

    #[test]
    fn single_page_omits_tabs_and_shows_question_above_choices() {
        let mut q = question("confirm_commit", false, None, &["yes", "no"]);
        q.header = Some("Commit confirmation".into());
        q.question = "Commit the changes?".into();
        let mut panel = panel(vec![q]);
        let screen = render(&mut panel, 80, 24);
        assert!(!screen.contains("confirm_commit"));
        assert!(!screen.contains("Commit confirmation"));
        assert!(screen.contains("Commit the changes?"));
        assert!(!screen.contains("Tab/←/→"));
        assert_eq!(
            panel.overlay.unwrap().height,
            8,
            "单页包含题干且不占用页签行"
        );
        let rows: Vec<_> = screen.lines().collect();
        let title = rows.iter().position(|row| row.contains("╭ Ask")).unwrap();
        assert!(rows[title + 1].contains("Commit the changes?"));
        assert!(rows[title + 2].contains("󰐽 yes"), "题干下方应直接显示选项");
        let compact = render(&mut panel, 80, 5);
        assert!(!compact.contains("Tab next"));

        let mut custom_only = self::panel(vec![question("custom", false, None, &[])]);
        let screen = render(&mut custom_only, 80, 24);
        assert_eq!(custom_only.overlay.unwrap().height, 6);
        assert!(screen.contains(OTHER_LABEL));
        assert!(screen.contains("╭ Ask"), "自由回答仍应使用完整边框");
    }

    #[test]
    fn tabs_highlight_current_question_and_review_including_narrow_layout() {
        let mut first = question("first", false, None, &["A"]);
        first.header = Some(" 范围\t".into());
        let second = question("mode", true, None, &["B"]);
        let mut panel = panel(vec![first, second]);
        for (page, label) in [(0, "范围"), (1, "mode"), (2, "Review")] {
            for width in [80, 18] {
                let mut terminal = Terminal::new(TestBackend::new(width, 30)).unwrap();
                terminal
                    .draw(|frame| panel.draw(frame, frame.area()))
                    .unwrap();
                let buffer = terminal.backend().buffer();
                let row = panel.overlay.unwrap().y + 1;
                // 宽字符的后续单元格只是占位，不属于显示文本。
                let columns = (0..width)
                    .filter(|&x| x == 0 || Span::raw(buffer[(x - 1, row)].symbol()).width() <= 1);
                let highlighted: String = columns
                    .clone()
                    .filter(|&x| buffer[(x, row)].bg == ASK_TAB_BG)
                    .map(|x| buffer[(x, row)].symbol())
                    .collect();
                assert_eq!(
                    highlighted,
                    format!(" {label} "),
                    "高亮背景应覆盖当前页签及其左右各一个空格"
                );
                for x in 2..width.saturating_sub(2) {
                    let cell = &buffer[(x, row)];
                    assert_ne!(cell.bg, ACCENT, "页签不应使用亮黄色背景");
                    if cell.bg == ASK_TAB_BG {
                        assert_eq!(cell.fg, ACTIVITY);
                        assert!(!cell.modifier.contains(Modifier::BOLD));
                    }
                }
                if width == 80 {
                    let text: String = columns.map(|x| buffer[(x, row)].symbol()).collect();
                    assert!(
                        text.contains(" 范围   mode   Review "),
                        "页签之间应正好留三个空格：{text}"
                    );
                }
            }
            assert_eq!(
                panel.page,
                if page < 2 {
                    Page::Question(page)
                } else {
                    Page::Review
                }
            );
            panel.key(key(KeyCode::Tab));
        }
    }

    #[test]
    fn question_wraps_and_changes_with_current_page() {
        const LONG: &str = "请选择本次操作需要执行的完整检查范围，并确认后继续";
        let wrapped = |id: &str| {
            let mut question = question(id, false, None, &["A"]);
            question.question = LONG.into();
            question
        };

        // 单题没有 Review 页签，高度只由题干折行决定。
        let mut single = panel(vec![wrapped("first")]);
        let wide = render(&mut single, 80, 30);
        let wide_height = single.overlay.unwrap().height;
        // TestBackend 为双宽字符保留空白占位单元格。
        assert!(wide.replace(' ', "").contains(LONG));
        let narrow = render(&mut single, 32, 30);
        assert!(
            single.overlay.unwrap().height > wide_height,
            "窄屏题干折行应撑高面板"
        );
        assert!(narrow.replace(' ', "").contains("请选择本次操作"));
        assert!(narrow.replace(' ', "").contains("确认后继续"));
        assert!(narrow.contains("󰐽 A"));

        // 多题同一高度下切换页签只换正文。
        let mut pages = panel(vec![
            wrapped("first"),
            question("second", false, None, &["B"]),
        ]);
        let first = render(&mut pages, 80, 30);
        assert!(first.replace(' ', "").contains(LONG));
        pages.key(key(KeyCode::Tab));
        let second = render(&mut pages, 80, 30);
        assert!(second.contains("Choose for second?"));
        assert!(!second.contains("请选择本次操作"));
        assert!(second.contains("󰐽 B"));
    }

    #[test]
    fn height_is_shared_across_tabs_and_tracks_wrapping_editor_and_validation() {
        let mut long = question("long", false, None, &["A", "B", "C"]);
        for option in &mut long.options {
            option.description =
                Some("A detailed option description that wraps at narrow widths".into());
        }
        let mut panel = panel(vec![long, question("short", false, None, &["D"])]);

        // 宽屏：最高的「long」页决定共用高度，切到「short」和 Review 都不变。
        render(&mut panel, 100, 40);
        let shared = panel.overlay.unwrap().height;
        assert_eq!(shared, 13, "题干、三项名称与说明、Other 和五行框架");
        panel.key(key(KeyCode::Tab));
        let screen = render(&mut panel, 100, 40);
        assert!(screen.contains("Choose for short?"));
        assert_eq!(
            panel.overlay.unwrap().height,
            shared,
            "切换页签不应改变面板高度"
        );

        // 校验错误在共用高度之外单独占一行。
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
            "Review 页沿用最高页高度"
        );
        panel.key(key(KeyCode::BackTab));
        panel.key(key(KeyCode::BackTab));
        assert_eq!(panel.page, Page::Question(0));

        // 窄屏：说明折行抬高共用高度，矮页跟随。
        render(&mut panel, 32, 40);
        let wrapped = panel.overlay.unwrap().height;
        assert_eq!(wrapped, 16, "说明折行应计入实际高度");
        panel.key(key(KeyCode::Tab));
        render(&mut panel, 32, 40);
        assert_eq!(
            panel.overlay.unwrap().height,
            wrapped,
            "窄屏切换页签同样保持高度"
        );

        // 短页的多行输入仍在共用高度内展开。
        panel.key(key(KeyCode::End));
        panel.key(key(KeyCode::Enter));
        panel.paste("one\ntwo\nthree");
        let screen = render(&mut panel, 32, 40);
        assert_eq!(
            panel.overlay.unwrap().height,
            wrapped,
            "短页三行输入不应超过共用高度"
        );
        assert!(screen.contains("three"));
        panel.key(key(KeyCode::Esc));
        assert!(panel.states[1].custom.is_empty());

        // Review 的折行答案超过最高题库时，面板按其自身高度增长并受 70% 上限约束。
        panel.key(key(KeyCode::BackTab));
        panel.key(key(KeyCode::End));
        panel.key(key(KeyCode::Enter));
        panel.paste(&"very long answer ".repeat(12));
        panel.key(key(KeyCode::Enter));
        panel.key(key(KeyCode::Tab));
        assert_eq!(panel.page, Page::Review);
        render(&mut panel, 32, 40);
        let review_height = panel.overlay.unwrap().height;
        assert!(review_height > wrapped, "Review 高度应包含答案折行");
        render(&mut panel, 32, 12);
        assert!(
            panel.overlay.unwrap().height <= 8,
            "面板不应超过屏幕高度的 70%"
        );
    }

    #[test]
    fn custom_answer_keeps_raw_controls_but_terminal_cells_are_safe() {
        let mut panel = panel(vec![question("q", false, None, &["A"])]);
        panel.key(key(KeyCode::End));
        panel.key(key(KeyCode::Enter));
        panel.paste("bad\u{1b}[31mtail\r\nnext\rmiddle\r");
        let screen = render(&mut panel, 80, 24);
        assert!(
            !screen.contains('\u{1b}'),
            "unsafe control reached terminal cells"
        );
        let Action::Submit(answers) = panel.key(key(KeyCode::Enter)) else {
            panic!("expected custom answer")
        };
        assert_eq!(
            answers[0].custom_input.as_deref(),
            Some("bad\u{1b}[31mtail\r\nnext\rmiddle\r")
        );
        assert!(matches!(panel.key(key(KeyCode::Enter)), Action::None));
    }
}
