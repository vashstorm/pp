use crate::config::{ModelChoice, ThinkLevel};
use anyhow::{Context, Result};
use ratatui::{
    style::{Color, Style},
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
const MODEL: Color = Color::Rgb(190, 133, 169);
const DIRECTORY: Color = Color::Rgb(36, 174, 181);
const GIT: Color = Color::Rgb(204, 167, 72);
// 与 omp 的 Nerd Font 图标一致；图标后保留一列空白，容纳字形右侧伸展。
const MODEL_ICON: &str = "\u{ec19}";
const THINK_ICON: &str = "\u{f0aa3}";
const DIRECTORY_ICON: &str = "\u{f07c}";
const GIT_ICON: &str = "\u{f126}";
const ICON_WIDTH: usize = 2;

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

pub(super) fn primary(
    model: Option<&ModelChoice>,
    think_level: ThinkLevel,
    context: &StatusContext,
    width: u16,
) -> Line<'static> {
    let width = usize::from(width);
    let model = model.map_or("—", |choice| choice.model.as_str());
    let model = compact(model, (width / 4).max(12).saturating_sub(ICON_WIDTH), false);
    let git = context
        .git
        .as_ref()
        .map(|git| compact(&git.label(), (width / 4).saturating_sub(ICON_WIDTH), false));
    let think = think_level.as_str();
    let reserved = Line::from(model.as_str()).width()
        + Line::from(think).width()
        + git
            .as_ref()
            .map_or(0, |git| Line::from(git.as_str()).width())
        + if git.is_some() {
            9 + 4 * ICON_WIDTH
        } else {
            6 + 3 * ICON_WIDTH
        }
        + 1;
    let workdir = compact(&context.workdir, width.saturating_sub(reserved), true);
    let separator = || Span::styled(" · ", Style::default().fg(Color::DarkGray));
    let icon = |glyph, color| Span::styled(format!("{glyph} "), Style::default().fg(color));
    let mut spans = vec![
        Span::raw(" "),
        icon(MODEL_ICON, MODEL),
        Span::styled(model, Style::default().fg(MODEL)),
        separator(),
        // default 表示省略请求参数，使用服务端默认值。
        icon(THINK_ICON, MODEL),
        Span::styled(think, Style::default().fg(MODEL)),
        separator(),
        icon(DIRECTORY_ICON, DIRECTORY),
        Span::styled(workdir, Style::default().fg(DIRECTORY)),
    ];
    if let Some(git) = git {
        spans.push(separator());
        spans.push(icon(GIT_ICON, GIT));
        spans.push(Span::styled(git, Style::default().fg(GIT)));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primary_displays_selected_think_level() {
        for level in [
            ThinkLevel::Low,
            ThinkLevel::Medium,
            ThinkLevel::High,
            ThinkLevel::Xhigh,
            ThinkLevel::Max,
        ] {
            let line = primary(None, level, &StatusContext::default(), 100).to_string();
            assert!(line.contains(&format!("{THINK_ICON} {} ·", level.as_str())));
        }
    }

    #[test]
    fn primary_keeps_field_order_and_git_visible_with_long_unicode_workdir() {
        let model = ModelChoice {
            reference: "fixture/alias".into(),
            model: "fixture-model".into(),
            protocol: crate::models::Protocol::Response,
        };
        let context = StatusContext {
            workdir: format!("/{}project", "中文目录/".repeat(30)),
            git: Some(GitInfo::parse(b"# branch.head main\0? file\0")),
        };
        for width in [60, 80, 100, 200] {
            let line = primary(Some(&model), ThinkLevel::Default, &context, width);
            assert!(line.width() <= usize::from(width));
            let text = line.to_string();
            assert!(text.starts_with(" \u{ec19} fixture-model · \u{f0aa3} default · \u{f07c} …"));
            assert!(text.ends_with("project · \u{f126} main ?1"));
            for glyph in [MODEL_ICON, THINK_ICON, DIRECTORY_ICON, GIT_ICON] {
                let icon = line
                    .spans
                    .iter()
                    .find(|span| span.content.starts_with(glyph))
                    .unwrap();
                assert_eq!(icon.width(), ICON_WIDTH);
                assert!(icon.content.ends_with(' '));
            }
        }
        let no_git = primary(None, ThinkLevel::Default, &StatusContext::default(), 100).to_string();
        assert!(!no_git.contains(GIT_ICON));
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
