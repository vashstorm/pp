//! 非按键事件：粘贴、滚轮与尺寸变化；面板独占时只标记重绘。

use crate::tui::{App, Event, MOUSE_SCROLL_LINES, MouseEventKind};

impl App {
    pub(super) fn handle_non_key_event(&mut self, event: Event, dirty: &mut bool) {
        match event {
            Event::Paste(_) if self.changes_panel.is_some() => {
                *dirty = true;
            }
            Event::Mouse(mouse) if self.changes_panel.is_some() => {
                if matches!(
                    mouse.kind,
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                ) {
                    self.changes_panel
                        .as_mut()
                        .expect("active changes panel")
                        .wheel(
                            matches!(mouse.kind, MouseEventKind::ScrollUp),
                            MOUSE_SCROLL_LINES,
                        );
                    *dirty = true;
                }
            }
            Event::Paste(_) if self.pipes_panel.is_some() => {
                *dirty = true;
            }
            Event::Mouse(mouse) if self.pipes_panel.is_some() => {
                if matches!(
                    mouse.kind,
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                ) {
                    self.pipes_panel
                        .as_mut()
                        .expect("active pipes panel")
                        .wheel(
                            matches!(mouse.kind, MouseEventKind::ScrollUp),
                            MOUSE_SCROLL_LINES,
                        );
                    *dirty = true;
                }
            }
            Event::Paste(_) if self.tree_panel.is_some() => {
                *dirty = true;
            }
            Event::Mouse(mouse) if self.tree_panel.is_some() => {
                if matches!(
                    mouse.kind,
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                ) {
                    self.tree_panel.as_mut().expect("active tree panel").wheel(
                        matches!(mouse.kind, MouseEventKind::ScrollUp),
                        MOUSE_SCROLL_LINES,
                    );
                    *dirty = true;
                }
            }
            Event::Paste(text) if self.plan_panel.is_some() => {
                if !self.plan_panel.as_mut().expect("当前计划面板").paste(&text) {
                    if pp_runtime::user_input::possible_image_paste(&text)
                        && self.resource_input_available()
                    {
                        self.paste_resource_input(text);
                    } else {
                        self.input
                            .insert_str(text.replace("\r\n", "\n").replace('\r', "\n"));
                        self.refresh_candidates(true);
                    }
                }
                *dirty = true;
            }
            Event::Paste(text) if self.plan_list_panel.is_some() => {
                if let Some(panel) = &mut self.plan_list_panel {
                    panel.paste(&text);
                }
                *dirty = true;
            }
            Event::Mouse(mouse) if self.plan_panel.is_some() => {
                if matches!(
                    mouse.kind,
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                ) {
                    let up = matches!(mouse.kind, MouseEventKind::ScrollUp);
                    self.plan_panel
                        .as_mut()
                        .expect("active plan panel")
                        .wheel(up, MOUSE_SCROLL_LINES);
                    *dirty = true;
                }
            }
            Event::Mouse(mouse) if self.plan_list_panel.is_some() => {
                if matches!(
                    mouse.kind,
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                ) {
                    let up = matches!(mouse.kind, MouseEventKind::ScrollUp);
                    if let Some(panel) = &mut self.plan_list_panel {
                        panel.wheel(up);
                    }
                    *dirty = true;
                }
            }
            Event::Paste(text) if self.ask_panel.is_some() => {
                let action = self
                    .ask_panel
                    .as_mut()
                    .expect("active ask panel")
                    .paste(&text);
                self.apply_ask_action(action);
                *dirty = true;
            }
            Event::Mouse(mouse)
                if self.ask_panel.is_some()
                    && !self
                        .todo_panel
                        .area
                        .contains(ratatui::layout::Position::new(mouse.column, mouse.row)) =>
            {
                if matches!(
                    mouse.kind,
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                ) {
                    let up = matches!(mouse.kind, MouseEventKind::ScrollUp);
                    if !self.ask_panel.as_mut().expect("active ask panel").wheel(
                        up,
                        mouse.column,
                        mouse.row,
                    ) {
                        if up {
                            self.conversation.scroll_up(MOUSE_SCROLL_LINES);
                        } else {
                            self.conversation.scroll_down(MOUSE_SCROLL_LINES);
                        }
                    }
                    *dirty = true;
                }
            }
            Event::Paste(_) if self.skill_panel.is_some() => {}
            Event::Mouse(_) if self.skill_panel.is_some() => {}
            Event::Paste(text) if self.session_panel.is_some() => {
                if let Some(panel) = &mut self.session_panel {
                    panel.paste(&text);
                }
                if self.session_panel.is_some() {
                    self.search_session_panel();
                }
                *dirty = true;
            }
            Event::Mouse(mouse) if self.session_panel.is_some() => {
                // 滚轮只改变列表选择，不滚动背后的对话。
                if let MouseEventKind::ScrollUp | MouseEventKind::ScrollDown = mouse.kind {
                    let up = matches!(mouse.kind, MouseEventKind::ScrollUp);
                    if let Some(panel) = &mut self.session_panel {
                        panel.wheel(up);
                    }
                    *dirty = true;
                }
            }
            Event::Paste(text) if self.model_panel.is_some() => {
                self.model_panel
                    .as_mut()
                    .expect("active model panel")
                    .paste(&text);
                self.refresh_candidates(true);
                *dirty = true;
            }
            Event::Mouse(mouse)
                if self
                    .model_panel
                    .as_mut()
                    .is_some_and(|panel| panel.wheel(mouse, MOUSE_SCROLL_LINES)) =>
            {
                *dirty = true;
            }
            Event::Paste(text) => {
                if self.resource_input_available()
                    && pp_runtime::user_input::possible_image_paste(&text)
                {
                    // 路径识别也访问文件系统，统一放入后台线程。
                    self.paste_resource_input(text);
                    *dirty = true;
                    return;
                }
                if self.queue_menu {
                    self.close_pending_menu();
                }
                self.insert_input(text.replace("\r\n", "\n").replace('\r', "\n"));
                self.refresh_candidates(true);
                *dirty = true;
            }
            Event::Mouse(mouse)
                if self
                    .todo_panel
                    .area
                    .contains(ratatui::layout::Position::new(mouse.column, mouse.row)) =>
            {
                if matches!(
                    mouse.kind,
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                ) {
                    self.todo_panel
                        .scroll(mouse.kind == MouseEventKind::ScrollUp, MOUSE_SCROLL_LINES);
                    *dirty = true;
                }
            }
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollUp => {
                    if !self.history_browser {
                        self.active_transcript().begin_browse();
                    }
                    self.history_browser = true;
                    self.active_transcript().scroll_up(MOUSE_SCROLL_LINES);
                    *dirty = true;
                }
                MouseEventKind::ScrollDown => {
                    self.active_transcript().scroll_down(MOUSE_SCROLL_LINES);
                    if self.conversation.following() {
                        self.history_browser = false;
                    }
                    *dirty = true;
                }
                _ => {}
            },
            Event::Resize(_, _) => *dirty = true,
            _ => {}
        }
    }
}
