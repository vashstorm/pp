use crate::mcp::{View, help};
use crossterm::event::{Event, KeyCode, KeyEventKind, MouseEventKind};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};

const GOLD: Color = Color::Rgb(255, 190, 55);
const MUTED: Color = Color::Rgb(125, 130, 145);
const BORDER: Color = Color::Rgb(0, 175, 215);

pub(super) struct Panel {
    lines: Vec<Line<'static>>,
    title: &'static str,
    scroll: usize,
    visible: usize,
}
impl Panel {
    pub fn list(view: &View) -> Self {
        let mut lines = Vec::new();
        for source in ["pp", "Claude Code", "Codex", "Disabled"] {
            let mut path = None;
            let mut entries: Vec<_> = view.entries.iter().collect();
            entries.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.name.cmp(&b.name)));
            for entry in entries {
                if (source == "Disabled") != (entry.status == "disabled") {
                    continue;
                }
                if source != "Disabled" && source != entry.source {
                    continue;
                }
                if path.is_none() || source != "Disabled" && path.as_ref() != Some(&entry.path) {
                    lines.push(Line::default());
                    lines.push(Line::from(vec![
                        Span::styled(format!(" {source}"), Style::default().fg(GOLD)),
                        Span::styled(
                            if source == "Disabled" {
                                " (configured servers):".into()
                            } else {
                                format!(" ({}):", entry.path.display())
                            },
                            Style::default().fg(MUTED),
                        ),
                    ]));
                    path = Some(entry.path.clone());
                }
                let (icon, color) = match entry.status {
                    "connected" => ("●", Color::Green),
                    "disabled" => ("◌", Color::Yellow),
                    status if status.starts_with("failed") || status.starts_with("unsupported") => {
                        ("○", Color::Red)
                    }
                    _ => ("○", MUTED),
                };
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("   {} ", super::display_text(&entry.name)),
                        Style::default().fg(GOLD),
                    ),
                    Span::styled(
                        format!("{icon} {}", entry.status),
                        Style::default().fg(color),
                    ),
                ]));
            }
        }
        if lines.is_empty() {
            lines.push(Line::from(" 未配置 MCP 服务；请编辑 ~/.pp/agent/mcp.json"));
        }
        for diagnostic in &view.diagnostics {
            lines.push(Line::styled(
                diagnostic.clone(),
                Style::default().fg(Color::Red),
            ));
        }
        Self {
            lines,
            title: " Configured MCP Servers ",
            scroll: 0,
            visible: 0,
        }
    }
    pub fn help() -> Self {
        Self {
            lines: help()
                .lines()
                .map(|line| Line::from(format!(" {line}")))
                .collect(),
            title: " MCP Server Management ",
            scroll: 0,
            visible: 0,
        }
    }
    pub fn handle(&mut self, event: &Event) -> bool {
        match event {
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                match key.code {
                    KeyCode::Esc => return true,
                    KeyCode::Up => self.scroll = self.scroll.saturating_sub(1),
                    KeyCode::Down => self.scroll = self.scroll.saturating_add(1),
                    KeyCode::PageUp => {
                        self.scroll = self.scroll.saturating_sub(self.visible.max(1))
                    }
                    KeyCode::PageDown => {
                        self.scroll = self.scroll.saturating_add(self.visible.max(1))
                    }
                    KeyCode::Home => self.scroll = 0,
                    KeyCode::End => self.scroll = self.lines.len(),
                    _ => {}
                }
            }
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollUp => self.scroll = self.scroll.saturating_sub(3),
                MouseEventKind::ScrollDown => self.scroll = self.scroll.saturating_add(3),
                _ => {}
            },
            _ => {}
        }
        self.scroll = self
            .scroll
            .min(self.lines.len().saturating_sub(self.visible));
        false
    }
    pub fn draw(&mut self, frame: &mut Frame, area: Rect) {
        frame.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(BORDER))
            .title(Line::styled(self.title, Style::default().fg(GOLD)));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        self.visible = usize::from(inner.height.saturating_sub(2));
        self.scroll = self
            .scroll
            .min(self.lines.len().saturating_sub(self.visible));
        let body = Rect::new(inner.x, inner.y, inner.width, self.visible as u16);
        frame.render_widget(
            Paragraph::new(
                self.lines
                    .iter()
                    .skip(self.scroll)
                    .take(self.visible)
                    .cloned()
                    .collect::<Vec<_>>(),
            ),
            body,
        );
        if inner.height > 0 {
            frame.render_widget(
                Line::styled(
                    " Esc to close · ↑↓ / PgUp/PgDn scroll",
                    Style::default().fg(MUTED),
                ),
                Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mcp_panel_groups_status_colors_scrolls_and_closes() {
        let view = View {
            entries: (0..24)
                .map(|i| crate::mcp::EntryView {
                    name: format!("server{i:02}"),
                    source: "Claude Code".into(),
                    path: "/fixture/.claude.json".into(),
                    status: if i == 23 { "disabled" } else { "connected" },
                })
                .collect(),
            diagnostics: vec![],
        };
        let mut panel = Panel::list(&view);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(90, 12)).unwrap();
        terminal
            .draw(|frame| panel.draw(frame, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert!(buffer.content.iter().any(|c| c.fg == BORDER));
        assert!(buffer.content.iter().any(|c| c.fg == Color::Green));
        let end = Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::End,
            crossterm::event::KeyModifiers::NONE,
        ));
        assert!(!panel.handle(&end));
        assert!(panel.scroll > 0);
        terminal
            .draw(|frame| panel.draw(frame, frame.area()))
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("Disabled"));
        assert!(text.contains("disabled"));
        assert!(panel.handle(&Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Esc,
            crossterm::event::KeyModifiers::NONE
        ))));
    }
}
