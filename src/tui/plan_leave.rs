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
                    label: "Cancel, keep planning".into(),
                    description: String::new(),
                    icon: None,
                    current: false,
                },
                MenuItem {
                    value: true,
                    label: if reenter {
                        "Discard changes and start a new plan"
                    } else {
                        "Discard changes and exit"
                    }
                    .into(),
                    description: String::new(),
                    icon: None,
                    current: false,
                },
            ],
            0,
        )
        .with_hint("↑↓ Navigate · Enter Confirm · Esc Cancel");
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
            .title(" Discard unsaved changes? ");
        let inner = block.inner(dialog);
        frame.render_widget(block, dialog);
        let menu_height = self.menu.height(inner.height);
        let message_height = inner.height.saturating_sub(menu_height);
        frame.render_widget(
            Paragraph::new(if self.reenter {
                "The current plan has not been saved. Discard these changes and start a new plan? Saved files and conversation history are kept."
            } else {
                "The current plan has not been saved. Discard these changes and exit Plan mode? Saved files and conversation history are kept."
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
