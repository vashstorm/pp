//! `/pipes` 面板绘制：run／流列表、分隔行与有界正文。

use crate::tui::transcript::wrap_block;
use crate::tui::transcript_style::ACCENT;
use crate::tui::transcript_style::ACTIVITY;
use crate::tui::transcript_style::ADDED;
use crate::tui::transcript_style::ASK_BG;
use crate::tui::transcript_style::MUTED;
use crate::tui::transcript_style::REMOVED;
use pp_core::text::display_text;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Clear,
};

use super::{Panel, RunView, StreamView};

impl Panel {
    pub(in crate::tui) fn draw(&mut self, frame: &mut Frame, mut area: Rect) {
        frame.render_widget(Clear, area);
        if area.is_empty() {
            return;
        }
        draw_line(
            frame,
            &mut area,
            Line::from(vec![
                Span::styled("Pipe streams", Style::default().fg(ACCENT)),
                Span::styled(
                    "  temporary stage streams · memory only",
                    Style::default().fg(MUTED),
                ),
            ]),
        );
        if let Some(error) = &self.error {
            draw_line(
                frame,
                &mut area,
                Line::styled(error.as_str(), Style::default().fg(REMOVED)),
            );
        }
        if self.loading {
            draw_line(
                frame,
                &mut area,
                Line::styled("Loading pipe streams…", Style::default().fg(ACCENT)),
            );
            return;
        }
        if self.runs.is_empty() || self.unavailable {
            let message = self.explicit_run.as_deref().map_or_else(
                || "No pipe runs in this session.".to_owned(),
                |run| {
                    format!(
                        "Pipe run {} is unavailable in this session.",
                        display_text(run)
                    )
                },
            );
            draw_line(
                frame,
                &mut area,
                Line::styled(message, Style::default().fg(MUTED)),
            );
            return;
        }
        self.draw_run(frame, &mut area);
    }

    fn draw_run(&mut self, frame: &mut Frame, area: &mut Rect) {
        let (summary, pipelines, stream_rows, list_height) = {
            let run = &self.runs[self.selected_run];
            let state = run_state(run);
            draw_line(
                frame,
                area,
                Line::from(vec![
                    Span::styled(
                        format!("run {}/{} ", self.selected_run + 1, self.runs.len()),
                        Style::default().fg(MUTED),
                    ),
                    Span::styled(
                        display_text(&run.run_id).into_owned(),
                        Style::default().fg(ACTIVITY),
                    ),
                    Span::styled(
                        format!("  {}", display_text(&run.owner)),
                        Style::default().fg(MUTED),
                    ),
                    Span::styled(
                        format!("  {state}"),
                        Style::default().fg(if run.exit_code.is_some_and(|code| code != 0) {
                            ACCENT
                        } else {
                            MUTED
                        }),
                    ),
                ]),
            );
            let mut spans = vec![Span::styled(
                format!(
                    "pipelines {} · streams {} · nonzero {}",
                    run.pipelines.len(),
                    run.streams.len(),
                    run.nonzero_stages
                ),
                Style::default().fg(if run.nonzero_stages > 0 {
                    ACCENT
                } else {
                    MUTED
                }),
            )];
            if run.incomplete {
                spans.push(Span::styled(" · incomplete", Style::default().fg(ACCENT)));
            }
            if run.omitted_pipelines > 0 || run.omitted_stages > 0 {
                spans.push(Span::styled(
                    format!(
                        " · omitted {}/{}",
                        run.omitted_pipelines, run.omitted_stages
                    ),
                    Style::default().fg(MUTED),
                ));
            }
            if run.untapped_edges > 0 {
                spans.push(Span::styled(
                    format!(" · untapped {}", run.untapped_edges),
                    Style::default().fg(ACCENT),
                ));
            }
            if run.evicted_streams > 0 {
                spans.push(Span::styled(
                    format!(" · evicted {}", run.evicted_streams),
                    Style::default().fg(ACCENT),
                ));
            }
            let stream_rows: Vec<Line<'static>> = run
                .streams
                .iter()
                .enumerate()
                .map(|(index, stream)| stream_row(stream, index == self.selected_stream))
                .collect();
            let list_height = run.streams.len().min(5).min(usize::from(area.height / 3));
            (
                Line::from(spans),
                pipeline_rows(run),
                stream_rows,
                list_height,
            )
        };
        draw_line(frame, area, summary);
        for line in pipelines {
            draw_line(frame, area, line);
        }
        if let Some(error) = self
            .runs
            .get(self.selected_run)
            .and_then(|run| run.capture_error.clone())
        {
            draw_line(
                frame,
                area,
                Line::styled(
                    format!("capture error: {}", display_text(&error)),
                    Style::default().fg(REMOVED),
                ),
            );
        }
        if stream_rows.is_empty() {
            draw_line(
                frame,
                area,
                Line::styled("No pipe streams in this run.", Style::default().fg(MUTED)),
            );
        } else if list_height > 0 {
            let start = self
                .selected_stream
                .saturating_add(1)
                .saturating_sub(list_height);
            for index in start..start + list_height {
                let Some(row) = stream_rows.get(index) else {
                    break;
                };
                let selected = index == self.selected_stream;
                let style = if selected {
                    Style::default().bg(ASK_BG).add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                let target = take_line(area);
                frame.render_widget(
                    Line::styled((if selected { "›" } else { " " }).to_string(), style),
                    target,
                );
                frame.render_widget(
                    row,
                    Rect::new(
                        target.x.saturating_add(2).min(target.right()),
                        target.y,
                        target.width.saturating_sub(2),
                        target.height,
                    ),
                );
            }
        }
        self.draw_divider(frame, area);
        self.draw_body(frame, *area);
    }

    fn draw_divider(&mut self, frame: &mut Frame, area: &mut Rect) {
        let (retained_start, retained_end, total_bytes, open) = if self.following {
            (
                self.live.retained_start,
                self.live.retained_end,
                self.live.total_bytes,
                self.live.open,
            )
        } else {
            (
                self.body.retained_start,
                self.body.retained_end,
                self.body.total_bytes,
                self.body.open,
            )
        };
        let mut spans = vec![Span::styled(
            format!(
                "output · kept {retained_start}..{retained_end} / {total_bytes} bytes · {}",
                if self.following { "follow" } else { "frozen" }
            ),
            Style::default().fg(if self.following { ADDED } else { MUTED }),
        )];
        if !open {
            spans.push(Span::styled(" · closed", Style::default().fg(MUTED)));
        }
        if self.body.trimmed {
            spans.push(Span::styled(
                " · earlier display trimmed",
                Style::default().fg(MUTED),
            ));
        }
        if retained_start > 0 {
            spans.push(Span::styled(
                " · earlier output evicted",
                Style::default().fg(ACCENT),
            ));
        }
        if !self.following {
            if self.new_bytes > 0 {
                spans.push(Span::styled(
                    format!(" · new +{} bytes", self.new_bytes),
                    Style::default().fg(ACCENT),
                ));
            }
            if self.evicted {
                spans.push(Span::styled(
                    " · cached bytes overwritten",
                    Style::default().fg(ACCENT),
                ));
            }
        }
        draw_line(frame, area, Line::from(spans));
    }

    fn draw_body(&mut self, frame: &mut Frame, mut area: Rect) {
        self.body_height = usize::from(area.height).max(1);
        if area.is_empty() {
            return;
        }
        if self.body.text.is_empty() {
            let message = if self.awaiting {
                "Loading stream output…"
            } else if self.live.open || self.body.open {
                "Waiting for output…"
            } else {
                "No output captured."
            };
            draw_line(
                frame,
                &mut area,
                Line::styled(message, Style::default().fg(MUTED)),
            );
            return;
        }
        let height = usize::from(area.height);
        let tail = self.tail_start(area.width, height);
        let start = if self.following {
            tail
        } else {
            self.scroll.min(tail)
        };
        self.scroll = start;
        let mut index = start;
        let mut rows = 0;
        while index < self.line_starts.len() && rows < height {
            let text = self.line_at(index).to_owned();
            for row in wrap_block(Line::raw(text), area.width, "") {
                if rows >= height {
                    break;
                }
                frame.render_widget(row, take_line(&mut area));
                rows += 1;
            }
            index += 1;
        }
    }

    fn tail_start(&self, width: u16, height: usize) -> usize {
        if height == 0 || self.line_starts.is_empty() {
            return 0;
        }
        let mut rows = 0;
        for index in (0..self.line_starts.len()).rev() {
            rows += wrap_block(Line::raw(self.line_at(index).to_owned()), width, "")
                .len()
                .max(1);
            if rows >= height {
                return index;
            }
        }
        0
    }

    fn line_at(&self, index: usize) -> &str {
        let start = self.line_starts[index];
        let end = self
            .line_starts
            .get(index + 1)
            .map_or(self.body.text.len(), |next| next.saturating_sub(1));
        self.body.text.get(start..end).unwrap_or("")
    }
}

fn run_state(run: &RunView) -> String {
    match run.state.as_str() {
        "running" => "running".to_owned(),
        "interrupted" => "interrupted".to_owned(),
        _ => run
            .exit_code
            .map_or_else(|| "exited".to_owned(), |code| format!("exited {code}")),
    }
}

fn pipeline_rows(run: &RunView) -> Vec<Line<'static>> {
    let mut rows = Vec::new();
    for pipeline in run.pipelines.iter().take(3) {
        let mut spans = vec![Span::styled(
            match pipeline.parent {
                Some(parent) => format!("p{}←{} {}s ", pipeline.id, parent, pipeline.stage_count),
                None => format!("p{} {}s ", pipeline.id, pipeline.stage_count),
            },
            Style::default().fg(MUTED),
        )];
        for (index, stage) in pipeline.stages.iter().take(4).enumerate() {
            if index > 0 {
                spans.push(Span::styled(" → ", Style::default().fg(MUTED)));
            }
            let detail = match (stage.status, stage.signal) {
                (_, Some(signal)) => format!("signal {signal}"),
                (Some(status), None) => status.to_string(),
                (None, None) => stage.state.clone(),
            };
            let nonzero = stage.status.is_some_and(|status| status != 0) || stage.signal.is_some();
            spans.push(Span::styled(
                format!("{}={detail}", display_text(&stage.command)),
                Style::default().fg(if nonzero { ACCENT } else { ACTIVITY }),
            ));
        }
        if pipeline.stages.len() > 4 {
            spans.push(Span::styled(
                format!(" +{}", pipeline.stages.len() - 4),
                Style::default().fg(MUTED),
            ));
        }
        let status = pipeline
            .status
            .map_or_else(|| "?".to_owned(), |status| status.to_string());
        spans.push(Span::styled(
            format!(
                "  status {status}{}{}",
                if pipeline.pipefail { " pipefail" } else { "" },
                if pipeline.bang { " !" } else { "" }
            ),
            Style::default().fg(if pipeline.status.is_some_and(|status| status != 0) {
                ACCENT
            } else {
                MUTED
            }),
        ));
        rows.push(Line::from(spans));
    }
    if run.pipelines.len() > 3 {
        rows.push(Line::styled(
            format!("+{} more pipelines", run.pipelines.len() - 3),
            Style::default().fg(MUTED),
        ));
    }
    rows
}

fn stream_row(stream: &StreamView, selected: bool) -> Line<'static> {
    let mut spans = vec![Span::styled(
        format!(
            "s{} p{} {}→{} ",
            stream.id, stream.pipeline, stream.producer_stage, stream.consumer_stage
        ),
        Style::default().fg(MUTED),
    )];
    spans.push(Span::styled(
        format!(
            "{} → {}",
            display_text(&stream.producer),
            display_text(&stream.consumer)
        ),
        Style::default().fg(if selected { ACTIVITY } else { MUTED }),
    ));
    spans.push(Span::styled(
        if stream.open { "  open" } else { "  closed" },
        Style::default().fg(if stream.open { ADDED } else { MUTED }),
    ));
    spans.push(Span::styled(
        format!(
            "  kept {}..{} / {}  rev {}",
            stream.retained_start, stream.retained_end, stream.total_bytes, stream.revision
        ),
        Style::default().fg(MUTED),
    ));
    if let Some(reason) = &stream.close_reason {
        spans.push(Span::styled(
            format!("  {}", display_text(reason)),
            Style::default().fg(MUTED),
        ));
    }
    Line::from(spans)
}

fn take_line(area: &mut Rect) -> Rect {
    let row = Rect::new(area.x, area.y, area.width, area.height.min(1));
    area.y = area.y.saturating_add(row.height);
    area.height = area.height.saturating_sub(row.height);
    row
}

fn draw_line(frame: &mut Frame, area: &mut Rect, line: Line<'_>) {
    if !area.is_empty() {
        frame.render_widget(line, take_line(area));
    }
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::*;

    #[test]
    fn draws_runs_pipelines_streams_and_body() {
        let mut panel = panel();
        let (_, text) = draw(&mut panel, 120, 30);
        assert!(text.contains("Pipe streams"));
        assert!(text.contains(&"a".repeat(32)));
        assert!(text.contains("printf=0"));
        assert!(text.contains("tail=0"));
        assert!(text.contains("s1 p1 1→2"));
        assert!(text.contains("open"));
        assert!(text.contains("line-20"), "{text}");
        assert!(panel.following());
    }
}
