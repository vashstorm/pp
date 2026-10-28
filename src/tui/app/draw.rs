use crate::tui::{
    Alignment, App, Block, Borders, Color, Frame, Line, Rect, StatusContext, Style, changes, menu,
    plan_list, platform_text, statusline, transcript_style, tree, welcome,
};

impl App {
    pub(in crate::tui) fn draw(&mut self, frame: &mut Frame, context: &StatusContext) {
        self.conversation.set_terminal_rows(frame.area().height);
        if let Some(review) = &mut self.review_transcript {
            review.set_terminal_rows(frame.area().height);
        }
        let area = frame.area();
        if area.width == 0 || area.height == 0 {
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
            self.notifications.draw(
                frame,
                Rect::new(area.x, area.y, area.width, area.height.saturating_sub(2)),
            );
            return;
        }
        if !self.native_render
            && let Some(panel) = &mut self.session_panel
        {
            panel.draw(frame, area);
            self.notifications.draw(
                frame,
                Rect::new(area.x, area.y, area.width, area.height.saturating_sub(2)),
            );
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
            self.notifications.draw(
                frame,
                Rect::new(area.x, area.y, area.width, area.height.saturating_sub(2)),
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
            self.notifications.draw(
                frame,
                Rect::new(area.x, area.y, area.width, area.height.saturating_sub(2)),
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
            self.notifications.draw(
                frame,
                Rect::new(area.x, area.y, area.width, area.height.saturating_sub(2)),
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
            self.notifications.draw(
                frame,
                Rect::new(area.x, area.y, area.width, area.height.saturating_sub(2)),
            );
            return;
        }
        let status_height = u16::from(area.height >= 2);
        let available = area.height.saturating_sub(status_height);
        let bordered = available >= 3 && area.width >= 3;
        let lines = self.input.lines().len().clamp(1, 6) as u16;
        let input_height = available.min(lines.saturating_add(if bordered { 2 } else { 0 }));
        // 菜单覆盖输入框上方，候选数量不参与对话区和输入框的布局。
        let conversation_height = available.saturating_sub(input_height);
        let menu_height = if self.native_render {
            0
        } else {
            self.menu
                .as_ref()
                .map_or(0, |menu| menu.height(conversation_height))
                .max(
                    self.model_panel
                        .as_ref()
                        .map_or(0, |panel| panel.height(conversation_height)),
                )
        };
        let menu_area = Rect::new(
            area.x,
            area.y + conversation_height - menu_height,
            area.width,
            menu_height,
        );
        let conversation = Rect::new(area.x, area.y, area.width, conversation_height);
        let input = Rect::new(
            area.x,
            area.y.saturating_add(conversation_height),
            area.width,
            input_height,
        );
        let status = Rect::new(area.x, input.bottom(), area.width, status_height);
        // ask 占用独立区域，先缩短消息 viewport，再绘制消息，避免覆盖最新内容。
        let ask_height = if self.native_render {
            0
        } else {
            self.ask_panel
                .as_mut()
                .map_or(0, |panel| panel.height(conversation, area.height))
        };
        let ask_area = Rect::new(
            conversation.x,
            conversation.bottom().saturating_sub(ask_height),
            conversation.width,
            ask_height,
        );
        let conversation = Rect {
            height: conversation.height.saturating_sub(ask_height),
            ..conversation
        };
        let conversation = if self.show_welcome && self.plan_panel.is_none() {
            welcome::draw(frame, conversation)
        } else {
            conversation
        };
        // 独立审查页优先渲染：底部输入与状态栏照常可用，草稿可继续编辑但提交被拒。
        if self.native_render {
            self.conversation.draw_native(frame, conversation);
        } else if let Some(panel) = &mut self.plan_panel {
            panel.draw(frame, conversation, self.plan_progress);
        } else {
            self.active_transcript().draw(frame, conversation);
        }
        if !self.native_render
            && let Some(panel) = &self.tree_panel
        {
            if input.height > 0 {
                panel.draw_status(frame, Rect::new(input.x, input.y, input.width, 1));
            }
            if input.height > 1 {
                frame.render_widget(
                    Line::styled(tree::HINT, Style::default().fg(transcript_style::MUTED)),
                    Rect::new(input.x, input.y + 1, input.width, 1),
                );
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
            self.notifications.draw(frame, conversation);
            return;
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
        self.notifications.draw(
            frame,
            Rect::new(
                area.x,
                area.y,
                area.width,
                menu_area.y.min(ask_area.y).saturating_sub(area.y),
            ),
        );
    }

    pub(in crate::tui) fn draw_input_status(&self, frame: &mut Frame, area: Rect) {
        // 上下文固定在右端；压缩等临时状态显示在它左侧，空间不足时优先保留上下文。
        let usage_style = Style::default().bg(Color::Rgb(26, 27, 38));
        let mut right = area.right();
        if let Some(usage) = self.usage.as_ref() {
            let (text, color) = statusline::context_field(usage);
            let line = Line::styled(format!(" {text} "), usage_style.fg(color))
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
