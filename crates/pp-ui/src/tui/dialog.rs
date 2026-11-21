//! 交互请求共用回复通道，各面板独立处理布局与输入。

use pp_core::interaction::{AskAnswer, AskRequest};
use ratatui::{Frame, layout::Rect};

use super::{ask, permissions};

#[derive(Debug)]
pub(super) enum Action {
    None,
    Submit(Vec<AskAnswer>),
    Cancel,
}

pub(super) enum Panel {
    Ask(Box<ask::Panel>),
    Permission(permissions::Prompt),
}

impl Panel {
    pub(super) fn new(request: AskRequest) -> Self {
        if permissions::is_prompt(&request.call_id) {
            Self::Permission(permissions::Prompt::new(request.args))
        } else {
            Self::Ask(Box::new(ask::Panel::new(request)))
        }
    }

    pub(super) fn height(&mut self, available: Rect, screen_height: u16) -> u16 {
        match self {
            Self::Ask(panel) => panel.height(available, screen_height),
            Self::Permission(panel) => panel.height(available, screen_height),
        }
    }

    pub(super) fn draw(&mut self, frame: &mut Frame, area: Rect) {
        match self {
            Self::Ask(panel) => panel.draw(frame, area),
            Self::Permission(panel) => panel.draw(frame, area),
        }
    }

    pub(super) fn key(&mut self, key: crossterm::event::KeyEvent) -> Action {
        match self {
            Self::Ask(panel) => panel.key(key),
            Self::Permission(panel) => panel.key(key),
        }
    }

    pub(super) fn paste(&mut self, text: &str) -> Action {
        match self {
            Self::Ask(panel) => panel.paste(text),
            Self::Permission(_) => Action::None,
        }
    }

    pub(super) fn wheel(&mut self, up: bool, column: u16, row: u16) -> bool {
        match self {
            Self::Ask(panel) => panel.wheel(up, column, row),
            Self::Permission(panel) => panel.wheel(up, column, row),
        }
    }
}
