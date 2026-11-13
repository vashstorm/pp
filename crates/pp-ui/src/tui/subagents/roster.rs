//! Roster 页：选择校验、列表绘制与概览指标。

use super::inspector::{compact, format_cost, model_line, status_icon, usage};
use super::*;

impl Panel {
    pub(super) fn validate_selection(&mut self) {
        let list = self.projection.list(self.tree, &self.filter);
        let known = self.selected.as_deref().is_none_or(|selected| {
            list.iter()
                .any(|row| self.projection.agents()[row.agent].id == selected)
        });
        if !known {
            self.selected = None;
            self.inspector = 0;
        }
        if self.selected.is_none() {
            self.selected = list
                .first()
                .map(|row| self.projection.agents()[row.agent].id.clone());
        }
    }

    pub(super) fn visible(&self) -> Vec<TreeRow> {
        self.projection.list(self.tree, &self.filter)
    }

    pub(super) fn selected_index(&self, list: &[TreeRow]) -> Option<usize> {
        let selected = self.selected.as_deref()?;
        list.iter()
            .position(|row| self.projection.agents()[row.agent].id == selected)
    }

    pub(super) fn selected_agent(&self) -> Option<&AgentView> {
        let list = self.visible();
        if list.is_empty() {
            return None;
        }
        // 过滤后旧选择可能不可见：控制只能作用于当前可见行。
        let index = self.selected_index(&list).unwrap_or(0);
        self.projection.agents().get(list[index].agent)
    }

    pub(super) fn move_selection(&mut self, delta: isize) {
        let list = self.visible();
        if list.is_empty() {
            self.selected = None;
            return;
        }
        let current = self.selected_index(&list).unwrap_or(0) as isize;
        let next = (current + delta).clamp(0, list.len() as isize - 1) as usize;
        if current != next as isize {
            self.inspector = 0;
        }
        self.selected = Some(self.projection.agents()[list[next].agent].id.clone());
    }

    pub(super) fn filter_editing(&mut self) -> bool {
        if self.page == Page::Agents {
            self.filtering
        } else {
            self.activity.editing
        }
    }

    pub(super) fn type_filter(&mut self, key: KeyEvent) -> bool {
        let buffer = if self.page == Page::Agents {
            &mut self.filter
        } else {
            &mut self.activity.query
        };
        match key.code {
            KeyCode::Backspace => {
                buffer.pop();
                true
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                buffer.push(character);
                true
            }
            _ => false,
        }
    }

    pub(super) fn filter_push(&mut self, text: &str) {
        for character in text.chars().filter(|character| !character.is_control()) {
            self.filter.push(character);
        }
    }

    pub(super) fn query_push(&mut self, text: &str) {
        for character in text.chars().filter(|character| !character.is_control()) {
            self.activity.query.push(character);
        }
    }

    pub(in crate::tui) fn draw(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        drafts: &mut Drafts,
        cost: &pp_config::config::CostDisplay,
    ) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        frame.render_widget(Clear, area);
        self.roster_area = Rect::default();
        self.hits.clear();
        self.activity_hits.clear();
        if self.detail.is_some() {
            let agent = self.detail.as_ref().map(|detail| detail.agent.clone());
            if let Some(view) = agent
                .as_deref()
                .and_then(|agent| self.projection.agent(agent))
                .cloned()
            {
                if let Some(detail) = &mut self.detail {
                    detail.draw(frame, area, &view, drafts, cost, self.clock);
                }
                return;
            }
            let message = self
                .error
                .clone()
                .unwrap_or_else(|| "Waiting for agent data…".into());
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    message,
                    Style::default().fg(MUTED),
                ))),
                area,
            );
            return;
        }
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(HUB_BG))
            .border_style(Style::default().fg(ASK_BORDER))
            .title(Line::from(colored(format!(" {TITLE} "), ACCENT)));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.width == 0 || inner.height == 0 {
            return;
        }
        let footer = inner.height.saturating_sub(1).min(2);
        let body = Rect::new(inner.x, inner.y, inner.width, inner.height - footer);
        if footer == 2 {
            frame.render_widget(
                Paragraph::new(format!("├{}┤", "─".repeat(inner.width as usize)))
                    .style(Style::default().fg(ASK_BORDER)),
                Rect::new(area.x, body.bottom(), area.width, 1),
            );
        }
        if footer > 0 {
            let hint_area = inset_columns(Rect::new(inner.x, inner.bottom() - 1, inner.width, 1));
            self.draw_footer(frame, hint_area);
        }
        match self.page {
            Page::Agents => self.draw_agents(frame, body, footer == 2),
            Page::Activity => {
                let mut body = inset_columns(body);
                if let Some(alert) = self.alert() {
                    draw_line(frame, body, alert);
                    body = below(body, 1);
                }
                self.draw_activity(frame, body);
            }
        }
    }

    fn draw_footer(&self, frame: &mut Frame, area: Rect) {
        let hints: &[&str] = match self.page {
            Page::Agents => &[
                "1:agents",
                "2:activity",
                "↵/click:open",
                "Esc:close",
                "Tab:focus",
                "j/k/wheel:select",
                "PgUp/PgDn:details",
                "t:by parent",
                "/:filter",
                "c:cancel",
                "r:revive",
                "x:kill",
            ],
            Page::Activity => &[
                "1:agents",
                "2:activity",
                "↵:open",
                "Esc:back",
                "j/k:move",
                "Space:follow",
                "f:category",
                "s:scope",
                "/:search",
            ],
        };
        let mut text = String::new();
        for hint in hints {
            let separator = if text.is_empty() { "" } else { "  " };
            if Line::raw(&text).width() + separator.len() + Line::raw(*hint).width()
                <= area.width as usize
            {
                text.push_str(separator);
                text.push_str(hint);
            }
        }
        draw_line(frame, area, Line::from(colored(text, DIM)));
    }

    fn alert(&self) -> Option<Line<'static>> {
        if let Some(error) = self
            .error
            .as_ref()
            .or_else(|| self.projection.snapshot()?.warnings.first())
        {
            Some(Line::from(colored(projection::truncate(error), REMOVED)))
        } else {
            self.notice
                .as_ref()
                .map(|notice| Line::from(colored(projection::truncate(notice), ACCENT)))
        }
    }

    fn draw_roster_header(&self, frame: &mut Frame, mut area: Rect) -> Rect {
        let agents = self.projection.agents();
        let mut spans = vec![
            colored("Roster · ", ACTIVITY),
            colored("Flat", if self.tree { MUTED } else { ACCENT }),
            colored(" / ", DIM),
            colored("By parent", if self.tree { ACCENT } else { MUTED }),
        ];
        for status in [
            AgentStatus::Running,
            AgentStatus::Parked,
            AgentStatus::Idle,
            AgentStatus::Aborted,
        ] {
            let count = agents.iter().filter(|agent| agent.status == status).count();
            if count > 0 || matches!(status, AgentStatus::Running | AgentStatus::Parked) {
                spans.push(colored(" · ", DIM));
                spans.push(colored(
                    format!(
                        "{} {count} {}",
                        status_icon(status, self.clock),
                        status.label()
                    ),
                    status_color(status),
                ));
            }
        }
        draw_line(frame, area, Line::from(spans));
        area = below(area, 1);
        let totals = |value: fn(&AgentView) -> Option<u64>| {
            agents.iter().filter_map(value).reduce(u64::saturating_add)
        };
        let cost = agents
            .iter()
            .filter_map(|agent| agent.cost)
            .reduce(|a, b| a + b);
        let active = totals(|agent| agent.active_ms).map_or_else(|| "—".into(), format_duration);
        let tokens = totals(|agent| agent.tokens).map_or_else(|| "—".into(), compact);
        let requests: usize = agents.iter().map(|agent| agent.requests).sum();
        let tools: usize = agents.iter().map(|agent| agent.tools).sum();
        let timed = agents
            .iter()
            .filter(|agent| agent.active_ms.is_some())
            .count();
        let measured = agents.iter().filter(|agent| agent.cost.is_some()).count();
        let count = agents.len();
        let cost = format_cost(cost);
        let full = format!(
            " · {active} active agent time · {requests} req · {tools} tools · {tokens} tok · {timed}/{count} timed · {measured}/{count} measured"
        );
        let compact = format!(
            " {active} {requests}req {tools}tools {tokens}tok {timed}/{count} timed {measured}/{count} measured"
        );
        let metrics = if Line::raw(&full).width() + Line::raw(&cost).width() <= area.width as usize
        {
            full
        } else {
            compact
        };
        let mut stats = vec![colored(cost, COST), colored(metrics, DIM)];
        if agents.iter().any(|agent| agent.loading) {
            stats.push(colored(" · loading", ACCENT));
        }
        draw_line(frame, area, Line::from(stats));
        area = below(area, 2);
        if let Some(alert) = self.alert() {
            draw_line(frame, area, alert);
            area = below(area, 1);
        }
        area
    }

    fn draw_agents(&mut self, frame: &mut Frame, area: Rect, footer_separator: bool) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let (roster, inspector) = if area.width >= NARROW {
            let roster_width = (u32::from(area.width - 1) * 60 / 100) as u16;
            let divider = area.x + roster_width;
            frame.render_widget(
                Block::default()
                    .borders(Borders::LEFT)
                    .border_style(Style::default().fg(ASK_BORDER)),
                Rect::new(divider, area.y, 1, area.height),
            );
            for (y, symbol) in [(area.y.saturating_sub(1), "┬"), (area.bottom(), "┴")] {
                if symbol == "┬" || footer_separator {
                    frame.render_widget(
                        Paragraph::new(symbol).style(Style::default().fg(ASK_BORDER)),
                        Rect::new(divider, y, 1, 1),
                    );
                }
            }
            (
                Rect::new(area.x, area.y, roster_width, area.height),
                Some(Rect::new(
                    divider + 1,
                    area.y,
                    area.width - roster_width - 1,
                    area.height,
                )),
            )
        } else if self.focus == Focus::Inspector {
            (Rect::default(), Some(area))
        } else {
            (area, None)
        };
        self.roster_area = roster;
        if roster.width > 0 {
            let list = self.draw_roster_header(frame, inset_columns(roster));
            self.draw_roster(frame, list);
        }
        if let Some(inspector) = inspector {
            self.draw_inspector(frame, inset_columns(inspector));
        }
    }

    fn draw_roster(&mut self, frame: &mut Frame, area: Rect) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let rows = self.visible();
        if rows.is_empty() {
            let message = if self.projection.loading() {
                "Loading agents…"
            } else if !self.filter.trim().is_empty() {
                "No matching agents"
            } else {
                "No subagents yet. Spawned agents will appear here."
            };
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    message,
                    Style::default().fg(MUTED),
                ))),
                area,
            );
            return;
        }
        let heights = rows
            .iter()
            .map(|row| {
                if self.projection.agents()[row.agent].status == AgentStatus::Running {
                    3
                } else {
                    2
                }
            })
            .collect::<Vec<u16>>();
        let selected = self.selected_index(&rows);
        let start = visible_start(&heights, selected, area.height);
        let mut y = area.y;
        for (index, row) in rows.iter().enumerate().skip(start) {
            let height = heights[index];
            if y >= area.bottom() {
                break;
            }
            let height = height.min(area.bottom() - y);
            let rect = Rect::new(area.x, y, area.width, height);
            let is_selected = selected == Some(index);
            let lines = self.row_lines(row, area.width);
            let mut content = Paragraph::new(lines);
            if is_selected {
                content = content.style(Style::default().bg(SELECTED_BG));
            }
            frame.render_widget(content, rect);
            let agent = self.projection.agents()[row.agent].id.clone();
            self.hits.push((rect, agent));
            y += height;
        }
    }

    fn row_lines(&self, row: &TreeRow, width: u16) -> Vec<Line<'static>> {
        let width = width as usize;
        let agent = &self.projection.agents()[row.agent];
        let marker = if self.selected.as_deref() == Some(agent.id.as_str()) {
            "› "
        } else {
            "  "
        };
        let icon = status_icon(agent.status, self.clock);
        let name = projection::truncate(&agent.name);
        let model_column = width.div_ceil(2);
        let model = model_line(agent, width - model_column);
        let left = vec![
            colored(marker, ACCENT),
            colored(row.prefix.clone(), MUTED),
            colored(format!("{icon} "), status_color(agent.status)),
            colored(name, if row.muted { MUTED } else { ACTIVITY }),
        ];
        let mut first = fit_line(Line::from(left), model_column.saturating_sub(1));
        first
            .spans
            .push(Span::raw(" ".repeat(model_column - first.width())));
        first.spans.extend(model.spans);
        let indent = 3 + Line::raw(&row.prefix).width() + Line::raw(icon).width();
        let mut lines = vec![first];
        if agent.status == AgentStatus::Running {
            lines.push(indented(
                agent.task.as_deref().unwrap_or("—"),
                indent,
                width,
                MUTED,
            ));
        }
        lines.push(indented(
            &format!("{}   {}", usage(agent, "   "), self.age(agent)),
            indent,
            width,
            DIM,
        ));
        lines
    }

    pub(super) fn age(&self, agent: &AgentView) -> String {
        agent.last_activity.map_or_else(
            || "—".into(),
            |timestamp| detail::format_age(self.clock.saturating_sub(timestamp)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::*;
    use super::*;
    use pp_config::config::ModelRole;
    use pp_core::events::AgentEvent;
    use pp_core::model::ThinkLevel;
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn wide_layout_shows_selected_agent_and_semantic_colors() {
        let mut drafts = Drafts::default();
        let mut panel = panel(
            vec![
                record("a1", "Main", "idle"),
                record("a2", "Main", "running"),
            ],
            vec![run("r1", "a2", "running", "child work")],
        );
        let screen = render(&mut panel, &mut drafts, 140, 40).join("\n");
        assert!(screen.contains("Roster · Flat / By parent"), "{screen}");
        assert!(screen.contains("1:agents"));
        assert!(screen.contains("2:activity"));
        assert!(screen.contains("a1-name") && screen.contains("a2-name"));
        assert!(screen.contains("running"));
        // 选中项语义色：running 使用 ACCENT。
        let mut terminal = Terminal::new(TestBackend::new(140, 40)).unwrap();
        terminal
            .draw(|frame| panel.draw(frame, frame.area(), &mut drafts, &Default::default()))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        for (position, symbol) in [
            ((0, 0), "╭"),
            ((139, 39), "╯"),
            ((0, 37), "├"),
            ((83, 0), "┬"),
            ((83, 20), "│"),
            ((83, 37), "┴"),
            ((139, 37), "┤"),
        ] {
            let cell = &buffer[position];
            assert_eq!(cell.symbol(), symbol, "{position:?}");
            assert_eq!(cell.fg, ASK_BORDER, "{position:?}");
        }
        let marker = &buffer[(2, 4)];
        assert_eq!(marker.symbol(), "›");
        assert_eq!(marker.fg, ACCENT);
        assert_eq!(
            buffer[(4, 4)].symbol(),
            waiting_clock(u128::from(panel.clock))
        );
        assert_eq!(buffer[(5, 4)].symbol(), " ");
        assert_eq!(buffer[(6, 4)].fg, ACTIVITY);
        assert_eq!(buffer[(81, 5)].bg, SELECTED_BG);
    }

    #[test]
    fn roster_roles_align_with_different_model_widths() {
        let mut slow = record("a1", "Main", "running");
        slow.spec.model_role = Some(ModelRole::Slow);
        slow.spec.model = "provider/long-model-name".into();
        slow.spec.thinking = ThinkLevel::High;
        let mut fast = record("a2", "Main", "idle");
        fast.spec.model_role = Some(ModelRole::Fast);
        fast.spec.model = "provider/fast".into();
        let mut panel = panel(vec![slow, fast], vec![]);
        let mut drafts = Drafts::default();

        for width in [80, 140] {
            let screen = render(&mut panel, &mut drafts, width, 24);
            let role_column = |role| {
                screen
                    .iter()
                    .find_map(|line| line.find(role).map(|offset| line[..offset].chars().count()))
                    .unwrap()
            };
            assert_eq!(role_column("SLOW · "), role_column("FAST · "), "{width}");
        }
    }

    #[test]
    fn narrow_layout_tabs_between_roster_and_inspector() {
        let mut drafts = Drafts::default();
        let mut panel = panel(
            vec![record("a1", "Main", "idle")],
            vec![run("r1", "a1", "completed", "narrow work")],
        );
        let roster = render(&mut panel, &mut drafts, 80, 24).join("\n");
        assert!(roster.contains("a1-name"));
        assert!(!roster.contains("Usage"));
        panel.handle(key(KeyCode::Tab), &mut drafts);
        let inspector = render(&mut panel, &mut drafts, 80, 24).join("\n");
        assert!(inspector.contains("Usage"), "{inspector}");
        // Esc 先退出 inspector，再关闭 Hub。
        let close = panel.handle(key(KeyCode::Esc), &mut drafts);
        assert!(!close);
        let close = panel.handle(key(KeyCode::Esc), &mut drafts);
        assert!(close);
    }

    #[test]
    fn hub_overview_preserves_summary_and_final_response_when_parked() {
        let mut drafts = Drafts::default();
        let mut agent = record("a1", "Main", "running");
        agent.registered_ms = Some(500);
        let task = "盘点工作区中的依赖和边界".repeat(12);
        let mut work = run("r1", "a1", "running", &task);
        let mut panel = panel(vec![agent.clone()], vec![work.clone()]);
        panel.clock = 5_000;
        let run_state = |offset, status: &str| {
            let mut state = entry(offset, "r1", AgentEvent::AssistantStarted);
            state.record.event = None;
            state.record.hub = Some(pp_runtime::subagent::HubEvent::RunState {
                status: status.to_owned(),
            });
            state
        };
        let delta = |offset, text: &str| {
            entry(
                offset,
                "r1",
                AgentEvent::ReasoningSummaryDelta { text: text.into() },
            )
        };
        let tool = entry(
            500,
            "r1",
            AgentEvent::ToolStarted {
                id: "bash-1".into(),
                name: "bash".into(),
                arguments: serde_json::json!({"command":"cargo tree"}).to_string(),
            },
        );
        let reply = |entries| Reply::Activity {
            epoch: 1,
            agent: "a1".into(),
            page: pp_runtime::subagent::EventPage {
                entries,
                cursor: None,
                reset: false,
                has_more: false,
                warnings: vec![],
            },
        };
        panel.apply(reply(vec![
            run_state(0, "running"),
            entry(1, "r1", AgentEvent::AssistantStarted),
            delta(2, "检查"),
            delta(3, "依赖边界"),
            entry(
                4,
                "r1",
                AgentEvent::AssistantFinished {
                    text: "intermediate response".into(),
                },
            ),
            entry(
                6,
                "r1",
                AgentEvent::ContextUpdated {
                    usage: pp_core::context::ContextUsage {
                        estimated_tokens: 10_000,
                        context_window: Some(20_000),
                        last_request: None,
                        session_output_tokens: 0,
                        auto_compact: false,
                        threshold_percent: 80,
                    },
                },
            ),
            tool,
        ]));
        panel.handle(key(KeyCode::Tab), &mut drafts);
        let running = render(&mut panel, &mut drafts, 70, 40);
        let task_row = running
            .iter()
            .position(|line| line.contains("│ Task "))
            .unwrap();
        assert!(running[task_row + 1].contains('…'));
        assert!(running[task_row + 2].trim_matches(['│', ' ']).is_empty());
        let running = running.join("\n");
        assert!(
            running.replace(' ', "").contains("检查依赖边界") && running.contains("4s active"),
            "{running}"
        );
        assert!(running.contains("10K/20K 50%"));
        assert!(
            running.replace(' ', "").contains("bash检查依赖边界"),
            "{running}"
        );
        assert!(!running.contains("cargo tree"), "{running}");
        assert!(!running.contains("intermediate response"));

        panel.apply(reply(vec![
            entry(
                750,
                "r1",
                AgentEvent::ToolFinished {
                    id: "bash-1".into(),
                    result: serde_json::json!({"ok":true}),
                },
            ),
            entry(1_000, "r1", AgentEvent::AssistantStarted),
            entry(
                1_100,
                "r1",
                AgentEvent::AssistantFinished {
                    text: String::new(),
                },
            ),
            entry(
                1_200,
                "r1",
                AgentEvent::ToolStarted {
                    id: "read-1".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({"path":"Cargo.toml"}).to_string(),
                },
            ),
            entry(
                1_300,
                "r1",
                AgentEvent::ToolFinished {
                    id: "read-1".into(),
                    result: serde_json::json!({"ok":true}),
                },
            ),
            run_state(4_000, "completed"),
        ]));
        agent.status = "parked".into();
        work.status = "completed".into();
        work.result = Some(serde_json::json!({"ok":true, "output":"final answer"}));
        panel.apply(Reply::Snapshot {
            epoch: 1,
            snapshot: Box::new(snapshot(vec![agent], vec![work])),
        });
        let parked = render(&mut panel, &mut drafts, 70, 40);
        let current = parked
            .iter()
            .position(|line| line.contains("│ Current "))
            .unwrap();
        assert!(
            parked[current + 1]
                .replace(' ', "")
                .contains("检查依赖边界")
        );
        let parked = parked.join("\n");
        for expected in [
            "parked",
            "4s span",
            "Registered",
            "agent://a1",
            "\u{f00c} bash",
            "\u{f00c} read",
            "◆ Response final answer",
        ] {
            assert!(parked.contains(expected), "missing {expected}\n{parked}");
        }
        assert!(parked.replace(' ', "").contains("\u{f00c}bash检查依赖边界"));
        assert!(!parked.replace(' ', "").contains("\u{f00c}read检查依赖边界"));
        assert!(!parked.contains("Cargo.toml"));
        assert!(!parked.contains("10K/20K 50%"));
        assert!(!parked.contains("intermediate response"));
    }

    #[test]
    fn filter_keeps_lineage_and_empty_state_blocks_controls() {
        let mut drafts = Drafts::default();
        let mut panel = panel(
            vec![record("a1", "Main", "idle"), record("a2", "a1", "parked")],
            vec![],
        );
        panel.handle(key(KeyCode::Char('t')), &mut drafts);
        panel.handle(key(KeyCode::Char('/')), &mut drafts);
        for character in "a2".chars() {
            panel.handle(key(KeyCode::Char(character)), &mut drafts);
        }
        panel.handle(key(KeyCode::Enter), &mut drafts);
        let rows = panel.visible();
        assert_eq!(rows.len(), 2, "树中保留匹配项的祖先");
        assert!(rows[0].muted && !rows[1].muted);
        // 无匹配时不保留隐藏选择，也不能继续发控制。
        for character in "zzz".chars() {
            panel.handle(key(KeyCode::Char('/')), &mut drafts);
            panel.handle(key(KeyCode::Char(character)), &mut drafts);
            panel.handle(key(KeyCode::Enter), &mut drafts);
        }
        assert!(panel.visible().is_empty());
        assert!(panel.selected_agent().is_none());
    }
}
