use crate::tui::{
    Alignment, App, Block, Borders, Color, Frame, Line, Rect, StatusContext, Style, changes, menu,
    plan_list, platform_text, statusline, transcript_style, tree, welcome,
};
use ratatui::text::Span;

/// 主屏幕和原生历史提交共用的几何结果，TODO 永远不进入对话 viewport。
pub(in crate::tui) struct MainLayout {
    pub conversation: Rect,
    todo: Rect,
    subagents: Rect,
    ask: Rect,
    input: Rect,
    status: Rect,
    bordered: bool,
}

impl App {
    pub(in crate::tui) fn main_layout(&mut self, area: Rect) -> MainLayout {
        let status_height = u16::from(area.height >= 2);
        let available = area.height.saturating_sub(status_height);
        let bordered = available >= 3 && area.width >= 3;
        let input_height = available
            .min(self.input.lines().len().clamp(1, 6) as u16 + if bordered { 2 } else { 0 });
        let above = available.saturating_sub(input_height);
        let base = Rect::new(area.x, area.y, area.width, above);
        let ask_height = if self.native_render {
            0
        } else {
            self.ask_panel
                .as_mut()
                .map_or(0, |panel| panel.height(base, area.height))
        }
        .min(above);
        let remaining = above.saturating_sub(ask_height);
        let todo_height = if self.native_render || self.plan_panel.is_none() {
            self.todo_panel
                .height(area.width, area.height, remaining.saturating_sub(1))
        } else {
            0
        };
        let has_agents = self
            .subagent_control
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|b| !b.summary().is_empty());
        let subagent_height = u16::from(has_agents && remaining > todo_height + 1);
        let conversation_height = remaining.saturating_sub(todo_height + subagent_height);
        let conversation = Rect::new(area.x, area.y, area.width, conversation_height);
        let ask = Rect::new(area.x, conversation.bottom(), area.width, ask_height);
        let todo = Rect::new(area.x, ask.bottom(), area.width, todo_height);
        let subagents = Rect::new(area.x, todo.bottom(), area.width, subagent_height);
        let input = Rect::new(area.x, subagents.bottom(), area.width, input_height);
        let status = Rect::new(area.x, input.bottom(), area.width, status_height);
        MainLayout {
            conversation,
            todo,
            subagents,
            ask,
            input,
            status,
            bordered,
        }
    }

    pub(in crate::tui) fn draw(&mut self, frame: &mut Frame, context: &StatusContext) {
        self.conversation.set_terminal_rows(frame.area().height);
        if let Some(review) = &mut self.review_transcript {
            review.set_terminal_rows(frame.area().height);
        }
        let area = frame.area();
        self.todo_panel.area = Rect::default();
        if area.width == 0 || area.height == 0 {
            return;
        }
        if !self.native_render
            && let Some(panel) = &mut self.roles_panel
        {
            panel.update_roles(
                self.model_roles.clone(),
                self.current_model
                    .as_ref()
                    .map(|model| model.reference.clone())
                    .unwrap_or_default(),
                self.think_level,
            );
            panel.draw(frame, area);

            return;
        }
        if !self.native_render && self.ask_panel.is_none() && self.subagent_panel.is_some() {
            if let Some(panel) = &mut self.subagent_panel {
                panel.draw(frame, area, &mut self.child_drafts);
            }

            return;
        }
        if !self.native_render
            && let Some(panel) = &mut self.auth_panel
        {
            panel.draw(frame, area);
            return;
        }
        if !self.native_render
            && let Some(panel) = &mut self.plan_leave_confirmation
        {
            panel.draw(frame, area);
            return;
        }
        if !self.native_render
            && let Some(panel) = &mut self.skill_panel
        {
            panel.draw(frame, area);

            return;
        }
        if !self.native_render
            && let Some(panel) = &mut self.session_panel
        {
            panel.draw(frame, area);

            return;
        }
        if !self.native_render
            && let Some(panel) = &mut self.changes_panel
        {
            let available = area.height.saturating_sub(1);
            let height = available.saturating_sub(1);
            panel.draw(frame, Rect::new(area.x, area.y, area.width, height));
            if available > 0 {
                frame.render_widget(
                    Line::styled(changes::HINT, Style::default().fg(transcript_style::MUTED)),
                    Rect::new(area.x, area.y + height, area.width, 1),
                );
            }
            frame.render_widget(
                statusline::primary(
                    self.current_model.as_ref(),
                    self.think_level,
                    self.workflow.mode,
                    context,
                    area.width,
                ),
                Rect::new(area.x, area.bottom() - 1, area.width, 1),
            );

            return;
        }
        if !self.native_render
            && let Some(panel) = &mut self.tree_panel
            && !panel.is_loading()
        {
            let available = area.height.saturating_sub(1);
            let height = available.saturating_sub(1);
            panel.draw(frame, Rect::new(area.x, area.y, area.width, height));
            if available > 0 {
                frame.render_widget(
                    Line::styled(tree::HINT, Style::default().fg(transcript_style::MUTED)),
                    Rect::new(area.x, area.y + height, area.width, 1),
                );
            }
            frame.render_widget(
                statusline::primary(
                    self.current_model.as_ref(),
                    self.think_level,
                    self.workflow.mode,
                    context,
                    area.width,
                ),
                Rect::new(area.x, area.bottom() - 1, area.width, 1),
            );

            return;
        }
        if !self.native_render
            && let Some(panel) = &mut self.plan_panel
            && panel.token().is_none()
        {
            // 全屏审阅/进度视图：底部保留状态栏；Esc 由面板处理，只关闭面板。
            // 从 /plan-list 打开的详情/审阅额外显示一行列表快捷键。
            let with_hint = self.plan_list_panel.is_some();
            let available = area.height.saturating_sub(1);
            let panel_height = available.saturating_sub(u16::from(with_hint));
            panel.draw(
                frame,
                Rect::new(area.x, area.y, area.width, panel_height),
                self.plan_progress,
            );
            if with_hint && panel_height < available {
                frame.render_widget(
                    Line::styled(
                        format!(" {}", plan_list::DETAIL_HINT),
                        Style::default().fg(transcript_style::MUTED),
                    ),
                    Rect::new(area.x, area.y + panel_height, area.width, 1),
                );
            }
            frame.render_widget(
                statusline::primary(
                    self.current_model.as_ref(),
                    self.think_level,
                    self.workflow.mode,
                    context,
                    area.width,
                ),
                Rect::new(area.x, area.bottom() - 1, area.width, 1),
            );

            return;
        }
        if !self.native_render
            && let Some(panel) = &mut self.plan_list_panel
            && self.plan_panel.is_none()
        {
            // 全屏计划管理页：底部保留状态栏；Esc 由面板处理，只关闭面板。
            let available = area.height.saturating_sub(1);
            panel.draw(frame, Rect::new(area.x, area.y, area.width, available));
            frame.render_widget(
                statusline::primary(
                    self.current_model.as_ref(),
                    self.think_level,
                    self.workflow.mode,
                    context,
                    area.width,
                ),
                Rect::new(area.x, area.bottom() - 1, area.width, 1),
            );

            return;
        }
        let MainLayout {
            conversation,
            todo,
            subagents,
            ask: ask_area,
            input,
            status,
            bordered,
        } = self.main_layout(area);
        if subagents.height > 0
            && let Some(binding) = self
                .subagent_control
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
        {
            frame.render_widget(Line::from(binding.summary()), subagents);
        }
        // 菜单只覆盖对话区，不盖住 TODO 或 ask。
        let menu_height = if self.native_render {
            0
        } else {
            self.menu
                .as_ref()
                .map_or(0, |menu| menu.height(conversation.height))
                .max(
                    self.model_panel
                        .as_ref()
                        .map_or(0, |panel| panel.height(conversation.height)),
                )
        };
        let menu_area = Rect::new(
            conversation.x,
            conversation.bottom().saturating_sub(menu_height),
            conversation.width,
            menu_height,
        );
        self.todo_panel.draw(frame, todo);
        let conversation =
            if self.show_welcome && self.plan_panel.is_none() && self.mcp_panel.is_none() {
                welcome::draw(frame, conversation)
            } else {
                conversation
            };
        // 独立审查页优先渲染：底部输入与状态栏照常可用，草稿可继续编辑但提交被拒。
        if self.native_render {
            self.conversation.draw_native(frame, conversation);
        } else if let Some(panel) = &mut self.mcp_panel {
            panel.draw(frame, conversation);
        } else if let Some(panel) = &mut self.plan_panel {
            panel.draw(frame, conversation, self.plan_progress);
        } else {
            self.active_transcript().draw(frame, conversation);
        }
        let editor_area = if bordered {
            let block = Block::default()
                .borders(Borders::TOP | Borders::BOTTOM)
                .border_style(
                    Style::default().fg(
                        if self
                            .plan_panel
                            .as_ref()
                            .is_some_and(|panel| !panel.input_focused())
                        {
                            transcript_style::MUTED
                        } else if self.menu.is_some() {
                            menu::SEPARATOR
                        } else if self.shell_mode {
                            // 模式独立于正文，首个切换用的 `!` 不在输入框显示。
                            statusline::SHELL_MODE
                        } else {
                            statusline::ACCENT
                        },
                    ),
                );
            let inner = block.inner(input);
            frame.render_widget(block, input);
            self.draw_input_status(frame, Rect::new(input.x, input.y, input.width, 1));
            inner
        } else {
            input
        };
        let prompt_width = editor_area.width.saturating_sub(1).min(2);
        if prompt_width > 0 && editor_area.height > 0 {
            frame.render_widget(
                Line::from("❯"),
                Rect::new(editor_area.x, editor_area.y, prompt_width, 1),
            );
        }
        let editor_area = Rect::new(
            editor_area.x + prompt_width,
            editor_area.y,
            editor_area.width - prompt_width,
            editor_area.height,
        );
        if editor_area.width > 0 && editor_area.height > 0 {
            frame.render_widget(&self.input, editor_area);
            // 渲染后获取折行和滚动偏移，让输入法与终端原生光标共享插入位置。
            let cursor = self.input.screen_cursor();
            let (top, left) = self.input.scroll_offset();
            if let (Some(row), Some(column)) = (
                cursor.row.checked_sub(usize::from(top)),
                cursor.col.checked_sub(usize::from(left)),
            ) && self.ask_panel.is_none()
                && self.model_panel.is_none()
                && self
                    .plan_panel
                    .as_ref()
                    .is_none_or(|panel| panel.input_focused())
                && row < usize::from(editor_area.height)
                && column < usize::from(editor_area.width)
            {
                frame.set_cursor_position((
                    editor_area.x + column as u16,
                    editor_area.y + row as u16,
                ));
            }
        }
        if menu_height > 0
            && let Some(menu) = &mut self.menu
        {
            menu.draw(frame, menu_area);
        }
        if !self.native_render
            && let Some(panel) = &mut self.model_panel
        {
            panel.draw(frame, menu_area);
        }
        if status.height > 0 {
            frame.render_widget(
                statusline::primary(
                    self.current_model.as_ref(),
                    self.think_level,
                    self.workflow.mode,
                    context,
                    status.width,
                ),
                status,
            );
        }
        if self.native_render {
            self.normal_snapshot = Some(frame.buffer_mut().clone());
        }
        if !self.native_render
            && let Some(panel) = &mut self.ask_panel
        {
            panel.draw(frame, ask_area);
        }
    }

    pub(in crate::tui) fn draw_input_status(&self, frame: &mut Frame, area: Rect) {
        // 总耗时紧随 Ctx；空间不足时整块隐藏耗时，优先保留上下文。
        let usage_style = Style::default().bg(Color::Rgb(26, 27, 38));
        let mut right = area.right();
        let mut fields = Vec::new();
        if let Some(usage) = self.usage.as_ref() {
            let (text, color) = statusline::context_field(usage);
            fields.push(Span::styled(
                format!(" {text} "),
                Style::default().fg(color),
            ));
        }
        if let Some(elapsed) = self.conversation.task_duration() {
            let text = if fields.is_empty() {
                format!(" {elapsed} ")
            } else {
                format!("· {elapsed} ")
            };
            if fields.iter().map(Span::width).sum::<usize>() + Line::from(text.as_str()).width()
                <= usize::from(area.width)
            {
                fields.push(Span::styled(text, Style::default().fg(Color::DarkGray)));
            }
        }
        if !fields.is_empty() {
            let line = Line::from(fields)
                .style(usage_style)
                .alignment(Alignment::Right);
            let width = u16::try_from(line.width())
                .unwrap_or(u16::MAX)
                .min(area.width);
            right -= width;
            frame.render_widget(line, Rect::new(right, area.y, width, 1));
        }
        let extra = self
            .secondary_status
            .as_deref()
            .filter(|text| !text.is_empty())
            .map(|extra| {
                Line::styled(format!(" {extra} "), Style::default().fg(Color::DarkGray))
                    .alignment(Alignment::Right)
            });
        let extra_width = extra.as_ref().map_or(0, |line| {
            u16::try_from(line.width())
                .unwrap_or(u16::MAX)
                .min((right - area.x).min(area.width / 2))
        });
        let mut left = area.x;
        if let Some(snapshot) = &self.queue_snapshot
            && !snapshot.items.is_empty()
        {
            let paused = if self.queue_paused {
                platform_text(" · Paused · ⌥ + R resume", " · Paused · Ctrl + R resume")
            } else {
                ""
            };
            let line = Line::styled(
                format!(
                    " Steering: {} · Queued: {}{paused} ",
                    snapshot.steering(),
                    snapshot.follow_up()
                ),
                Style::default().fg(Color::DarkGray),
            );
            let width = u16::try_from(line.width())
                .unwrap_or(u16::MAX)
                .min((right - left).saturating_sub(extra_width));
            frame.render_widget(line, Rect::new(left, area.y, width, 1));
            left += width;
        }
        if let Some(usage) = self.usage.as_ref() {
            let metrics = statusline::metric_fields(
                usage.last_request.as_ref(),
                self.token_speed.value(),
                (right - left).saturating_sub(extra_width),
            )
            .style(usage_style);
            let width = u16::try_from(metrics.width()).unwrap_or(u16::MAX);
            right -= width;
            frame.render_widget(metrics, Rect::new(right, area.y, width, 1));
        }
        if let Some(extra) = extra {
            right -= extra_width;
            frame.render_widget(extra, Rect::new(right, area.y, extra_width, 1));
        }
    }
}
