use super::*;
use crate::tui::{statusline, transcript_style};
use pp_core::model::ThinkLevel;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

pub(super) fn draw(
    frame: &mut Frame,
    area: Rect,
    p: &Palette,
    selected_theme: Theme,
    selected_icons: IconMode,
    ascii: bool,
) {
    if area.width < 12 || area.height < 4 {
        return;
    }
    draw_header(
        frame,
        Rect::new(area.x, area.y, area.width, 1),
        p,
        selected_theme,
        selected_icons,
        ascii,
    );

    let margin = u16::from(area.width >= 24);
    let viewport = Rect::new(
        area.x + margin,
        area.y + 1,
        area.width.saturating_sub(margin * 2),
        area.height.saturating_sub(1),
    );
    if viewport.width < 8 || viewport.height < 2 {
        return;
    }
    frame
        .buffer_mut()
        .set_style(viewport, Style::default().bg(p.md_bg).fg(p.md_text));

    let total_h = viewport.height;
    let bottom_h = if total_h >= 12 {
        2
    } else if total_h >= 3 {
        1
    } else {
        0
    };
    let body_h = total_h.saturating_sub(bottom_h);
    if bottom_h == 2 {
        draw_input_metrics_bar(
            frame,
            Rect::new(viewport.x, viewport.bottom() - 2, viewport.width, 1),
            p,
            selected_icons,
        );
        draw_status_bar(
            frame,
            Rect::new(viewport.x, viewport.bottom() - 1, viewport.width, 1),
            p,
            selected_icons,
        );
    } else if bottom_h == 1 {
        draw_status_bar(
            frame,
            Rect::new(viewport.x, viewport.bottom() - 1, viewport.width, 1),
            p,
            selected_icons,
        );
    }

    let body = Rect::new(viewport.x, viewport.y, viewport.width, body_h);
    draw_body(frame, body, p, selected_theme, selected_icons);
}

fn clip_spans(spans: Vec<Span<'static>>, max_width: usize) -> Vec<Span<'static>> {
    let mut out = Vec::with_capacity(spans.len());
    let mut used = 0usize;
    for span in spans {
        let span_width = span.width();
        if used + span_width <= max_width {
            used += span_width;
            out.push(span);
            continue;
        }
        let mut clipped = String::new();
        for grapheme in span.styled_graphemes(Style::default()) {
            let gw = Span::raw(grapheme.symbol).width();
            if used + gw > max_width {
                break;
            }
            clipped.push_str(grapheme.symbol);
            used += gw;
        }
        if !clipped.is_empty() {
            out.push(Span::styled(clipped, span.style));
        }
        break;
    }
    out
}

fn paint_row(frame: &mut Frame, rect: Rect, bg: Color, spans: Vec<Span<'static>>) {
    if rect.is_empty() {
        return;
    }
    frame.buffer_mut().set_style(rect, Style::default().bg(bg));
    if !spans.is_empty() {
        let clipped = clip_spans(spans, usize::from(rect.width));
        frame.render_widget(Line::from(clipped), rect);
    }
}

fn push_row(frame: &mut Frame, area: Rect, y: &mut u16, bg: Color, spans: Vec<Span<'static>>) {
    if *y < area.bottom() {
        paint_row(frame, Rect::new(area.x, *y, area.width, 1), bg, spans);
        *y += 1;
    }
}

fn draw_header(
    frame: &mut Frame,
    area: Rect,
    p: &Palette,
    selected_theme: Theme,
    selected_icons: IconMode,
    ascii: bool,
) {
    let width = usize::from(area.width);
    if width < 4 {
        return;
    }
    let title_icon = if ascii { "*" } else { "\u{f09a9}" };
    let mut left = vec![
        Span::styled("─ ", Style::default().fg(p.panel_border)),
        Span::styled(
            format!("{title_icon} Live Preview"),
            Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" · ", Style::default().fg(p.muted)),
        Span::styled(
            selected_theme.label(),
            Style::default().fg(p.text).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" ({}) ", selected_icons.label()),
            Style::default().fg(p.muted),
        ),
    ];
    let left_w: usize = left.iter().map(Span::width).sum();

    let mut right = vec![Span::raw(" ")];
    for (idx, color) in p.swatch().into_iter().enumerate() {
        if idx > 0 {
            right.push(Span::raw(" "));
        }
        right.push(Span::styled("●", Style::default().fg(color)));
    }
    right.push(Span::styled(" ─", Style::default().fg(p.panel_border)));
    let right_w: usize = right.iter().map(Span::width).sum();

    let mut spans = if left_w + right_w + 2 <= width {
        let fill = width - left_w - right_w;
        left.push(Span::styled(
            "─".repeat(fill),
            Style::default().fg(p.panel_border),
        ));
        left.extend(right);
        left
    } else if left_w <= width {
        left.push(Span::styled(
            "─".repeat(width - left_w),
            Style::default().fg(p.panel_border),
        ));
        left
    } else {
        vec![
            Span::styled("─ ", Style::default().fg(p.panel_border)),
            Span::styled(
                format!("{title_icon} Live Preview "),
                Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
            ),
        ]
    };
    let used: usize = spans.iter().map(Span::width).sum();
    if used < width {
        spans.push(Span::styled(
            "─".repeat(width - used),
            Style::default().fg(p.panel_border),
        ));
    }
    paint_row(frame, area, p.panel_bg, spans);
}

fn draw_body(
    frame: &mut Frame,
    area: Rect,
    p: &Palette,
    selected_theme: Theme,
    selected_icons: IconMode,
) {
    if area.is_empty() {
        return;
    }
    let h = area.height;
    let w = usize::from(area.width);
    let mut y = area.y;
    let end = area.bottom();

    // 1. 用户消息条（全宽 user_bg，无角色前缀标签，对齐真实会话样式）
    push_row(
        frame,
        area,
        &mut y,
        p.user_bg,
        vec![Span::styled(
            format!(
                "  Apply {} & inspect src/tui/themes.rs",
                selected_theme.as_str()
            ),
            Style::default().fg(p.text),
        )],
    );
    if h >= 12 {
        push_row(frame, area, &mut y, p.md_bg, Vec::new());
    }

    // 2. 助手 Markdown 与语法高亮代码块
    if h >= 14 {
        push_row(
            frame,
            area,
            &mut y,
            p.md_bg,
            vec![Span::styled(
                "  # Assistant",
                Style::default()
                    .fg(p.md_heading)
                    .add_modifier(Modifier::BOLD),
            )],
        );
    }
    push_row(
        frame,
        area,
        &mut y,
        p.md_bg,
        vec![
            Span::styled("  • ", Style::default().fg(p.md_bullet)),
            Span::styled(
                "Bold",
                Style::default()
                    .fg(p.md_strong)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(", inline ", Style::default().fg(p.md_text)),
            Span::styled("`tui.theme`", Style::default().fg(p.md_code)),
            Span::styled(" · ", Style::default().fg(p.dim)),
            Span::styled(
                "https://pp.dev",
                Style::default()
                    .fg(p.link)
                    .add_modifier(Modifier::UNDERLINED),
            ),
        ],
    );

    if y < end {
        let fence_w = w.saturating_sub(4);
        if h >= 9 && fence_w >= 10 {
            push_row(
                frame,
                area,
                &mut y,
                p.md_bg,
                vec![
                    Span::styled("  ── ", Style::default().fg(p.dim)),
                    Span::styled("rust ", Style::default().fg(p.muted)),
                    Span::styled("─".repeat(fence_w - 8), Style::default().fg(p.dim)),
                ],
            );
        }
        if h >= 16 {
            push_row(
                frame,
                area,
                &mut y,
                p.md_bg,
                vec![
                    Span::styled("    fn ", Style::default().fg(p.syntax_keyword)),
                    Span::styled("apply_theme", Style::default().fg(p.syntax_function)),
                    Span::styled("(mode: ", Style::default().fg(p.md_text)),
                    Span::styled("IconMode", Style::default().fg(p.edit_type)),
                    Span::styled(") -> ", Style::default().fg(p.md_text)),
                    Span::styled("&'static ", Style::default().fg(p.syntax_keyword)),
                    Span::styled("str", Style::default().fg(p.edit_type)),
                    Span::styled(" {", Style::default().fg(p.md_text)),
                ],
            );
            push_row(
                frame,
                area,
                &mut y,
                p.md_bg,
                vec![
                    Span::styled(
                        format!("        \"{}\"", selected_theme.as_str()),
                        Style::default().fg(p.edit_string),
                    ),
                    Span::styled(" // preview · ", Style::default().fg(p.syntax_comment)),
                    Span::styled("60", Style::default().fg(p.edit_number)),
                    Span::styled(" fps", Style::default().fg(p.syntax_comment)),
                ],
            );
            if h >= 18 {
                push_row(
                    frame,
                    area,
                    &mut y,
                    p.md_bg,
                    vec![Span::styled("    }", Style::default().fg(p.md_text))],
                );
            }
        } else {
            push_row(
                frame,
                area,
                &mut y,
                p.md_bg,
                vec![
                    Span::styled("    fn ", Style::default().fg(p.syntax_keyword)),
                    Span::styled("apply_theme", Style::default().fg(p.syntax_function)),
                    Span::styled("() -> ", Style::default().fg(p.md_text)),
                    Span::styled(
                        format!("\"{}\"", selected_theme.as_str()),
                        Style::default().fg(p.edit_string),
                    ),
                    Span::styled(" // ", Style::default().fg(p.syntax_comment)),
                    Span::styled("60", Style::default().fg(p.edit_number)),
                    Span::styled(" fps", Style::default().fg(p.syntax_comment)),
                ],
            );
        }
        if h >= 11 && fence_w >= 4 {
            push_row(
                frame,
                area,
                &mut y,
                p.md_bg,
                vec![Span::styled(
                    format!("  {}", "─".repeat(fence_w)),
                    Style::default().fg(p.dim),
                )],
            );
        }
    }

    if h >= 13 {
        push_row(frame, area, &mut y, p.md_bg, Vec::new());
    }

    // 3. Read 摘要与 Edit 框卡
    let check_icon = transcript_style::done_icon(selected_icons);
    if y < end {
        push_row(
            frame,
            area,
            &mut y,
            p.md_bg,
            vec![
                Span::styled(format!("  {check_icon} "), Style::default().fg(p.added)),
                Span::styled("Read", Style::default().fg(p.read_title)),
                Span::styled(" · ", Style::default().fg(p.read_summary_meta)),
                Span::styled(
                    "crates/pp-config/src/config/theme.rs",
                    Style::default().fg(p.accent),
                ),
                Span::styled(":1-42", Style::default().fg(p.activity)),
            ],
        );
    }

    let remaining = end.saturating_sub(y);
    if remaining >= 4 {
        let card_rows = 4u16;
        let inset = u16::from(area.width >= 20) * 2;
        let card_rect = Rect::new(
            area.x + inset,
            y,
            area.width.saturating_sub(inset * 2),
            card_rows,
        );
        draw_edit_card(frame, card_rect, p, selected_theme, selected_icons);
        y += card_rows;
    } else if remaining >= 3 {
        let inset = u16::from(area.width >= 20) * 2;
        let card_rect = Rect::new(
            area.x + inset,
            y,
            area.width.saturating_sub(inset * 2),
            remaining,
        );
        draw_edit_card(frame, card_rect, p, selected_theme, selected_icons);
        return;
    }

    // 4. 宽裕高度下追加工具与思考档位图标预览（方便 Icon Style 直观对比）
    let remaining = end.saturating_sub(y);
    if remaining >= 3 {
        push_row(frame, area, &mut y, p.md_bg, Vec::new());
        let mut tool_spans = vec![Span::styled("  Tools   ", Style::default().fg(p.muted))];
        for (idx, (tool, color)) in [
            ("read", p.read_title),
            ("rg", p.search_title),
            ("glob", p.glob_title),
            ("write", p.write_title),
            ("bash", p.shell_title),
            ("mcp__srv__run", p.mcp_title),
        ]
        .into_iter()
        .enumerate()
        {
            if idx > 0 {
                tool_spans.push(Span::raw("  "));
            }
            let label = if tool.starts_with("mcp__") {
                "mcp"
            } else {
                tool
            };
            tool_spans.push(Span::styled(
                format!("{} ", transcript_style::tool_icon_for(tool, selected_icons)),
                Style::default().fg(color),
            ));
            tool_spans.push(Span::styled(label, Style::default().fg(p.activity)));
        }
        push_row(frame, area, &mut y, p.md_bg, tool_spans);

        let mut effort_spans = vec![Span::styled("  Effort  ", Style::default().fg(p.muted))];
        for (idx, level) in ThinkLevel::ALL.into_iter().enumerate() {
            if idx > 0 {
                effort_spans.push(Span::raw("  "));
            }
            let active = level == ThinkLevel::High;
            let icon_style = if active {
                Style::default().fg(p.accent).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(p.status_model)
            };
            let text_style = if active {
                Style::default().fg(p.text).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(p.muted)
            };
            effort_spans.push(Span::styled(
                format!(
                    "{} ",
                    transcript_style::think_icon_for(level, selected_icons)
                ),
                icon_style,
            ));
            effort_spans.push(Span::styled(level.as_str(), text_style));
        }
        push_row(frame, area, &mut y, p.md_bg, effort_spans);
    }
}

fn draw_edit_card(
    frame: &mut Frame,
    area: Rect,
    p: &Palette,
    selected_theme: Theme,
    selected_icons: IconMode,
) {
    if area.height < 3 || area.width < 10 {
        return;
    }
    let w = usize::from(area.width);
    let bg = p.tool_bg;
    let border = Style::default().fg(p.tool_border).bg(bg);
    let edit_icon = transcript_style::tool_icon_for("edit", selected_icons);
    let file_icon = transcript_style::file_icon("src/tui/themes.rs", selected_icons);

    let mut top = vec![
        Span::styled("╭─ ", border),
        Span::styled(
            format!("{edit_icon} Edit: "),
            Style::default()
                .fg(p.edit_title)
                .bg(bg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(file_icon, Style::default().fg(p.edit_meta).bg(bg)),
        Span::styled(
            "src/tui/themes.rs",
            Style::default().fg(p.edit_title).bg(bg),
        ),
        Span::styled(" ⟨", Style::default().fg(p.edit_meta).bg(bg)),
        Span::styled("+1", Style::default().fg(p.edit_added).bg(bg)),
        Span::styled("/", Style::default().fg(p.edit_meta).bg(bg)),
        Span::styled("-1", Style::default().fg(p.edit_removed).bg(bg)),
        Span::styled("⟩ ", Style::default().fg(p.edit_meta).bg(bg)),
    ];
    top = clip_spans(top, w.saturating_sub(1));
    let top_used: usize = top.iter().map(Span::width).sum();
    if w > top_used + 1 {
        top.push(Span::styled("─".repeat(w - top_used - 1), border));
    }
    top.push(Span::styled("╮", border));
    paint_row(frame, Rect::new(area.x, area.y, area.width, 1), bg, top);

    let code_lines: [(&str, String, Color); 2] = [
        (
            "-",
            "const THEME: &str = \"old\";".to_owned(),
            p.edit_removed,
        ),
        (
            "+",
            format!("let theme = \"{}\";", selected_theme.as_str()),
            p.edit_added,
        ),
    ];
    let body_rows = usize::from(area.height.saturating_sub(2)).min(code_lines.len());
    for (idx, (sign, code, color)) in code_lines.into_iter().take(body_rows).enumerate() {
        let row_rect = Rect::new(area.x, area.y + 1 + idx as u16, area.width, 1);
        let spans = vec![
            Span::styled("│ ", border),
            Span::styled(" 12 ", Style::default().fg(p.edit_meta).bg(bg)),
            Span::styled(format!("{sign} "), Style::default().fg(color).bg(bg)),
            Span::styled(code, Style::default().fg(color).bg(bg)),
        ];
        paint_row(
            frame,
            Rect::new(row_rect.x, row_rect.y, row_rect.width.saturating_sub(1), 1),
            bg,
            spans,
        );
        frame.render_widget(
            Span::styled("│", border),
            Rect::new(row_rect.right() - 1, row_rect.y, 1, 1),
        );
    }

    let bottom_y = area.y + 1 + body_rows as u16;
    if bottom_y < area.bottom() {
        let bottom = vec![
            Span::styled("╰", border),
            Span::styled("─".repeat(w.saturating_sub(2)), border),
            Span::styled("╯", border),
        ];
        paint_row(
            frame,
            Rect::new(area.x, bottom_y, area.width, 1),
            bg,
            bottom,
        );
    }
}

fn draw_input_metrics_bar(frame: &mut Frame, area: Rect, p: &Palette, selected_icons: IconMode) {
    let w = usize::from(area.width);
    if w < 8 {
        return;
    }
    let bg = p.input_status_bg;
    let sep = || Span::styled(" · ", Style::default().fg(p.dim).bg(bg));
    let speed = vec![
        Span::styled(
            format!(" {} ", statusline::speed_fast_icon(selected_icons)),
            Style::default().fg(p.status_speed).bg(bg),
        ),
        Span::styled("84.2 tok/s", Style::default().fg(p.status_speed).bg(bg)),
    ];
    let ctx = vec![
        Span::styled(
            format!("{} ", statusline::context_icon(selected_icons)),
            Style::default().fg(p.status_context).bg(bg),
        ),
        Span::styled("32%/128K", Style::default().fg(p.status_context).bg(bg)),
    ];
    let io = vec![
        Span::styled(
            format!("{} ", statusline::io_icon(selected_icons)),
            Style::default().fg(p.status_input).bg(bg),
        ),
        Span::styled("12K", Style::default().fg(p.status_input).bg(bg)),
        Span::styled("/", Style::default().fg(p.dim).bg(bg)),
        Span::styled("4K", Style::default().fg(p.status_output).bg(bg)),
    ];
    let cost = vec![
        Span::styled(
            format!("{} ", statusline::cost_icon(selected_icons)),
            Style::default().fg(p.status_cost).bg(bg),
        ),
        Span::styled("0.14 ", Style::default().fg(p.status_cost).bg(bg)),
    ];

    // 至少保留 4 个边框字符；宽度不足时逐组降级。
    let candidates: [Vec<Span<'static>>; 3] = [
        [
            speed.clone(),
            vec![sep()],
            ctx.clone(),
            vec![sep()],
            io,
            vec![sep()],
            cost.clone(),
        ]
        .concat(),
        [speed, vec![sep()], ctx.clone(), vec![sep()], cost].concat(),
        [vec![Span::raw(" ")], ctx, vec![Span::raw(" ")]].concat(),
    ];
    let border = Style::default().fg(p.status_accent);
    for metrics in candidates {
        let mw: usize = metrics.iter().map(Span::width).sum();
        if mw + 5 <= w {
            let mut spans = vec![Span::styled("─".repeat(w - mw - 1), border)];
            spans.extend(metrics);
            spans.push(Span::styled("─", border));
            paint_row(frame, area, p.md_bg, spans);
            return;
        }
    }
    paint_row(
        frame,
        area,
        p.md_bg,
        vec![Span::styled("─".repeat(w), border)],
    );
}

fn draw_status_bar(frame: &mut Frame, area: Rect, p: &Palette, selected_icons: IconMode) {
    let w = usize::from(area.width);
    if w == 0 {
        return;
    }
    let bg = p.input_status_bg;
    let sep = || Span::styled(" · ", Style::default().fg(p.dim).bg(bg));
    let app_icon = statusline::app_icon(selected_icons);
    let model_icon = statusline::model_icon(selected_icons);
    let dir_icon = statusline::directory_icon(selected_icons);
    let git_icon = statusline::git_icon(selected_icons);
    let think_high = transcript_style::think_icon_for(ThinkLevel::High, selected_icons);

    let head = vec![
        Span::styled(
            format!(" {app_icon}"),
            Style::default()
                .fg(p.status_accent)
                .bg(bg)
                .add_modifier(Modifier::BOLD),
        ),
        sep(),
        Span::styled(
            format!("{model_icon} gpt-5"),
            Style::default()
                .fg(p.status_model)
                .bg(bg)
                .add_modifier(Modifier::BOLD),
        ),
        sep(),
        Span::styled(
            format!("{think_high} high"),
            Style::default()
                .fg(p.status_model)
                .bg(bg)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    let dir = vec![
        sep(),
        Span::styled(
            format!("{dir_icon} ~/pp"),
            Style::default().fg(p.status_directory).bg(bg),
        ),
    ];
    let git = vec![
        sep(),
        Span::styled(
            format!("{git_icon} main* "),
            Style::default().fg(p.status_git).bg(bg),
        ),
    ];

    let full_w = head
        .iter()
        .chain(&dir)
        .chain(&git)
        .map(Span::width)
        .sum::<usize>();
    let dir_w = head.iter().chain(&dir).map(Span::width).sum::<usize>();
    let spans = if full_w <= w {
        head.into_iter().chain(dir).chain(git).collect()
    } else if dir_w <= w {
        head.into_iter().chain(dir).collect()
    } else {
        head
    };
    paint_row(frame, area, bg, spans);
}
