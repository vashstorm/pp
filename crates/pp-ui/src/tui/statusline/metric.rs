//! Construction of metric fields and group-wide degradation by width.

use super::*;
use pp_runtime::subagent::SubagentCounts;

/// K from a thousand, M from a million, dropping a meaningless .0; the CLI still uses full integers.
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

/// Speed icon: below 60 tok/s is low, 60 to 120 is medium, 120 and above is high.
fn speed_icon(rate: Option<f64>) -> &'static str {
    let icons = themes::current_icons();
    match rate {
        Some(rate) if rate >= SPEED_FAST_RATE => speed_fast_icon(icons),
        Some(rate) if rate >= SPEED_MEDIUM_RATE => speed_medium_icon(icons),
        _ => speed_slow_icon(icons),
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
    field(context_icon(themes::current_icons()), text, context_color())
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
    // Accumulate the column width grapheme by grapheme instead of remeasuring the whole string for every prefix.
    let mut columns = 0;
    for grapheme in text.graphemes(true) {
        let size = Span::raw(grapheme).width();
        if columns + size > budget {
            break;
        }
        title.push_str(grapheme);
        columns += size;
    }
    if truncated {
        title.push('…');
    }
    field(title_icon(themes::current_icons()), title, plan_mode())
}

fn cost_amount(usd: f64, display: &pp_config::config::CostDisplay) -> Option<String> {
    let total = usd * display.exchange_rate;
    if !total.is_finite() || usd < 0.0 {
        return None;
    }
    let precision = if total > 10.0 { 2 } else { 3 };
    Some(format!("{total:.precision$}"))
}

/// Cost field: the icon of a supported currency plus the amount converted per `CostDisplay`.
pub(super) fn amount_field(
    usd: f64,
    display: &pp_config::config::CostDisplay,
) -> Option<Line<'static>> {
    let text = cost_amount(usd, display)?;
    let icon = if themes::is_ascii() {
        display.ascii_icon()
    } else {
        display.icon()
    }?;
    Some(field(icon.encode_utf8(&mut [0; 4]), text, cost_color()))
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
    Span::styled(METRIC_SEPARATOR, Style::default().fg(Color::DarkGray))
}

/// Separator between metrics; concatenation and the width budget share the same text.
const METRIC_SEPARATOR: &str = " · ";

/// Column width of a row assembled from existing fields: sum of field widths + leading/trailing spaces and separators.
fn joined_width(fields: &[Option<Line<'static>>]) -> usize {
    let mut width = 0;
    let mut count = 0;
    for line in fields.iter().flatten() {
        width += line.width();
        count += 1;
    }
    if count == 0 {
        return 0;
    }
    width + 2 + (count - 1) * Line::raw(METRIC_SEPARATOR).width()
}

/// Usage input for the status bar metric cluster: the caller builds `context` once and reuses it
/// so the same field is not formatted twice in one frame.
pub(in crate::tui) struct Metrics<'a> {
    pub usage: &'a ContextUsage,
    pub context: Line<'static>,
}

pub(super) fn speed_field(rate: Option<f64>) -> Line<'static> {
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
    field(speed_icon(rate), speed_text, speed_color())
}

fn subagent_fields(counts: SubagentCounts) -> [Option<Line<'static>>; 2] {
    let icons = themes::current_icons();
    let badge = |count: usize, color| {
        let icon = match count {
            0 => return None,
            1 => subagent_one_icon(icons),
            2 => subagent_two_icon(icons),
            _ => subagent_group_icon(icons),
        };
        Some(field(icon, count.to_string(), color))
    };
    [
        badge(counts.running, subagents_color()),
        badge(counts.sessions, Color::DarkGray),
    ]
}

pub(in crate::tui) fn subagent_counts_line(counts: SubagentCounts, width: u16) -> Line<'static> {
    let width = usize::from(width);
    let mut fields = subagent_fields(counts);
    if fields[0].is_some() && joined_width(&fields) > width {
        fields[1] = None;
    }
    if joined_width(&fields) > width {
        return Line::default();
    }
    join_fields(&fields)
}

fn activity_field(rate: Option<f64>, counts: SubagentCounts) -> Line<'static> {
    // Subagent counts and speed share the existing group-wide width budget.
    let mut spans = Vec::new();
    for badge in subagent_fields(counts).into_iter().flatten() {
        spans.extend(badge.spans);
        spans.push(metric_separator());
    }
    spans.extend(speed_field(rate).spans);
    Line::from(spans)
}

/// Whether to show usage metrics alongside the badges. The context baseline alone is hidden.
pub(in crate::tui) fn has_data(
    usage: &ContextUsage,
    speed: Option<(f64, bool)>,
    running_subagents: usize,
    live_output: Option<u64>,
    task: Option<(
        &pp_config::task_status::Snapshot,
        &pp_config::config::CostDisplay,
    )>,
    elapsed: Option<&str>,
) -> bool {
    usage.last_request.is_some()
        || usage.session_output_tokens > 0
        || speed.is_some()
        || running_subagents > 0
        || live_output.is_some()
        || elapsed.is_some()
        || task.is_some_and(|(t, d)| t.title.is_some() || cost_field(t, d).is_some())
}

/// All metrics share a display-column budget: shorten the title first, then degrade group by group, and keep Ctx last.
pub(in crate::tui) fn metric_fields(
    metrics: Metrics<'_>,
    speed: Option<(f64, bool)>,
    subagents: SubagentCounts,
    live_output: Option<u64>,
    task: Option<(
        &pp_config::task_status::Snapshot,
        &pp_config::config::CostDisplay,
    )>,
    elapsed: Option<&str>,
    width: u16,
) -> Line<'static> {
    let usage = metrics.usage;
    let icons = themes::current_icons();
    let count = |value: Option<u64>| value.map_or_else(|| "—".into(), abbreviate_tokens);
    let last = usage.last_request;
    // out accumulates the server output tokens for the whole session; while a generating request has no usage yet,
    // add the estimate for received bytes on top of the total and refresh it together with the total once finished.
    let session_output = usage.session_output_tokens;
    let output_tokens = match live_output {
        Some(live) => Some(session_output.saturating_add(live)),
        None if last.is_some() || session_output > 0 => Some(session_output),
        None => None,
    };
    let mut fields = [
        Some(activity_field(speed.map(|(rate, _)| rate), subagents)),
        task.and_then(|(t, _)| t.title.as_ref())
            .map(|title| title_field(title, 32)),
        Some(metrics.context),
        Some(pair_field(
            io_icon(icons),
            count(last.map(|u| u.input_tokens)),
            count(output_tokens),
            (input_color(), output_color()),
        )),
        Some(pair_field(
            cache_icon(icons),
            count(last.and_then(|u| u.cached_tokens)),
            count(last.and_then(|u| u.cache_write_tokens)),
            (cache_read_color(), cache_write_color()),
        )),
        task.and_then(|(task, display)| cost_field(task, display)),
        elapsed.map(|text| field(timer_icon(icons), text.replace(' ', ""), context_color())),
    ];
    let budget = usize::from(width);
    // Decide which fields to keep from the column budget first, then concatenate exactly once.
    let mut total = joined_width(&fields);
    if total > budget
        && let Some(title) = fields[1].as_ref()
    {
        let text = title.spans[1].content.as_ref();
        let available = Line::from(text).width().saturating_sub(total - budget);
        fields[1] = (available >= 2).then(|| title_field(text, available));
        total = joined_width(&fields);
    }
    for remove in [1, 4, 3, 0, 5, 6, 2] {
        if total <= budget {
            break;
        }
        fields[remove] = None;
        total = joined_width(&fields);
    }
    join_fields(&fields)
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::*;
    use super::*;
    use pp_core::context::TokenUsage;

    /// Tests call the metric cluster the same way the caller builds the ctx field.
    fn metrics(usage: &ContextUsage) -> Metrics<'_> {
        Metrics {
            usage,
            context: context_field(usage),
        }
    }

    #[test]
    fn metric_fields_units_colors_missing_usage_and_live_estimate() {
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

        let icons = themes::current_icons();
        let io = io_icon(icons);
        let cache = cache_icon(icons);
        {
            let usage = status_usage();
            let task = pp_config::task_status::Snapshot {
                title: Some("fix session history".into()),
                cost: 0.0123,
                ..Default::default()
            };
            let line = metric_fields(
                metrics(&usage),
                Some((30.2, true)),
                SubagentCounts::default(),
                None,
                Some((&task, &Default::default())),
                Some("1m 25s"),
                240,
            );
            assert_eq!(
                line_text(&line),
                format!(
                    " {} 30.2 tok/s · {} fix session history · {} 81.6%/500K · {} 12.3K/45.7K · {} 8K/0 · {} 0.012 · {} 1m25s ",
                    speed_slow_icon(icons),
                    title_icon(icons),
                    context_icon(icons),
                    io,
                    cache,
                    cost_icon(icons),
                    timer_icon(icons),
                )
            );
            for (text, color) in [
                ("30.2 tok/s", speed_color()),
                ("fix session history", plan_mode()),
                ("81.6%/500K", context_color()),
                ("12.3K", input_color()),
                ("45.7K", output_color()),
                ("8K", cache_read_color()),
                ("0", cache_write_color()),
                ("0.012", cost_color()),
                ("1m25s", context_color()),
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

        {
            let mut usage = status_usage();
            usage.last_request = None;
            usage.session_output_tokens = 0;
            let text = line_text(&metric_fields(
                metrics(&usage),
                None,
                SubagentCounts::default(),
                None,
                None,
                None,
                240,
            ));
            assert!(text.contains(&format!("{io} —/—")));
            assert!(text.contains(&format!("{cache} —/—")));
            // Switching models or compacting clears the latest-request metrics; the session total output stays visible.
            usage.session_output_tokens = 45_678;
            let text = line_text(&metric_fields(
                metrics(&usage),
                None,
                SubagentCounts::default(),
                None,
                None,
                None,
                240,
            ));
            assert!(text.contains(&format!("{io} —/45.7K")), "{text}");
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
                    reasoning_tokens: None,
                });
                let text = line_text(&metric_fields(
                    metrics(&usage),
                    None,
                    SubagentCounts::default(),
                    None,
                    None,
                    None,
                    240,
                ));
                assert!(text.contains(&format!("{io} 0/0")));
                assert!(text.contains(&format!("{cache} {expected}")));
            }
        }

        {
            let usage = status_usage();
            // Generating: out adds the live estimate to the session total, while in and cache still come from the last completed request.
            let text = line_text(&metric_fields(
                metrics(&usage),
                None,
                SubagentCounts::default(),
                Some(4200),
                None,
                None,
                240,
            ));
            assert!(text.contains(&format!("{io} 12.3K/49.9K")), "{text}");
            assert!(text.contains(&format!("{cache} 8K/0")), "{text}");
            // Without an estimate, out is the session total and never falls back to a single request's output count.
            let text = line_text(&metric_fields(
                metrics(&usage),
                None,
                SubagentCounts::default(),
                None,
                None,
                None,
                240,
            ));
            assert!(text.contains(&format!("{io} 12.3K/45.7K")), "{text}");
            // With no historical usage for the first request, in/cache stay unknown and out is just this request's live estimate.
            let first = ContextUsage {
                last_request: None,
                session_output_tokens: 0,
                ..usage
            };
            let text = line_text(&metric_fields(
                metrics(&first),
                None,
                SubagentCounts::default(),
                Some(260),
                None,
                None,
                240,
            ));
            assert!(text.contains(&format!("{io} —/260")), "{text}");
        }
    }

    #[test]
    fn speed_context_and_cost_fields_formatting() {
        let icons = themes::current_icons();
        let slow = speed_slow_icon(icons);
        let medium = speed_medium_icon(icons);
        let fast = speed_fast_icon(icons);
        for (rate, icon) in [
            (None, slow),
            (Some(0.0), slow),
            (Some(59.9), slow),
            (Some(60.0), medium),
            (Some(119.9), medium),
            (Some(120.0), fast),
            (Some(240.0), fast),
        ] {
            assert_eq!(speed_icon(rate), icon, "{rate:?}");
        }
        for icon in [slow, medium, fast] {
            assert_eq!(Line::from(icon).width(), 1, "{icon}");
        }
        let text = line_text(&metric_fields(
            metrics(&status_usage()),
            Some((61.5, true)),
            SubagentCounts::default(),
            None,
            None,
            None,
            240,
        ));
        assert!(text.contains(&format!("{medium} 61.5 tok/s")), "{text}");

        let title = title_icon(icons);
        let usage = status_usage();
        for (rate, expected) in [
            (999.0, "999.0 tok/s"),
            (1000.0, "1.0K tok/s"),
            (1_000_000.0, "1.0M tok/s"),
        ] {
            assert!(
                line_text(&metric_fields(
                    metrics(&usage),
                    Some((rate, false)),
                    SubagentCounts::default(),
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
            format!("{title} 🙂👩‍💻…")
        );

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
            let ctx = context_icon(themes::current_icons());
            assert_eq!(line_text(&line), format!("{ctx} {expected}"));
            assert!(
                line.spans
                    .iter()
                    .all(|span| span.style.fg == Some(context_color()))
            );
        }

        let cost = cost_icon(themes::current_icons());
        let mut task = pp_config::task_status::Snapshot::default();
        for amount in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            task.cost = amount;
            assert!(cost_field(&task, &Default::default()).is_none());
        }
        for (amount, expected) in [(0.0016, "0.002"), (10.0, "10.000"), (10.1251, "10.13")] {
            task.cost = amount;
            let line = cost_field(&task, &Default::default()).unwrap();
            assert_eq!(line_text(&line), format!("{cost} {expected}"));
        }
        task.cost = 0.000001;
        task.incomplete = true;
        let line = cost_field(&task, &Default::default()).unwrap();
        assert_eq!(line_text(&line), format!("{cost} 0.000"));
        assert_eq!(line.spans[1].style.fg, Some(cost_color()));

        let converted = pp_config::task_status::Snapshot {
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
                cost_field(&converted, &display).map(|line| line_text(&line)),
                Some(format!("{icon} {expected}")),
                "{currency}"
            );
        }
        let unsupported = pp_config::config::CostDisplay {
            currency: "GBP".into(),
            exchange_rate: 1.0,
        };
        assert!(cost_field(&converted, &unsupported).is_none());

        let render = |snap: &pp_config::task_status::Snapshot| {
            cost_field(snap, &Default::default()).map(|line| line_text(&line))
        };
        let mut subtotal = pp_config::task_status::Snapshot {
            live: 0.0023,
            ..Default::default()
        };
        assert!(render(&subtotal).is_none());
        subtotal.incomplete = true;
        assert!(render(&subtotal).is_none());
        subtotal.history = Some(pp_config::task_status::HistoricalCost {
            usd: Some(0.01),
            incomplete: false,
        });
        subtotal.cost = 0.01;
        subtotal.incomplete = false;
        subtotal.live = 0.003;
        assert_eq!(render(&subtotal).unwrap(), format!("{cost} 0.020"));
        subtotal.cost += 0.001;
        subtotal.live = 0.0;
        assert_eq!(render(&subtotal).unwrap(), format!("{cost} 0.021"));
    }

    #[test]
    fn metric_fields_shrink_title_then_drop_whole_groups_by_priority() {
        let icons = themes::current_icons();
        let title = title_icon(icons);
        let cache = cache_icon(icons);
        let io = io_icon(icons);
        let cost = cost_icon(icons);
        let timer = timer_icon(icons);
        let context = context_icon(icons);
        let group = subagent_group_icon(icons);
        let usage = status_usage();
        let task = pp_config::task_status::Snapshot {
            title: Some("需要保留中文以及🙂的长任务标题以验证显示宽度".into()),
            cost: 0.02,
            ..Default::default()
        };
        // Below 60 tok/s the speed field uses the low-speed icon.
        let rate = 30.2;
        let render = |width, task| {
            metric_fields(
                metrics(&usage),
                Some((rate, false)),
                SubagentCounts {
                    running: 3,
                    sessions: 12,
                },
                None,
                Some((task, &Default::default())),
                Some("1m 25s"),
                width,
            )
        };
        let full = render(240, &task);
        assert!(line_text(&full).starts_with(&format!(" {group} 3 · {group} 12 · ")));
        let full_width = full.width() as u16;
        assert_eq!(render(full_width, &task), full);
        let shrunk = render(full_width - 2, &task);
        assert!(line_text(&shrunk).contains(title));
        assert!(line_text(&shrunk).contains('…'));
        let removal_order = [
            title,
            cache,
            io,
            speed_icon(Some(rate)),
            cost,
            timer,
            context,
        ];
        let mut previous = 0;
        for width in (0..=full_width).rev() {
            let line = render(width, &task);
            assert!(line.width() <= usize::from(width));
            let text = line_text(&line);
            assert_eq!(text.contains(group), text.contains("tok/s"));
            assert_eq!(
                text.contains(&format!("{group} 12")),
                text.contains("tok/s")
            );
            let missing = removal_order
                .iter()
                .take_while(|icon| !text.contains(**icon))
                .count();
            assert!(
                missing >= previous,
                "a narrower width must not bring back a hidden field"
            );
            previous = missing;
            for icon in &removal_order[missing..] {
                assert!(
                    text.contains(icon),
                    "degradation priority must not be skipped: {text}"
                );
            }
            for pair in ["12.3K/45.7K", "8K/0"] {
                let icon = if pair.starts_with("12") { io } else { cache };
                assert_eq!(text.contains(icon), text.contains(pair));
            }
        }
        assert!(line_text(&full).len() > full.width());
    }

    #[test]
    fn has_data_ignores_context_baseline_and_tracks_any_metric() {
        let baseline = ContextUsage {
            estimated_tokens: 1200,
            context_window: Some(40_000),
            auto_compact: false,
            threshold_percent: 80,
            last_request: None,
            session_output_tokens: 0,
        };
        let display = pp_config::config::CostDisplay::default();
        let empty_task = pp_config::task_status::Snapshot::default();
        assert!(!has_data(
            &baseline,
            None,
            0,
            None,
            Some((&empty_task, &display)),
            None
        ));

        let with_last_req = ContextUsage {
            last_request: Some(TokenUsage {
                input_tokens: 0,
                output_tokens: 0,
                cached_tokens: None,
                cache_write_tokens: None,
                reasoning_tokens: None,
            }),
            ..baseline
        };
        let with_session_out = ContextUsage {
            session_output_tokens: 1,
            ..baseline
        };
        let titled = pp_config::task_status::Snapshot {
            title: Some("t".into()),
            ..Default::default()
        };
        let costed = pp_config::task_status::Snapshot {
            cost: 0.001,
            ..Default::default()
        };

        for (case, usage, speed, subagents, live, task, elapsed) in [
            ("last_request", &with_last_req, None, 0, None, None, None),
            (
                "session_output",
                &with_session_out,
                None,
                0,
                None,
                None,
                None,
            ),
            ("speed", &baseline, Some((0.0, false)), 0, None, None, None),
            ("subagents", &baseline, None, 1, None, None, None),
            ("live_output", &baseline, None, 0, Some(0), None, None),
            (
                "task_title",
                &baseline,
                None,
                0,
                None,
                Some((&titled, &display)),
                None,
            ),
            (
                "task_cost",
                &baseline,
                None,
                0,
                None,
                Some((&costed, &display)),
                None,
            ),
            ("elapsed", &baseline, None, 0, None, None, Some("0s")),
        ] {
            assert!(
                has_data(usage, speed, subagents, live, task, elapsed),
                "{case} should make the metric row visible"
            );
        }
    }
}
