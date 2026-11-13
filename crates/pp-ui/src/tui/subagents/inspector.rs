//! Inspector 栏：选中 agent 的详情、指标与降级文本辅助。

use super::activity::activity_icon;
use super::*;

impl Panel {
    pub(super) fn draw_inspector(&mut self, frame: &mut Frame, area: Rect) {
        let Some(agent) = self.selected_agent() else {
            draw_line(
                frame,
                area,
                Line::from(colored("Select an agent to inspect.", MUTED)),
            );
            return;
        };
        let lines = self.inspector_lines(agent, area);
        let max =
            u16::try_from(lines.len().saturating_sub(area.height as usize)).unwrap_or(u16::MAX);
        self.inspector = self.inspector.min(max);
        frame.render_widget(Paragraph::new(lines).scroll((self.inspector, 0)), area);
    }

    fn inspector_lines(&self, agent: &AgentView, area: Rect) -> Vec<Line<'static>> {
        let width = area.width as usize;
        let mut lines = vec![Line::from(vec![
            colored(
                format!("{} ", status_icon(agent.status, self.clock)),
                status_color(agent.status),
            ),
            colored(projection::truncate(&agent.definition), ACTIVITY),
        ])];
        if agent.name != agent.definition {
            lines.push(Line::from(colored(
                projection::truncate(&agent.name),
                MUTED,
            )));
        }
        let status = agent
            .run_status
            .as_deref()
            .unwrap_or_else(|| agent.status.label());
        let age = self.age(agent);
        lines.push(Line::from(vec![
            colored(status.to_owned(), status_color(agent.status)),
            colored(format!(" · {} · active {age}", agent_time(agent)), DIM),
        ]));
        lines.push(model_line(agent, width));
        if let Some(change) = &agent.model_change {
            lines.push(Line::from(colored(projection::truncate(change), MUTED)));
        }
        section(&mut lines, "Task");
        lines.push(Line::from(colored(
            projection::truncate(agent.task.as_deref().unwrap_or("—")),
            ACTIVITY,
        )));
        section(&mut lines, "Current");
        lines.push(Line::from(colored(
            projection::truncate(
                agent
                    .current
                    .as_deref()
                    .or(agent.task.as_deref())
                    .unwrap_or("—"),
            ),
            ACTIVITY,
        )));
        section(&mut lines, "Usage");
        lines.push(Line::from(colored(usage(agent, " · "), ACTIVITY)));
        if agent.run_status.as_deref() == Some("running")
            && let Some(context) = agent.context
            && let Some(window) = context.context_window.filter(|window| *window > 0)
            && let Some(percent) = context.percent()
        {
            let filled = (percent.min(100) as usize + 5) / 10;
            lines.push(Line::from(vec![
                colored("━".repeat(filled), ACCENT),
                colored("─".repeat(10 - filled), DIM),
                colored(
                    format!(
                        " {}/{} {percent}%",
                        compact(context.estimated_tokens),
                        compact(window)
                    ),
                    ACTIVITY,
                ),
            ]));
        }
        section(&mut lines, "Lineage");
        let parent = self
            .projection
            .agent(&agent.parent)
            .map_or(agent.parent.as_str(), |parent| parent.name.as_str());
        lines.push(Line::from(colored(
            format!("Spawned by {}", projection::truncate(parent)),
            ACTIVITY,
        )));
        if let Some(registered) = agent
            .registered_ms
            .and_then(|ms| i64::try_from(ms).ok())
            .and_then(chrono::DateTime::from_timestamp_millis)
        {
            lines.push(Line::from(colored(
                format!(
                    "Registered {}",
                    registered
                        .with_timezone(&chrono::Local)
                        .format("%Y-%m-%d %H:%M %:z")
                ),
                DIM,
            )));
        }
        lines.push(Line::from(colored(format!("Agent ID {}", agent.id), DIM)));
        section(&mut lines, "Changes");
        let workspace = match (agent.readonly, agent.isolated) {
            (true, _) => "Read-only agent · no workspace writes",
            (_, true) => "Isolated workspace · patches only",
            _ => "Shared workspace · per-agent LoC not attributable",
        };
        lines.push(Line::from(colored(workspace, DIM)));
        if let Some((output, patches)) = agent.artifacts.split_first() {
            for patch in patches {
                lines.push(Line::from(colored(projection::truncate(patch), MUTED)));
            }
            lines.push(Line::from(colored("Output", ACTIVITY)));
            lines.push(Line::from(colored(output.clone(), ACTIVITY)));
        }
        if let Some(error) = &agent.last_error {
            lines.push(Line::from(colored(
                format!("last error: {}", projection::truncate(error)),
                REMOVED,
            )));
        }
        section(&mut lines, "Recent activity");
        // 概要始终保留最新几项，独立 Activity 页可查看更早事件。
        let limit = (area.height as usize)
            .saturating_sub(lines.len())
            .clamp(1, 20);
        let recent = self.projection.recent(&agent.id, limit);
        if recent.is_empty() {
            lines.push(Line::from(colored("—", DIM)));
        }
        for row in recent {
            let symbol = if row.kind == Kind::Response {
                "◆"
            } else {
                activity_icon(row.status, self.clock)
            };
            lines.push(Line::from(vec![
                colored(format!("{} ", detail::format_clock(row.timestamp)), DIM),
                colored(format!("{symbol} "), row_color(row.status)),
                colored(format!("{} ", projection::truncate(&row.title)), MUTED),
                colored(projection::truncate(&row.summary), ACTIVITY),
            ]));
        }
        lines
            .into_iter()
            .map(|line| fit_line(line, width))
            .collect()
    }
}

pub(super) fn model_line(agent: &AgentView, width: usize) -> Line<'static> {
    let role = agent.role.unwrap_or(&agent.definition);
    let prefix = colored(
        format!("{} · ", projection::truncate(role)),
        if agent.role.is_some() { ACCENT } else { MUTED },
    );
    let suffix = agent
        .thinking
        .parse::<pp_core::model::ThinkLevel>()
        .ok()
        .map(|level| colored(format!(" {} {}", think_icon(level), level.as_str()), EFFORT));
    let suffix_width = suffix.as_ref().map_or(0, Span::width);
    let model_width = width.saturating_sub(prefix.width() + suffix_width);
    if model_width < 3 {
        return fit_line(
            Line::from(colored(projection::truncate(&agent.model), MUTED)),
            width,
        );
    }
    let model =
        super::super::tool_display::clip(&projection::truncate(&agent.model), model_width, true);
    let mut spans = vec![prefix, colored(model, MUTED)];
    spans.extend(suffix);
    Line::from(spans)
}

pub(super) fn status_icon(status: AgentStatus, clock: u64) -> &'static str {
    match status {
        AgentStatus::Running => waiting_clock(u128::from(clock)),
        // Material Design 实心圆，与运行中的时钟图标同字体族、同尺寸。
        AgentStatus::Idle | AgentStatus::Parked => "\u{f0765}",
        AgentStatus::Aborted => "\u{f00d}",
    }
}

pub(super) fn format_cost(cost: Option<f64>) -> String {
    cost.map_or_else(|| "—".into(), |cost| format!("${cost:.3}"))
}

pub(super) fn compact(value: u64) -> String {
    if value >= 1_000_000 {
        format!("{:.1}M", value as f64 / 1_000_000.0).replace(".0M", "M")
    } else if value >= 1_000 {
        format!("{:.0}K", value as f64 / 1_000.0)
    } else {
        value.to_string()
    }
}

pub(super) fn agent_time(agent: &AgentView) -> String {
    let (time, label) = if agent.status == AgentStatus::Running && agent.active_ms.is_some() {
        (agent.active_ms, "active")
    } else if agent.status != AgentStatus::Running && agent.span_ms.is_some() {
        (agent.span_ms, "span")
    } else {
        (agent.time_ms, "run time")
    };
    time.map_or_else(
        || "—".into(),
        |ms| format!("{} {label}", format_duration(ms)),
    )
}

pub(super) fn usage(agent: &AgentView, separator: &str) -> String {
    let tokens = agent.tokens.map_or_else(|| "—".into(), compact);
    [
        format_cost(agent.cost),
        agent_time(agent),
        format!("{} req", agent.requests),
        format!("{} tools", agent.tools),
        format!("{tokens} tok"),
    ]
    .join(separator)
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::*;
    use super::*;
    use pp_core::context::TokenUsage;
    use pp_core::events::AgentEvent;
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn detail_first_draw_resolves_run_scoped_anchor_and_reset_keeps_read_preview() {
        use super::super::worker::DetailHistory;
        use crate::tui::Transcript;
        use pp_runtime::subagent::EventPage;
        use serde_json::json;

        let runs = vec![
            run("r1", "a1", "completed", "first task"),
            run("r2", "a1", "completed", "second task"),
        ];
        let mut panel = panel(vec![record("a1", "Main", "idle")], runs.clone());
        panel.opened(
            "a1",
            1,
            Some(Anchor {
                run: Some("r1".into()),
                offset: 0,
                tool: Some("same".into()),
            }),
            Transcript::with_read_preview(true),
        );
        let mut entries = vec![];
        for (id, path) in [("r1", "first.rs"), ("r2", "second.rs")] {
            entries.push(entry(
                entries.len() as u64,
                id,
                AgentEvent::ToolStarted {
                    id: "same".into(),
                    name: "read".into(),
                    arguments: json!({"path":path}).to_string(),
                },
            ));
            entries.push(entry(
                entries.len() as u64,
                id,
                AgentEvent::ToolFinished {
                    id: "same".into(),
                    result: json!({"ok":true,"content":format!("body of {path}")}),
                },
            ));
        }
        entries.push(entry(
            4,
            "r2",
            AgentEvent::AssistantFinished {
                text: (0..40)
                    .map(|i| format!("tail-{i}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            },
        ));
        panel.apply(Reply::Detail {
            epoch: 1,
            view: 1,
            agent: "a1".into(),
            history: DetailHistory {
                page: EventPage {
                    entries,
                    cursor: None,
                    reset: false,
                    has_more: false,
                    warnings: vec![],
                },
                runs: runs.clone(),
                settled: std::collections::BTreeSet::from(["r1".into(), "r2".into()]),
            },
        });
        // 历史可在首帧取得宽度前读完；此后没有新事件也必须定位。
        let mut drafts = Drafts::default();
        let screen = render(&mut panel, &mut drafts, 100, 12).join("\n");
        assert!(screen.contains("body of first.rs"), "{screen}");
        assert!(!screen.contains("tail-39"), "{screen}");
        panel.apply(Reply::Detail {
            epoch: 1,
            view: 1,
            agent: "a1".into(),
            history: DetailHistory {
                page: EventPage {
                    entries: vec![
                        entry(
                            0,
                            "r2",
                            AgentEvent::ToolStarted {
                                id: "same".into(),
                                name: "read".into(),
                                arguments: json!({"path":"replacement.rs"}).to_string(),
                            },
                        ),
                        entry(
                            1,
                            "r2",
                            AgentEvent::ToolFinished {
                                id: "same".into(),
                                result: json!({"ok":true,"content":"replacement body"}),
                            },
                        ),
                    ],
                    cursor: None,
                    reset: true,
                    has_more: false,
                    warnings: vec![],
                },
                runs,
                settled: std::collections::BTreeSet::from(["r2".into()]),
            },
        });
        let screen = render(&mut panel, &mut drafts, 100, 18).join("\n");
        assert!(screen.contains("replacement body"), "{screen}");
        assert!(
            !screen.contains("first.rs") && !screen.contains("tail-39"),
            "{screen}"
        );
    }

    #[test]
    fn detail_header_is_one_line_without_model_effort_or_usage_summary() {
        let mut drafts = Drafts::default();
        let mut panel = panel(
            vec![record("a1", "Main", "idle")],
            vec![run("r1", "a1", "running", "child work")],
        );
        panel.opened("a1", 1, None, crate::tui::Transcript::new());
        let screen = render(&mut panel, &mut drafts, 120, 30);
        assert!(
            screen[0].contains("Agent Hub · a1-name / a1 · running · parent Main"),
            "{}",
            screen[0]
        );
        for absent in ["fixture-model", "effort", "tokens"] {
            assert!(!screen[0].contains(absent), "{absent}: {}", screen[0]);
        }
        panel.apply(Reply::Detail {
            epoch: 1,
            view: 1,
            agent: "a1".into(),
            history: super::super::worker::DetailHistory {
                page: pp_runtime::subagent::EventPage {
                    entries: vec![entry(
                        0,
                        "r1",
                        AgentEvent::AssistantDelta {
                            text: (0..30)
                                .map(|i| format!("line-{i}"))
                                .collect::<Vec<_>>()
                                .join("\n"),
                            refusal: false,
                        },
                    )],
                    cursor: None,
                    reset: false,
                    has_more: false,
                    warnings: vec![],
                },
                runs: vec![run("r1", "a1", "running", "child work")],
                settled: std::collections::BTreeSet::new(),
            },
        });
        let screen = render(&mut panel, &mut drafts, 120, 30);
        let activity = screen
            .iter()
            .position(|line| line.contains("Responding"))
            .unwrap();
        assert!(screen[activity - 1].trim().is_empty(), "{screen:#?}");
    }

    #[test]
    fn detail_input_metrics_and_status_bar_reuse_main_styles() {
        let mut drafts = Drafts::default();
        let mut panel = usage_detail();
        panel.apply(Reply::Detail {
            epoch: 1,
            view: 1,
            agent: "a1".into(),
            history: super::super::worker::DetailHistory {
                page: pp_runtime::subagent::EventPage {
                    entries: vec![entry(
                        0,
                        "r1",
                        AgentEvent::ModelRequestFinished {
                            elapsed_ms: 2_000,
                            usage: Some(TokenUsage {
                                input_tokens: 100,
                                output_tokens: 123,
                                cached_tokens: None,
                                cache_write_tokens: None,
                            }),
                            success: true,
                        },
                    )],
                    cursor: None,
                    reset: false,
                    has_more: false,
                    warnings: vec![],
                },
                runs: vec![run("r1", "a1", "completed", "child work")],
                settled: std::collections::BTreeSet::from(["r1".into()]),
            },
        });
        let buffer = render_cells(&mut panel, &mut drafts, &Default::default(), 120, 30);
        let screen = rows(&buffer, 120, 30);
        let banner = screen
            .iter()
            .position(|line| line.contains("subagent"))
            .unwrap_or_else(|| panic!("{screen:#?}"));
        // 顶边框行：标识、速度、ctx、io 对与费用；左侧边框字符保留。
        assert!(screen[banner].starts_with('─'), "{}", screen[banner]);
        for expected in ["subagent", "61.5 tok/s", "12.3K/45.7K", "0.012"] {
            assert!(
                screen[banner].contains(expected),
                "{expected}: {}",
                screen[banner]
            );
        }
        let offset = screen[banner].find("subagent").unwrap();
        let column = screen[banner][..offset].chars().count();
        assert_eq!(
            buffer[(column as u16, banner as u16)].fg,
            Color::Rgb(254, 188, 56),
            "标识琥珀色"
        );
        // 提示行在输入框上方。
        assert!(
            screen[banner - 1].contains("Shift+Enter newline"),
            "{}",
            screen[banner - 1]
        );
        // 底边框整行是 ─。
        assert_eq!(screen[banner + 2], "─".repeat(120));
        // 末行状态栏复用主结构，不带旧摘要。
        for expected in ["fixture/primary", "hig", "/work/app"] {
            assert!(screen[29].contains(expected), "{expected}: {}", screen[29]);
        }
        let text = screen.join("\n");
        assert!(!text.contains(" tokens · "), "{text}");
    }

    #[test]
    fn detail_focus_requests_cursor_inside_editor() {
        let mut drafts = Drafts::default();
        let mut panel = panel(vec![record("a1", "Main", "idle")], vec![]);
        panel.opened("a1", 1, None, crate::tui::Transcript::new());
        let mut unfocused = Terminal::new(TestBackend::new(100, 30)).unwrap();
        unfocused
            .draw(|frame| panel.draw(frame, frame.area(), &mut drafts, &Default::default()))
            .unwrap();
        assert!(!unfocused.backend().cursor_visible(), "未聚焦不显示光标");
        panel.focus_editor();
        let mut focused = Terminal::new(TestBackend::new(100, 30)).unwrap();
        focused
            .draw(|frame| panel.draw(frame, frame.area(), &mut drafts, &Default::default()))
            .unwrap();
        assert!(focused.backend().cursor_visible(), "聚焦后显示光标");
        // 前 2 列为 ❯ 提示，下面依次是底边框与状态栏。
        assert_eq!(
            focused.backend().cursor_position(),
            ratatui::layout::Position::new(2, 30 - 3)
        );
    }

    #[test]
    fn detail_narrow_and_tiny_sizes_keep_badge_without_panicking() {
        let mut drafts = Drafts::default();
        let mut panel = usage_detail();
        let icons = [
            "\u{e62a}",
            "\u{f04e2}",
            "\u{f01bc}",
            "\u{f01c1}",
            "\u{f051b}",
        ];
        let counts = [120u16, 80, 60, 40, 30, 20]
            .into_iter()
            .map(|width| {
                let screen = render(&mut panel, &mut drafts, width, 24);
                let row = screen
                    .iter()
                    .find(|line| line.contains('\u{f007}'))
                    .unwrap_or_else(|| panic!("{width}: {screen:#?}"));
                icons.iter().filter(|icon| row.contains(**icon)).count()
            })
            .collect::<Vec<_>>();
        assert!(counts[0] > counts[counts.len() - 1], "{counts:?}");
        assert!(
            counts.windows(2).all(|pair| pair[0] >= pair[1]),
            "指标图标数量随宽度单调不增: {counts:?}"
        );
        for width in [40u16, 20] {
            let screen = render(&mut panel, &mut drafts, width, 24).join("\n");
            assert!(screen.contains("subagent"), "{width}: {screen}");
        }
        // 极矮或极窄的终端不得越界或 panic。
        for (width, height) in [
            (1u16, 1u16),
            (2, 2),
            (3, 3),
            (10, 3),
            (20, 1),
            (120, 2),
            (80, 1),
            (1, 12),
        ] {
            let _ = render(&mut panel, &mut drafts, width, height);
        }
    }
}
