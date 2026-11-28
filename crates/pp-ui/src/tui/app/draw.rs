use crate::tui::{
    Alignment, App, Block, Borders, Color, Frame, Line, MainLayout, Rect, StatusContext, Style,
    Transcript, menu, plan_list, platform_text, statusline, transcript_style, tree, welcome,
};

impl App {
    fn input_transcript(&self) -> &Transcript {
        self.review_transcript
            .as_ref()
            .filter(|_| !self.native_render)
            .unwrap_or(&self.conversation)
    }

    pub(in crate::tui) fn main_layout(&mut self, area: Rect) -> MainLayout {
        let status_height = u16::from(area.height >= 2);
        let available = area.height - status_height;
        let activity_height =
            u16::from(available > 1 && self.input_transcript().has_live_activity());
        let available = available - activity_height;
        let bordered = available >= 3 && area.width >= 3;
        let border_height = if bordered { 2 } else { 0 };
        let prompt_width = area.width.saturating_sub(1).min(2);
        let input_height = crate::tui::input_rows(
            &self.input,
            area.width - prompt_width,
            available.saturating_sub(border_height).min(6),
        ) + border_height;
        let slash_menu = self
            .menu
            .as_ref()
            .filter(|menu| !self.native_render && menu.anchored_to_input());
        // Keep one row for the conversation, plus one for command completion when space
        // allows.
        let content_min =
            1 + u16::from(slash_menu.is_some() && available.saturating_sub(input_height) > 1);
        let image_count = if self.resources.images.is_empty() {
            0
        } else {
            self.resources.active_images(&self.input_text()).len()
        };
        // Border plus one body row; when space runs short this yields to the conversation and
        // the input.
        let retry_height = u16::from(self.retry_visible()) * 3;
        let attachments_height = (if image_count > 0 { 8 } else { 0 }
            + u16::from(self.resources.job.is_some()))
        .min(available.saturating_sub(input_height + retry_height + content_min));
        let retry_height = retry_height
            .min(available.saturating_sub(input_height + attachments_height + content_min));
        let above = available.saturating_sub(input_height + attachments_height + retry_height);
        let activity_spacing = u16::from(activity_height > 0 && above > content_min);
        let above = above - activity_spacing;
        let base = Rect::new(area.x, area.y, area.width, above);
        let ask_height = if self.native_render {
            0
        } else {
            self.ask_panel
                .as_mut()
                .map_or(0, |panel| panel.height(base, area.height))
        }
        .min(above);
        // A very short terminal keeps at least one line of conversation content first.
        let ask_spacing = u16::from(ask_height > 0 && above - ask_height > 1);
        let remaining = above - ask_height - ask_spacing;
        let panel_budget = remaining.saturating_sub(content_min);
        let todo_height = if self.native_render || self.plan_panel.is_none() {
            self.todo_panel
                .height(area.width, area.height, panel_budget)
        } else {
            0
        };
        let has_agents = self
            .subagent_control
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|b| !b.summary().is_empty());
        let subagent_height = u16::from(has_agents && panel_budget > todo_height);
        let todo_spacing =
            u16::from(todo_height > 0 && panel_budget > todo_height + subagent_height);
        let conversation_budget =
            remaining.saturating_sub(todo_height + subagent_height + todo_spacing);
        let completion_height =
            slash_menu.map_or(0, |menu| menu.height(conversation_budget.saturating_sub(1)));
        let conversation_height = conversation_budget - completion_height;
        let conversation = Rect::new(area.x, area.y, area.width, conversation_height);
        let ask = Rect::new(
            area.x,
            conversation.bottom() + ask_spacing,
            area.width,
            ask_height,
        );
        let todo = Rect::new(area.x, ask.bottom() + todo_spacing, area.width, todo_height);
        let subagents = Rect::new(area.x, todo.bottom(), area.width, subagent_height);
        let attachments = Rect::new(area.x, subagents.bottom(), area.width, attachments_height);
        let retry = Rect::new(area.x, attachments.bottom(), area.width, retry_height);
        let activity = Rect::new(
            area.x,
            retry.bottom() + activity_spacing,
            area.width,
            activity_height,
        );
        let slash_completion =
            slash_menu.map(|_| Rect::new(area.x, activity.bottom(), area.width, completion_height));
        let input = Rect::new(
            area.x,
            activity.bottom() + completion_height,
            area.width,
            input_height,
        );
        let status = Rect::new(area.x, input.bottom(), area.width, status_height);
        MainLayout {
            conversation,
            todo,
            subagents,
            ask,
            activity,
            retry,
            slash_completion,
            input,
            attachments,
            status,
            bordered,
        }
    }

    /// Entry point for unit tests that draw directly; the production path always reuses the
    /// precomputed layout from `paint_surface`.
    #[cfg(test)]
    pub(in crate::tui) fn draw(&mut self, frame: &mut Frame, context: &StatusContext) {
        self.draw_with_layout(frame, context, None, false);
    }

    /// Reuses `layout` when `paint_surface` computed it first, so the input box is not
    /// re-laid out every frame; with `exiting` a full-frame snapshot is also kept for the exit
    /// draw, while other frames avoid the extra copy.
    pub(in crate::tui) fn draw_with_layout(
        &mut self,
        frame: &mut Frame,
        context: &StatusContext,
        known: Option<&MainLayout>,
        exiting: bool,
    ) {
        let area = frame.area();
        self.draw_base_layout(frame, context, known, exiting);
        if !self.native_render
            && let Some(overlay) = &mut self.permission_overlay
        {
            overlay.draw(frame, area);
        }
    }

    fn draw_base_layout(
        &mut self,
        frame: &mut Frame,
        context: &StatusContext,
        known: Option<&MainLayout>,
        exiting: bool,
    ) {
        self.conversation.set_terminal_rows(frame.area().height);
        if let Some(review) = &mut self.review_transcript {
            review.set_terminal_rows(frame.area().height);
        }
        let area = frame.area();
        self.image_view.begin_frame(area);
        self.todo_panel.area = Rect::default();
        if area.width == 0 || area.height == 0 {
            return;
        }
        if !self.native_render
            && let Some(panel) = &mut self.settings_panel
        {
            panel.draw(frame, area);
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
            panel.update_override(self.override_selection.clone());
            panel.draw(frame, area);

            return;
        }
        if !self.native_render && self.ask_panel.is_none() && self.subagent_panel.is_some() {
            if let Some(panel) = &mut self.subagent_panel {
                panel.draw(frame, area, &mut self.child_drafts, &self.cost_display);
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
            && let Some(panel) = &mut self.themes_panel
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
            && let Some(panel) = &mut self.context_panel
        {
            panel.draw(frame, area);
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
                    Line::styled(tree::hint(), Style::default().fg(transcript_style::muted())),
                    Rect::new(area.x, area.y + height, area.width, 1),
                );
            }
            frame.render_widget(
                statusline::primary(
                    self.current_model.as_ref(),
                    self.think_level,
                    self.workflow.mode,
                    self.permission_mode,
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
            // Full-screen review/progress view: the status line stays at the bottom; Esc is
            // handled by the panel and only closes it. Details/review opened from /plan-list
            // also show one row of list shortcuts.
            let with_hint = self.plan_list_panel.is_some();
            let available = area.height.saturating_sub(1);
            let panel_height = available.saturating_sub(u16::from(with_hint));
            panel.draw(
                frame,
                Rect::new(area.x, area.y, area.width, panel_height),
                self.plan_progress,
                self.usage.as_ref(),
            );
            if with_hint && panel_height < available {
                frame.render_widget(
                    Line::styled(
                        format!(" {}", plan_list::DETAIL_HINT),
                        Style::default().fg(transcript_style::muted()),
                    ),
                    Rect::new(area.x, area.y + panel_height, area.width, 1),
                );
            }
            frame.render_widget(
                statusline::primary(
                    self.current_model.as_ref(),
                    self.think_level,
                    self.workflow.mode,
                    self.permission_mode,
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
            // Full-screen plan management page: the status line stays at the bottom; Esc is
            // handled by the panel and only closes it.
            let available = area.height.saturating_sub(1);
            panel.draw(frame, Rect::new(area.x, area.y, area.width, available));
            frame.render_widget(
                statusline::primary(
                    self.current_model.as_ref(),
                    self.think_level,
                    self.workflow.mode,
                    self.permission_mode,
                    context,
                    area.width,
                ),
                Rect::new(area.x, area.bottom() - 1, area.width, 1),
            );

            return;
        }
        let computed;
        let layout = match known {
            Some(layout) => layout,
            None => {
                computed = self.main_layout(area);
                &computed
            }
        };
        let MainLayout {
            conversation,
            todo,
            subagents,
            ask: ask_area,
            activity,
            retry,
            slash_completion,
            input,
            attachments,
            status,
            bordered,
        } = *layout;
        let images = if self.resources.images.is_empty() {
            Vec::new()
        } else {
            self.resources.active_images(&self.input_text())
        };
        let loading = u16::from(self.resources.job.is_some() && attachments.height > 0);
        if loading > 0 {
            frame.render_widget(
                Line::styled(
                    "preparing input resources… Esc to cancel",
                    Style::default().fg(statusline::accent()),
                ),
                Rect::new(attachments.x, attachments.y, attachments.width, 1),
            );
        }
        self.image_view.cards(
            frame,
            Rect::new(
                attachments.x,
                attachments.y + loading,
                attachments.width,
                attachments.height.saturating_sub(loading),
            ),
            &images,
        );
        if subagents.height > 0
            && let Some(binding) = self
                .subagent_control
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
        {
            frame.render_widget(Line::from(binding.summary()), subagents);
        }
        // File completion and the pending queue still draw their overlays inside the
        // conversation area.
        let menu_overlay_height = if self.native_render || slash_completion.is_some() {
            0
        } else {
            self.menu
                .as_ref()
                .map_or(0, |menu| menu.height(conversation.height))
        };
        let menu_area = slash_completion.unwrap_or_else(|| {
            Rect::new(
                conversation.x,
                conversation.bottom() - menu_overlay_height,
                conversation.width,
                menu_overlay_height,
            )
        });
        self.todo_panel.draw(frame, todo);
        let preview_area = conversation;
        let has_trailing = self
            .review_transcript
            .as_ref()
            .filter(|_| !self.native_render)
            .unwrap_or(&self.conversation)
            .has_entries();
        let conversation =
            if self.show_welcome && self.plan_panel.is_none() && self.mcp_panel.is_none() {
                welcome::draw(
                    frame,
                    conversation,
                    welcome::WelcomeContext {
                        mcp_tips: &self.mcp_tip,
                        reserve_trailing: has_trailing,
                    },
                )
            } else {
                conversation
            };
        // The standalone review tab renders first: the bottom input and status line stay
        // usable, and the draft stays editable while submissions are rejected.
        if self.native_render {
            self.conversation.draw_native(frame, conversation);
        } else if let Some(panel) = &mut self.mcp_panel {
            panel.draw(
                frame,
                Rect {
                    height: conversation.height.saturating_sub(menu_overlay_height),
                    ..conversation
                },
            );
        } else if let Some(panel) = &mut self.plan_panel {
            panel.draw(frame, conversation, self.plan_progress, self.usage.as_ref());
        } else {
            self.active_transcript().draw(frame, conversation);
        }
        self.input_transcript().draw_live_activity(frame, activity);
        self.draw_retry_notice(frame, retry);
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
                            transcript_style::muted()
                        } else if self.menu.is_some() {
                            menu::separator()
                        } else if self.shell_mode {
                            // The mode is independent of the body, so the leading `!` used to
                            // switch is not shown in the input box.
                            statusline::shell_mode()
                        } else {
                            statusline::accent()
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
            if !self.resources.bindings.is_empty() {
                let (top, left) = self.input.scroll_offset();
                for row in 0..editor_area.height {
                    for col in 0..editor_area.width {
                        let cursor = self.input.screen_to_data(
                            usize::from(row) + usize::from(top),
                            usize::from(col) + usize::from(left),
                        );
                        let byte = crate::tui::attachment_edit::offset(&self.input, cursor);
                        if self
                            .resources
                            .bindings
                            .values()
                            .any(|range| range.contains(&byte))
                        {
                            frame.buffer_mut()[(editor_area.x + col, editor_area.y + row)]
                                .set_fg(crate::tui::attachment_edit::color());
                        }
                    }
                }
            }
            // Read the wrap and scroll offsets after rendering so the IME and the terminal
            // caret share the insertion position.
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
        if menu_area.height > 0
            && let Some(menu) = &mut self.menu
        {
            menu.draw(frame, menu_area);
        }
        if status.height > 0 {
            frame.render_widget(
                statusline::primary(
                    self.current_model.as_ref(),
                    self.think_level,
                    self.workflow.mode,
                    self.permission_mode,
                    context,
                    status.width,
                ),
                status,
            );
        }
        if self.native_render && exiting {
            self.normal_snapshot = Some(std::sync::Arc::new(frame.buffer_mut().clone()));
        }
        if self.resource_input_available() && self.menu.is_none() {
            self.image_view.preview(frame, preview_area);
        }
        if !self.native_render
            && let Some(panel) = &mut self.model_panel
        {
            // The model panel expands upward, leaving room for the activity state above the
            // input box.
            let height = panel.height(activity.y.saturating_sub(area.y));
            let popup = Rect::new(input.x, activity.y - height, input.width, height);
            panel.draw(frame, popup);
        }
        if !self.native_render
            && let Some(panel) = &mut self.ask_panel
        {
            panel.draw(frame, ask_area);
        }
    }

    pub(in crate::tui) fn draw_input_status(&self, frame: &mut Frame, area: Rect) {
        let usage_style = Style::default().bg(statusline::input_status_bg());
        let mut right = area.right();
        let speed = self.token_speed.value();
        let running_subagents = self.subagent_status.map_or(0, |(_, count)| count);
        let live_output = self.token_speed.live_output_tokens();
        let task = self
            .task_status
            .as_ref()
            .map(|task| (task, &self.cost_display));
        let elapsed = self.conversation.task_duration();
        let usage = self.usage.as_ref().filter(|usage| {
            statusline::has_data(
                usage,
                speed,
                running_subagents,
                live_output,
                task,
                elapsed.as_deref(),
            )
        });
        // The ctx field is built once per frame, so the width budget and the real metric
        // cluster share one row.
        let context = usage.map(statusline::context_field);
        let context_width = context.as_ref().map_or(0, |context| {
            u16::try_from(context.width() + 2).unwrap_or(u16::MAX)
        });
        let extra = self
            .secondary_status
            .as_deref()
            .filter(|text| !text.is_empty())
            .map(|extra| {
                Line::styled(format!(" {extra} "), Style::default().fg(Color::DarkGray))
                    .alignment(Alignment::Right)
            });
        let extra_width = extra.as_ref().map_or(0, |line| {
            u16::try_from(line.width()).unwrap_or(u16::MAX).min(
                (right - area.x)
                    .saturating_sub(context_width)
                    .min(area.width / 2),
            )
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
            let width = u16::try_from(line.width()).unwrap_or(u16::MAX).min(
                (right - left)
                    .saturating_sub(extra_width)
                    .saturating_sub(context_width),
            );
            frame.render_widget(line, Rect::new(left, area.y, width, 1));
            left += width;
        }
        if let Some(usage) = usage {
            let metrics = statusline::metric_fields(
                statusline::Metrics {
                    usage,
                    context: context.expect("ctx field is built alongside usage"),
                },
                speed,
                running_subagents,
                live_output,
                task,
                elapsed.as_deref(),
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
