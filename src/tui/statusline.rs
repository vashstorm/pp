use crate::config::{ModelChoice, ThinkLevel};
use crate::context::{ContextUsage, TokenUsage};
use anyhow::{Context, Result};
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

pub(super) const ACCENT: Color = Color::Rgb(127, 166, 106);
// Plan 模式标签：与 transcript 高亮一致的琥珀色。
const PLAN_MODE: Color = Color::Rgb(254, 188, 56);
// 模式标签之外仍需可容纳的最小字段空间（图标+至少一个字符）。
const MIN_AFTER_MODE: usize = FIELD_ICON_WIDTH + 1;
const MODEL: Color = Color::Rgb(223, 167, 209);
const DIRECTORY: Color = Color::Rgb(36, 174, 181);
const GIT: Color = Color::Rgb(204, 167, 72);
pub(super) const CONTEXT: Color = Color::Rgb(122, 162, 247);
const SPEED: Color = Color::Rgb(224, 175, 104);
const INPUT: Color = Color::Rgb(125, 207, 255);
const OUTPUT: Color = Color::Rgb(158, 206, 106);
const CACHED: Color = Color::Rgb(187, 154, 247);
// 与 omp 的 Nerd Font 图标一致；图标后保留一列空白，容纳字形右侧伸展。
// Paper Plane 的首字母与应用名 pp 对应。
const APP_ICON: &str = "\u{f1d8}";
const MODEL_ICON: &str = "\u{ec19}";
const THINK_ICON: &str = "\u{f0766}";
const PLAN_ICON: &str = "\u{f0132}";
const DIRECTORY_ICON: &str = "\u{f07c}";
const GIT_ICON: &str = "\u{f126}";
const SPEED_ICON: &str = "\u{f0e7}";
const INPUT_ICON: &str = "\u{f019}";
const OUTPUT_ICON: &str = "\u{f093}";
const CACHED_ICON: &str = "\u{f1c0}";
const ICON_WIDTH: usize = 2;
// 分隔符与图标列在宽度预算中的固定开销。
const SEPARATOR_WIDTH: usize = 3;
const FIELD_ICON_WIDTH: usize = ICON_WIDTH + 1;
// 目录字段低于该宽度时先隐藏 git、再缩短模型，而不是把目录挤成空。
const MIN_DIRECTORY: usize = 8;

/// 满 1000 使用 k，满 100 万使用 m，缩写后保留一位小数；CLI 仍用完整整数。
fn abbreviate_tokens(tokens: u64) -> String {
    if tokens >= 1000 {
        abbreviate_metric(tokens as f64)
    } else {
        tokens.to_string()
    }
}

fn abbreviate_metric(value: f64) -> String {
    if value >= 1_000_000.0 {
        format!("{:.1}m", value / 1_000_000.0)
    } else if value >= 1000.0 {
        format!("{:.1}k", value / 1000.0)
    } else {
        format!("{value:.1}")
    }
}

/// 输入栏上沿的上下文用量：容量未知时只显示当前估算值。
pub(super) fn context_field(usage: &ContextUsage) -> (String, Color) {
    let percent = usage.percent();
    let text = match usage.context_window {
        Some(window) => format!(
            "ctx {}/{}",
            abbreviate_tokens(usage.estimated_tokens),
            abbreviate_tokens(window),
        ),
        None => format!("ctx {}", abbreviate_tokens(usage.estimated_tokens)),
    };
    let color = match percent {
        Some(percent) if percent >= 100 => Color::Red,
        Some(percent) if percent >= u128::from(usage.threshold_percent) => Color::Yellow,
        _ => CONTEXT,
    };
    (text, color)
}

/// 输入栏上沿的 token 指标：速度 → 输入 → 输出 → 缓存，末尾留分隔符以衔接 ctx。
/// 空间不足时按 cached → in → out → speed 的整字段顺序降级，绝不切开图标或文字。
pub(super) fn metric_fields(
    usage: Option<&TokenUsage>,
    speed: Option<(f64, bool)>,
    width: u16,
) -> Line<'static> {
    let speed_text = speed.map_or_else(
        || "— tok/s".to_owned(),
        |(rate, _)| format!("{} tok/s", abbreviate_metric(rate)),
    );
    // 缺失用量显示 —，显式的 0 仍显示 0。
    let (input_text, output_text, cached_text) = usage.map_or_else(
        || ("—".to_owned(), "—".to_owned(), "—".to_owned()),
        |usage| {
            (
                abbreviate_tokens(usage.input_tokens),
                abbreviate_tokens(usage.output_tokens),
                usage
                    .cached_tokens
                    .map_or_else(|| "—".to_owned(), abbreviate_tokens),
            )
        },
    );
    let fields = [
        (SPEED_ICON, SPEED, speed_text),
        (INPUT_ICON, INPUT, format!("in {input_text}")),
        (OUTPUT_ICON, OUTPUT, format!("out {output_text}")),
        (CACHED_ICON, CACHED, format!("cached {cached_text}")),
    ];
    let separator = || Span::styled(" · ", Style::default().fg(Color::DarkGray));
    let mut spans = Vec::with_capacity(13);
    spans.push(Span::raw(" "));
    for (icon, color, text) in fields {
        let style = Style::default().fg(color);
        spans.push(Span::styled(format!("{icon} "), style));
        spans.push(Span::styled(text, style));
        spans.push(separator());
    }
    let mut line = Line::from(spans);
    let mut keep = [true; 4];
    for remove in [3, 1, 2, 0] {
        if line.width() <= usize::from(width) {
            return line;
        }
        // Each complete field owns its icon, value and following separator.
        let start = 1 + keep[..remove].iter().filter(|&&kept| kept).count() * 3;
        line.spans.drain(start..start + 3);
        keep[remove] = false;
    }
    Line::default()
}

const TICK_INTERVAL: Duration = Duration::from_millis(250);
const SPEED_WINDOW: Duration = Duration::from_secs(2);

/// 当前流式请求的输出速度：生成中按已观测字节估算，完成后用服务端 usage 校正。
#[derive(Debug, Default)]
pub(super) struct TokenSpeed {
    started_at: Option<Instant>,
    output_bytes: u64,
    /// 显示快照只在采样或请求完成时更新，重绘不重新计算。
    displayed_rate: Option<(f64, bool)>,
    /// 按刷新节奏保存累计字节基线，避免每个 delta 分配样本。
    samples: VecDeque<(Instant, u64)>,
    last_tick: Option<Instant>,
}

impl TokenSpeed {
    pub(super) fn start(&mut self, now: Instant) {
        *self = Self {
            started_at: Some(now),
            // 等待下一轮和工具执行期间保留上一笔速度，直到有新的有效采样。
            displayed_rate: self.displayed_rate,
            ..Self::default()
        };
    }

    /// 本次请求截至当前的累计输出字节数。
    pub(super) fn progress(&mut self, output_bytes: u64, now: Instant) {
        if self.started_at.is_some() && output_bytes > self.output_bytes {
            if self.samples.is_empty() {
                // 从首个输出开始计时，排除建连、排队和限流等待。
                self.samples.push_back((now, 0));
                self.last_tick = Some(now);
            }
            self.output_bytes = output_bytes;
        }
    }

    /// 结束计时：成功且有服务端结果时显示权威速率，否则保留或丢弃估算。
    pub(super) fn finish(&mut self, elapsed_ms: u64, output_tokens: Option<u64>, success: bool) {
        let elapsed_secs = elapsed_ms as f64 / 1000.0;
        self.started_at = None;
        self.last_tick = None;
        self.samples.clear();
        self.displayed_rate = if !success || elapsed_secs <= 0.0 {
            None
        } else if let Some(tokens) = output_tokens {
            Some((tokens as f64 / elapsed_secs, false))
        } else {
            estimate_rate(self.output_bytes, elapsed_secs)
                .map(|rate| (rate, true))
                .or(self.displayed_rate)
        };
    }

    pub(super) fn clear(&mut self) {
        *self = Self::default();
    }

    /// 每 250ms 更新约两秒窗口内的估速，采样数量与 delta 频率无关。
    pub(super) fn tick(&mut self, now: Instant) -> bool {
        if self.started_at.is_none() || self.output_bytes == 0 {
            return false;
        }
        match self.last_tick {
            Some(last) if now.saturating_duration_since(last) < TICK_INTERVAL => false,
            _ => {
                self.last_tick = Some(now);
                let cutoff = now.checked_sub(SPEED_WINDOW).unwrap_or(now);
                while self.samples.len() > 1 && self.samples[1].0 <= cutoff {
                    self.samples.pop_front();
                }
                let Some(&(baseline, bytes)) = self.samples.front() else {
                    return false;
                };
                let elapsed = now.saturating_duration_since(baseline).as_secs_f64();
                let rate = (self.output_bytes.saturating_sub(bytes) as f64 / 3.0) / elapsed;
                self.samples.push_back((now, self.output_bytes));
                let next = Some((rate, true));
                let changed = self.displayed_rate != next;
                self.displayed_rate = next;
                changed
            }
        }
    }

    /// 返回缓存速率；`bool` 表示是否为估算值。
    pub(super) fn value(&self) -> Option<(f64, bool)> {
        self.displayed_rate
    }

    pub(super) fn is_active(&self) -> bool {
        self.started_at.is_some()
    }
}

/// 沿用项目 `/3` 的字节启发式；首个 token 等待时间计入分母。
fn estimate_rate(output_bytes: u64, elapsed_secs: f64) -> Option<f64> {
    if output_bytes == 0 || elapsed_secs <= 0.0 {
        return None;
    }
    Some(output_bytes.div_ceil(3) as f64 / elapsed_secs)
}

#[derive(Default)]
pub(super) struct StatusContext {
    pub(super) workdir: String,
    pub(super) git: Option<GitInfo>,
}

pub(super) struct WorkspaceStatus {
    pub(super) context: StatusContext,
    updates: Receiver<Option<GitInfo>>,
    _stop: Sender<()>,
}

impl WorkspaceStatus {
    pub(super) fn new(cwd: &Path) -> Result<Self> {
        let workdir = std::env::var_os("HOME")
            .and_then(|home| cwd.strip_prefix(home).ok().map(|path| path.to_owned()))
            .map_or_else(
                || cwd.display().to_string(),
                |path| {
                    if path.as_os_str().is_empty() {
                        "~".into()
                    } else {
                        format!("~/{}", path.display())
                    }
                },
            );
        let cwd = cwd.to_owned();
        let (sender, updates) = mpsc::sync_channel(1);
        let (stop, stopped) = mpsc::channel::<()>();
        thread::Builder::new()
            .name("pp-workspace-status".into())
            .spawn(move || {
                let mut previous = None;
                loop {
                    let git = GitInfo::read(&cwd);
                    if previous.as_ref() != Some(&git) {
                        if sender.send(git.clone()).is_err() {
                            break;
                        }
                        previous = Some(git);
                    }
                    if stopped.recv_timeout(Duration::from_secs(2))
                        != Err(mpsc::RecvTimeoutError::Timeout)
                    {
                        break;
                    }
                }
            })
            .context("starting workspace status worker")?;
        Ok(Self {
            context: StatusContext { workdir, git: None },
            updates,
            _stop: stop,
        })
    }

    pub(super) fn receive(&mut self) -> bool {
        let mut changed = false;
        while let Ok(git) = self.updates.try_recv() {
            self.context.git = git;
            changed = true;
        }
        changed
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct GitInfo {
    branch: String,
    changed: usize,
    untracked: usize,
    ahead: usize,
    behind: usize,
}

impl GitInfo {
    fn read(cwd: &Path) -> Option<Self> {
        let output = Command::new("git")
            .args([
                "--no-optional-locks",
                "status",
                "--porcelain=v2",
                "--branch",
                "-z",
                "--untracked-files=normal",
            ])
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        output.status.success().then(|| Self::parse(&output.stdout))
    }

    fn parse(output: &[u8]) -> Self {
        let mut info = Self::default();
        let mut oid = String::new();
        let mut records = output.split(|byte| *byte == 0);
        while let Some(record) = records.next() {
            if let Some(branch) = record.strip_prefix(b"# branch.head ") {
                info.branch = String::from_utf8_lossy(branch).into_owned();
            } else if let Some(hash) = record.strip_prefix(b"# branch.oid ") {
                oid = String::from_utf8_lossy(hash).chars().take(7).collect();
            } else if let Some(counts) = record.strip_prefix(b"# branch.ab ") {
                let counts = String::from_utf8_lossy(counts);
                if let Some((ahead, behind)) = counts.split_once(' ') {
                    info.ahead = ahead.trim_start_matches('+').parse().unwrap_or(0);
                    info.behind = behind.trim_start_matches('-').parse().unwrap_or(0);
                }
            } else {
                match record.first() {
                    Some(b'1' | b'u') => info.changed += 1,
                    Some(b'2') => {
                        info.changed += 1;
                        // 重命名记录后面紧跟原路径，不能把它当作另一条状态记录。
                        records.next();
                    }
                    Some(b'?') => info.untracked += 1,
                    _ => {}
                }
            }
        }
        if info.branch == "(detached)" {
            info.branch = format!("detached@{oid}");
        }
        info
    }

    fn label(&self) -> String {
        let mut text = self.branch.clone();
        for (marker, count) in [
            ("*", self.changed),
            ("?", self.untracked),
            ("↑", self.ahead),
            ("↓", self.behind),
        ] {
            if count > 0 {
                text.push_str(&format!(" {marker}{count}"));
            }
        }
        text
    }
}

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
// 仅 Plan 显示模式标签：预算不足时先压缩其它字段，再缩为单字母，最后只留模式。
pub(super) fn primary(
    model: Option<&ModelChoice>,
    think_level: ThinkLevel,
    mode: crate::plan::WorkMode,
    context: &StatusContext,
    width: u16,
) -> Line<'static> {
    use crate::plan::WorkMode as Mode;
    let width = usize::from(width);
    let mode_span = |text: &'static str| {
        Span::styled(
            text,
            Style::default().fg(PLAN_MODE).add_modifier(Modifier::BOLD),
        )
    };
    let mode_slot = |text: &str| SEPARATOR_WIDTH + FIELD_ICON_WIDTH + Line::from(text).width();
    let separator = || Span::styled(" · ", Style::default().fg(Color::DarkGray));
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
    let think = match think_level {
        ThinkLevel::None => "off",
        ThinkLevel::Low => "low",
        ThinkLevel::Medium => "med",
        ThinkLevel::High => "hig",
        ThinkLevel::Xhigh => "xhi",
        ThinkLevel::Max => "max",
    };
    // 思考级别统一使用 Nerd Font Material Design 图标，避免混用普通圆形字形导致宽度不一。
    let think_icon = match think_level {
        ThinkLevel::None => THINK_ICON,
        ThinkLevel::Low => "\u{f0a9f}",
        ThinkLevel::Medium => "\u{f0aa1}",
        ThinkLevel::High => "\u{f0aa3}",
        ThinkLevel::Xhigh => "\u{f0aa5}",
        ThinkLevel::Max => "\u{f0238}",
    };
    let icon = |glyph, color| Span::styled(format!("{glyph} "), Style::default().fg(color));
    let model_src = model.map_or("—", |choice| choice.model.as_str());
    let think_width = Line::from(think).width();
    let effort_width = SEPARATOR_WIDTH + FIELD_ICON_WIDTH + think_width;
    let dir_fixed = SEPARATOR_WIDTH + FIELD_ICON_WIDTH;
    let model_field = |text: &str| SEPARATOR_WIDTH + FIELD_ICON_WIDTH + Line::from(text).width();
    let git_field = |text: &str| SEPARATOR_WIDTH + FIELD_ICON_WIDTH + Line::from(text).width();
    let workdir_full = Line::from(context.workdir.as_str()).width();
    let git = context.git.as_ref().map(|git| git.label());

    let mut model_text = model_src.to_owned();
    let mut git_text = git.clone();
    let mut workdir_text = context.workdir.clone();
    if budget
        < model_field(model_src)
            + effort_width
            + dir_fixed
            + workdir_full
            + git.as_deref().map_or(0, git_field)
    {
        // 1) 缩短 cwd（git 保留）。
        let mut cwd_slot = budget.saturating_sub(
            model_field(model_src) + effort_width + dir_fixed + git.as_deref().map_or(0, git_field),
        );
        if cwd_slot < MIN_DIRECTORY && git_text.is_some() {
            // 2) 隐藏 git，把空间让给目录。
            git_text = None;
            cwd_slot = budget.saturating_sub(model_field(model_src) + effort_width + dir_fixed);
        }
        if cwd_slot < MIN_DIRECTORY {
            // 3) 缩短模型。
            model_text = compact(
                model_src,
                (budget / 4).max(12).saturating_sub(FIELD_ICON_WIDTH),
                false,
            );
            if budget < model_field(&model_text) + effort_width + dir_fixed {
                // 4) 仍不足：仅显示模型前缀与模式；模式图标和标签优先保留。
                let mut spans = vec![Span::styled(
                    compact(model_src, budget, false),
                    Style::default().fg(MODEL).add_modifier(Modifier::BOLD),
                )];
                if !mode_text.is_empty() {
                    if budget > 0 {
                        spans.push(separator());
                    }
                    spans.push(icon(PLAN_ICON, PLAN_MODE));
                    spans.push(mode_span(mode_text));
                }
                return Line::from(spans);
            }
            cwd_slot = budget.saturating_sub(model_field(&model_text) + effort_width + dir_fixed);
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
    if !mode_text.is_empty() {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_abbreviation_uses_thousands_and_millions_with_one_decimal() {
        for (tokens, expected) in [
            (0, "0"),
            (999, "999"),
            (1000, "1.0k"),
            (1050, "1.1k"),
            (9999, "10.0k"),
            (10000, "10.0k"),
            (10999, "11.0k"),
            (12345, "12.3k"),
            (128000, "128.0k"),
            (999900, "999.9k"),
            (1_000_000, "1.0m"),
            (1_240_000, "1.2m"),
            (1_280_000, "1.3m"),
            (1_000_000_000, "1000.0m"),
        ] {
            assert_eq!(abbreviate_tokens(tokens), expected);
        }
    }

    #[test]
    fn git_status_handles_renames_conflicts_and_detached_head() {
        let info = GitInfo::parse(
            b"# branch.oid abcdef123456\0# branch.head main\0# branch.ab +2 -3\0\
              1 .M metadata file\0\
              2 R. metadata renamed\0? original\0\
              u UU metadata conflict\0? untracked\nfile\0",
        );
        assert_eq!(info.label(), "main *3 ?1 ↑2 ↓3");
        assert_eq!(
            GitInfo::parse(b"# branch.oid abcdef123456\0# branch.head (detached)\0").label(),
            "detached@abcdef1"
        );
        assert_eq!(
            GitInfo::parse(b"# branch.oid (initial)\0# branch.head main\0").label(),
            "main"
        );
    }

    #[test]
    fn git_status_reads_disposable_repository_and_detects_changes() {
        let dir = tempfile::tempdir().unwrap();
        assert!(GitInfo::read(dir.path()).is_none());
        assert!(
            Command::new("git")
                .args(["init", "-q", "--initial-branch=fixture"])
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success()
        );
        assert_eq!(GitInfo::read(dir.path()).unwrap().label(), "fixture");
        std::fs::write(dir.path().join("draft-中"), "draft").unwrap();
        assert_eq!(GitInfo::read(dir.path()).unwrap().label(), "fixture ?1");
        assert!(
            Command::new("git")
                .args(["add", "--", "draft-中"])
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success()
        );
        assert_eq!(GitInfo::read(dir.path()).unwrap().label(), "fixture *1");
    }

    #[test]
    fn token_speed_samples_from_first_output_and_caches_between_ticks() {
        let base = Instant::now();
        let mut speed = TokenSpeed::default();
        speed.start(base);
        let first = base + Duration::from_secs(10);
        assert!(!speed.tick(first));
        assert_eq!(speed.value(), None);
        speed.progress(75, first);
        assert!(!speed.tick(first + Duration::from_millis(249)));
        assert!(speed.tick(first + TICK_INTERVAL));
        assert_eq!(speed.value(), Some((100.0, true)), "不计首字节等待");
        speed.progress(150, first + Duration::from_millis(300));
        speed.progress(90, first + Duration::from_millis(310));
        assert_eq!(speed.value(), Some((100.0, true)), "delta 不改变显示快照");
        assert!(!speed.tick(first + Duration::from_millis(400)));
        speed.tick(first + Duration::from_millis(500));
        assert_eq!(speed.value(), Some((100.0, true)), "累计字节不能倒退");
    }

    #[test]
    fn token_speed_window_tracks_recent_output_and_bounds_samples() {
        let base = Instant::now();
        let mut speed = TokenSpeed::default();
        speed.start(base);
        for i in 0..=40 {
            let now = base + TICK_INTERVAL * i;
            // 前两秒每秒 100 token，之后每秒 200 token。
            let bytes = if i <= 8 {
                75 * (i + 1)
            } else {
                675 + 150 * (i - 8)
            };
            speed.progress(u64::from(bytes), now);
            speed.tick(now);
            assert!(speed.samples.len() <= 10, "窗口样本必须有界");
        }
        assert_eq!(speed.value(), Some((200.0, true)), "旧速率退出窗口");
        let stopped = base + Duration::from_secs(10);
        for i in 1..=9 {
            speed.tick(stopped + TICK_INTERVAL * i);
        }
        assert_eq!(
            speed.value(),
            Some((0.0, true)),
            "停流后显示零，不能悬挂旧速率"
        );
    }

    #[test]
    fn token_speed_finish_uses_authoritative_usage_and_stops_ticks() {
        let base = Instant::now();
        let mut speed = TokenSpeed::default();
        speed.start(base);
        speed.progress(600, base);
        speed.finish(2000, Some(300), true);
        assert!(!speed.is_active());
        assert_eq!(speed.value(), Some((150.0, false)));
        assert!(!speed.tick(base + Duration::from_secs(60)));
        assert_eq!(speed.value(), Some((150.0, false)));
    }

    #[test]
    fn token_speed_short_request_without_usage_keeps_final_estimate() {
        let base = Instant::now();
        let mut speed = TokenSpeed::default();
        speed.start(base);
        speed.progress(600, base);
        speed.finish(2000, None, true);
        assert!(!speed.is_active());
        assert_eq!(speed.value(), Some((100.0, true)));
        // 没有新数据的成功请求不抹掉上一笔速度。
        speed.start(base);
        speed.finish(2000, None, true);
        assert_eq!(speed.value(), Some((100.0, true)));
    }

    #[test]
    fn token_speed_zero_elapsed_or_cancel_has_no_rate() {
        let base = Instant::now();
        let mut speed = TokenSpeed::default();
        speed.start(base);
        speed.progress(600, base);
        assert_eq!(speed.value(), None, "零耗时不产生 inf/NaN");
        speed.finish(0, Some(300), true);
        assert!(!speed.is_active());
        assert_eq!(speed.value(), None);
        speed.start(base);
        speed.progress(600, base);
        speed.tick(base + TICK_INTERVAL);
        speed.finish(500, Some(300), false);
        assert!(!speed.is_active());
        assert_eq!(speed.value(), None);
    }

    #[test]
    fn token_speed_next_request_retains_display_but_resets_samples() {
        let base = Instant::now();
        let mut speed = TokenSpeed::default();
        speed.start(base);
        speed.progress(600, base);
        speed.finish(2000, Some(300), true);
        let second = base + Duration::from_secs(5);
        speed.start(second);
        assert!(speed.is_active());
        assert!(!speed.tick(second + Duration::from_secs(1)));
        assert_eq!(speed.value(), Some((150.0, false)), "等待时保留上一笔速度");
        speed.progress(75, second + Duration::from_secs(2));
        assert_eq!(speed.value(), Some((150.0, false)), "采样未满时不闪回未知");
        speed.tick(second + Duration::from_millis(2250));
        assert_eq!(speed.value(), Some((100.0, true)), "仅计算当前请求字节");
        speed.clear();
        assert!(!speed.is_active());
        assert_eq!(speed.value(), None);
    }

    fn metric_usage(
        input_tokens: u64,
        output_tokens: u64,
        cached_tokens: Option<u64>,
    ) -> TokenUsage {
        TokenUsage {
            input_tokens,
            output_tokens,
            cached_tokens,
            cache_write_tokens: None,
        }
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn metric_fields_hide_estimate_marker_and_preserve_unknown_speed() {
        let usage = metric_usage(0, 0, Some(0));
        let text = |speed| line_text(&metric_fields(Some(&usage), speed, 200));
        assert!(text(None).contains(" \u{f0e7} — tok/s · "));
        assert!(!text(None).contains('~'));
        assert!(text(Some((0.0, false))).contains("\u{f0e7} 0.0 tok/s"));
        assert!(text(Some((150.0, false))).contains("\u{f0e7} 150.0 tok/s"));
        assert!(!text(Some((150.0, false))).contains('~'));
        assert!(text(Some((42.5, true))).contains("\u{f0e7} 42.5 tok/s"));
        assert_eq!(text(Some((42.5, true))), text(Some((42.5, false))));
        assert!(text(Some((999.0, false))).contains("\u{f0e7} 999.0 tok/s"));
        assert!(text(Some((1000.0, true))).contains("\u{f0e7} 1.0k tok/s"));
        assert!(text(Some((12345.0, false))).contains("\u{f0e7} 12.3k tok/s"));
        assert!(text(Some((1_000_000.0, false))).contains("\u{f0e7} 1.0m tok/s"));
        assert!(text(Some((1_280_000.0, true))).contains("\u{f0e7} 1.3m tok/s"));
    }

    #[test]
    fn metric_fields_distinguish_missing_usage_from_zero() {
        let missing = line_text(&metric_fields(None, Some((12.0, false)), 200));
        assert!(missing.contains("in —"));
        assert!(missing.contains("out —"));
        assert!(missing.contains("cached —"));
        assert!(!missing.contains("in 0"));

        let absent_cached = metric_usage(0, 0, None);
        let text = line_text(&metric_fields(Some(&absent_cached), None, 200));
        assert!(text.contains("in 0"));
        assert!(text.contains("out 0"));
        assert!(text.contains("cached —"));

        let zero_cached = metric_usage(1000, 9999, Some(0));
        let text = line_text(&metric_fields(Some(&zero_cached), None, 200));
        assert!(text.contains("in 1.0k"));
        assert!(text.contains("out 10.0k"));
        assert!(text.contains("cached 0"));

        let abbreviated = metric_usage(12000, 10800, Some(10000));
        let text = line_text(&metric_fields(Some(&abbreviated), Some((1.0, false)), 200));
        assert!(text.contains("in 12.0k"));
        assert!(text.contains("out 10.8k"));
        assert!(text.contains("cached 10.0k"));

        let millions = metric_usage(1_280_000, 1_000_000, Some(1_240_000));
        let text = line_text(&metric_fields(Some(&millions), None, 200));
        assert!(text.contains("in 1.3m"));
        assert!(text.contains("out 1.0m"));
        assert!(text.contains("cached 1.2m"));
    }

    #[test]
    fn metric_fields_drop_whole_fields_cached_then_in_then_out_then_speed() {
        let usage = metric_usage(12000, 300, Some(40));
        let speed = Some((150.0, false));
        let fields = [
            ("\u{f0e7}", "150.0 tok/s"),
            ("\u{f019}", "in 12.0k"),
            ("\u{f093}", "out 300"),
            ("\u{f1c0}", "cached 40"),
        ];
        // 移除顺序 cached → in → out → speed；任何宽度下都只能是这些完整的行形态。
        let kept_states = [vec![0, 1, 2, 3], vec![0, 1, 2], vec![0, 2], vec![0], vec![]];
        let candidates: Vec<String> = kept_states
            .iter()
            .map(|kept| {
                if kept.is_empty() {
                    return String::new();
                }
                let mut text = String::from(" ");
                for (position, index) in kept.iter().enumerate() {
                    if position > 0 {
                        text.push_str(" · ");
                    }
                    text.push_str(fields[*index].0);
                    text.push(' ');
                    text.push_str(fields[*index].1);
                }
                text.push_str(" · ");
                text
            })
            .collect();
        let widths: Vec<usize> = candidates
            .iter()
            .map(|text| Line::from(text.as_str()).width())
            .collect();
        for pair in widths.windows(2) {
            assert!(pair[0] > pair[1], "每次降级都必须严格变窄: {widths:?}");
        }
        for width in 0..=u16::try_from(widths[0] + 2).unwrap() {
            let line = metric_fields(Some(&usage), speed, width);
            assert!(
                line.width() <= usize::from(width),
                "宽度 {width} 放不下 {line:?}"
            );
            let expected = widths
                .iter()
                .position(|candidate| *candidate <= usize::from(width))
                .unwrap();
            assert_eq!(line_text(&line), candidates[expected], "宽度 {width}");
        }
    }

    #[test]
    fn metric_fields_fit_display_columns_not_utf8_bytes() {
        let usage = metric_usage(12000, 300, Some(40));
        let speed = Some((150.0, false));
        let full = metric_fields(Some(&usage), speed, u16::MAX);
        let text = line_text(&full);
        assert!(
            text.len() > full.width(),
            "Nerd 图标是多字节 UTF-8，但只占少量显示列"
        );
        let width = u16::try_from(full.width()).unwrap();
        assert_eq!(line_text(&metric_fields(Some(&usage), speed, width)), text);
        let narrower = line_text(&metric_fields(Some(&usage), speed, width - 1));
        assert!(!narrower.contains("cached 40"), "{narrower}");
        assert!(Line::from(narrower.as_str()).width() < full.width());
    }

    #[test]
    fn metric_fields_are_empty_when_no_field_fits() {
        let usage = metric_usage(12000, 300, Some(40));
        let first = (0..=200)
            .find(|width| !metric_fields(Some(&usage), None, *width).spans.is_empty())
            .unwrap();
        assert!(first > 0);
        for width in 0..first {
            let line = metric_fields(Some(&usage), None, width);
            assert!(line.spans.is_empty(), "宽度 {width}: {line:?}");
            assert_eq!(line.width(), 0);
        }
        let line = metric_fields(Some(&usage), None, first);
        assert_eq!(line.width(), usize::from(first));
        assert_eq!(line_text(&line), " \u{f0e7} — tok/s · ");
    }
}
