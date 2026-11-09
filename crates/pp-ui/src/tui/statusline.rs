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

/// 满千使用 K、满百万使用 M，去掉无意义的 .0；CLI 仍用完整整数。
pub(super) fn abbreviate_tokens(tokens: u64) -> String {
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

fn field(icon: &str, text: String, color: Color) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{icon} "), Style::default().fg(color)),
        Span::styled(text, Style::default().fg(color)),
    ])
}

pub(super) fn context_field(usage: &ContextUsage) -> Line<'static> {
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

fn pair_field(icon: &str, left: String, right: String, colors: (Color, Color)) -> Line<'static> {
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
fn amount_field(usd: f64, display: &pp_config::config::CostDisplay) -> Option<Line<'static>> {
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

fn join_fields(fields: &[Option<Line<'static>>]) -> Line<'static> {
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

fn speed_field(rate: Option<f64>, running_subagents: usize) -> Line<'static> {
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
pub(super) fn metric_fields(
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

/// 子 Agent 详情指标簇的输入：上下文用量、token 总量回退、费用与耗时。
pub(super) struct SubagentUsage<'a> {
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
/// 其余字段沿用主状态栏的构造与配色，宽度不足时按 cache → io → 费用 → 耗时 → ctx
/// 整组隐藏，任何宽度下返回行宽都不超过 `width`。
pub(super) fn subagent_metrics(
    usage: SubagentUsage<'_>,
    display: &pp_config::config::CostDisplay,
    width: u16,
) -> Line<'static> {
    let width = usize::from(width);
    let count = |value: Option<u64>| value.map_or_else(|| "—".into(), abbreviate_tokens);
    let mut fields: [Option<Line<'static>>; 6] =
        [Some(subagent_badge()), None, None, None, None, None];
    if let Some(context) = usage.usage {
        let last = context.last_request;
        // 与主状态栏一致：最近请求和会话累计都为空时右侧显示 —，而不是 0。
        let output = (last.is_some() || context.session_output_tokens > 0)
            .then_some(context.session_output_tokens);
        fields[1] = Some(context_field(context));
        fields[2] = Some(pair_field(
            IO_ICON,
            count(last.map(|request| request.input_tokens)),
            count(output),
            (INPUT, OUTPUT),
        ));
        fields[3] = Some(pair_field(
            CACHE_ICON,
            count(last.and_then(|request| request.cached_tokens)),
            count(last.and_then(|request| request.cache_write_tokens)),
            (CACHE_READ, CACHE_WRITE),
        ));
    } else if let Some(tokens) = usage.tokens {
        // 旧日志没有 ContextUpdated 时保留原先摘要里的 token 总量；它替代
        // ctx/io/cache 三个字段，因此在降级顺序里与 ctx 同位（最后隐藏）。
        fields[1] = Some(token_total_field(tokens));
    }
    fields[4] = usage.cost.and_then(|usd| {
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
    fields[5] = usage
        .time_ms
        .map(|ms| field(TIMER_ICON, duration_text(ms), Color::White));

    let mut line = join_fields(&fields);
    for remove in [3, 2, 4, 5, 1] {
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
        super::tool_display::clip(&text, width, true),
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

    /// 生成中按项目统一的 `/3` 字节启发式估算本次请求的输出 token；
    /// 请求结束后返回 `None`，交给服务端 usage 校正。
    pub(super) fn live_output_tokens(&self) -> Option<u64> {
        (self.started_at.is_some() && self.output_bytes > 0).then(|| self.output_bytes.div_ceil(3))
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

/// 工作目录显示名：HOME 内折叠为 `~` 或 `~/…`，其余路径原样显示。
pub(super) fn workdir_label(cwd: &Path) -> String {
    std::env::var_os("HOME")
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
        )
}

pub(super) struct WorkspaceStatus {
    pub(super) context: StatusContext,
    updates: Receiver<Option<GitInfo>>,
    _stop: Sender<()>,
}

impl WorkspaceStatus {
    pub(super) fn new(cwd: &Path) -> Result<Self> {
        let workdir = workdir_label(cwd);
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
// Plan 模式标签由调用方按预算决定：不足时先压缩其它字段，再缩为单字母，最后只留模式。
fn status_line(
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

    let mut model_text = model.to_owned();
    let mut git_text = git.clone();
    let mut workdir_text = context.workdir.clone();
    if width
        < field_slot(model)
            + effort_width
            + dir_fixed
            + workdir_full
            + git.as_deref().map_or(0, field_slot)
    {
        // 1) 缩短 cwd（git 保留）。
        let mut cwd_slot = width.saturating_sub(
            field_slot(model) + effort_width + dir_fixed + git.as_deref().map_or(0, field_slot),
        );
        if cwd_slot < MIN_DIRECTORY && git_text.is_some() {
            // 2) 隐藏 git，把空间让给目录。
            git_text = None;
            cwd_slot = width.saturating_sub(field_slot(model) + effort_width + dir_fixed);
        }
        if cwd_slot < MIN_DIRECTORY {
            // 3) 缩短模型。
            model_text = compact(
                model,
                (width / 4).max(12).saturating_sub(FIELD_ICON_WIDTH),
                false,
            );
            if width < field_slot(&model_text) + effort_width + dir_fixed {
                // 4) 仍不足：仅显示模型前缀与模式；模式图标和标签优先保留。
                let mut spans = vec![Span::styled(
                    compact(model, width, false),
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

/// 子 Agent 详情底栏：共用主状态栏的头部、配色与降级规则，不显示 git 与模式标签。
pub(super) fn subagent_status(
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
    use super::*;
    use pp_core::context::TokenUsage;
    use ratatui::{Terminal, backend::TestBackend, widgets::Paragraph};
    use std::path::PathBuf;

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
        // TMPDIR 可能指向工作树内部（pp 的 bash 工具把命令临时目录设在项目里），
        // 那样临时目录会成为仓库子目录；用系统临时目录保持“非仓库目录”前提。
        let dir = tempfile::Builder::new()
            .prefix("pp-git-")
            .tempdir_in("/tmp")
            .unwrap();
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

    #[test]
    fn token_speed_live_output_grows_with_bytes_and_stops_at_usage() {
        let base = Instant::now();
        let mut speed = TokenSpeed::default();
        assert_eq!(speed.live_output_tokens(), None, "未开始请求时不估算");
        speed.start(base);
        assert_eq!(speed.live_output_tokens(), None, "尚无输出时保留上一个数字");
        speed.progress(600, base + Duration::from_millis(10));
        assert_eq!(speed.live_output_tokens(), Some(200));
        speed.progress(900, base + Duration::from_millis(20));
        assert_eq!(speed.live_output_tokens(), Some(300), "随已接收字节增长");
        speed.finish(1000, Some(280), true);
        assert_eq!(
            speed.live_output_tokens(),
            None,
            "完成后必须交给权威 usage，而不是保留估算"
        );
        speed.start(base);
        speed.progress(30, base);
        speed.finish(1000, None, false);
        assert_eq!(speed.live_output_tokens(), None, "失败或取消不保留估算");
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
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
    fn status_usage() -> ContextUsage {
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
                " {SUBAGENT_ONE_ICON} subagent · {CONTEXT_ICON} 81.6%/500K · {IO_ICON} 12.3K/45.7K · {CACHE_ICON} 8K/0 · {COST_ICON} 0.012 · {TIMER_ICON} 1m25s "
            )
        );
        let badge = line
            .spans
            .iter()
            .find(|span| span.content == format!("{SUBAGENT_ONE_ICON} subagent"))
            .unwrap();
        assert!(badge.style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(fg_at(line.clone(), 240, "subagent"), SUBAGENTS);
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
                " {SUBAGENT_ONE_ICON} subagent · 12.3K tok · {COST_ICON} 0.500 · {TIMER_ICON} 1s "
            )
        );
        assert_eq!(fg_at(fallback.clone(), 240, "12.3K"), INPUT);
        assert_eq!(fg_at(fallback, 240, " tok"), Color::DarkGray);
    }

    #[test]
    fn subagent_metrics_never_hides_badge_and_hides_groups_in_priority_order() {
        let usage = status_usage();
        let render = |width: u16| {
            subagent_metrics(
                SubagentUsage {
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
        let removal_order = [CACHE_ICON, IO_ICON, COST_ICON, TIMER_ICON, CONTEXT_ICON];
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

    #[test]
    fn workdir_label_folds_home_and_keeps_outside_paths() {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        if let Some(home) = home.as_ref() {
            assert_eq!(workdir_label(home), "~");
            assert_eq!(workdir_label(&home.join("sub").join("dir")), "~/sub/dir");
        }
        let outside = PathBuf::from("/pp-workdir-label-outside/project");
        if !home.as_ref().is_some_and(|home| outside.starts_with(home)) {
            assert_eq!(workdir_label(&outside), "/pp-workdir-label-outside/project");
        }
    }
}
