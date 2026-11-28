//! Metric cluster, badge clipping and bottom bar for the subagent detail view.

use super::metric::{amount_field, field, join_fields, pair_field, speed_field};
use super::*;

/// Inputs for the subagent detail metric cluster: speed, context usage, total-token fallback, cost and elapsed time.
pub(in crate::tui) struct SubagentUsage<'a> {
    /// The estimated speed of the current request or the settled speed of the last successful one.
    pub speed: Option<f64>,
    /// With `ContextUpdated`, ctx, io and cache are shown the same way as the main status bar.
    pub usage: Option<&'a ContextUsage>,
    /// Falls back to a single total-token field when context usage is missing.
    pub tokens: Option<u64>,
    /// Settled cost of the subagent (USD).
    pub cost: Option<f64>,
    /// Runs with measured cost out of total runs.
    pub measured: usize,
    pub runs: usize,
    /// `agent time` in milliseconds.
    pub time_ms: Option<u64>,
}

/// Metric cluster on the top border of the subagent detail input box: the leading amber `subagent`
/// badge never hides, the remaining fields reuse the main status bar's construction and palette, and
/// under pressure they hide as a group in cache → io → speed → cost → elapsed → ctx order, never exceeding `width`.
pub(in crate::tui) fn subagent_metrics(
    usage: SubagentUsage<'_>,
    display: &pp_config::config::CostDisplay,
    width: u16,
) -> Line<'static> {
    let width = usize::from(width);
    let count = |value: Option<u64>| value.map_or_else(|| "—".into(), abbreviate_tokens);
    let mut fields = [
        Some(subagent_badge()),
        Some(speed_field(usage.speed)),
        None,
        None,
        None,
        None,
        None,
    ];
    if let Some(context) = usage.usage {
        let icons = super::super::themes::current_icons();
        let last = context.last_request;
        // Consistent with the main status bar: with neither a latest request nor a session total, show — on the right instead of 0.
        let output = (last.is_some() || context.session_output_tokens > 0)
            .then_some(context.session_output_tokens);
        fields[2] = Some(context_field(context));
        fields[3] = Some(pair_field(
            io_icon(icons),
            count(last.map(|request| request.input_tokens)),
            count(output),
            (input_color(), output_color()),
        ));
        fields[4] = Some(pair_field(
            cache_icon(icons),
            count(last.and_then(|request| request.cached_tokens)),
            count(last.and_then(|request| request.cache_write_tokens)),
            (cache_read_color(), cache_write_color()),
        ));
    } else if let Some(tokens) = usage.tokens {
        // Old logs without ContextUpdated keep the total token count from the previous summary; it replaces
        // the ctx/io/cache fields, so it shares ctx's position in the degradation order (hidden last).
        fields[2] = Some(token_total_field(tokens));
    }
    fields[5] = usage.cost.and_then(|usd| {
        amount_field(usd, display).map(|mut line| {
            if usage.measured < usage.runs {
                line.spans.push(Span::styled(
                    format!(" ({} measured)", usage.measured),
                    Style::default().fg(muted_color()),
                ));
            }
            line
        })
    });
    let icons = super::super::themes::current_icons();
    fields[6] = usage
        .time_ms
        .map(|ms| field(timer_icon(icons), duration_text(ms), timer_color()));

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
        // The row is narrower than the badge itself: keep only the clipped badge text.
        clipped_subagent_badge(width)
    }
}

/// Fixed leading badge of the metric cluster, bold amber; it is the last boundary of the group-wide degradation.
fn subagent_badge() -> Line<'static> {
    let icons = super::super::themes::current_icons();
    Line::from(Span::styled(
        format!("{} subagent", subagent_one_icon(icons)),
        Style::default()
            .fg(subagents_color())
            .add_modifier(Modifier::BOLD),
    ))
}

/// Clip the badge text at very narrow widths; returns an empty row when `width` is 0.
fn clipped_subagent_badge(width: usize) -> Line<'static> {
    let icons = super::super::themes::current_icons();
    let text = format!("{} subagent", subagent_one_icon(icons));
    if Line::from(text.as_str()).width() <= width {
        return subagent_badge();
    }
    if width == 0 {
        return Line::from("");
    }
    Line::from(Span::styled(
        super::super::tool_display::clip(&text, width, true),
        Style::default()
            .fg(subagents_color())
            .add_modifier(Modifier::BOLD),
    ))
}

/// Single total-token field without context usage: the number keeps the input color while the unit stays dim.
fn token_total_field(tokens: u64) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            abbreviate_tokens(tokens),
            Style::default().fg(input_color()),
        ),
        Span::styled(" tok", Style::default().fg(muted_color())),
    ])
}

/// Matches the data source of the subagent's old summary: `{s}s`, `{m}m{ss:02}s`, `{h}h{mm:02}m`.
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

/// Subagent detail bottom bar: reuses the main status bar's header, palette and degradation rules, without git or mode tags.
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
    // Same basis as primary's budget_total: subtract the leading blank and the paper-plane icon column.
    let budget = usize::from(width).saturating_sub(1 + ICON_WIDTH);
    status_line(model, think, None, None, &context, budget)
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::*;
    use super::*;
    use ratatui::{Terminal, backend::TestBackend, widgets::Paragraph};

    /// Render one row into a test terminal, returning each column's symbol and foreground color.
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

    /// Actual foreground color of the cell holding the first character of `needle`.
    fn fg_at(line: Line<'static>, width: u16, needle: &str) -> Color {
        fg_at_offset(line, width, needle, 0)
    }

    /// Actual foreground color of the cell holding the character `offset` positions after the start of `needle`.
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
            .unwrap_or_else(|| panic!("{needle:?} is not in {text:?}"));
        let index = text[..byte].chars().count() + offset;
        cells[columns[index]].1
    }

    #[test]
    fn subagent_status_reuses_main_status_bar_head_and_colors() {
        let icons = themes::current_icons();
        let app = app_icon(icons);
        let model_ic = model_icon(icons);
        let dir_ic = directory_icon(icons);
        let line = subagent_status("fixture/primary", ThinkLevel::Medium, "~/dev/pp", 120);
        assert_eq!(
            line_text(&line),
            format!(
                " {app} · {model_ic} fixture/primary · {} med · {dir_ic} ~/dev/pp",
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
            "model reference is bold like the main status bar"
        );
        assert_eq!(fg_at(line.clone(), 120, app), accent());
        assert_eq!(fg_at(line.clone(), 120, "fixture/primary"), model_color());
        assert_eq!(fg_at(line.clone(), 120, "med"), model_color());
        assert_eq!(fg_at(line, 120, "~/dev/pp"), directory_color());
        // Every thinking level shows the same icon and label as the main status bar.
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
                    " {app} · {model_ic} m · {} {text} · {dir_ic} ~",
                    palette::think_icon(level)
                ),
                "{level:?}"
            );
        }
    }

    #[test]
    fn subagent_status_degrades_cwd_then_model_then_prefix_within_width() {
        let icons = themes::current_icons();
        let app = app_icon(icons);
        let dir_ic = directory_icon(icons);
        let model = "fixture/primary";
        let workdir = "~/dev/projects/pp";
        let think = ThinkLevel::Medium;
        let full = subagent_status(model, think, workdir, 120);
        assert_eq!(full.width(), 52, "rendered width of the full status bar");
        assert!(line_text(&full).ends_with(&format!("{dir_ic} {workdir}")));

        // 1) Shrink only the cwd: model and header remain, the directory keeps its tail and starts with ….
        let narrowed = line_text(&subagent_status(model, think, workdir, 52));
        assert!(narrowed.contains(app), "{narrowed}");
        assert!(narrowed.contains(model), "{narrowed}");
        let cwd = narrowed
            .split_once(&format!("{dir_ic} "))
            .map(|(_, cwd)| cwd)
            .unwrap();
        assert!(cwd.starts_with('…'), "{narrowed}");
        assert!(
            workdir.ends_with(cwd.strip_prefix('…').unwrap()),
            "{narrowed}"
        );

        // 2) Then shrink the model: the model field ends with … and the directory is still present.
        let narrowed = subagent_status(model, think, workdir, 43);
        let text = line_text(&narrowed);
        let shortened = narrowed
            .spans
            .iter()
            .find(|span| span.content.starts_with("fixture"))
            .unwrap();
        assert!(
            shortened.content.ends_with('…'),
            "model should be shortened: {}",
            shortened.content
        );
        assert!(!text.contains(model), "{text}");
        assert!(text.contains(app), "{text}");
        assert!(text.contains(dir_ic), "{text}");

        // 3) Finally only the model prefix remains: header, effort and directory are all gone.
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
        let icons = themes::current_icons();
        let sub_ic = subagent_one_icon(icons);
        let spd_ic = speed_medium_icon(icons);
        let ctx_ic = context_icon(icons);
        let io_ic = io_icon(icons);
        let cache_ic = cache_icon(icons);
        let cost_ic = cost_icon(icons);
        let timer_ic = timer_icon(icons);
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
                " {sub_ic} subagent · {spd_ic} 61.5 tok/s · {ctx_ic} 81.6%/500K · {io_ic} 12.3K/45.7K · {cache_ic} 8K/0 · {cost_ic} 0.012 · {timer_ic} 1m25s "
            )
        );
        let badge = line
            .spans
            .iter()
            .find(|span| span.content == format!("{sub_ic} subagent"))
            .unwrap();
        assert!(badge.style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(fg_at(line.clone(), 240, "subagent"), subagents_color());
        assert_eq!(fg_at(line.clone(), 240, "61.5 tok/s"), speed_color());
        assert_eq!(fg_at(line.clone(), 240, "81.6%/500K"), context_color());
        assert_eq!(fg_at(line.clone(), 240, "12.3K"), input_color());
        assert_eq!(fg_at(line.clone(), 240, "45.7K"), output_color());
        assert_eq!(fg_at(line.clone(), 240, "8K"), cache_read_color());
        assert_eq!(
            fg_at_offset(line.clone(), 240, "8K/0", 3),
            cache_write_color()
        );
        assert_eq!(fg_at(line.clone(), 240, "0.012"), cost_color());
        assert_eq!(fg_at(line, 240, "1m25s"), timer_color());
    }

    #[test]
    fn subagent_metrics_appends_measured_suffix_and_uses_token_fallback() {
        let icons = themes::current_icons();
        let sub_ic = subagent_one_icon(icons);
        let spd_ic = speed_medium_icon(icons);
        let cost_ic = cost_icon(icons);
        let timer_ic = timer_icon(icons);
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
            Some(muted_color())
        );
        // No suffix is appended when the measured count equals the total.
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

        // No ContextUpdated: a single total-token field replaces ctx/io/cache.
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
                " {sub_ic} subagent · {spd_ic} 61.5 tok/s · 12.3K tok · {cost_ic} 0.500 · {timer_ic} 1s "
            )
        );
        assert_eq!(fg_at(fallback.clone(), 240, "12.3K"), input_color());
        assert_eq!(fg_at_offset(fallback, 240, "12.3K tok", 5), muted_color());
    }

    #[test]
    fn subagent_metrics_never_hides_badge_and_hides_groups_in_priority_order() {
        let icons = themes::current_icons();
        let sub_ic = subagent_one_icon(icons);
        let spd_ic = speed_medium_icon(icons);
        let ctx_ic = context_icon(icons);
        let io_ic = io_icon(icons);
        let cache_ic = cache_icon(icons);
        let cost_ic = cost_icon(icons);
        let timer_ic = timer_icon(icons);
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
        let removal_order = [cache_ic, io_ic, spd_ic, cost_ic, timer_ic, ctx_ic];
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
                assert!(text.is_empty(), "zero width returns only an empty row");
                continue;
            }
            let missing = removal_order
                .iter()
                .take_while(|icon| !text.contains(**icon))
                .count();
            assert!(
                missing >= previous,
                "a narrower width must not bring back a hidden field: {text}"
            );
            previous = missing;
            for icon in &removal_order[missing..] {
                assert!(
                    text.contains(icon),
                    "degradation priority must not be skipped: {text}"
                );
            }
            assert!(
                text.contains(sub_ic) || text.contains('…'),
                "the badge never disappears: {text}"
            );
        }
        // At the narrowest widths only the clipped badge remains, and it never overflows.
        for width in [3u16, 1, 0] {
            let line = render(width);
            let text = line_text(&line);
            assert!(line.width() <= usize::from(width));
            for icon in [io_ic, cache_ic, cost_ic, timer_ic, ctx_ic] {
                assert!(!text.contains(icon), "width {width}: {text}");
            }
            assert_eq!(text.is_empty(), width == 0, "width {width}: {text}");
        }
        assert_eq!(line_text(&render(3)), format!("{sub_ic} …"));
        assert_eq!(line_text(&render(1)), "…");
    }
}
