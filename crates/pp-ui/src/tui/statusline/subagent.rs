//! 子 Agent 详情的指标簇、标识裁剪与底栏。

use super::metric::{amount_field, field, join_fields, pair_field, speed_field};
use super::*;

/// 子 Agent 详情指标簇的输入：速度、上下文用量、token 总量回退、费用与耗时。
pub(in crate::tui) struct SubagentUsage<'a> {
    /// 当前请求的估速或最近一次成功请求的结算速度。
    pub speed: Option<f64>,
    /// 有 `ContextUpdated` 时按主状态栏口径显示 ctx、io 与缓存。
    pub usage: Option<&'a ContextUsage>,
    /// 缺少上下文用量时退回单个 token 总量字段。
    pub tokens: Option<u64>,
    /// 子 Agent 已结算的费用（USD）。
    pub cost: Option<f64>,
    /// 已计量费用的运行次数与总运行次数。
    pub measured: usize,
    pub runs: usize,
    /// `agent time` 毫秒数。
    pub time_ms: Option<u64>,
}

/// 子 Agent 详情输入框顶边框的指标簇：簇首琥珀色 `subagent` 标识永不隐藏，
/// 其余字段沿用主状态栏的构造与配色，宽度不足时按 cache → io → speed → 费用 → 耗时 → ctx
/// 整组隐藏，任何宽度下返回行宽都不超过 `width`。
pub(in crate::tui) fn subagent_metrics(
    usage: SubagentUsage<'_>,
    display: &pp_config::config::CostDisplay,
    width: u16,
) -> Line<'static> {
    let width = usize::from(width);
    let count = |value: Option<u64>| value.map_or_else(|| "—".into(), abbreviate_tokens);
    let mut fields = [
        Some(subagent_badge()),
        Some(speed_field(usage.speed, 0)),
        None,
        None,
        None,
        None,
        None,
    ];
    if let Some(context) = usage.usage {
        let last = context.last_request;
        // 与主状态栏一致：最近请求和会话累计都为空时右侧显示 —，而不是 0。
        let output = (last.is_some() || context.session_output_tokens > 0)
            .then_some(context.session_output_tokens);
        fields[2] = Some(context_field(context));
        fields[3] = Some(pair_field(
            IO_ICON,
            count(last.map(|request| request.input_tokens)),
            count(output),
            (INPUT, OUTPUT),
        ));
        fields[4] = Some(pair_field(
            CACHE_ICON,
            count(last.and_then(|request| request.cached_tokens)),
            count(last.and_then(|request| request.cache_write_tokens)),
            (CACHE_READ, CACHE_WRITE),
        ));
    } else if let Some(tokens) = usage.tokens {
        // 旧日志没有 ContextUpdated 时保留原先摘要里的 token 总量；它替代
        // ctx/io/cache 三个字段，因此在降级顺序里与 ctx 同位（最后隐藏）。
        fields[2] = Some(token_total_field(tokens));
    }
    fields[5] = usage.cost.and_then(|usd| {
        amount_field(usd, display).map(|mut line| {
            if usage.measured < usage.runs {
                line.spans.push(Span::styled(
                    format!(" ({} measured)", usage.measured),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            line
        })
    });
    fields[6] = usage
        .time_ms
        .map(|ms| field(TIMER_ICON, duration_text(ms), Color::White));

    let mut line = join_fields(&fields);
    for remove in [4, 3, 1, 5, 6, 2] {
        if line.width() <= width {
            break;
        }
        fields[remove] = None;
        line = join_fields(&fields);
    }
    if line.width() <= width {
        line
    } else {
        // 整行比标识还窄：只保留裁剪后的标识文本。
        clipped_subagent_badge(width)
    }
}

/// 指标簇首的固定标识，琥珀色粗体；作为整组参与降级的最后一道边界。
fn subagent_badge() -> Line<'static> {
    Line::from(Span::styled(
        format!("{SUBAGENT_ONE_ICON} subagent"),
        Style::default().fg(SUBAGENTS).add_modifier(Modifier::BOLD),
    ))
}

/// 极窄宽度下裁剪标识文本；`width` 为 0 时返回空行。
fn clipped_subagent_badge(width: usize) -> Line<'static> {
    let text = format!("{SUBAGENT_ONE_ICON} subagent");
    if Line::from(text.as_str()).width() <= width {
        return subagent_badge();
    }
    if width == 0 {
        return Line::from("");
    }
    Line::from(Span::styled(
        super::super::tool_display::clip(&text, width, true),
        Style::default().fg(SUBAGENTS).add_modifier(Modifier::BOLD),
    ))
}

/// 没有上下文用量时的单个 token 总量字段：数字沿用输入色，单位保持暗色。
fn token_total_field(tokens: u64) -> Line<'static> {
    Line::from(vec![
        Span::styled(abbreviate_tokens(tokens), Style::default().fg(INPUT)),
        Span::styled(" tok", Style::default().fg(Color::DarkGray)),
    ])
}

/// 与子 Agent 旧摘要的数据源一致：`{s}s`、`{m}m{ss:02}s`、`{h}h{mm:02}m`。
fn duration_text(ms: u64) -> String {
    let seconds = ms / 1000;
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m{:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h{:02}m", seconds / 3600, (seconds % 3600) / 60)
    }
}

/// 子 Agent 详情底栏：共用主状态栏的头部、配色与降级规则，不显示 git 与模式标签。
pub(in crate::tui) fn subagent_status(
    model: &str,
    think: ThinkLevel,
    workdir: &str,
    width: u16,
) -> Line<'static> {
    let context = StatusContext {
        workdir: workdir.to_owned(),
        git: None,
    };
    // 与 primary 的 budget_total 同口径：扣除前导空白与纸飞机图标列。
    let budget = usize::from(width).saturating_sub(1 + ICON_WIDTH);
    status_line(model, think, None, &context, budget)
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::*;
    use super::*;
    use ratatui::{Terminal, backend::TestBackend, widgets::Paragraph};

    /// 把单行渲染到测试终端，返回每一列的符号与前景色。
    fn rendered(line: Line<'static>, width: u16) -> Vec<(String, Color)> {
        let mut terminal = Terminal::new(TestBackend::new(width, 1)).unwrap();
        terminal
            .draw(|frame| frame.render_widget(Paragraph::new(line), frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..width)
            .filter_map(|column| buffer.cell((column, 0)))
            .map(|cell| (cell.symbol().to_owned(), cell.fg))
            .collect()
    }

    /// `needle` 起始字符所在单元格的实际前景色。
    fn fg_at(line: Line<'static>, width: u16, needle: &str) -> Color {
        fg_at_offset(line, width, needle, 0)
    }

    /// `needle` 起始字符后第 `offset` 个字符所在单元格的实际前景色。
    fn fg_at_offset(line: Line<'static>, width: u16, needle: &str, offset: usize) -> Color {
        let cells = rendered(line, width);
        let mut text = String::new();
        let mut columns = Vec::new();
        for (column, (symbol, _)) in cells.iter().enumerate() {
            for ch in symbol.chars() {
                text.push(ch);
                columns.push(column);
            }
        }
        let byte = text
            .find(needle)
            .unwrap_or_else(|| panic!("{needle:?} 不在 {text:?} 中"));
        let index = text[..byte].chars().count() + offset;
        cells[columns[index]].1
    }

    #[test]
    fn subagent_status_reuses_main_status_bar_head_and_colors() {
        let line = subagent_status("fixture/primary", ThinkLevel::Medium, "~/dev/pp", 120);
        assert_eq!(
            line_text(&line),
            format!(
                " {APP_ICON} · {MODEL_ICON} fixture/primary · {} med · {DIRECTORY_ICON} ~/dev/pp",
                palette::think_icon(ThinkLevel::Medium)
            )
        );
        let model = line
            .spans
            .iter()
            .find(|span| span.content == "fixture/primary")
            .unwrap();
        assert!(
            model.style.add_modifier.contains(Modifier::BOLD),
            "模型引用与主状态栏同为粗体"
        );
        assert_eq!(fg_at(line.clone(), 120, APP_ICON), ACCENT);
        assert_eq!(fg_at(line.clone(), 120, "fixture/primary"), MODEL);
        assert_eq!(fg_at(line.clone(), 120, "med"), MODEL);
        assert_eq!(fg_at(line, 120, "~/dev/pp"), DIRECTORY);
        // 每个思考档位都显示与主状态栏相同的图标和文案。
        for (level, text) in [
            (ThinkLevel::None, "off"),
            (ThinkLevel::Low, "low"),
            (ThinkLevel::Medium, "med"),
            (ThinkLevel::High, "hig"),
            (ThinkLevel::Xhigh, "xhi"),
            (ThinkLevel::Max, "max"),
        ] {
            assert_eq!(
                line_text(&subagent_status("m", level, "~", 120)),
                format!(
                    " {APP_ICON} · {MODEL_ICON} m · {} {text} · {DIRECTORY_ICON} ~",
                    palette::think_icon(level)
                ),
                "{level:?}"
            );
        }
    }

    #[test]
    fn subagent_status_degrades_cwd_then_model_then_prefix_within_width() {
        let model = "fixture/primary";
        let workdir = "~/dev/projects/pp";
        let think = ThinkLevel::Medium;
        let full = subagent_status(model, think, workdir, 120);
        assert_eq!(full.width(), 52, "完整状态栏的渲染宽度");
        assert!(line_text(&full).ends_with(&format!("{DIRECTORY_ICON} {workdir}")));

        // 1) 只缩短 cwd：模型与头部保留，目录以 … 开头保留尾部。
        let narrowed = line_text(&subagent_status(model, think, workdir, 52));
        assert!(narrowed.contains(APP_ICON), "{narrowed}");
        assert!(narrowed.contains(model), "{narrowed}");
        let cwd = narrowed
            .split_once(&format!("{DIRECTORY_ICON} "))
            .map(|(_, cwd)| cwd)
            .unwrap();
        assert!(cwd.starts_with('…'), "{narrowed}");
        assert!(
            workdir.ends_with(cwd.strip_prefix('…').unwrap()),
            "{narrowed}"
        );

        // 2) 再缩短模型：模型字段以 … 结尾，目录仍存在。
        let narrowed = subagent_status(model, think, workdir, 43);
        let text = line_text(&narrowed);
        let shortened = narrowed
            .spans
            .iter()
            .find(|span| span.content.starts_with("fixture"))
            .unwrap();
        assert!(
            shortened.content.ends_with('…'),
            "模型应被缩短: {}",
            shortened.content
        );
        assert!(!text.contains(model), "{text}");
        assert!(text.contains(APP_ICON), "{text}");
        assert!(text.contains(DIRECTORY_ICON), "{text}");

        // 3) 最后只留模型前缀：头部、档位与目录都消失。
        let prefix = line_text(&subagent_status(model, think, workdir, 17));
        assert_eq!(prefix, "fixture/prima…");
        let tiny = line_text(&subagent_status(model, think, workdir, 10));
        assert_eq!(tiny, "fixtur…");
        for width in 0..=120 {
            let line = subagent_status(model, think, workdir, width);
            assert!(
                line.width() <= usize::from(width),
                "width {width}: {} > {width}",
                line.width()
            );
        }
    }

    #[test]
    fn subagent_metrics_orders_fields_and_matches_main_status_bar_colors() {
        let usage = status_usage();
        let line = subagent_metrics(
            SubagentUsage {
                speed: Some(61.5),
                usage: Some(&usage),
                tokens: Some(12_300),
                cost: Some(0.012),
                measured: 1,
                runs: 1,
                time_ms: Some(85_000),
            },
            &Default::default(),
            240,
        );
        assert_eq!(
            line_text(&line),
            format!(
                " {SUBAGENT_ONE_ICON} subagent · {SPEED_MEDIUM_ICON} 61.5 tok/s · {CONTEXT_ICON} 81.6%/500K · {IO_ICON} 12.3K/45.7K · {CACHE_ICON} 8K/0 · {COST_ICON} 0.012 · {TIMER_ICON} 1m25s "
            )
        );
        let badge = line
            .spans
            .iter()
            .find(|span| span.content == format!("{SUBAGENT_ONE_ICON} subagent"))
            .unwrap();
        assert!(badge.style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(fg_at(line.clone(), 240, "subagent"), SUBAGENTS);
        assert_eq!(fg_at(line.clone(), 240, "61.5 tok/s"), SPEED);
        assert_eq!(fg_at(line.clone(), 240, "81.6%/500K"), Color::White);
        assert_eq!(fg_at(line.clone(), 240, "12.3K"), INPUT);
        assert_eq!(fg_at(line.clone(), 240, "45.7K"), OUTPUT);
        assert_eq!(fg_at(line.clone(), 240, "8K"), CACHE_READ);
        assert_eq!(fg_at_offset(line.clone(), 240, "8K/0", 3), CACHE_WRITE);
        assert_eq!(fg_at(line.clone(), 240, "0.012"), Color::Red);
        assert_eq!(fg_at(line, 240, "1m25s"), Color::White);
    }

    #[test]
    fn subagent_metrics_appends_measured_suffix_and_uses_token_fallback() {
        let usage = status_usage();
        let partial = subagent_metrics(
            SubagentUsage {
                speed: Some(61.5),
                usage: Some(&usage),
                tokens: None,
                cost: Some(0.012),
                measured: 1,
                runs: 3,
                time_ms: None,
            },
            &Default::default(),
            240,
        );
        assert!(line_text(&partial).contains("0.012 (1 measured)"));
        assert_eq!(
            partial
                .spans
                .iter()
                .find(|span| span.content == " (1 measured)")
                .unwrap()
                .style
                .fg,
            Some(Color::DarkGray)
        );
        // 已计量次数与总次数一致时不追加后缀。
        let settled = subagent_metrics(
            SubagentUsage {
                speed: Some(61.5),
                usage: Some(&usage),
                tokens: None,
                cost: Some(0.012),
                measured: 3,
                runs: 3,
                time_ms: None,
            },
            &Default::default(),
            240,
        );
        assert!(!line_text(&settled).contains("measured"));

        // 没有 ContextUpdated：用单个 token 总量字段替代 ctx/io/cache。
        let fallback = subagent_metrics(
            SubagentUsage {
                speed: Some(61.5),
                usage: None,
                tokens: Some(12_300),
                cost: Some(0.5),
                measured: 1,
                runs: 1,
                time_ms: Some(1_000),
            },
            &Default::default(),
            240,
        );
        assert_eq!(
            line_text(&fallback),
            format!(
                " {SUBAGENT_ONE_ICON} subagent · {SPEED_MEDIUM_ICON} 61.5 tok/s · 12.3K tok · {COST_ICON} 0.500 · {TIMER_ICON} 1s "
            )
        );
        assert_eq!(fg_at(fallback.clone(), 240, "12.3K"), INPUT);
        assert_eq!(fg_at_offset(fallback, 240, "12.3K tok", 5), Color::DarkGray);
    }

    #[test]
    fn subagent_metrics_never_hides_badge_and_hides_groups_in_priority_order() {
        let usage = status_usage();
        let render = |width: u16| {
            subagent_metrics(
                SubagentUsage {
                    speed: Some(61.5),
                    usage: Some(&usage),
                    tokens: None,
                    cost: Some(0.012),
                    measured: 1,
                    runs: 1,
                    time_ms: Some(85_000),
                },
                &Default::default(),
                width,
            )
        };
        let full = render(240);
        let full_width = u16::try_from(full.width()).unwrap();
        assert_eq!(line_text(&render(full_width)), line_text(&full));
        let removal_order = [
            CACHE_ICON,
            IO_ICON,
            SPEED_MEDIUM_ICON,
            COST_ICON,
            TIMER_ICON,
            CONTEXT_ICON,
        ];
        let mut previous = 0;
        for width in (0..=full_width).rev() {
            let line = render(width);
            assert!(
                line.width() <= usize::from(width),
                "width {width}: {} > {width}",
                line.width()
            );
            let text = line_text(&line);
            if width == 0 {
                assert!(text.is_empty(), "零宽度只返回空行");
                continue;
            }
            let missing = removal_order
                .iter()
                .take_while(|icon| !text.contains(**icon))
                .count();
            assert!(missing >= previous, "宽度下降不能恢复已隐藏的字段: {text}");
            previous = missing;
            for icon in &removal_order[missing..] {
                assert!(text.contains(icon), "不能跳过降级优先级: {text}");
            }
            assert!(
                text.contains(SUBAGENT_ONE_ICON) || text.contains('…'),
                "标识永不消失: {text}"
            );
        }
        // 极窄宽度只剩裁剪后的标识，且不越界。
        for width in [3u16, 1, 0] {
            let line = render(width);
            let text = line_text(&line);
            assert!(line.width() <= usize::from(width));
            for icon in [IO_ICON, CACHE_ICON, COST_ICON, TIMER_ICON, CONTEXT_ICON] {
                assert!(!text.contains(icon), "width {width}: {text}");
            }
            assert_eq!(text.is_empty(), width == 0, "width {width}: {text}");
        }
        assert_eq!(line_text(&render(3)), format!("{SUBAGENT_ONE_ICON} …"));
        assert_eq!(line_text(&render(1)), "…");
    }
}
