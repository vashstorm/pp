//! 主屏幕增量提交与备用屏幕共享唯一输出入口。

use crossterm::{
    SynchronizedUpdate,
    cursor::{Hide, MoveTo, Show},
    event::{DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture},
    execute, queue,
    style::{Attribute, ResetColor, SetAttribute},
    terminal::{
        self, Clear, ClearType, EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen,
    },
};
use ratatui::{
    backend::{Backend, CrosstermBackend},
    buffer::{Buffer, Cell, CellDiffOption},
    layout::{Position, Rect},
    text::Span,
};
use std::{
    io::{self, Write},
    sync::atomic::{AtomicBool, Ordering},
};

static ALTERNATE_ACTIVE: AtomicBool = AtomicBool::new(false);

pub(super) fn initialize() -> io::Result<()> {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        previous(info);
    }));
    terminal::enable_raw_mode()?;
    execute!(io::stdout(), EnableBracketedPaste, EnableMouseCapture)
}

pub(super) fn restore() {
    let mut stdout = io::stdout();
    if ALTERNATE_ACTIVE.swap(false, Ordering::SeqCst) {
        let _ = execute!(stdout, LeaveAlternateScreen);
    }
    let _ = execute!(
        stdout,
        EndSynchronizedUpdate,
        DisableMouseCapture,
        DisableBracketedPaste,
        ResetColor,
        SetAttribute(Attribute::Reset),
        Show
    );
    let _ = terminal::disable_raw_mode();
}

/// 宽度变化只裁切屏幕内的冻结行，不重发已进入原生滚动历史的行。
pub(super) fn copy_row(cells: &[Cell], target: &mut Buffer, x: u16, y: u16, width: u16) {
    let mut column = 0;
    while column < cells.len().min(usize::from(width)) {
        let cell = &cells[column];
        let span = Span::raw(cell.symbol()).width().max(1);
        if column + span > usize::from(width) {
            break;
        }
        target[(x + column as u16, y)] = cell.clone();
        for continuation in 1..span {
            if let Some(cell) = cells.get(column + continuation) {
                target[(x + (column + continuation) as u16, y)] = cell.clone();
            }
        }
        column += span;
    }
}

pub(super) struct Surface<W: Write> {
    writer: W,
    previous: Option<Buffer>,
    main: Option<Buffer>,
    alternate: bool,
    reset_pending: bool,
    started: bool,
}

impl<W: Write> Surface<W> {
    pub(super) fn new(writer: W) -> Self {
        Self {
            writer,
            previous: None,
            main: None,
            alternate: false,
            reset_pending: false,
            started: false,
        }
    }

    pub(super) fn reset_history(&mut self) {
        self.reset_pending = true;
    }

    /// 一批历史与对应尾帧在同一次短同步更新内提交。
    pub(super) fn paint(
        &mut self,
        buffer: &Buffer,
        cursor: Option<Position>,
        alternate: bool,
        history: &[Vec<Cell>],
    ) -> io::Result<()> {
        let reset = self.reset_pending;
        let switch = self.alternate != alternate;
        let previous = if reset || !history.is_empty() {
            None
        } else if switch && !alternate {
            self.main.as_ref()
        } else if switch {
            None
        } else {
            self.previous.as_ref()
        };
        let unknown = Buffer::empty(buffer.area);
        let previous = previous.filter(|old| old.area == buffer.area);
        let forced = previous.is_none().then(|| {
            let mut next = buffer.clone();
            for cell in &mut next.content {
                if cell.diff_option == CellDiffOption::None {
                    cell.set_diff_option(CellDiffOption::AlwaysUpdate);
                }
            }
            next
        });
        let previous = previous.unwrap_or(&unknown);
        let next = forced.as_ref().unwrap_or(buffer);
        let was_alternate = self.alternate;
        let started = self.started;
        self.writer.sync_update(|writer| -> io::Result<()> {
            queue!(writer, Hide)?;
            if !started {
                // 为主界面预留独立屏幕，将启动前的 shell 内容自然推入历史。
                writer.write_all("\r\n".repeat(usize::from(buffer.area.height)).as_bytes())?;
            }
            if was_alternate && (!alternate || reset) {
                queue!(writer, LeaveAlternateScreen)?;
            }
            if reset {
                queue!(
                    writer,
                    Clear(ClearType::Purge),
                    Clear(ClearType::All),
                    MoveTo(0, 0)
                )?;
            }
            if alternate && (!was_alternate || reset) {
                queue!(writer, EnterAlternateScreen, Clear(ClearType::All))?;
                ALTERNATE_ACTIVE.store(true, Ordering::SeqCst);
            }
            for cells in history {
                let area = Rect::new(0, 0, buffer.area.width, 1);
                let mut row = Buffer::empty(area);
                copy_row(cells, &mut row, 0, 0, area.width);
                queue!(
                    writer,
                    ResetColor,
                    SetAttribute(Attribute::Reset),
                    MoveTo(0, 0),
                    Clear(ClearType::CurrentLine)
                )?;
                super::scrollback::write_row(writer, &row, area)?;
                queue!(
                    writer,
                    ResetColor,
                    SetAttribute(Attribute::Reset),
                    MoveTo(0, buffer.area.height.saturating_sub(1))
                )?;
                writer.write_all(b"\r\n")?;
            }
            let mut backend = CrosstermBackend::new(writer);
            backend.draw(previous.diff(next).into_iter())?;
            if let Some(cursor) = cursor {
                backend.set_cursor_position(cursor)?;
                backend.show_cursor()?;
            } else {
                backend.hide_cursor()?;
            }
            Backend::flush(&mut backend)
        })??;
        self.previous = Some(buffer.clone());
        if reset
            || self
                .main
                .as_ref()
                .is_some_and(|main| main.area != buffer.area)
        {
            // 备用屏幕期间的清理或缩放也会改变主屏幕，不能复用旧差分基线。
            self.main = None;
        }
        if !alternate {
            self.main = Some(buffer.clone());
        }
        self.alternate = alternate;
        ALTERNATE_ACTIVE.store(alternate, Ordering::SeqCst);
        self.reset_pending = false;
        self.started = true;
        Ok(())
    }

    pub(super) fn finish(&mut self, height: u16) -> io::Result<()> {
        // 正常帧已留在主屏幕；只给 shell 腾出紧邻内容的一行。
        queue!(
            self.writer,
            ResetColor,
            SetAttribute(Attribute::Reset),
            MoveTo(0, height.saturating_sub(1)),
            Show
        )?;
        self.writer.write_all(b"\r\n")?;
        self.writer.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(text: &str) -> Buffer {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 24, 6));
        buffer.set_string(0, 0, text, ratatui::style::Style::default());
        buffer
    }

    #[test]
    fn native_surface_keeps_main_frame_across_overlays_and_exit_without_replay() {
        let mut surface = Surface::new(Vec::new());
        let main = frame("retained 中😀");
        surface
            .paint(&main, Some(Position::new(1, 4)), false, &[])
            .unwrap();
        let first = surface.writer.len();
        surface
            .paint(&frame("temporary panel"), None, true, &[])
            .unwrap();
        surface
            .paint(&main, Some(Position::new(1, 4)), false, &[])
            .unwrap();
        let restored = surface.writer.len();
        let mut parser = vt100::Parser::new(6, 24, 100);
        parser.process(&surface.writer);
        assert!(!parser.screen().alternate_screen());
        assert!(parser.screen().contents().contains("retained 中😀"));
        assert!(!parser.screen().contents().contains("temporary panel"));
        assert!(!String::from_utf8_lossy(&surface.writer[first..]).contains("retained"));
        surface
            .paint(&main, Some(Position::new(1, 4)), false, &[])
            .unwrap();
        surface.finish(6).unwrap();
        let exit = String::from_utf8_lossy(&surface.writer[restored..]);
        assert!(!exit.contains("retained"));
        assert!(!exit.contains("\x1b[2J") && !exit.contains("\x1b[3J"));
    }

    #[test]
    fn native_surface_only_purges_for_explicit_history_reset() {
        let mut surface = Surface::new(Vec::new());
        surface.paint(&frame("old"), None, false, &[]).unwrap();
        let before = surface.writer.len();
        surface.reset_history();
        surface.paint(&frame("new"), None, false, &[]).unwrap();
        let reset = String::from_utf8_lossy(&surface.writer[before..]);
        assert_eq!(reset.matches("\x1b[3J").count(), 1);
        assert_eq!(reset.matches("\x1b[2J").count(), 1);
        let before = surface.writer.len();
        surface.paint(&frame("new"), None, false, &[]).unwrap();
        assert!(!String::from_utf8_lossy(&surface.writer[before..]).contains("\x1b[3J"));
    }

    #[test]
    fn native_surface_invalidates_main_cache_when_reset_or_resized_under_overlay() {
        for reset in [false, true] {
            let mut surface = Surface::new(Vec::new());
            let mut main = frame("normal content");
            main.set_string(0, 5, "persistent footer", ratatui::style::Style::default());
            surface.paint(&main, None, false, &[]).unwrap();
            surface.paint(&frame("panel"), None, true, &[]).unwrap();
            if reset {
                surface.reset_history();
                surface.paint(&frame("new panel"), None, true, &[]).unwrap();
            } else {
                surface
                    .paint(&Buffer::empty(Rect::new(0, 0, 12, 4)), None, true, &[])
                    .unwrap();
                surface
                    .paint(&frame("panel restored"), None, true, &[])
                    .unwrap();
            }
            assert!(surface.main.is_none());
            surface.paint(&main, None, false, &[]).unwrap();
            let mut parser = vt100::Parser::new(6, 24, 100);
            parser.process(&surface.writer);
            assert!(parser.screen().contents().contains("normal content"));
            assert!(parser.screen().contents().contains("persistent footer"));
        }
    }

    struct FailingWriter;
    impl Write for FailingWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::other("测试写入失败"))
        }
    }

    #[test]
    fn native_surface_never_acknowledges_failed_output() {
        let mut surface = Surface::new(FailingWriter);
        assert!(surface.paint(&frame("pending"), None, false, &[]).is_err());
        assert!(surface.previous.is_none());
        assert!(surface.main.is_none());
        assert!(!surface.started);
    }
}
