use crate::config::{ModelChoice, ThinkLevel};
use crate::context::ContextUsage;
use anyhow::{Context, Result};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use std::{
    path::Path,
    process::{Command, Stdio},
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::Duration,
};

pub(super) const ACCENT: Color = Color::Rgb(127, 166, 106);
const MODEL: Color = Color::Rgb(223, 167, 209);
const DIRECTORY: Color = Color::Rgb(36, 174, 181);
const GIT: Color = Color::Rgb(204, 167, 72);
pub(super) const CONTEXT: Color = Color::Rgb(122, 162, 247);
// 与 omp 的 Nerd Font 图标一致；图标后保留一列空白，容纳字形右侧伸展。
const MODEL_ICON: &str = "\u{ec19}";
const THINK_ICON: &str = "\u{f0766}";
const DIRECTORY_ICON: &str = "\u{f07c}";
const GIT_ICON: &str = "\u{f126}";
const ICON_WIDTH: usize = 2;
// 分隔符与图标列在宽度预算中的固定开销。
const SEPARATOR_WIDTH: usize = 3;
const FIELD_ICON_WIDTH: usize = ICON_WIDTH + 1;
// 目录字段低于该宽度时先隐藏 git、再缩短模型，而不是把目录挤成空。
const MIN_DIRECTORY: usize = 8;

/// 只在缩写后至少为 10 时使用 k，省略小数；CLI 任务结束信息用完整整数。
fn abbreviate_tokens(tokens: u64) -> String {
    if tokens >= 10000 {
        format!("{}k", tokens / 1000)
    } else {
        tokens.to_string()
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
// 每级布局都保证整行不超过给定宽度。
pub(super) fn primary(
    model: Option<&ModelChoice>,
    think_level: ThinkLevel,
    context: &StatusContext,
    width: u16,
) -> Line<'static> {
    let width = usize::from(width);
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
    let budget = width.saturating_sub(3);
    let separator = || Span::styled(" · ", Style::default().fg(Color::DarkGray));
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
                // 4) 仍不足：仅显示模型前缀。
                return Line::from(Span::styled(
                    compact(model_src, width, false),
                    Style::default().fg(MODEL),
                ));
            }
            cwd_slot = budget.saturating_sub(model_field(&model_text) + effort_width + dir_fixed);
        }
        workdir_text = compact(&context.workdir, cwd_slot, true);
    }

    let model_text_style = Style::default().fg(MODEL).add_modifier(Modifier::BOLD);
    let mut spans = vec![
        Span::raw(" "),
        Span::styled(
            "pp",
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
    fn context_abbreviation_uses_integer_units_only_from_ten() {
        for (tokens, expected) in [
            (0, "0"),
            (999, "999"),
            (1000, "1000"),
            (9999, "9999"),
            (10000, "10k"),
            (10999, "10k"),
            (12345, "12k"),
            (128000, "128k"),
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
}
