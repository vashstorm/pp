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
    widgets::{Block, Borders, Clear, Paragraph},
};

use super::transcript_style as palette;

mod keys;
mod picker;
mod render;
mod rows;

fn bg() -> Color {
    super::themes::palette().panel_bg
}

fn border() -> Color {
    super::themes::palette().panel_border
}

fn gold() -> Color {
    super::themes::palette().accent
}

fn text_fg() -> Color {
    super::themes::palette().text
}

fn muted() -> Color {
    if super::themes::current_theme() == pp_config::config::Theme::Default {
        Color::Rgb(98, 104, 118)
    } else {
        super::themes::palette().muted
    }
}

fn green() -> Color {
    if super::themes::current_theme() == pp_config::config::Theme::Default {
        Color::Rgb(131, 207, 126)
    } else {
        super::themes::palette().added
    }
}

// 与状态栏／transcript 一致：Nerd Font Material Design 图标与 ASCII 占位均为 1 列宽。
fn roles_icon() -> &'static str {
    if super::themes::is_ascii() {
        "R"
    } else {
        "\u{f000e}"
    }
}

fn models_icon() -> &'static str {
    if super::themes::is_ascii() {
        "M"
    } else {
        "\u{f0279}"
    }
}

fn provider_icon() -> &'static str {
    if super::themes::is_ascii() {
        "P"
    } else {
        "\u{f048d}"
    }
}

fn enabled_icon() -> &'static str {
    if super::themes::is_ascii() {
        "*"
    } else {
        "\u{f0765}"
    }
}

fn disabled_icon() -> &'static str {
    if super::themes::is_ascii() {
        "o"
    } else {
        "\u{f0766}"
    }
}

fn fallback_icon() -> &'static str {
    if super::themes::is_ascii() {
        ">"
    } else {
        "\u{f060d}"
    }
}

fn search_icon() -> &'static str {
    if super::themes::is_ascii() {
        "/"
    } else {
        "\u{f0349}"
    }
}

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
    pub(super) area: Rect,
    body: Rect,
    sidebar_area: Rect,
    has_top_bar: bool,
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
            has_top_bar: false,
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

    pub(super) fn is_typing_input(&self) -> bool {
        self.view == View::Models && !self.sidebar_focus
    }

    /// 模型／档位选择器打开时，←/→ 与 Tab 归选择器，外层页签让位。
    pub(super) fn is_picking(&self) -> bool {
        self.view != View::Roles
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
    fn model_favorites_sort_and_shortcut() {
        let mut p1 = panel();
        p1.begin_models();
        p1.selected = 1;
        p1.update_favorites(vec!["fixture/two".into(), "unavailable/model".into()]);
        assert_eq!(p1.model_indices(), vec![1, 0]);
        assert_eq!(p1.selected, 0);
        let text: String = draw(&mut p1, 140, 24)
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("❯ fixture/two \u{f005}") && text.contains("Ctrl + F favorite"));
        p1.sidebar = 2;
        p1.query = "TWO".into();
        p1.update_favorites(vec![]);
        assert_eq!((p1.model_indices(), p1.selected), (vec![1], 0));
        p1.query.clear();
        assert_eq!(p1.model_indices(), vec![0, 1]);

        let mut p2 = panel();
        let shortcut = KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL);
        assert!(matches!(p2.handle(Event::Key(shortcut)), Action::None));
        p2.begin_models();
        for kind in [KeyEventKind::Repeat, KeyEventKind::Release] {
            assert!(matches!(
                p2.handle(Event::Key(KeyEvent { kind, ..shortcut })),
                Action::None
            ));
        }
        assert!(
            matches!(p2.handle(Event::Key(shortcut)), Action::Favorite(r) if r == "fixture/one")
        );
        p2.query = "missing".into();
        assert!(matches!(p2.handle(Event::Key(shortcut)), Action::None));
        p2.query.clear();
        p2.sidebar_focus = true;
        assert!(matches!(p2.handle(Event::Key(shortcut)), Action::None));
    }

    #[test]
    fn roles_layout_icons_and_enabled_toggles() {
        let mut p1 = panel();
        p1.roles.insert(
            ModelRole::Fast,
            RoleAssignment {
                fallback: None,
                enabled: true,
                model: "fixture/two".into(),
                effort: ThinkLevel::None,
            },
        );
        let b1 = draw(&mut p1, 140, 24);
        let t1: String = b1.content.iter().map(|c| c.symbol()).collect();
        for label in [
            "Model roles",
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
            assert!(t1.contains(label), "{label}: {t1}");
        }
        assert!(!t1.contains("SMOL") && b1[(50, 20)].bg == bg() && b1[(0, 5)].fg == border());
        for (w, h) in [(80, 12), (42, 8), (12, 4), (1, 1)] {
            draw(&mut p1, w, h);
        }

        let mut p2 = panel();
        p2.sidebar_focus = false;
        p2.roles.insert(
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
        let b2 = draw(&mut p2, 140, 24);
        let t2: String = b2.content.iter().map(|c| c.symbol()).collect();
        for icon in [
            roles_icon(),
            models_icon(),
            provider_icon(),
            enabled_icon(),
            disabled_icon(),
            fallback_icon(),
        ] {
            assert!(t2.contains(icon) && Span::raw(icon).width() == 1);
        }
        let x = p2.body.x;
        let y = p2.body.y + p2.content_offset();
        assert_eq!(
            (
                b2[(x + 2, y)].symbol(),
                b2[(x + 11, y + 1)].symbol(),
                b2[(x + 11, y + 1)].fg
            ),
            (enabled_icon(), fallback_icon(), muted())
        );
        assert!(!t2.contains("FALLBACK"));
        p2.roles.get_mut(&ModelRole::Default).unwrap().model =
            format!("fixture/{}", "long-model".repeat(10));
        for width in [42, 80, 140] {
            let bw = draw(&mut p2, width, 24);
            let yw = p2.body.y + p2.content_offset();
            let row: String = (p2.body.x..p2.body.right())
                .map(|c| bw[(c, yw)].symbol())
                .collect();
            assert!(row.ends_with("high"));
        }
        draw(&mut p2, 140, 24);
        let Action::Save(ModelRole::Default, Some(off)) =
            p2.handle(Event::Mouse(crossterm::event::MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: x + 2,
                row: y,
                modifiers: KeyModifiers::NONE,
            }))
        else {
            panic!("点击开关图标应切换启用状态");
        };
        assert!(!off.enabled && off.fallback.is_some());

        let mut p3 = panel();
        let mut val = RoleAssignment {
            fallback: None,
            enabled: true,
            model: "fixture/two".into(),
            effort: ThinkLevel::None,
        };
        p3.roles.insert(ModelRole::Fast, val.clone());
        p3.roles.insert(
            ModelRole::Review,
            RoleAssignment {
                enabled: false,
                ..val.clone()
            },
        );
        key(&mut p3, KeyCode::Tab);
        key(&mut p3, KeyCode::Down);
        key(&mut p3, KeyCode::Down);
        let b3 = draw(&mut p3, 140, 24);
        let x3 = p3.body.x;
        let y3 = p3.body.y + p3.content_offset();
        assert_ne!(b3[(x3 + 2, y3 + 1)].fg, muted());
        assert_eq!(b3[(x3 + 13, y3 + 4)].fg, muted());
        key(&mut p3, KeyCode::Up);
        let Action::Save(ModelRole::Fast, Some(off3)) = key(&mut p3, KeyCode::Char(' ')) else {
            panic!("空格应切换启用状态");
        };
        val.enabled = false;
        assert_eq!(off3, val);
    }

    #[test]
    fn roles_effort_selection_cycling_cancel_and_busy() {
        let mut p1 = panel();
        key(&mut p1, KeyCode::Tab);
        key(&mut p1, KeyCode::Down);
        assert_eq!(p1.role(), ModelRole::Fast);
        key(&mut p1, KeyCode::Enter);
        p1.handle(Event::Paste("two".into()));
        assert!(matches!(key(&mut p1, KeyCode::Enter), Action::None));
        assert_eq!(p1.efforts(), [ThinkLevel::None]);
        let Action::Save(ModelRole::Fast, Some(a1)) = key(&mut p1, KeyCode::Enter) else {
            panic!("应一次保存");
        };
        assert_eq!(
            (a1.model.as_str(), a1.effort),
            ("fixture/two", ThinkLevel::None)
        );

        let mut p2 = panel();
        key(&mut p2, KeyCode::Tab);
        key(&mut p2, KeyCode::Enter);
        key(&mut p2, KeyCode::Enter);
        let selected_bg = super::super::themes::palette().panel_alt_bg;
        for (width, expected_rows) in [(110, 1), (42, 2)] {
            let buffer = draw(&mut p2, width, 20);
            let mut rows = std::collections::BTreeSet::new();
            for level in ThinkLevel::ALL {
                let label = format!("{} {} ", palette::think_icon(level), level.as_str());
                let (row, cells, start) = buffer
                    .content
                    .chunks(usize::from(width))
                    .enumerate()
                    .find_map(|(r, cs)| {
                        let t: String = cs.iter().map(|c| c.symbol()).collect();
                        let s = t.find(&label)?;
                        Some((r, cs, t[..s].chars().count()))
                    })
                    .unwrap();
                let focused = level == ThinkLevel::High;
                assert_eq!(cells[start - 2].symbol(), if focused { "[" } else { " " });
                assert_eq!(
                    cells[start + 2].bg,
                    if focused { selected_bg } else { bg() }
                );
                rows.insert(row);
            }
            assert_eq!(rows.len(), expected_rows);
        }
        key(&mut p2, KeyCode::Right);
        key(&mut p2, KeyCode::Right);
        let focused: String = draw(&mut p2, 18, 7)
            .content
            .iter()
            .filter(|c| c.bg == selected_bg)
            .map(|c| c.symbol())
            .collect();
        assert_eq!(
            focused,
            format!("[ {} max ]", palette::think_icon(ThinkLevel::Max))
        );

        let mut p3 = panel();
        key(&mut p3, KeyCode::Tab);
        key(&mut p3, KeyCode::Enter);
        key(&mut p3, KeyCode::Enter);
        key(&mut p3, KeyCode::Esc);
        key(&mut p3, KeyCode::Esc);
        assert!(
            p3.roles.is_empty()
                && matches!(
                    key(&mut p3, KeyCode::Delete),
                    Action::Save(ModelRole::Default, None)
                )
        );
        p3.busy = true;
        assert!(
            matches!(key(&mut p3, KeyCode::Enter), Action::None)
                && matches!(key(&mut p3, KeyCode::Esc), Action::None)
        );
        p3.saved();
        assert!(matches!(key(&mut p3, KeyCode::Esc), Action::Close));

        let mut p4 = panel();
        key(&mut p4, KeyCode::Enter);
        key(&mut p4, KeyCode::Enter);
        key(&mut p4, KeyCode::Enter);
        key(&mut p4, KeyCode::Tab);
        key(&mut p4, KeyCode::Right);
        assert_eq!(p4.selected, 5);
        key(&mut p4, KeyCode::Left);
        key(&mut p4, KeyCode::BackTab);
        key(&mut p4, KeyCode::Left);
        key(&mut p4, KeyCode::Left);
        key(&mut p4, KeyCode::Left);
        assert!(!p4.sidebar_focus && p4.selected == 0);
        let Action::Save(ModelRole::Default, Some(a4)) = key(&mut p4, KeyCode::Enter) else {
            panic!("save");
        };
        assert_eq!(a4.effort, p4.efforts()[0]);
    }

    #[test]
    fn fallback_roles_shortcuts_same_model_and_before_primary() {
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
        key(&mut panel, KeyCode::Tab);
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

        for editing_fallback in [false, true] {
            let mut ps = self::panel();
            let value = RoleAssignment {
                enabled: true,
                model: "fixture/one".into(),
                effort: ThinkLevel::High,
                fallback: Some(RoleModel {
                    model: "fixture/one".into(),
                    effort: ThinkLevel::Low,
                }),
            };
            ps.roles.insert(ModelRole::Default, value.clone());
            key(&mut ps, KeyCode::Tab);
            if editing_fallback {
                key(&mut ps, KeyCode::Down);
            }
            key(&mut ps, KeyCode::Enter);
            key(&mut ps, KeyCode::Enter);
            let Action::Save(ModelRole::Default, Some(updated)) = key(&mut ps, KeyCode::Enter)
            else {
                panic!("主备模型相同时也应允许保存");
            };
            assert_eq!(updated, value);
        }

        let mut pb = self::panel();
        key(&mut pb, KeyCode::Tab);
        key(&mut pb, KeyCode::Char('f'));
        key(&mut pb, KeyCode::Enter);
        let Action::Save(ModelRole::Default, Some(ab)) = key(&mut pb, KeyCode::Enter) else {
            panic!("缺主模型时应允许保存备用");
        };
        assert!(ab.model.is_empty() && !ab.enabled && ab.fallback.unwrap().model == "fixture/one");
    }

    #[test]
    fn override_row_is_read_only_and_backspace_clears_it_only_when_present() {
        let mut panel = panel();
        key(&mut panel, KeyCode::Tab);
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
