use anyhow::{Context, Result};
use pp_config::config::{IconMode, ModelChoice};
use pp_core::context::ContextUsage;
use pp_core::model::ThinkLevel;
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use std::{
    collections::VecDeque,
    path::Path,
    process::{Command, Stdio},
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::{Duration, Instant},
};
use unicode_segmentation::UnicodeSegmentation;

use super::{themes, transcript_style as palette};

mod metric;
mod speed;
mod subagent;
mod workspace;

pub(super) use metric::{Metrics, abbreviate_tokens, context_field, has_data, metric_fields};
pub(super) use speed::TokenSpeed;
pub(super) use subagent::{SubagentUsage, subagent_metrics, subagent_status};
pub(super) use workspace::{StatusContext, WorkspaceStatus, workdir_label};

pub(super) fn accent() -> Color {
    themes::palette().status_accent
}
// Shell mode: input-box border color when the draft starts with `!`, distinguishing it from ordinary input.
pub(super) fn shell_mode() -> Color {
    themes::palette().shell_mode
}
// Authoritative background for the main input box metric row: the main input box and subagent detail share one dark base.
pub(super) fn input_status_bg() -> Color {
    themes::palette().input_status_bg
}
// Plan mode tag: amber, matching the transcript highlight.
fn plan_mode() -> Color {
    themes::palette().plan_mode
}
// Minimum field space still needed beside the mode tag (icon plus at least one character).
const MIN_AFTER_MODE: usize = FIELD_ICON_WIDTH + 1;
fn model_color() -> Color {
    themes::palette().status_model
}
fn directory_color() -> Color {
    themes::palette().status_directory
}
fn git_color() -> Color {
    themes::palette().status_git
}
fn speed_color() -> Color {
    themes::palette().status_speed
}
fn subagents_color() -> Color {
    themes::palette().status_subagents
}
fn input_color() -> Color {
    themes::palette().status_input
}
fn output_color() -> Color {
    themes::palette().status_output
}
fn cache_read_color() -> Color {
    themes::palette().status_cache_read
}
fn cache_write_color() -> Color {
    themes::palette().status_cache_write
}
fn context_color() -> Color {
    themes::palette().status_context
}
fn cost_color() -> Color {
    themes::palette().status_cost
}
fn muted_color() -> Color {
    palette::muted()
}
fn timer_color() -> Color {
    context_color()
}

pub(super) fn app_icon(icons: IconMode) -> &'static str {
    match icons {
        IconMode::Nerd => "\u{f1d9}",
        IconMode::Ascii => ">",
    }
}
pub(super) fn model_icon(icons: IconMode) -> &'static str {
    match icons {
        IconMode::Nerd => "\u{ec19}",
        IconMode::Ascii => "@",
    }
}
fn plan_icon(icons: IconMode) -> &'static str {
    match icons {
        IconMode::Nerd => "\u{f0132}",
        IconMode::Ascii => "P",
    }
}
pub(super) fn directory_icon(icons: IconMode) -> &'static str {
    match icons {
        IconMode::Nerd => "\u{f07c}",
        IconMode::Ascii => "/",
    }
}
pub(super) fn git_icon(icons: IconMode) -> &'static str {
    match icons {
        IconMode::Nerd => "\u{f126}",
        IconMode::Ascii => "*",
    }
}
fn subagent_one_icon(icons: IconMode) -> &'static str {
    match icons {
        IconMode::Nerd => "\u{f007}",
        IconMode::Ascii => "&",
    }
}
fn subagent_two_icon(icons: IconMode) -> &'static str {
    match icons {
        IconMode::Nerd => "\u{f000e}",
        IconMode::Ascii => "&",
    }
}
fn subagent_group_icon(icons: IconMode) -> &'static str {
    match icons {
        IconMode::Nerd => "\u{f0c0}",
        IconMode::Ascii => "G",
    }
}
fn speed_slow_icon(icons: IconMode) -> &'static str {
    match icons {
        IconMode::Nerd => "\u{f0f86}",
        IconMode::Ascii => "~",
    }
}
fn speed_medium_icon(icons: IconMode) -> &'static str {
    match icons {
        IconMode::Nerd => "\u{f0f85}",
        IconMode::Ascii => "=",
    }
}
pub(super) fn speed_fast_icon(icons: IconMode) -> &'static str {
    match icons {
        IconMode::Nerd => "\u{f04c5}",
        IconMode::Ascii => "^",
    }
}
pub(super) fn io_icon(icons: IconMode) -> &'static str {
    match icons {
        IconMode::Nerd => "\u{f04e2}",
        IconMode::Ascii => "%",
    }
}
fn cache_icon(icons: IconMode) -> &'static str {
    match icons {
        IconMode::Nerd => "\u{f01bc}",
        IconMode::Ascii => "#",
    }
}
fn title_icon(icons: IconMode) -> &'static str {
    match icons {
        IconMode::Nerd => "\u{f09a9}",
        IconMode::Ascii => "*",
    }
}
pub(super) fn context_icon(icons: IconMode) -> &'static str {
    match icons {
        IconMode::Nerd => "\u{e62a}",
        IconMode::Ascii => "c",
    }
}
pub(super) fn cost_icon(icons: IconMode) -> &'static str {
    match icons {
        IconMode::Nerd => "\u{f01c1}",
        IconMode::Ascii => "$",
    }
}
fn timer_icon(icons: IconMode) -> &'static str {
    match icons {
        IconMode::Nerd => "\u{f051b}",
        IconMode::Ascii => "t",
    }
}
const ICON_WIDTH: usize = 2;
// Fixed overhead of separator and icon columns in the width budget.
const SEPARATOR_WIDTH: usize = 3;
const FIELD_ICON_WIDTH: usize = ICON_WIDTH + 1;
// Below this width for the directory field, hide git first and then shorten the model rather than squeezing the directory to nothing.
const MIN_DIRECTORY: usize = 8;
// Speed icon thresholds (tok/s): reaching the medium tier, reaching the high tier.
const SPEED_MEDIUM_RATE: f64 = 60.0;
const SPEED_FAST_RATE: f64 = 120.0;

fn compact(text: &str, width: usize, keep_end: bool) -> String {
    if Span::raw(text).width() <= width {
        return text.into();
    }
    if width == 0 {
        return String::new();
    }
    let budget = width - 1;
    let mut result = String::with_capacity(text.len().min(width));
    if text.is_ascii() {
        let mut used = 0usize;
        let mut push_byte = |b: u8| {
            let buf = [b];
            let s = std::str::from_utf8(&buf).unwrap_or("");
            let w = Span::raw(s).width();
            if used + w > budget {
                return false;
            }
            used += w;
            result.push(char::from(b));
            true
        };
        if keep_end {
            for &b in text.as_bytes().iter().rev() {
                if !push_byte(b) {
                    break;
                }
            }
        } else {
            for &b in text.as_bytes() {
                if !push_byte(b) {
                    break;
                }
            }
        }
    } else {
        let mut chars: Vec<_> = text.chars().collect();
        if keep_end {
            chars.reverse();
        }
        for ch in chars {
            let prev_len = result.len();
            result.push(ch);
            if Span::raw(result.as_str()).width() > budget {
                result.truncate(prev_len);
                break;
            }
        }
    }
    if keep_end {
        format!("…{}", result.chars().rev().collect::<String>())
    } else {
        format!("{result}…")
    }
}

// When it does not fit, degrade in order: shorten cwd → hide git → shorten model → show only the model prefix with permission/mode.
// The caller budgets the Plan mode tag: squeeze other fields first, then shorten it to one letter, and finally keep only the mode.
// The model field carries only the configured reference: the switch source is explained by the notice row in the content column and does not take status bar width.
pub(super) fn status_line(
    model: &str,
    think_level: ThinkLevel,
    permission: Option<&Span<'static>>,
    mode: Option<&'static str>,
    context: &StatusContext,
    width: usize,
) -> Line<'static> {
    let icons = themes::current_icons();
    let mode = mode.filter(|text| !text.is_empty());
    let mode_span = |text: &'static str| {
        Span::styled(
            text,
            Style::default()
                .fg(plan_mode())
                .add_modifier(Modifier::BOLD),
        )
    };
    let separator = || Span::styled(" · ", Style::default().fg(Color::DarkGray));
    let think = match think_level {
        ThinkLevel::None => "off",
        ThinkLevel::Low => "low",
        ThinkLevel::Medium => "med",
        ThinkLevel::High => "hig",
        ThinkLevel::Xhigh => "xhi",
        ThinkLevel::Max => "max",
    };
    // Thinking levels uniformly use Nerd Font icons (none through xhigh are Material Design rings, max is the
    // Font Awesome flame); ASCII mode uses ladder characters.
    let think_icon = palette::think_icon(think_level);
    let icon = |glyph, color| Span::styled(format!("{glyph} "), Style::default().fg(color));
    let think_width = Line::from(think).width();
    let effort_width = SEPARATOR_WIDTH + FIELD_ICON_WIDTH + think_width;
    let dir_fixed = SEPARATOR_WIDTH + FIELD_ICON_WIDTH;
    let field_slot = |text: &str| SEPARATOR_WIDTH + FIELD_ICON_WIDTH + Line::from(text).width();
    let workdir_full = Line::from(context.workdir.as_str()).width();
    let git = context.git.as_ref().map(|git| git.label());

    let model_display = model.to_owned();
    let mut model_text = model_display.clone();
    let mut git_text = git.clone();
    let mut workdir_text = context.workdir.clone();
    if width
        < field_slot(&model_display)
            + effort_width
            + dir_fixed
            + workdir_full
            + git.as_deref().map_or(0, field_slot)
    {
        // 1) Shorten the cwd (git is kept).
        let mut cwd_slot = width.saturating_sub(
            field_slot(&model_display)
                + effort_width
                + dir_fixed
                + git.as_deref().map_or(0, field_slot),
        );
        if cwd_slot < MIN_DIRECTORY && git_text.is_some() {
            // 2) Hide git and give the space to the directory.
            git_text = None;
            cwd_slot = width.saturating_sub(field_slot(&model_display) + effort_width + dir_fixed);
        }
        if cwd_slot < MIN_DIRECTORY {
            // 3) Shorten the model.
            model_text = compact(
                &model_display,
                (width / 4).max(12).saturating_sub(FIELD_ICON_WIDTH),
                false,
            );
            if width < field_slot(&model_text) + effort_width + dir_fixed {
                // 4) Still too wide: keep the model prefix, permission and mode.
                let mut spans = vec![Span::styled(
                    compact(&model_display, width, false),
                    Style::default()
                        .fg(model_color())
                        .add_modifier(Modifier::BOLD),
                )];
                if let Some(permission) = permission {
                    if width > 0 {
                        spans.push(separator());
                    }
                    spans.push(permission.clone());
                }
                if let Some(mode_text) = mode {
                    if width > 0 || permission.is_some() {
                        spans.push(separator());
                    }
                    spans.push(icon(plan_icon(icons), plan_mode()));
                    spans.push(mode_span(mode_text));
                }
                return Line::from(spans);
            }
            cwd_slot = width.saturating_sub(field_slot(&model_text) + effort_width + dir_fixed);
        }
        workdir_text = compact(&context.workdir, cwd_slot, true);
    }

    let model_text_style = Style::default()
        .fg(model_color())
        .add_modifier(Modifier::BOLD);
    let mut spans = vec![
        Span::raw(" "),
        Span::styled(
            app_icon(icons),
            Style::default().fg(accent()).add_modifier(Modifier::BOLD),
        ),
    ];
    spans.push(separator());
    spans.push(icon(model_icon(icons), model_color()));
    spans.push(Span::styled(model_text, model_text_style));
    spans.push(separator());
    // off means explicitly sending none, asking for thinking to be disabled.
    spans.push(icon(think_icon, model_color()));
    spans.push(Span::styled(think, model_text_style));
    if let Some(permission) = permission {
        spans.extend([separator(), permission.clone()]);
    }
    if let Some(mode_text) = mode {
        spans.push(separator());
        spans.push(icon(plan_icon(icons), plan_mode()));
        spans.push(mode_span(mode_text));
    }
    spans.push(separator());
    spans.push(icon(directory_icon(icons), directory_color()));
    spans.push(Span::styled(
        workdir_text,
        Style::default().fg(directory_color()),
    ));
    if let Some(git_text) = git_text {
        spans.push(separator());
        spans.push(icon(git_icon(icons), git_color()));
        spans.push(Span::styled(git_text, Style::default().fg(git_color())));
    }
    Line::from(spans)
}

fn permission_field(mode: pp_config::config::PermissionMode) -> Span<'static> {
    use pp_config::config::PermissionMode;
    let theme = themes::palette();
    let (label, color) = match mode {
        PermissionMode::WorkspaceWrite => ("Ask for approval", theme.status_input),
        PermissionMode::AutoReview => ("Approve for me", theme.plan_mode),
        PermissionMode::DangerFullAccess => ("Full Access", theme.error_border),
        PermissionMode::ReadOnly => ("Read Only", theme.plan_mode),
    };
    let icon = match themes::current_icons() {
        IconMode::Nerd => "\u{ed25}",
        IconMode::Ascii => "!",
    };
    Span::styled(
        format!("{icon} {label}"),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )
}

/// Main status bar: reserve columns for permission and Plan first, then defer to the shared field assembly and degradation logic.
pub(super) fn primary(
    model: Option<&ModelChoice>,
    think_level: ThinkLevel,
    mode: pp_core::plan::WorkMode,
    permissions: Option<pp_config::config::PermissionMode>,
    context: &StatusContext,
    width: u16,
) -> Line<'static> {
    use pp_core::plan::WorkMode as Mode;
    let width = usize::from(width);
    let permission = permissions.map(permission_field);
    let remaining = width.saturating_sub(
        permission
            .as_ref()
            .map_or(0, |span| SEPARATOR_WIDTH + span.width()),
    );
    let mode_slot = |text: &str| SEPARATOR_WIDTH + FIELD_ICON_WIDTH + Line::from(text).width();
    let budget_total = remaining.saturating_sub(1 + ICON_WIDTH);
    let (mode_text, budget) = if mode == Mode::Execute {
        ("", budget_total)
    } else if budget_total >= mode_slot("Plan") + MIN_AFTER_MODE {
        ("Plan", budget_total - mode_slot("Plan"))
    } else if budget_total >= mode_slot("P") + MIN_AFTER_MODE {
        ("P", budget_total - mode_slot("P"))
    } else if budget_total >= mode_slot("P") {
        ("P", 0)
    } else {
        ("", budget_total)
    };
    let model = model.map_or("—", |choice| choice.reference.as_str());
    let fields = status_line(
        model,
        think_level,
        permission.as_ref(),
        (!mode_text.is_empty()).then_some(mode_text),
        context,
        budget,
    );
    if let Some(permission) = permission
        && (remaining == 0
            || fields.width() > width
            || (mode == Mode::Plan && mode_text.is_empty()))
    {
        let mut spans = vec![Span::styled(
            compact(&permission.content, width, false),
            permission.style,
        )];
        if mode == Mode::Plan && remaining > 0 {
            let label = if remaining >= "Plan".len() {
                "Plan"
            } else {
                "P"
            };
            spans.extend([
                Span::styled(" · ", Style::default().fg(Color::DarkGray)),
                Span::styled(label, Style::default().fg(plan_mode())),
            ]);
        }
        return Line::from(spans);
    }
    fields
}

#[cfg(test)]
mod fixtures {
    use super::*;
    use pp_core::context::TokenUsage;

    pub(super) fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    pub(super) fn status_usage() -> ContextUsage {
        ContextUsage {
            estimated_tokens: 408_000,
            context_window: Some(500_000),
            auto_compact: true,
            threshold_percent: 80,
            last_request: Some(TokenUsage {
                input_tokens: 12_300,
                output_tokens: 2_100,
                cached_tokens: Some(8_000),
                cache_write_tokens: Some(0),
                reasoning_tokens: None,
            }),
            session_output_tokens: 45_678,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    #[test]
    fn primary_reserves_width_for_ascii_permission_field() {
        use pp_config::config::{PermissionMode, Theme};
        use pp_core::plan::WorkMode;

        let (theme, icons) = themes::current();
        themes::set_active(Theme::Default, IconMode::Ascii);
        for width in [0, 1, 8, 17, 20, 24, 32, 48, 100] {
            let line = primary(
                None,
                ThinkLevel::High,
                WorkMode::Plan,
                Some(PermissionMode::AutoReview),
                &StatusContext::default(),
                width,
            );
            let text = line_text(&line);
            assert!(line.width() <= usize::from(width), "{width}: {text}");
            if width >= 17 {
                assert!(text.contains("! Approve for me"), "{width}: {text}");
            }
            if width == 20 {
                assert!(text.ends_with(" · P"), "{text}");
            }
            if width == 100 {
                assert!(text.contains("hig · ! Approve for me ·"), "{text}");
            }
        }
        themes::set_active(theme, icons);
    }

    #[test]
    fn primary_model_label_uses_configured_reference() {
        for reference in ["ark/glm-5.3", "gateway/team/secondary"] {
            let choice = ModelChoice {
                reference: reference.into(),
                model: "glm-5.3-flash".into(),
                protocol: pp_config::models::Protocol::Completion,
                thinking: pp_config::models::ModelThinking::default(),
                context_window: None,
            };
            let text = line_text(&primary(
                Some(&choice),
                ThinkLevel::None,
                pp_core::plan::WorkMode::Execute,
                None,
                &StatusContext::default(),
                100,
            ));
            assert!(
                text.contains(reference),
                "the status bar should use the same reference as the model list"
            );
            assert!(
                !text.contains(&choice.model),
                "the actually requested model name must not replace the configured reference"
            );
        }
    }

    #[test]
    fn primary_model_field_has_no_switch_source_suffix() {
        let choice = ModelChoice {
            reference: "ark/glm-5.3".into(),
            model: "glm-5.3-flash".into(),
            protocol: pp_config::models::Protocol::Completion,
            thinking: pp_config::models::ModelThinking::default(),
            context_window: None,
        };
        let text = line_text(&primary(
            Some(&choice),
            ThinkLevel::None,
            pp_core::plan::WorkMode::Execute,
            None,
            &StatusContext::default(),
            100,
        ));
        for word in ["override", "fallback", "role", "default"] {
            assert!(
                !text.contains(word),
                "{word} must not appear in the status bar: {text}"
            );
        }
        // Narrow screens still obey the same width budget.
        for width in [20, 32, 48] {
            let line = primary(
                Some(&choice),
                ThinkLevel::High,
                pp_core::plan::WorkMode::Plan,
                None,
                &StatusContext::default(),
                width,
            );
            assert!(line.width() <= usize::from(width), "{width}: {line:?}");
        }
    }
}
