//! 键鼠事件路由：roster/activity 导航与 child 详情输入。

use super::*;

impl Panel {
    /// 返回 true 表示关闭整个 Hub。
    pub(in crate::tui) fn handle(&mut self, event: Event, drafts: &mut Drafts) -> bool {
        if let Event::Key(key) = &event
            && key.kind == KeyEventKind::Release
        {
            return false;
        }
        if self.detail.is_some() {
            return self.handle_detail(event, drafts);
        }
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                let close = self.handle_key(key, drafts);
                // 过滤/树切换后让选择始终落在可见行上。
                self.validate_selection();
                close
            }
            Event::Paste(text) => {
                if self.filter_editing() {
                    if self.page == Page::Agents {
                        self.filter_push(&text);
                    } else {
                        self.query_push(&text);
                    }
                }
                false
            }
            Event::Mouse(mouse) => {
                self.handle_mouse(mouse);
                false
            }
            Event::Resize(..) => false,
            _ => false,
        }
    }

    fn handle_detail(&mut self, event: Event, drafts: &mut Drafts) -> bool {
        let Some(detail) = self.detail.as_mut() else {
            return false;
        };
        match event {
            Event::Key(key) => {
                if key.code == KeyCode::Esc && key.modifiers.is_empty() {
                    // Esc 保留 child 草稿，返回来源页。
                    self.commands.push(Command::CloseDetail);
                    self.detail = None;
                    return false;
                }
                if let Some(text) = detail.key(key, drafts) {
                    let agent = detail.agent.clone();
                    let revision = drafts.revision(&agent);
                    if self.sending.as_deref() != Some(agent.as_str()) {
                        self.sending = Some(agent.clone());
                        self.commands.push(Command::Submit {
                            agent,
                            text,
                            revision,
                        });
                    }
                }
                false
            }
            Event::Paste(text) => {
                detail.paste(&text, drafts);
                false
            }
            Event::Mouse(mouse) => {
                match mouse.kind {
                    MouseEventKind::ScrollUp => detail.wheel(true, 3),
                    MouseEventKind::ScrollDown => detail.wheel(false, 3),
                    _ => {}
                }
                false
            }
            Event::Resize(..) => false,
            _ => false,
        }
    }

    fn handle_key(&mut self, key: KeyEvent, drafts: &mut Drafts) -> bool {
        let _ = drafts;
        let press = key.kind == KeyEventKind::Press;
        if self.filter_editing() {
            match key.code {
                KeyCode::Esc => {
                    if self.page == Page::Agents {
                        self.filter.clear();
                        self.filtering = false;
                    } else {
                        self.activity.query.clear();
                        self.activity.editing = false;
                    }
                    return false;
                }
                KeyCode::Enter => {
                    if self.page == Page::Agents {
                        self.filtering = false;
                    } else {
                        self.activity.editing = false;
                    }
                    return false;
                }
                _ => {
                    self.type_filter(key);
                    return false;
                }
            }
        }
        match key.code {
            KeyCode::Esc => {
                // 先清过滤，再退出 inspector，最后才关闭 Hub。
                if self.page == Page::Agents && !self.filter.is_empty() {
                    self.filter.clear();
                    return false;
                }
                if self.page == Page::Agents && self.focus == Focus::Inspector {
                    self.focus = Focus::Roster;
                    return false;
                }
                if self.page == Page::Activity {
                    self.page = Page::Agents;
                    return false;
                }
                return true;
            }
            KeyCode::Tab => {
                self.focus = match self.focus {
                    Focus::Roster => Focus::Inspector,
                    Focus::Inspector => Focus::Roster,
                };
            }
            KeyCode::Char('1') => self.page = Page::Agents,
            KeyCode::Char('2') => self.page = Page::Activity,
            KeyCode::Char('/') => {
                if self.page == Page::Agents {
                    self.filtering = true;
                } else {
                    self.activity.editing = true;
                }
            }
            KeyCode::Char('t') if self.page == Page::Agents => self.tree = !self.tree,
            KeyCode::F(5) => {
                self.commands.push(Command::Refresh);
                self.notice = Some("Reloading agent hub…".into());
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if self.page == Page::Agents {
                    if self.focus == Focus::Inspector {
                        self.inspector = self.inspector.saturating_sub(1);
                    } else {
                        self.move_selection(-1);
                    }
                } else {
                    self.activity_step(-1);
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.page == Page::Agents {
                    if self.focus == Focus::Inspector {
                        self.inspector = self.inspector.saturating_add(1);
                    } else {
                        self.move_selection(1);
                    }
                } else {
                    self.activity_step(1);
                }
            }
            KeyCode::PageUp => {
                if self.page == Page::Agents {
                    self.inspector = self.inspector.saturating_sub(10);
                } else {
                    self.activity_step(-10);
                }
            }
            KeyCode::PageDown => {
                if self.page == Page::Agents {
                    self.inspector = self.inspector.saturating_add(10);
                } else {
                    self.activity_step(10);
                }
            }
            KeyCode::Char(' ') if self.page == Page::Activity => {
                self.activity.follow = true;
            }
            KeyCode::Char('f') if self.page == Page::Activity => {
                self.activity.category = self.activity.category.next();
            }
            KeyCode::Char('s') if self.page == Page::Activity => {
                self.activity.scope = self.activity.scope.next();
            }
            KeyCode::Enter => {
                if self.page == Page::Agents {
                    if let Some(agent) = self.selected_agent() {
                        let agent = agent.id.clone();
                        self.commands.push(Command::Open {
                            agent,
                            anchor: None,
                        });
                    }
                } else {
                    self.activity.follow = false;
                    let rows = self.activity_rows();
                    if let Some(index) = self.activity_index(&rows)
                        && let Some(row) = self.projection.rows().get(rows[index])
                    {
                        self.activity.selected = Some(row.id());
                        let agent = row.agent.clone();
                        let anchor = Anchor {
                            run: row.run.clone(),
                            offset: row.offset,
                            tool: row.tool.clone(),
                        };
                        self.commands.push(Command::Open {
                            agent,
                            anchor: Some(anchor),
                        });
                    }
                }
            }
            KeyCode::Char('e') if self.page == Page::Agents && press => {
                if let Some(agent) = self.selected_agent() {
                    let agent_id = agent.id.clone();
                    if agent.status == AgentStatus::Aborted && agent.run_status.is_none() {
                        self.notice =
                            Some("Aborted agents need r or a new message to resume.".into());
                    }
                    self.commands.push(Command::Open {
                        agent: agent_id,
                        anchor: None,
                    });
                    self.commands.push(Command::FocusEditor);
                }
            }
            KeyCode::Char('c') if press && self.page == Page::Agents => {
                self.control(Control::Cancel, None);
            }
            KeyCode::Char('x') if press && self.page == Page::Agents => {
                self.control(Control::Kill, None);
            }
            KeyCode::Char('r') if press && self.page == Page::Agents => {
                if let Some(agent) = self.selected_agent() {
                    // 有活动 Run 时恢复没有意义；parked/aborted 才真正重建实例。
                    if agent.run_status.is_some() {
                        self.notice =
                            Some("Agent is busy; cancel the run or send a message.".into());
                    } else {
                        self.control(Control::Revive, None);
                    }
                }
            }
            _ => {}
        }
        false
    }

    fn control(&mut self, action: Control, text: Option<String>) {
        let Some(agent) = self.selected_agent() else {
            return;
        };
        let available = match action {
            Control::Cancel => agent.run_status.is_some(),
            // 恢复是幂等的：空闲实例上为无操作，parked/aborted 才重建。
            Control::Revive => agent.run_status.is_none(),
            Control::Kill | Control::Send => true,
        };
        if !available {
            self.notice = Some(match action {
                Control::Cancel => "No active run to cancel.".into(),
                Control::Revive => "Agent is not parked or aborted.".into(),
                _ => String::new(),
            });
            return;
        }
        let id = agent.id.clone();
        self.commands.push(Command::Control {
            agent: id,
            action,
            text,
        });
    }

    fn handle_mouse(&mut self, mouse: crossterm::event::MouseEvent) {
        let point = (mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let up = mouse.kind == MouseEventKind::ScrollUp;
                if self.page == Page::Agents {
                    let in_inspector = !contains(self.roster_area, point);
                    if in_inspector && self.detail.is_none() {
                        self.inspector = if up {
                            self.inspector.saturating_sub(1)
                        } else {
                            self.inspector.saturating_add(1)
                        };
                    } else {
                        self.move_selection(if up { -1 } else { 1 });
                    }
                } else {
                    self.activity_step(if up { -1 } else { 1 });
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if self.page == Page::Agents {
                    if let Some((_, agent)) = self
                        .hits
                        .iter()
                        .find(|(rect, _)| contains(*rect, point))
                        .cloned()
                    {
                        self.selected = Some(agent.clone());
                        self.inspector = 0;
                        self.commands.push(Command::Open {
                            agent,
                            anchor: None,
                        });
                    }
                } else if let Some((_, index)) = self
                    .activity_hits
                    .iter()
                    .find(|(rect, _)| contains(*rect, point))
                    .copied()
                    && let Some(row) = self.projection.rows().get(index)
                {
                    self.activity.selected = Some(row.id());
                    self.activity.follow = false;
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::*;
    use super::*;

    #[test]
    fn mouse_click_on_roster_row_opens_detail() {
        let mut drafts = Drafts::default();
        let mut panel = panel(vec![record("a1", "Main", "idle")], vec![]);
        let _ = render(&mut panel, &mut drafts, 140, 40);
        let (rect, agent) = panel.hits.first().cloned().expect("roster hit rect");
        assert_eq!(agent, "a1");
        panel.handle(
            Event::Mouse(crossterm::event::MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: rect.x + 2,
                row: rect.y + 1,
                modifiers: KeyModifiers::NONE,
            }),
            &mut drafts,
        );
        let commands = panel.take_commands();
        assert!(
            matches!(commands.as_slice(), [Command::Open { agent, .. }] if agent == "a1"),
            "roster click must open the mapped agent"
        );
    }
}
