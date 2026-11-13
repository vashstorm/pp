use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};
use pp_config::config::ModelChoice;
use pp_config::config::ModelRole;
use pp_config::config::ModelRoles;
use pp_config::config::RoleAssignment;
use pp_config::config::RoleModel;
use pp_config::model_state::Selection;
use pp_core::model::ThinkLevel;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};

use super::transcript_style as palette;

mod keys;
mod picker;
mod render;
mod rows;

const BG: Color = Color::Rgb(40, 42, 54);
const BORDER: Color = Color::Rgb(36, 126, 155);
const GOLD: Color = Color::Rgb(255, 191, 51);
const TEXT: Color = Color::Rgb(205, 207, 214);
const MUTED: Color = Color::Rgb(98, 104, 118);
const GREEN: Color = Color::Rgb(131, 207, 126);
// 与状态栏／transcript 一致：Nerd Font Material Design 图标占一列，其后保留一列空白。
const ROLES_ICON: &str = "\u{f000e}";
const MODELS_ICON: &str = "\u{f0279}";
const PROVIDER_ICON: &str = "\u{f048d}";
// 角色行前使用全尺寸实心圆／空心圆，保持单列占位与点击位置不变。
const ENABLED_ICON: &str = "\u{f0765}";
const DISABLED_ICON: &str = "\u{f0766}";
const FALLBACK_ICON: &str = "\u{f060d}";
const SEARCH_ICON: &str = "\u{f0349}";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum View {
    Roles,
    Models,
    Effort,
}

pub(super) enum Action {
    None,
    Close,
    Save(ModelRole, Option<RoleAssignment>),
    Favorite(String),
    /// 清除主会话手动覆盖，回到配置默认。
    ClearOverride,
}

pub(super) struct Panel {
    choices: Vec<ModelChoice>,
    favorites: Vec<String>,
    roles: ModelRoles,
    providers: Vec<String>,
    default_model: String,
    default_effort: ThinkLevel,
    /// 当前手动覆盖 `a`；只读展示，清除走 [`Action::ClearOverride`]。
    override_selection: Option<Selection>,
    view: View,
    sidebar: usize,
    sidebar_focus: bool,
    role_index: usize,
    editing_fallback: bool,
    selected: usize,
    query: String,
    pending: Option<RoleAssignment>,
    area: Rect,
    body: Rect,
    sidebar_area: Rect,
    sidebar_scroll: usize,
    scroll: usize,
    pub(super) busy: bool,
    message: String,
}

impl Panel {
    pub(super) fn new(
        choices: Vec<ModelChoice>,
        favorites: Vec<String>,
        roles: ModelRoles,
        default_model: String,
        effort: ThinkLevel,
        override_selection: Option<Selection>,
    ) -> Self {
        let mut panel = Self {
            choices: vec![],
            favorites,
            roles,
            providers: vec![],
            default_model,
            default_effort: effort,
            override_selection,
            view: View::Roles,
            sidebar: 0,
            sidebar_focus: true,
            role_index: 0,
            editing_fallback: false,
            selected: 0,
            query: String::new(),
            pending: None,
            area: Rect::default(),
            body: Rect::default(),
            sidebar_area: Rect::default(),
            sidebar_scroll: 0,
            scroll: 0,
            busy: false,
            message: String::new(),
        };
        panel.update_choices(choices);
        panel
    }

    pub(super) fn update_override(&mut self, selection: Option<Selection>) {
        self.override_selection = selection;
    }

    pub(super) fn update_choices(&mut self, choices: Vec<ModelChoice>) {
        self.providers = choices
            .iter()
            .filter_map(|choice| choice.reference.split_once('/').map(|(p, _)| p.to_owned()))
            .collect();
        self.providers.sort();
        self.providers.dedup();
        self.choices = choices;
        self.sidebar = self.sidebar.min(self.providers.len() + 1);
        self.selected = self
            .selected
            .min(self.model_indices().len().saturating_sub(1));
    }

    pub(super) fn update_favorites(&mut self, favorites: Vec<String>) {
        if self.favorites == favorites {
            return;
        }
        let selected = (self.view == View::Models)
            .then(|| self.model_indices().get(self.selected).copied())
            .flatten();
        self.favorites = favorites;
        if let Some(selected) = selected {
            self.selected = self
                .model_indices()
                .iter()
                .position(|&index| index == selected)
                .unwrap_or(0);
        }
    }

    pub(super) fn update_roles(
        &mut self,
        roles: ModelRoles,
        default_model: String,
        effort: ThinkLevel,
    ) {
        let selected = (self.role(), self.on_fallback());
        self.roles = roles;
        let rows = self.rows();
        self.role_index = rows
            .iter()
            .position(|r| *r == selected)
            .or_else(|| rows.iter().position(|r| *r == (selected.0, false)))
            .unwrap_or(0);
        self.default_model = default_model;
        self.default_effort = effort;
    }

    pub(super) fn saved(&mut self) {
        self.busy = false;
        self.view = View::Roles;
        self.sidebar = 0;
        self.sidebar_focus = false;
        self.pending = None;
        self.message.clear();
        self.scroll = 0;
    }

    pub(super) fn runtime_status(&mut self, message: String) {
        self.message = pp_core::text::display_text(&message).into_owned();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;
    use ratatui::{Terminal, backend::TestBackend};

    fn panel() -> Panel {
        let choices = ["one", "two"]
            .into_iter()
            .map(|name| ModelChoice {
                reference: format!("fixture/{name}"),
                model: name.into(),
                protocol: pp_config::models::Protocol::Completion,
                thinking: if name == "two" {
                    pp_config::models::ModelThinking::without_reasoning()
                } else {
                    Default::default()
                },
                context_window: None,
            })
            .collect();
        Panel::new(
            choices,
            vec![],
            ModelRoles::new(),
            "fixture/one".into(),
            ThinkLevel::High,
            None,
        )
    }

    fn key(panel: &mut Panel, code: KeyCode) -> Action {
        panel.handle(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)))
    }

    fn draw(panel: &mut Panel, width: u16, height: u16) -> ratatui::buffer::Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| panel.draw(frame, frame.area()))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    #[test]
    fn model_favorites_sort_with_filters_and_preserve_selection() {
        let mut panel = panel();
        panel.begin_models();
        panel.selected = 1;
        panel.update_favorites(vec!["fixture/two".into(), "unavailable/model".into()]);
        assert_eq!(panel.model_indices(), vec![1, 0]);
        assert_eq!(panel.selected, 0);
        let buffer = draw(&mut panel, 140, 24);
        let text = buffer
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(text.contains("❯ fixture/two \u{f005}"));
        assert!(text.contains("Ctrl + F favorite"));
        panel.sidebar = 2;
        panel.query = "TWO".into();
        assert_eq!(panel.model_indices(), vec![1]);
        panel.update_favorites(vec![]);
        assert_eq!(panel.model_indices(), vec![1]);
        assert_eq!(panel.selected, 0);
        panel.query.clear();
        assert_eq!(panel.model_indices(), vec![0, 1]);
    }

    #[test]
    fn favorite_shortcut_only_handles_model_rows_and_key_presses() {
        let mut panel = panel();
        let shortcut = KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL);
        assert!(matches!(panel.handle(Event::Key(shortcut)), Action::None));
        panel.begin_models();
        for kind in [KeyEventKind::Repeat, KeyEventKind::Release] {
            assert!(matches!(
                panel.handle(Event::Key(KeyEvent { kind, ..shortcut })),
                Action::None
            ));
        }
        assert!(
            matches!(panel.handle(Event::Key(shortcut)), Action::Favorite(reference) if reference == "fixture/one")
        );
        assert!(panel.query.is_empty());
        panel.query = "missing".into();
        assert!(matches!(panel.handle(Event::Key(shortcut)), Action::None));
        panel.query.clear();
        panel.sidebar_focus = true;
        assert!(matches!(panel.handle(Event::Key(shortcut)), Action::None));
    }

    #[test]
    fn roles_layout_has_seven_roles_sidebar_colors_and_independent_efforts() {
        let mut panel = panel();
        panel.roles.insert(
            ModelRole::Fast,
            RoleAssignment {
                fallback: None,
                enabled: true,
                model: "fixture/two".into(),
                effort: ThinkLevel::None,
            },
        );
        let buffer = draw(&mut panel, 140, 24);
        let text = buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        for label in [
            "Models",
            "Roles",
            "All models",
            "DEFAULT",
            "FAST",
            "SLOW",
            "VISION",
            "PLAN",
            "REVIEW",
            "EXPLORER",
            "high",
            "none",
            "1/7",
        ] {
            assert!(text.contains(label), "{label}: {text}");
        }
        assert!(!text.contains("SMOL"));
        assert_eq!(buffer[(50, 20)].bg, BG);
        assert_eq!(buffer[(0, 5)].fg, BORDER);
        for (width, height) in [(80, 12), (42, 8), (12, 4), (1, 1), (140, 24)] {
            draw(&mut panel, width, height);
        }
    }

    #[test]
    fn roles_icons_match_statusline_glyphs_without_moving_the_click_column() {
        let mut panel = panel();
        panel.sidebar_focus = false;
        panel.roles.insert(
            ModelRole::Default,
            RoleAssignment {
                enabled: true,
                model: "fixture/one".into(),
                effort: ThinkLevel::High,
                fallback: Some(RoleModel {
                    model: "fixture/two".into(),
                    effort: ThinkLevel::None,
                }),
            },
        );
        let buffer = draw(&mut panel, 140, 24);
        let text = buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        for icon in [
            ROLES_ICON,
            MODELS_ICON,
            PROVIDER_ICON,
            ENABLED_ICON,
            DISABLED_ICON,
            FALLBACK_ICON,
        ] {
            assert!(text.contains(icon), "面板应显示 Nerd Font 图标：{icon}");
            assert!(
                icon.chars()
                    .next()
                    .is_some_and(|glyph| ('\u{f0000}'..='\u{ffffd}').contains(&glyph)),
                "图标应与状态栏一样使用 Material Design 字形：{icon}"
            );
            assert_eq!(Span::raw(icon).width(), 1, "图标布局宽度应为一列：{icon}");
        }
        // effort 标签复用状态栏的思考档位图标。
        assert!(text.contains(palette::think_icon(ThinkLevel::High)));
        assert!(text.contains(palette::think_icon(ThinkLevel::None)));
        let x = panel.body.x;
        let y = panel.body.y + panel.content_offset();
        assert_eq!(
            buffer[(x + 2, y)].symbol(),
            ENABLED_ICON,
            "启用图标必须落在鼠标点击列"
        );
        assert_eq!(buffer[(x + 11, y + 1)].symbol(), FALLBACK_ICON);
        assert_eq!(buffer[(x + 11, y + 1)].fg, MUTED);
        assert!(!text.contains("FALLBACK"), "备用行只显示缩进箭头");
        for (row, model, effort) in [
            (y, "fixture/one", ThinkLevel::High),
            (y + 1, "fixture/two", ThinkLevel::None),
        ] {
            let model_x = x + 13;
            assert_eq!(buffer[(model_x + 8, row)].fg, GREEN);
            let effort_x = model_x + model.len() as u16 + 1;
            assert_eq!(
                buffer[(effort_x, row)].symbol(),
                palette::think_icon(effort)
            );
            let rendered_effort = (effort_x + 2..effort_x + 2 + effort.as_str().len() as u16)
                .map(|col| buffer[(col, row)].symbol())
                .collect::<String>();
            assert_eq!(rendered_effort, effort.as_str(), "主备各自显示 effort");
        }
        // 长引用不能挤掉 effort，窄面板仍可查看主备各自的档位。
        panel.roles.get_mut(&ModelRole::Default).unwrap().model =
            format!("fixture/{}", "long-model".repeat(10));
        panel
            .roles
            .get_mut(&ModelRole::Default)
            .unwrap()
            .fallback
            .as_mut()
            .unwrap()
            .model = format!("fixture/{}", "long-backup".repeat(10));
        for width in [42, 80, 140] {
            let buffer = draw(&mut panel, width, 24);
            let y = panel.body.y + panel.content_offset();
            for (row, effort) in [(y, "high"), (y + 1, "none")] {
                let text = (panel.body.x..panel.body.right())
                    .map(|col| buffer[(col, row)].symbol())
                    .collect::<String>();
                assert!(text.ends_with(effort), "长模型名仍应保留 effort：{text}");
            }
        }
        draw(&mut panel, 140, 24);
        let Action::Save(ModelRole::Default, Some(off)) =
            panel.handle(Event::Mouse(crossterm::event::MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: x + 2,
                row: y,
                modifiers: KeyModifiers::NONE,
            }))
        else {
            panic!("点击开关图标应切换启用状态");
        };
        assert!(!off.enabled);
        assert!(off.fallback.is_some(), "切换不能丢失备用模型");
    }

    #[test]
    fn roles_choose_model_then_only_supported_effort_before_atomic_save() {
        let mut panel = panel();
        key(&mut panel, KeyCode::Right);
        key(&mut panel, KeyCode::Down);
        assert_eq!(panel.role(), ModelRole::Fast);
        key(&mut panel, KeyCode::Enter);
        panel.handle(Event::Paste("two".into()));
        assert!(matches!(key(&mut panel, KeyCode::Enter), Action::None));
        assert_eq!(panel.efforts(), [ThinkLevel::None]);
        let Action::Save(role, Some(assignment)) = key(&mut panel, KeyCode::Enter) else {
            panic!("应一次保存 model 与 effort");
        };
        assert_eq!(role, ModelRole::Fast);
        assert_eq!(assignment.model, "fixture/two");
        assert_eq!(assignment.effort, ThinkLevel::None);
        assert!(panel.roles.is_empty());
    }

    #[test]
    fn roles_cancel_and_clear_and_busy_never_leak_partial_configuration() {
        let mut panel = panel();
        key(&mut panel, KeyCode::Right);
        key(&mut panel, KeyCode::Enter);
        key(&mut panel, KeyCode::Enter);
        key(&mut panel, KeyCode::Esc);
        key(&mut panel, KeyCode::Esc);
        assert!(panel.roles.is_empty());
        assert!(matches!(
            key(&mut panel, KeyCode::Delete),
            Action::Save(ModelRole::Default, None)
        ));
        panel.busy = true;
        assert!(matches!(key(&mut panel, KeyCode::Enter), Action::None));
        assert!(matches!(key(&mut panel, KeyCode::Esc), Action::None));
        panel.saved();
        assert!(matches!(key(&mut panel, KeyCode::Esc), Action::Close));
    }

    #[test]
    fn roles_only_enabled_rows_are_colored_and_space_or_dot_toggles_preserving_assignment() {
        let mut panel = panel();
        let mut value = RoleAssignment {
            fallback: None,
            enabled: true,
            model: "fixture/two".into(),
            effort: ThinkLevel::None,
        };
        panel.roles.insert(ModelRole::Fast, value.clone());
        panel.roles.insert(
            ModelRole::Review,
            RoleAssignment {
                enabled: false,
                ..value.clone()
            },
        );
        key(&mut panel, KeyCode::Right);
        key(&mut panel, KeyCode::Down);
        key(&mut panel, KeyCode::Down);
        let buffer = draw(&mut panel, 140, 24);
        let x = panel.body.x;
        let y = panel.body.y + panel.content_offset();
        for row in [0, 2, 3, 4, 5] {
            for col in x..panel.body.right() {
                let cell = &buffer[(col, y + row)];
                if !cell.symbol().trim().is_empty() {
                    assert_eq!(cell.fg, MUTED, "未选中的行不能因光标高亮变彩色");
                }
            }
        }
        assert_ne!(buffer[(x + 2, y + 1)].fg, MUTED);
        key(&mut panel, KeyCode::Up);
        let Action::Save(ModelRole::Fast, Some(off)) = key(&mut panel, KeyCode::Char(' ')) else {
            panic!("空格应切换启用状态");
        };
        value.enabled = false;
        assert_eq!(off, value);
        panel.roles.insert(ModelRole::Fast, off);
        draw(&mut panel, 140, 24);
        let Action::Save(ModelRole::Fast, Some(on)) =
            panel.handle(Event::Mouse(crossterm::event::MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: x + 2,
                row: y + 1,
                modifiers: KeyModifiers::NONE,
            }))
        else {
            panic!("点击圆点应切换启用状态");
        };
        assert!(on.enabled);
        assert_eq!(on.model, value.model);
        assert_eq!(on.effort, value.effort);
    }
    #[test]
    fn fallback_roles_shortcuts_select_clear_and_preserve_disabled_primary() {
        let mut panel = panel();
        panel.roles.insert(
            ModelRole::Default,
            RoleAssignment {
                enabled: true,
                model: "fixture/one".into(),
                effort: ThinkLevel::High,
                fallback: Some(RoleModel {
                    model: "fixture/two".into(),
                    effort: ThinkLevel::None,
                }),
            },
        );
        key(&mut panel, KeyCode::Right);
        key(&mut panel, KeyCode::Down);
        assert!(panel.on_fallback());
        assert_eq!(panel.role(), ModelRole::Default);
        key(&mut panel, KeyCode::Char('t'));
        assert_eq!(panel.efforts(), [ThinkLevel::None]);
        let Action::Save(_, Some(updated)) = key(&mut panel, KeyCode::Enter) else {
            panic!("应保存独立 thinking");
        };
        assert_eq!(updated.effort, ThinkLevel::High);
        panel.saved();
        let Action::Save(_, Some(updated)) = key(&mut panel, KeyCode::Char('x')) else {
            panic!("应仅清除备用模型");
        };
        assert!(updated.fallback.is_none());
        assert!(!updated.model.is_empty());
        key(&mut panel, KeyCode::Up);
        let Action::Save(_, Some(updated)) = key(&mut panel, KeyCode::Char('x')) else {
            panic!("应保留备用配置");
        };
        assert!(updated.model.is_empty());
        assert!(updated.fallback.is_some());
        assert!(!updated.enabled);
        panel.roles.insert(ModelRole::Default, updated);
        assert!(matches!(key(&mut panel, KeyCode::Char(' ')), Action::None));
        key(&mut panel, KeyCode::Enter);
        key(&mut panel, KeyCode::Enter);
        let Action::Save(_, Some(updated)) = key(&mut panel, KeyCode::Enter) else {
            panic!("应补齐主模型");
        };
        assert!(!updated.enabled);
        assert!(updated.fallback.is_some());
        panel.saved();
        key(&mut panel, KeyCode::Char('f'));
        assert!(panel.editing_fallback);
        key(&mut panel, KeyCode::Esc);
        assert_eq!(panel.view, View::Roles);
        for (width, height) in [(140, 24), (70, 7), (12, 4)] {
            draw(&mut panel, width, height);
        }
    }
    #[test]
    fn fallback_roles_allow_selecting_the_same_model_in_either_row() {
        for editing_fallback in [false, true] {
            let mut panel = panel();
            let value = RoleAssignment {
                enabled: true,
                model: "fixture/one".into(),
                effort: ThinkLevel::High,
                fallback: Some(RoleModel {
                    model: "fixture/one".into(),
                    effort: ThinkLevel::Low,
                }),
            };
            panel.roles.insert(ModelRole::Default, value.clone());
            key(&mut panel, KeyCode::Right);
            if editing_fallback {
                key(&mut panel, KeyCode::Down);
            }
            key(&mut panel, KeyCode::Enter);
            key(&mut panel, KeyCode::Enter);
            assert_eq!(panel.view, View::Effort);
            assert!(panel.message.is_empty());
            let Action::Save(ModelRole::Default, Some(updated)) = key(&mut panel, KeyCode::Enter)
            else {
                panic!("主备模型相同时也应允许保存");
            };
            assert_eq!(updated, value);
        }
    }

    #[test]
    fn fallback_roles_can_be_configured_before_primary() {
        let mut panel = panel();
        key(&mut panel, KeyCode::Right);
        key(&mut panel, KeyCode::Char('f'));
        key(&mut panel, KeyCode::Enter);
        let Action::Save(ModelRole::Default, Some(assignment)) = key(&mut panel, KeyCode::Enter)
        else {
            panic!("缺主模型时应允许保存备用");
        };
        assert!(assignment.model.is_empty());
        assert!(!assignment.enabled);
        assert_eq!(assignment.fallback.unwrap().model, "fixture/one");
    }

    #[test]
    fn override_row_is_read_only_and_backspace_clears_it_only_when_present() {
        let mut panel = panel();
        key(&mut panel, KeyCode::Right);
        // 无覆盖时 Backspace 不产生清除动作，也不误删角色。
        assert!(matches!(key(&mut panel, KeyCode::Backspace), Action::None));
        let buffer = draw(&mut panel, 140, 24);
        let text = buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("Override"));
        assert!(text.contains("none"));
        assert!(text.contains("Backspace clear override"));

        panel.update_override(Some(Selection {
            reference: "fixture/two".into(),
            effort: ThinkLevel::None,
        }));
        let buffer = draw(&mut panel, 140, 24);
        let rows = buffer
            .content
            .chunks(usize::from(buffer.area.width))
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>();
        let override_row = rows
            .iter()
            .find(|row| row.contains("Override"))
            .expect("Override 只读行");
        assert!(override_row.contains("fixture/two"), "{override_row}");
        // effort 列显示 none 档位；模型列本身不能再出现 none 占位。
        let (model_part, _) = override_row
            .split_once(palette::think_icon(ThinkLevel::None))
            .unwrap_or((override_row.as_str(), ""));
        assert!(!model_part.contains("none"), "{override_row}");
        assert!(matches!(
            key(&mut panel, KeyCode::Backspace),
            Action::ClearOverride
        ));
        // 只读行不参与角色选择：Delete 仍是删除选中角色。
        assert!(matches!(
            key(&mut panel, KeyCode::Delete),
            Action::Save(ModelRole::Default, None)
        ));
        // 视图切换与窄面板不得 panic。
        for (width, height) in [(42, 12), (20, 6), (8, 4)] {
            draw(&mut panel, width, height);
        }
    }
}
