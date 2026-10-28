//! 未保存规划的本地放弃确认，不读取文件或调用模型。
use super::menu::{MenuEvent, MenuItem, SelectionMenu};
use crossterm::event::KeyEvent;
use ratatui::{
    Frame,
    layout::Rect,
    widgets::{Block, Borders, Paragraph, Wrap},
};

pub(super) enum Action {
    None,
    Cancel,
    Discard,
}

pub(super) struct Panel {
    pub(super) plan_id: String,
    pub(super) reenter: bool,
    menu: SelectionMenu<bool>,
}

impl Panel {
    pub(super) fn new(plan_id: String, reenter: bool) -> Self {
        let menu = SelectionMenu::new(
            vec![
                MenuItem {
                    value: false,
                    label: "取消，继续规划".into(),
                    description: String::new(),
                    icon: None,
                    current: false,
                },
                MenuItem {
                    value: true,
                    label: if reenter {
                        "放弃修改并新建规划"
                    } else {
                        "放弃修改并退出"
                    }
                    .into(),
                    description: String::new(),
                    icon: None,
                    current: false,
                },
            ],
            0,
        )
        .with_hint("↑↓ 选择 · Enter 确认 · Esc 取消");
        Self {
            plan_id,
            reenter,
            menu,
        }
    }

    pub(super) fn key(&mut self, key: KeyEvent) -> Action {
        match self.menu.handle_key(key) {
            MenuEvent::Cancel | MenuEvent::Confirm(false) => Action::Cancel,
            MenuEvent::Confirm(true) => Action::Discard,
            _ => Action::None,
        }
    }

    pub(super) fn draw(&mut self, frame: &mut Frame, area: Rect) {
        let width = area.width.min(76);
        let height = area.height.min(9);
        let dialog = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 2,
            width,
            height,
        );
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" 放弃未保存的修改？ ");
        let inner = block.inner(dialog);
        frame.render_widget(block, dialog);
        let menu_height = self.menu.height(inner.height);
        let message_height = inner.height.saturating_sub(menu_height);
        frame.render_widget(
            Paragraph::new(if self.reenter {
                "当前规划尚未保存。是否放弃这些修改并开始新的规划？已保存的文件和对话历史会保留。"
            } else {
                "当前规划尚未保存。是否放弃这些修改并退出 Plan 模式？已保存的文件和对话历史会保留。"
            })
            .wrap(Wrap { trim: false }),
            Rect::new(inner.x, inner.y, inner.width, message_height),
        );
        self.menu.draw(
            frame,
            Rect::new(inner.x, inner.y + message_height, inner.width, menu_height),
        );
    }
}
