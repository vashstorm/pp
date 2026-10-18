use super::statusline;
use crossterm::{
    clipboard::CopyToClipboard,
    event::{MouseButton, MouseEvent, MouseEventKind},
    execute,
};
use ratatui::{
    Frame,
    layout::{Alignment, Position, Rect},
    style::{Color, Style},
    text::{Line, Text},
    widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph, Wrap},
};
use std::{
    io,
    time::{Duration, Instant},
};

const LIFETIME: Duration = Duration::from_secs(10);
const FADE_IN_MS: u128 = 200;
const FADE_OUT_MS: u128 = 400;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Level {
    Normal,
    Important,
    Error,
}

struct Notification {
    level: Level,
    message: String,
    created_at: Instant,
    countdown_started_at: Option<Instant>,
}

pub(super) enum MouseAction {
    Ignored,
    Consumed,
    Copy,
}

#[derive(Default)]
pub(super) struct Notifications {
    current: Option<Notification>,
    step: u8,
    area: Option<Rect>,
    pointer: Option<Position>,
    hovered: bool,
    copy_status: Option<bool>,
}

impl Notifications {
    pub(super) fn clear(&mut self) {
        *self = Self {
            pointer: self.pointer,
            ..Self::default()
        };
    }

    pub(super) fn show(&mut self, level: Level, message: String, now: Instant) {
        if message.trim().is_empty() {
            return;
        }
        self.current = Some(Notification {
            level,
            message,
            created_at: now,
            countdown_started_at: Some(now),
        });
        self.step = 1;
        self.hovered = false;
        self.copy_status = None;
    }

    pub(super) fn tick(&mut self, now: Instant) -> bool {
        let Some(notification) = &self.current else {
            return false;
        };
        let elapsed = notification
            .countdown_started_at
            .map(|started| now.saturating_duration_since(started).as_millis());
        if elapsed.is_some_and(|elapsed| elapsed >= LIFETIME.as_millis()) {
            self.current = None;
            self.area = None;
            self.hovered = false;
            return true;
        }
        let fade_in = now
            .saturating_duration_since(notification.created_at)
            .as_millis();
        let step = if self.hovered {
            20
        } else if fade_in < FADE_IN_MS {
            (fade_in * 20 / FADE_IN_MS).max(1)
        } else if let Some(elapsed) = elapsed
            && elapsed >= LIFETIME.as_millis() - FADE_OUT_MS
        {
            ((LIFETIME.as_millis() - elapsed) * 20 / FADE_OUT_MS).max(1)
        } else {
            20
        } as u8;
        let changed = step != self.step;
        self.step = step;
        changed
    }

    fn update_hover(&mut self, now: Instant) -> bool {
        let hovered = self.current.is_some()
            && self
                .area
                .zip(self.pointer)
                .is_some_and(|(area, pointer)| area.contains(pointer));
        if hovered == self.hovered {
            return false;
        }
        self.hovered = hovered;
        if let Some(notification) = &mut self.current {
            notification.countdown_started_at = if hovered { None } else { Some(now) };
        }
        self.step = 20;
        true
    }

    pub(super) fn mouse(&mut self, mouse: MouseEvent, now: Instant) -> (bool, MouseAction) {
        let pointer = Position::new(mouse.column, mouse.row);
        self.pointer = Some(pointer);
        let changed = self.update_hover(now);
        if !self.hovered {
            return (changed, MouseAction::Ignored);
        }
        let copy = self
            .area
            .is_some_and(|area| area.width >= 24 && self.copy_area(area).contains(pointer))
            && mouse.kind == MouseEventKind::Down(MouseButton::Left);
        (
            changed,
            if copy {
                MouseAction::Copy
            } else {
                MouseAction::Consumed
            },
        )
    }

    fn copy_label(&self) -> &'static str {
        match self.copy_status {
            None => "[Copy]",
            Some(true) => "[Sent]",
            Some(false) => "[Failed]",
        }
    }

    fn copy_area(&self, area: Rect) -> Rect {
        let width = (Line::raw(self.copy_label()).width() as u16).min(area.width.saturating_sub(2));
        Rect::new(area.right().saturating_sub(width + 1), area.y, width, 1)
    }

    pub(super) fn copy_to_clipboard(&mut self, writer: &mut impl io::Write) {
        if let Some(notification) = &self.current {
            self.copy_status = Some(
                execute!(
                    writer,
                    CopyToClipboard::to_clipboard_from(notification.message.as_bytes())
                )
                .is_ok(),
            );
        }
    }

    pub(super) fn draw(&mut self, frame: &mut Frame, above_input: Rect) {
        self.area = None;
        let Some(notification) = &self.current else {
            return;
        };
        let width = u16::try_from(
            Text::raw(notification.message.as_str())
                .width()
                .saturating_add(4)
                .clamp(24, 60),
        )
        .unwrap_or(60)
        .min(above_input.width.saturating_sub(2));
        if width < 6 || above_input.height < 3 {
            self.update_hover(Instant::now());
            return;
        }
        let paragraph = Paragraph::new(notification.message.as_str()).wrap(Wrap { trim: false });
        let height = u16::try_from(paragraph.line_count(width - 4).saturating_add(2))
            .unwrap_or(u16::MAX)
            .min(8)
            .min(above_input.height);
        let area = Rect::new(
            above_input.right().saturating_sub(width + 1),
            above_input.bottom().saturating_sub(height),
            width,
            height,
        );
        self.area = Some(area);
        self.update_hover(Instant::now());
        let notification = self
            .current
            .as_ref()
            .expect("notification is still present");
        let (color, title) = match notification.level {
            Level::Normal => (statusline::ACCENT, " Note "),
            Level::Important => (Color::Rgb(229, 192, 92), " Important "),
            Level::Error => (Color::Rgb(224, 108, 117), " Error "),
        };
        let paragraph = Paragraph::new(notification.message.as_str())
            .style(Style::default().fg(dim(Color::Rgb(220, 220, 220), self.step)))
            .wrap(Wrap { trim: false });
        let border = Style::default().fg(dim(color, self.step));
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(border)
            .title_style(border)
            .title(title)
            .padding(Padding::horizontal(1));
        frame.render_widget(Clear, area);
        frame.render_widget(paragraph.block(block), area);
        let copy_area = self.copy_area(area);
        if area.width >= 24 {
            frame.render_widget(
                Line::raw(self.copy_label())
                    .style(border)
                    .alignment(Alignment::Right),
                copy_area,
            );
        }
    }
}

fn dim(color: Color, step: u8) -> Color {
    let Color::Rgb(red, green, blue) = color else {
        unreachable!("notification colors are RGB");
    };
    let channel = |value| (64 + (i32::from(value) - 64) * i32::from(step) / 20) as u8;
    Color::Rgb(channel(red), channel(green), channel(blue))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    use ratatui::{Terminal, backend::TestBackend, buffer::Buffer};
    use std::time::Duration;

    fn render(notifications: &mut Notifications, width: u16, height: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                frame.render_widget(
                    Paragraph::new(
                        format!("{}\n", "background".repeat(12)).repeat(usize::from(height)),
                    ),
                    frame.area(),
                );
                let above = Rect::new(0, 0, width, height.saturating_sub(3));
                notifications.draw(frame, above);
                frame.render_widget(
                    Paragraph::new("❯ 草稿🙂"),
                    Rect::new(0, above.bottom(), width, height.min(3)),
                );
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn border(buffer: &Buffer) -> Option<(u16, u16, Color)> {
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                let cell = &buffer[(x, y)];
                if cell.symbol() == "╭" {
                    return Some((x, y, cell.fg));
                }
            }
        }
        None
    }

    fn mouse(x: u16, y: u16, kind: MouseEventKind) -> MouseEvent {
        MouseEvent {
            column: x,
            row: y,
            kind,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn hover_holds_and_each_leave_restarts_ten_seconds() {
        let now = Instant::now();
        let mut notifications = Notifications::default();
        notifications.show(Level::Error, "保持显示".into(), now);
        render(&mut notifications, 100, 30);
        let area = notifications.area.unwrap();
        let inside = mouse(area.x, area.y, MouseEventKind::Moved);
        let outside = mouse(area.right(), area.y, MouseEventKind::Moved);
        assert!(notifications.mouse(inside, now).0);
        assert!(!notifications.tick(now + Duration::from_secs(60)));
        assert!(notifications.current.is_some());
        let leave = now + Duration::from_secs(61);
        assert!(notifications.mouse(outside, leave).0);
        assert!(
            !notifications
                .mouse(outside, leave + Duration::from_secs(8))
                .0
        );
        notifications.tick(leave + Duration::from_millis(9800));
        assert_eq!(notifications.step, 10);
        assert!(
            notifications
                .mouse(inside, leave + Duration::from_millis(9900))
                .0
        );
        assert_eq!(notifications.step, 20);
        assert!(!notifications.tick(leave + Duration::from_secs(120)));
        let leave = leave + Duration::from_secs(121);
        notifications.mouse(outside, leave);
        notifications.tick(leave + Duration::from_millis(9999));
        assert!(notifications.current.is_some());
        assert!(notifications.tick(leave + LIFETIME));
        assert!(notifications.current.is_none());
    }

    #[test]
    fn stationary_pointer_holds_replacement_and_hidden_overlay_releases_hover() {
        let now = Instant::now();
        let mut notifications = Notifications::default();
        notifications.mouse(mouse(95, 25, MouseEventKind::Moved), now);
        notifications.show(Level::Normal, "first".into(), now);
        render(&mut notifications, 100, 30);
        assert!(notifications.hovered);
        assert!(!notifications.tick(now + Duration::from_secs(60)));
        notifications.show(Level::Error, "second".into(), now);
        render(&mut notifications, 100, 30);
        assert!(notifications.hovered);
        assert!(!notifications.tick(now + Duration::from_secs(120)));
        notifications.clear();
        notifications.show(Level::Normal, "after reset".into(), now);
        render(&mut notifications, 100, 30);
        assert!(notifications.hovered);
        render(&mut notifications, 1, 1);
        assert!(!notifications.hovered);
        assert!(notifications.area.is_none());
        assert!(notifications.tick(Instant::now() + LIFETIME));
    }

    #[test]
    fn copy_button_sends_full_unicode_message_including_truncated_content() {
        let now = Instant::now();
        let mut notifications = Notifications::default();
        notifications.show(
            Level::Important,
            format!("{}\n中文🙂", "abc".repeat(100)),
            now,
        );
        let buffer = render(&mut notifications, 100, 30);
        assert!(!buffer.content.iter().any(|cell| cell.symbol() == "中"));
        let area = notifications.area.unwrap();
        let (_, action) = notifications.mouse(
            mouse(
                area.x + 1,
                area.y + 1,
                MouseEventKind::Down(MouseButton::Left),
            ),
            now,
        );
        assert!(matches!(action, MouseAction::Consumed));
        let copy = notifications.copy_area(area);
        let (_, action) = notifications.mouse(
            mouse(copy.x, copy.y, MouseEventKind::Down(MouseButton::Left)),
            now,
        );
        assert!(matches!(action, MouseAction::Copy));
        let mut output = Vec::new();
        notifications.copy_to_clipboard(&mut output);
        assert_eq!(
            String::from_utf8(output).unwrap(),
            format!("\x1b]52;c;{}CuS4reaWh/CfmYI=\x1b\\", "YWJj".repeat(100))
        );
        assert_eq!(notifications.copy_status, Some(true));
        // 写入错误保留原消息，并允许再次点击重试。
        struct Broken;
        impl io::Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        notifications.copy_to_clipboard(&mut Broken);
        assert_eq!(notifications.copy_status, Some(false));
        notifications.copy_to_clipboard(&mut io::sink());
        assert_eq!(notifications.copy_status, Some(true));
        render(&mut notifications, 20, 30);
        let area = notifications.area.unwrap();
        let (_, action) = notifications.mouse(
            mouse(
                area.right() - 2,
                area.y,
                MouseEventKind::Down(MouseButton::Left),
            ),
            now,
        );
        assert!(matches!(action, MouseAction::Consumed));
    }

    #[test]
    fn fades_and_requests_final_redraw_without_input() {
        let now = Instant::now();
        let mut notifications = Notifications::default();
        let background = render(&mut notifications, 100, 30);
        notifications.show(Level::Error, "invalid command".into(), now);
        for (ms, step, changed) in [
            (0, 1, false),
            (100, 10, true),
            (200, 20, true),
            (9600, 20, false),
            (9800, 10, true),
        ] {
            assert_eq!(notifications.tick(now + Duration::from_millis(ms)), changed);
            let buffer = render(&mut notifications, 100, 30);
            assert_eq!(
                border(&buffer).unwrap().2,
                dim(Color::Rgb(224, 108, 117), step)
            );
            assert_ne!(buffer, background);
        }
        assert!(notifications.tick(now + Duration::from_millis(10000)));
        assert_eq!(render(&mut notifications, 100, 30), background);
        assert!(!notifications.tick(now + Duration::from_secs(11)));
    }

    #[test]
    fn severity_wrapping_and_geometry_survive_resize() {
        let now = Instant::now();
        for (level, color) in [
            (Level::Normal, statusline::ACCENT),
            (Level::Important, Color::Rgb(229, 192, 92)),
            (Level::Error, Color::Rgb(224, 108, 117)),
        ] {
            let mut notifications = Notifications::default();
            notifications.show(level, format!("中文🙂\n{}", "abcdefghij".repeat(20)), now);
            notifications.tick(now + Duration::from_millis(200));
            let full = render(&mut notifications, 100, 30);
            let (x, y, fg) = border(&full).unwrap();
            assert_eq!((x, y, fg), (39, 20, color));
            assert_eq!(full[(98, 26)].symbol(), "╯");
            assert_eq!(full[(41, 21)].symbol(), "中");
            assert_eq!(full[(45, 21)].symbol(), "🙂");
            assert_eq!(full[(41, 23)].symbol(), "g");
            for (width, height) in [(20, 4), (1, 1), (100, 30)] {
                let buffer = render(&mut notifications, width, height);
                if height < 6 {
                    assert!(border(&buffer).is_none());
                } else {
                    assert_eq!(buffer[(98, 26)].symbol(), "╯");
                }
                assert_eq!(buffer[(0, height.saturating_sub(3))].symbol(), "❯");
            }
            notifications.tick(now + Duration::from_millis(10000));
            assert!(border(&render(&mut notifications, 100, 30)).is_none());
        }
    }

    #[test]
    fn bounded_overlay_respects_offset_and_small_available_rectangles() {
        let now = Instant::now();
        let mut notifications = Notifications::default();
        notifications.show(Level::Error, "long unbroken text".repeat(100), now);
        notifications.tick(now + Duration::from_millis(200));
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        for above in [Rect::new(4, 3, 30, 10), Rect::new(4, 3, 8, 3)] {
            terminal
                .draw(|frame| notifications.draw(frame, above))
                .unwrap();
            let buffer = terminal.backend().buffer();
            let (left, top, _) = border(buffer).unwrap();
            assert_eq!(left, above.x + 1);
            assert_eq!(top, above.bottom() - above.height.min(8));
            assert_eq!(
                buffer[(above.right() - 2, above.bottom() - 1)].symbol(),
                "╯"
            );
            assert!(buffer.content.iter().enumerate().all(|(index, cell)| {
                cell.symbol() == " "
                    || above.contains((index as u16 % 100, index as u16 / 100).into())
            }));
        }
    }

    #[test]
    fn replacement_restarts_lifetime_and_blank_is_ignored() {
        let now = Instant::now();
        let mut notifications = Notifications::default();
        notifications.show(Level::Normal, "first".into(), now);
        notifications.tick(now + Duration::from_millis(3000));
        let replacement = now + Duration::from_millis(3100);
        notifications.show(Level::Error, "second".into(), replacement);
        notifications.show(Level::Important, " \n\t".into(), replacement);
        assert!(!notifications.tick(now));
        assert!(notifications.tick(replacement + Duration::from_millis(200)));
        let buffer = render(&mut notifications, 100, 30);
        assert_eq!(border(&buffer).unwrap().2, Color::Rgb(224, 108, 117));
        assert_eq!(buffer[(77, 25)].symbol(), "s");
        assert!(!notifications.tick(replacement + Duration::from_millis(9600)));
        assert!(notifications.tick(replacement + Duration::from_millis(10000)));
        assert!(border(&render(&mut notifications, 100, 30)).is_none());
    }
}
