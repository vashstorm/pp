use anyhow::{Context, Result};
use pp_config::config::ModelChoice;
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

use super::transcript_style as palette;

mod metric;
mod speed;
mod subagent;
mod workspace;

pub(super) use metric::{abbreviate_tokens, context_field, metric_fields};
pub(super) use speed::TokenSpeed;
pub(super) use subagent::{SubagentUsage, subagent_metrics, subagent_status};
pub(super) use workspace::{StatusContext, WorkspaceStatus, workdir_label};

pub(super) const ACCENT: Color = Color::Rgb(127, 166, 106);
// Shell 模式：草稿以 `!` 开头时输入框上下边框的颜色，区分于普通输入。
pub(super) const SHELL_MODE: Color = Color::Rgb(255, 121, 198);
// 主输入框指标行背景的权威取值：主输入框与子 Agent 详情共用同一深色底。
pub(super) const INPUT_STATUS_BG: Color = Color::Rgb(26, 27, 38);
// Plan 模式标签：与 transcript 高亮一致的琥珀色。
const PLAN_MODE: Color = Color::Rgb(254, 188, 56);
// 模式标签之外仍需可容纳的最小字段空间（图标+至少一个字符）。
const MIN_AFTER_MODE: usize = FIELD_ICON_WIDTH + 1;
const MODEL: Color = Color::Rgb(223, 167, 209);
const DIRECTORY: Color = Color::Rgb(36, 174, 181);
const GIT: Color = Color::Rgb(204, 167, 72);
const SPEED: Color = Color::Rgb(255, 95, 162);
const SUBAGENTS: Color = Color::Rgb(254, 188, 56);
const INPUT: Color = Color::Rgb(125, 207, 255);
const OUTPUT: Color = Color::Rgb(158, 206, 106);
const CACHE_READ: Color = Color::Rgb(187, 154, 247);
const CACHE_WRITE: Color = Color::Rgb(255, 158, 100);
// 与 omp 的 Nerd Font 图标一致；图标后保留一列空白，容纳字形右侧伸展。
// Paper Plane 的首字母与应用名 pp 对应。
const APP_ICON: &str = "\u{f1d8}";
const MODEL_ICON: &str = "\u{ec19}";
const PLAN_ICON: &str = "\u{f0132}";
const DIRECTORY_ICON: &str = "\u{f07c}";
const GIT_ICON: &str = "\u{f126}";
const SUBAGENT_ONE_ICON: &str = "\u{f007}";
const SUBAGENT_TWO_ICON: &str = "\u{f000e}";
const SUBAGENT_GROUP_ICON: &str = "\u{f0c0}";
// 速度图标按 tok/s 分三档；没有速率数据时使用低速档。
const SPEED_SLOW_ICON: &str = "\u{f0f86}";
const SPEED_MEDIUM_ICON: &str = "\u{f0f85}";
const SPEED_FAST_ICON: &str = "\u{f04c5}";
const IO_ICON: &str = "\u{f04e2}";
const CACHE_ICON: &str = "\u{f01bc}";
const TITLE_ICON: &str = "\u{f09a9}";
const CONTEXT_ICON: &str = "\u{e62a}";
#[cfg(test)]
const COST_ICON: &str = "\u{f01c1}";
const TIMER_ICON: &str = "\u{f051b}";
const ICON_WIDTH: usize = 2;
// 分隔符与图标列在宽度预算中的固定开销。
const SEPARATOR_WIDTH: usize = 3;
const FIELD_ICON_WIDTH: usize = ICON_WIDTH + 1;
// 目录字段低于该宽度时先隐藏 git、再缩短模型，而不是把目录挤成空。
const MIN_DIRECTORY: usize = 8;
// 速度图标分档阈值（tok/s）：达到中速档、达到高速档。
const SPEED_MEDIUM_RATE: f64 = 60.0;
const SPEED_FAST_RATE: f64 = 120.0;

fn compact(text: &str, width: usize, keep_end: bool) -> String {
    if Line::from(text).width() <= width {
        return text.into();
    }
    if width == 0 {
        return String::new();
    }
    let mut chars: Vec<_> = text.chars().collect();
    if keep_end {
        chars.reverse();
    }
    let mut result = String::new();
    for ch in chars {
        if Line::from(format!("{result}{ch}")).width() > width - 1 {
            break;
        }
        result.push(ch);
    }
    if keep_end {
        format!("…{}", result.chars().rev().collect::<String>())
    } else {
        format!("{result}…")
    }
}

// 放不下时依次降级：缩短 cwd → 隐藏 git → 缩短模型 → 仅显示模型前缀。
// Plan 模式标签由调用方按预算决定：不足时先压缩其它字段，再缩为单字母，最后只留模式。
// 模型字段只放配置引用：切换来源由内容栏的提示行交代，不挤占状态栏宽度。
pub(super) fn status_line(
    model: &str,
    think_level: ThinkLevel,
    mode: Option<&'static str>,
    context: &StatusContext,
    width: usize,
) -> Line<'static> {
    let mode = mode.filter(|text| !text.is_empty());
    let mode_span = |text: &'static str| {
        Span::styled(
            text,
            Style::default().fg(PLAN_MODE).add_modifier(Modifier::BOLD),
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
    // 思考级别统一使用 Nerd Font 图标（none 到 xhigh 为 Material Design 圆环，max 为
    // Font Awesome 火苗），避免混用普通圆形字形导致宽度不一。
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
        // 1) 缩短 cwd（git 保留）。
        let mut cwd_slot = width.saturating_sub(
            field_slot(&model_display)
                + effort_width
                + dir_fixed
                + git.as_deref().map_or(0, field_slot),
        );
        if cwd_slot < MIN_DIRECTORY && git_text.is_some() {
            // 2) 隐藏 git，把空间让给目录。
            git_text = None;
            cwd_slot = width.saturating_sub(field_slot(&model_display) + effort_width + dir_fixed);
        }
        if cwd_slot < MIN_DIRECTORY {
            // 3) 缩短模型。
            model_text = compact(
                &model_display,
                (width / 4).max(12).saturating_sub(FIELD_ICON_WIDTH),
                false,
            );
            if width < field_slot(&model_text) + effort_width + dir_fixed {
                // 4) 仍不足：仅显示模型前缀与模式；模式图标和标签优先保留。
                let mut spans = vec![Span::styled(
                    compact(&model_display, width, false),
                    Style::default().fg(MODEL).add_modifier(Modifier::BOLD),
                )];
                if let Some(mode_text) = mode {
                    if width > 0 {
                        spans.push(separator());
                    }
                    spans.push(icon(PLAN_ICON, PLAN_MODE));
                    spans.push(mode_span(mode_text));
                }
                return Line::from(spans);
            }
            cwd_slot = width.saturating_sub(field_slot(&model_text) + effort_width + dir_fixed);
        }
        workdir_text = compact(&context.workdir, cwd_slot, true);
    }

    let model_text_style = Style::default().fg(MODEL).add_modifier(Modifier::BOLD);
    let mut spans = vec![
        Span::raw(" "),
        Span::styled(
            APP_ICON,
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
    ];
    spans.push(separator());
    spans.push(icon(MODEL_ICON, MODEL));
    spans.push(Span::styled(model_text, model_text_style));
    spans.push(separator());
    // off 表示显式发送 none，请求关闭思考。
    spans.push(icon(think_icon, MODEL));
    spans.push(Span::styled(think, model_text_style));
    if let Some(mode_text) = mode {
        spans.push(separator());
        spans.push(icon(PLAN_ICON, PLAN_MODE));
        spans.push(mode_span(mode_text));
    }
    spans.push(separator());
    spans.push(icon(DIRECTORY_ICON, DIRECTORY));
    spans.push(Span::styled(workdir_text, Style::default().fg(DIRECTORY)));
    if let Some(git_text) = git_text {
        spans.push(separator());
        spans.push(icon(GIT_ICON, GIT));
        spans.push(Span::styled(git_text, Style::default().fg(GIT)));
    }
    Line::from(spans)
}

/// 主状态栏：先按模式标签预留预算，再交给共享的字段装配与降级逻辑。
pub(super) fn primary(
    model: Option<&ModelChoice>,
    think_level: ThinkLevel,
    mode: pp_core::plan::WorkMode,
    context: &StatusContext,
    width: u16,
) -> Line<'static> {
    use pp_core::plan::WorkMode as Mode;
    let width = usize::from(width);
    let mode_slot = |text: &str| SEPARATOR_WIDTH + FIELD_ICON_WIDTH + Line::from(text).width();
    // 头部为空白加纸飞机图标；为 effort 后的模式图标与标签预留空间。
    let budget_total = width.saturating_sub(1 + ICON_WIDTH);
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
    status_line(
        model,
        think_level,
        (!mode_text.is_empty()).then_some(mode_text),
        context,
        budget,
    )
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
                &StatusContext::default(),
                100,
            ));
            assert!(text.contains(reference), "状态栏应与模型列表使用同一引用");
            assert!(
                !text.contains(&choice.model),
                "实际请求模型名不应替代配置引用"
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
            &StatusContext::default(),
            100,
        ));
        for word in ["override", "fallback", "role", "default"] {
            assert!(!text.contains(word), "{word} 不应出现在状态栏: {text}");
        }
        // 窄屏仍守同一套宽度预算。
        for width in [20, 32, 48] {
            let line = primary(
                Some(&choice),
                ThinkLevel::High,
                pp_core::plan::WorkMode::Plan,
                &StatusContext::default(),
                width,
            );
            assert!(line.width() <= usize::from(width), "{width}: {line:?}");
        }
    }
}
