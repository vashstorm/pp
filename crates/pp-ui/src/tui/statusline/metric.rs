//! 指标字段的构造与按宽度整组降级。

use super::*;

/// 满千使用 K、满百万使用 M，去掉无意义的 .0；CLI 仍用完整整数。
pub(in crate::tui) fn abbreviate_tokens(tokens: u64) -> String {
    if tokens < 1000 {
        tokens.to_string()
    } else {
        abbreviate_metric(tokens as f64)
    }
}

fn abbreviate_metric(value: f64) -> String {
    let (value, unit) = if value >= 1_000_000.0 {
        (value / 1_000_000.0, "M")
    } else if value >= 1000.0 {
        (value / 1000.0, "K")
    } else {
        (value, "")
    };
    let text = format!("{value:.1}");
    format!("{}{unit}", text.strip_suffix(".0").unwrap_or(&text))
}

/// 速度图标：低于 60 tok/s 为低速档，60 到 120 为中速档，120 及以上为高速档。
fn speed_icon(rate: Option<f64>) -> &'static str {
    match rate {
        Some(rate) if rate >= SPEED_FAST_RATE => SPEED_FAST_ICON,
        Some(rate) if rate >= SPEED_MEDIUM_RATE => SPEED_MEDIUM_ICON,
        _ => SPEED_SLOW_ICON,
    }
}

pub(super) fn field(icon: &str, text: String, color: Color) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{icon} "), Style::default().fg(color)),
        Span::styled(text, Style::default().fg(color)),
    ])
}

pub(in crate::tui) fn context_field(usage: &ContextUsage) -> Line<'static> {
    let text = match usage.context_window {
        Some(window) => format!(
            "{:.1}%/{}",
            usage.estimated_tokens as f64 * 100.0 / window as f64,
            abbreviate_tokens(window)
        ),
        None => format!("{}/?", abbreviate_tokens(usage.estimated_tokens)),
    };
    field(CONTEXT_ICON, text, Color::White)
}

pub(super) fn pair_field(
    icon: &str,
    left: String,
    right: String,
    colors: (Color, Color),
) -> Line<'static> {
    let mut line = field(icon, left, colors.0);
    line.spans
        .push(Span::styled("/", Style::default().fg(Color::DarkGray)));
    line.spans
        .push(Span::styled(right, Style::default().fg(colors.1)));
    line
}

fn title_field(text: &str, width: usize) -> Line<'static> {
    let truncated = Line::from(text).width() > width;
    let budget = width.saturating_sub(usize::from(truncated));
    let mut title = String::new();
    for grapheme in text.graphemes(true) {
        if Line::from(format!("{title}{grapheme}")).width() > budget {
            break;
        }
        title.push_str(grapheme);
    }
    if truncated {
        title.push('…');
    }
    field(TITLE_ICON, title, PLAN_MODE)
}

fn cost_amount(usd: f64, display: &pp_config::config::CostDisplay) -> Option<String> {
    let total = usd * display.exchange_rate;
    if !total.is_finite() || usd < 0.0 {
        return None;
    }
    let precision = if total > 10.0 { 2 } else { 3 };
    Some(format!("{total:.precision$}"))
}

/// 费用金额字段：受支持币种的图标加按 `CostDisplay` 换算后的金额，红色。
pub(super) fn amount_field(
    usd: f64,
    display: &pp_config::config::CostDisplay,
) -> Option<Line<'static>> {
    let text = cost_amount(usd, display)?;
    Some(field(
        display.icon()?.encode_utf8(&mut [0; 4]),
        text,
        Color::Red,
    ))
}

fn cost_field(
    task: &pp_config::task_status::Snapshot,
    display: &pp_config::config::CostDisplay,
) -> Option<Line<'static>> {
    let history = task.history.and_then(|history| history.usd);
    let usd = history.unwrap_or(0.0) + task.cost;
    if usd == 0.0 && history.is_none() {
        return None;
    }
    amount_field(usd, display)
}

pub(super) fn join_fields(fields: &[Option<Line<'static>>]) -> Line<'static> {
    let mut spans = Vec::new();
    for line in fields.iter().flatten() {
        if spans.is_empty() {
            spans.push(Span::raw(" "));
        } else {
            spans.push(metric_separator());
        }
        spans.extend(line.spans.iter().cloned());
    }
    if !spans.is_empty() {
        spans.push(Span::raw(" "));
    }
    Line::from(spans)
}

fn metric_separator() -> Span<'static> {
    Span::styled(" · ", Style::default().fg(Color::DarkGray))
}

pub(super) fn speed_field(rate: Option<f64>, running_subagents: usize) -> Line<'static> {
    let speed_text = rate.map_or_else(
        || "— tok/s".into(),
        |rate| {
            let rate = if rate >= 1_000_000.0 {
                format!("{:.1}M", rate / 1_000_000.0)
            } else if rate >= 1000.0 {
                format!("{:.1}K", rate / 1000.0)
            } else {
                format!("{rate:.1}")
            };
            format!("{rate} tok/s")
        },
    );
    let speed = field(speed_icon(rate), speed_text, SPEED);
    let icon = match running_subagents {
        0 => return speed,
        1 => SUBAGENT_ONE_ICON,
        2 => SUBAGENT_TWO_ICON,
        _ => SUBAGENT_GROUP_ICON,
    };
    // 子代理数量和速度作为同一组参与窄屏降级。
    let mut line = field(icon, running_subagents.to_string(), SUBAGENTS);
    line.spans.push(metric_separator());
    line.spans.extend(speed.spans);
    line
}

/// 所有指标共享显示列预算：先缩短标题，再按整组降级，最后保留 Ctx。
pub(in crate::tui) fn metric_fields(
    usage: &ContextUsage,
    speed: Option<(f64, bool)>,
    running_subagents: usize,
    live_output: Option<u64>,
    task: Option<(
        &pp_config::task_status::Snapshot,
        &pp_config::config::CostDisplay,
    )>,
    elapsed: Option<&str>,
    width: u16,
) -> Line<'static> {
    let count = |value: Option<u64>| value.map_or_else(|| "—".into(), abbreviate_tokens);
    let last = usage.last_request;
    // out 累计整个会话的服务端 output tokens；生成中的请求还没有 usage 时，
    // 在累计值上叠加已接收字节的估算，完成后随累计值一起刷新。
    let session_output = usage.session_output_tokens;
    let output_tokens = match live_output {
        Some(live) => Some(session_output.saturating_add(live)),
        None if last.is_some() || session_output > 0 => Some(session_output),
        None => None,
    };
    let mut fields = [
        Some(speed_field(speed.map(|(rate, _)| rate), running_subagents)),
        task.and_then(|(t, _)| t.title.as_ref())
            .map(|title| title_field(title, 32)),
        Some(context_field(usage)),
        Some(pair_field(
            IO_ICON,
            count(last.map(|u| u.input_tokens)),
            count(output_tokens),
            (INPUT, OUTPUT),
        )),
        Some(pair_field(
            CACHE_ICON,
            count(last.and_then(|u| u.cached_tokens)),
            count(last.and_then(|u| u.cache_write_tokens)),
            (CACHE_READ, CACHE_WRITE),
        )),
        task.and_then(|(task, display)| cost_field(task, display)),
        elapsed.map(|text| field(TIMER_ICON, text.replace(' ', ""), Color::White)),
    ];
    let budget = usize::from(width);
    let mut line = join_fields(&fields);
    if line.width() > budget
        && let Some(title) = fields[1].as_ref()
    {
        let text = title.spans[1].content.as_ref();
        let available = Line::from(text)
            .width()
            .saturating_sub(line.width() - budget);
        fields[1] = (available >= 2).then(|| title_field(text, available));
        line = join_fields(&fields);
    }
    for remove in [1, 4, 3, 0, 5, 6, 2] {
        if line.width() <= budget {
            return line;
        }
        fields[remove] = None;
        line = join_fields(&fields);
    }
    line
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::*;
    use super::*;
    use pp_core::context::TokenUsage;

    #[test]
    fn context_abbreviation_uses_thousands_and_millions_with_one_decimal() {
        for (tokens, expected) in [
            (0, "0"),
            (999, "999"),
            (1000, "1K"),
            (1050, "1.1K"),
            (9999, "10K"),
            (10000, "10K"),
            (10999, "11K"),
            (12345, "12.3K"),
            (128000, "128K"),
            (999900, "999.9K"),
            (1_000_000, "1M"),
            (1_240_000, "1.2M"),
            (1_280_000, "1.3M"),
            (1_000_000_000, "1000M"),
        ] {
            assert_eq!(abbreviate_tokens(tokens), expected);
        }
    }

    #[test]
    fn metric_fields_order_units_and_independent_pair_colors() {
        let usage = status_usage();
        let task = pp_config::task_status::Snapshot {
            title: Some("修复会话历史".into()),
            cost: 0.0123,
            ..Default::default()
        };
        let line = metric_fields(
            &usage,
            Some((30.2, true)),
            0,
            None,
            Some((&task, &Default::default())),
            Some("1m 25s"),
            240,
        );
        assert_eq!(
            line_text(&line),
            format!(
                " {SPEED_SLOW_ICON} 30.2 tok/s · {TITLE_ICON} 修复会话历史 · {CONTEXT_ICON} 81.6%/500K · {IO_ICON} 12.3K/45.7K · {CACHE_ICON} 8K/0 · {COST_ICON} 0.012 · {TIMER_ICON} 1m25s "
            )
        );
        for (text, color) in [
            ("30.2 tok/s", SPEED),
            ("修复会话历史", PLAN_MODE),
            ("81.6%/500K", Color::White),
            ("12.3K", INPUT),
            ("45.7K", OUTPUT),
            ("8K", CACHE_READ),
            ("0", CACHE_WRITE),
            ("0.012", Color::Red),
            ("1m25s", Color::White),
        ] {
            assert_eq!(
                line.spans
                    .iter()
                    .find(|span| span.content == text)
                    .unwrap()
                    .style
                    .fg,
                Some(color),
                "{text}"
            );
        }
    }

    #[test]
    fn metric_fields_distinguish_missing_usage_and_independent_zero_cache() {
        let mut usage = status_usage();
        usage.last_request = None;
        usage.session_output_tokens = 0;
        let text = line_text(&metric_fields(&usage, None, 0, None, None, None, 240));
        assert!(text.contains(&format!("{IO_ICON} —/—")));
        assert!(text.contains(&format!("{CACHE_ICON} —/—")));
        // 切模型或压缩清空最近请求指标，会话累计输出继续显示。
        usage.session_output_tokens = 45_678;
        let text = line_text(&metric_fields(&usage, None, 0, None, None, None, 240));
        assert!(text.contains(&format!("{IO_ICON} —/45.7K")), "{text}");
        usage.session_output_tokens = 0;
        for (read, write, expected) in [
            (None, Some(0), "—/0"),
            (Some(0), None, "0/—"),
            (Some(0), Some(0), "0/0"),
        ] {
            usage.last_request = Some(TokenUsage {
                input_tokens: 0,
                output_tokens: 0,
                cached_tokens: read,
                cache_write_tokens: write,
            });
            let text = line_text(&metric_fields(&usage, None, 0, None, None, None, 240));
            assert!(text.contains(&format!("{IO_ICON} 0/0")));
            assert!(text.contains(&format!("{CACHE_ICON} {expected}")));
        }
    }

    #[test]
    fn metric_fields_out_accumulates_session_usage_and_adds_live_estimate() {
        let usage = status_usage();
        // 生成中：out 在会话累计上叠加实时估算，in 与缓存仍是最近一次完成的请求。
        let text = line_text(&metric_fields(&usage, None, 0, Some(4200), None, None, 240));
        assert!(text.contains(&format!("{IO_ICON} 12.3K/49.9K")), "{text}");
        assert!(text.contains(&format!("{CACHE_ICON} 8K/0")), "{text}");
        // 没有估算时 out 就是会话累计，不回到单次请求的输出量。
        let text = line_text(&metric_fields(&usage, None, 0, None, None, None, 240));
        assert!(text.contains(&format!("{IO_ICON} 12.3K/45.7K")), "{text}");
        // 首个请求没有历史 usage 时，in/缓存保持未知，out 也只是本次实时估算。
        let first = ContextUsage {
            last_request: None,
            session_output_tokens: 0,
            ..usage
        };
        let text = line_text(&metric_fields(&first, None, 0, Some(260), None, None, 240));
        assert!(text.contains(&format!("{IO_ICON} —/260")), "{text}");
    }

    #[test]
    fn speed_icon_follows_rate_tiers() {
        for (rate, icon) in [
            (None, SPEED_SLOW_ICON),
            (Some(0.0), SPEED_SLOW_ICON),
            (Some(59.9), SPEED_SLOW_ICON),
            (Some(60.0), SPEED_MEDIUM_ICON),
            (Some(119.9), SPEED_MEDIUM_ICON),
            (Some(120.0), SPEED_FAST_ICON),
            (Some(240.0), SPEED_FAST_ICON),
        ] {
            assert_eq!(speed_icon(rate), icon, "{rate:?}");
        }
        // 三档图标都是单列字形，状态栏按固定图标列排版。
        for icon in [SPEED_SLOW_ICON, SPEED_MEDIUM_ICON, SPEED_FAST_ICON] {
            assert_eq!(Line::from(icon).width(), 1, "{icon}");
        }
        let text = line_text(&metric_fields(
            &status_usage(),
            Some((61.5, true)),
            0,
            None,
            None,
            None,
            240,
        ));
        assert!(
            text.contains(&format!("{SPEED_MEDIUM_ICON} 61.5 tok/s")),
            "{text}"
        );
    }

    #[test]
    fn metric_fields_speed_preserves_decimal_and_title_graphemes() {
        let usage = status_usage();
        for (rate, expected) in [
            (999.0, "999.0 tok/s"),
            (1000.0, "1.0K tok/s"),
            (1_000_000.0, "1.0M tok/s"),
        ] {
            assert!(
                line_text(&metric_fields(
                    &usage,
                    Some((rate, false)),
                    0,
                    None,
                    None,
                    None,
                    200
                ))
                .contains(expected)
            );
        }
        assert_eq!(
            line_text(&title_field("🙂👩‍💻标题", 5)),
            format!("{TITLE_ICON} 🙂👩‍💻…")
        );
    }

    #[test]
    fn context_field_is_white_with_only_a_leading_icon() {
        let mut usage = status_usage();
        for (window, tokens, expected) in [
            (Some(500_000), 250_000, "50.0%/500K"),
            (Some(500_000), 400_000, "80.0%/500K"),
            (Some(500_000), 600_000, "120.0%/500K"),
            (None, 600_000, "600K/?"),
        ] {
            usage.context_window = window;
            usage.estimated_tokens = tokens;
            let line = context_field(&usage);
            assert_eq!(line_text(&line), format!("\u{e62a} {expected}"));
            assert!(
                line.spans
                    .iter()
                    .all(|span| span.style.fg == Some(Color::White))
            );
        }
    }

    #[test]
    fn cost_field_rounds_amount_and_displays_partial_subtotal() {
        let mut task = pp_config::task_status::Snapshot::default();
        for amount in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            task.cost = amount;
            assert!(cost_field(&task, &Default::default()).is_none());
        }
        for (amount, expected) in [(0.0016, "0.002"), (10.0, "10.000"), (10.1251, "10.13")] {
            task.cost = amount;
            let line = cost_field(&task, &Default::default()).unwrap();
            assert_eq!(line_text(&line), format!("{COST_ICON} {expected}"));
        }
        task.cost = 0.000001;
        task.incomplete = true;
        let line = cost_field(&task, &Default::default()).unwrap();
        assert_eq!(line_text(&line), format!("{COST_ICON} 0.000"));
        assert_eq!(line.spans[1].style.fg, Some(Color::Red));
    }

    #[test]
    fn cost_field_shows_currency_icon_and_converted_amount() {
        let task = pp_config::task_status::Snapshot {
            cost: 0.012,
            ..Default::default()
        };
        for (currency, exchange_rate, icon, expected) in [
            ("USD", 1.0, "\u{f01c1}", "0.012"),
            ("CNY", 7.2, "\u{f07ba}", "0.086"),
            ("BRL", 5.0, "\u{f0b85}", "0.060"),
        ] {
            let display = pp_config::config::CostDisplay {
                currency: currency.into(),
                exchange_rate,
            };
            assert_eq!(
                cost_field(&task, &display).map(|line| line_text(&line)),
                Some(format!("{icon} {expected}")),
                "{currency}"
            );
        }
        // 校验拒绝的币种没有对应图标，费用字段整体隐藏。
        let unsupported = pp_config::config::CostDisplay {
            currency: "GBP".into(),
            exchange_rate: 1.0,
        };
        assert!(cost_field(&task, &unsupported).is_none());
    }

    #[test]
    fn cost_field_displays_only_settled_session_subtotal() {
        let render = |task: &pp_config::task_status::Snapshot| {
            cost_field(task, &Default::default()).map(|line| line_text(&line))
        };
        let mut task = pp_config::task_status::Snapshot {
            live: 0.0023,
            ..Default::default()
        };
        assert!(render(&task).is_none());
        task.incomplete = true;
        assert!(render(&task).is_none());

        task.history = Some(pp_config::task_status::HistoricalCost {
            usd: Some(0.01),
            incomplete: false,
        });
        task.cost = 0.01;
        task.incomplete = false;
        task.live = 0.003;
        assert_eq!(render(&task).unwrap(), format!("{COST_ICON} 0.020"));
        task.cost += 0.001;
        task.live = 0.0;
        assert_eq!(render(&task).unwrap(), format!("{COST_ICON} 0.021"));
    }

    #[test]
    fn metric_fields_shrink_title_then_drop_whole_groups_by_priority() {
        let usage = status_usage();
        let task = pp_config::task_status::Snapshot {
            title: Some("需要保留中文以及🙂的长任务标题以验证显示宽度".into()),
            cost: 0.02,
            ..Default::default()
        };
        // 低于 60 tok/s 时速度字段使用低速档图标。
        let rate = 30.2;
        let render = |width, task| {
            metric_fields(
                &usage,
                Some((rate, false)),
                3,
                None,
                Some((task, &Default::default())),
                Some("1m 25s"),
                width,
            )
        };
        let full = render(240, &task);
        let full_width = full.width() as u16;
        assert_eq!(render(full_width, &task), full);
        let shrunk = render(full_width - 2, &task);
        assert!(line_text(&shrunk).contains(TITLE_ICON));
        assert!(line_text(&shrunk).contains('…'));
        let removal_order = [
            TITLE_ICON,
            CACHE_ICON,
            IO_ICON,
            speed_icon(Some(rate)),
            COST_ICON,
            TIMER_ICON,
            CONTEXT_ICON,
        ];
        let mut previous = 0;
        for width in (0..=full_width).rev() {
            let line = render(width, &task);
            assert!(line.width() <= usize::from(width));
            let text = line_text(&line);
            assert_eq!(text.contains(SUBAGENT_GROUP_ICON), text.contains("tok/s"));
            let missing = removal_order
                .iter()
                .take_while(|icon| !text.contains(**icon))
                .count();
            assert!(missing >= previous, "宽度下降不能恢复已隐藏的字段");
            previous = missing;
            for icon in &removal_order[missing..] {
                assert!(text.contains(icon), "不能跳过降级优先级: {text}");
            }
            for pair in ["12.3K/45.7K", "8K/0"] {
                let icon = if pair.starts_with("12") {
                    IO_ICON
                } else {
                    CACHE_ICON
                };
                assert_eq!(text.contains(icon), text.contains(pair));
            }
        }
        assert!(line_text(&full).len() > full.width());
    }
}
