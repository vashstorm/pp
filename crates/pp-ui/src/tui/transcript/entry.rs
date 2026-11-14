use super::DisplayContext;

use super::{
    Activity, Color, Duration, EntryBody, EntryKind, EntryStatus, Instant, Line, Modifier, Span,
    Style, TranscriptEntry, Value, palette,
};

impl TranscriptEntry {
    pub(super) fn new(kind: EntryKind, title: String, text: String, tool: Option<String>) -> Self {
        Self {
            render: Default::default(),
            kind,
            body: EntryBody::Plain,
            markdown_dirty: matches!(kind, EntryKind::Assistant | EntryKind::Think),
            status: EntryStatus::Complete,
            title,
            text,
            refusal: String::new(),
            stdout: String::new(),
            stderr: String::new(),
            tool,
            cwd: None,
            layout: None,
            expanded: false,
            footer: String::new(),
            footer_warning: String::new(),
            pipes_badge: None,
            pipeline_preview: None,
            shell: None,
            started: Some(Instant::now()),
            bash_timeout_seconds: None,
            read_clock: palette::waiting_clock(0),
            arguments: Value::Null,
            result: None,
            diagnostics: Vec::new(),
            display: DisplayContext::default(),
            display_title: None,
            hidden: false,
        }
    }

    pub(super) fn visible_text(&self) -> &str {
        if self.text.trim().is_empty() && !self.refusal.is_empty() {
            &self.refusal
        } else {
            &self.text
        }
    }

    pub(super) fn finish_markdown(&mut self, status: EntryStatus) {
        self.status = status;
        self.markdown_dirty = true;
    }

    /// 同一路径只保留最新报告；内联事件、工具结果与后续同步不会叠成重复卡片。
    pub(super) fn set_diagnostic(&mut self, report: pp_core::lsp_report::DiagnosticReport) {
        if let Some(existing) = self
            .diagnostics
            .iter_mut()
            .find(|existing| existing.path == report.path)
        {
            *existing = report;
        } else {
            self.diagnostics.push(report);
        }
        self.layout = None;
    }
}

impl Activity {
    const SUMMARY_INTERVAL: Duration = Duration::from_secs(5);
    const SUMMARY_SETTLE_DELAY: Duration = Duration::from_millis(500);
    const SUMMARY_WIDTH: usize = 60;

    pub(super) fn summary(&self) -> Option<&TranscriptEntry> {
        (!self.running).then_some(&self.entry)
    }

    pub(super) fn summary_mut(&mut self) -> Option<&mut TranscriptEntry> {
        (!self.running).then_some(&mut self.entry)
    }

    pub(super) fn new(started: Instant) -> Self {
        Self {
            entry: TranscriptEntry::new(EntryKind::Activity, String::new(), String::new(), None),
            frame: 0,
            clock: palette::waiting_clock(0),
            started: Some(started),
            waiting_started: None,
            label: "Requesting model".into(),
            summary_label: None,
            pending_summary: None,
            summary_updated_at: None,
            summary_line: String::new(),
            summary_has_title: false,
            seconds: Some(0),
            run_seconds: 0,
            running: true,
        }
    }

    pub(super) fn tick(&mut self, now: Instant) -> bool {
        let Some(started) = self.started else {
            return false;
        };
        let summary_changed = self.publish_reasoning_summary(now);
        let elapsed = now.saturating_duration_since(started);
        let run_seconds = elapsed.as_secs();
        let frame = elapsed.as_millis() / 80;
        let clock = palette::waiting_clock(elapsed.as_millis());
        let seconds = now
            .saturating_duration_since(self.waiting_started.unwrap_or(started))
            .as_secs();
        if Some(seconds) == self.seconds
            && run_seconds == self.run_seconds
            && self.frame == frame
            && self.clock == clock
        {
            return summary_changed;
        }
        self.frame = frame;
        self.clock = clock;
        self.run_seconds = run_seconds;
        self.seconds = Some(seconds);
        true
    }

    pub(super) fn refresh_summary(&mut self) {
        self.entry.text = format!("✻ {} · {}", self.label, self.duration_text());
        self.entry.layout = None;
    }

    pub(super) fn live_line(&self) -> Line<'static> {
        let label = self.summary_label.as_deref().unwrap_or(&self.label);
        if self.waiting_started.is_some() {
            self.sweep_line(&format!("{label} · {}", self.duration_text()))
        } else {
            self.sweep_line(label)
        }
    }

    pub(super) fn push_reasoning_summary(&mut self, text: &str, now: Instant) {
        if !self.running || self.label == "Cancelling" {
            return;
        }
        for part in text.split_inclusive('\n') {
            // 只保留当前行的有界前缀；摘要正文仍由 Transcript 完整管理。
            for character in part.trim_end_matches('\n').chars() {
                if self.summary_line.len() + character.len_utf8() > 1024 {
                    break;
                }
                self.summary_line.push(character);
            }
            let line = pp_core::text::display_text(&self.summary_line).into_owned();
            let line = line.trim();
            let title = line
                .strip_prefix("**")
                .and_then(|text| text.strip_suffix("**"))
                .or_else(|| {
                    let text = line.trim_start_matches('#');
                    (line.len() - text.len() <= 6 && text.len() < line.len())
                        .then(|| text.strip_prefix(' '))
                        .flatten()
                })
                .map(str::trim)
                .filter(|text| !text.is_empty());
            if let Some(title) = title {
                let complete = part.ends_with('\n') || line.starts_with("**");
                self.queue_reasoning_summary(title, complete, now);
                self.summary_has_title = true;
            } else if !self.summary_has_title && !line.is_empty() && !line.starts_with(['*', '#']) {
                self.queue_reasoning_summary(line, part.ends_with('\n'), now);
            }
            if part.ends_with('\n') {
                self.summary_line.clear();
            }
        }
        self.publish_reasoning_summary(now);
    }

    fn queue_reasoning_summary(&mut self, label: &str, complete: bool, now: Instant) {
        let label = Self::summary_excerpt(label);
        if self.summary_label.as_deref() == Some(&label) {
            self.pending_summary = None;
            return;
        }
        let ready_at = if complete {
            now
        } else {
            now + Self::SUMMARY_SETTLE_DELAY
        };
        // 同一候选补齐换行时可提前就绪；正文继续流入不推迟已有标题。
        match &mut self.pending_summary {
            Some((pending, ready)) if *pending == label => *ready = (*ready).min(ready_at),
            _ => self.pending_summary = Some((label, ready_at)),
        }
    }

    fn summary_excerpt(label: &str) -> String {
        let line = Line::raw(label);
        if line.width() <= Self::SUMMARY_WIDTH {
            return label.to_owned();
        }
        let mut excerpt = String::new();
        let mut width = 0;
        // 按字素和显示列截断，为省略号留一列，保留组合字符与 Emoji。
        for grapheme in line.styled_graphemes(Style::default()) {
            width += Span::raw(grapheme.symbol).width();
            if width >= Self::SUMMARY_WIDTH {
                break;
            }
            excerpt.push_str(grapheme.symbol);
        }
        let mut excerpt = excerpt.trim_end().to_owned();
        excerpt.push('…');
        excerpt
    }

    fn publish_reasoning_summary(&mut self, now: Instant) -> bool {
        if !self.running || self.label == "Cancelling" {
            return false;
        }
        let Some((_, ready_at)) = &self.pending_summary else {
            return false;
        };
        if now < *ready_at
            || self.summary_updated_at.is_some_and(|updated| {
                now.saturating_duration_since(updated) < Self::SUMMARY_INTERVAL
            })
        {
            return false;
        }
        // 只发布最新候选，不轮播限流期间积压的中间摘要。
        self.summary_label = self.pending_summary.take().map(|(label, _)| label);
        self.summary_updated_at = Some(now);
        true
    }

    fn sweep_line(&self, label: &str) -> Line<'static> {
        let base = Style::default().fg(palette::DIM);
        let line = Line::raw(label);
        let graphemes: Vec<_> = line.styled_graphemes(base).collect();
        // 金色光带从首字移至末字，尾部留出短暂停顿后重新开始。
        let head = (self.frame % (graphemes.len() as u128 + 8)) as usize;
        let mut spans = vec![Span::styled(
            format!("{} ", self.clock),
            base.add_modifier(Modifier::BOLD),
        )];
        spans.extend(graphemes.into_iter().enumerate().map(|(index, grapheme)| {
            let color = match index.abs_diff(head) {
                0 | 1 => palette::ACCENT,
                2 => Color::Rgb(171, 135, 68),
                3 => Color::Rgb(119, 109, 87),
                _ => palette::DIM,
            };
            Span::styled(grapheme.symbol.to_owned(), base.fg(color))
        }));
        Line::from(spans)
    }

    pub(super) fn duration_text(&self) -> String {
        self.seconds
            .map(Self::format_duration)
            .unwrap_or_else(|| "—".into())
    }

    pub(super) fn format_duration(seconds: u64) -> String {
        match seconds {
            0..60 => format!("{seconds}s"),
            60..3600 => format!("{}m {}s", seconds / 60, seconds % 60),
            _ => format!(
                "{}h {}m {}s",
                seconds / 3600,
                seconds / 60 % 60,
                seconds % 60
            ),
        }
    }

    pub(super) fn phase(&mut self, label: &str) {
        if self.running && self.label != "Cancelling" && self.label != label {
            self.label = label.into();
            let now = Instant::now();
            // ask 单独计算等待时间；其他阶段继续使用整轮任务的起点。
            self.waiting_started =
                (label == "Waiting for input" && self.started.is_some()).then_some(now);
            self.tick(now);
        }
    }

    pub(super) fn stop(&mut self, label: &str) {
        if self.running {
            self.waiting_started = None;
            self.tick(Instant::now());
            self.started = None;
            self.running = false;
            self.label = label.into();
            self.refresh_summary();
        }
    }
}
