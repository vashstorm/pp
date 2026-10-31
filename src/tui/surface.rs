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
    cursor: Option<Position>,
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
            cursor: None,
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
        let previous = if reset {
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
        let cursor_known = self.started && !reset && !switch && previous.is_some();
        // 每次历史提交都会把整个可见屏幕上移一行，底部留下默认样式的空行。
        // 差分必须基于滚动后的物理屏幕，不能沿用滚动前的坐标或强制重发所有行。
        let scrolled = previous.filter(|_| !history.is_empty()).map(|old| {
            let mut scrolled = Buffer::empty(buffer.area);
            let offset =
                history.len().min(usize::from(buffer.area.height)) * usize::from(buffer.area.width);
            let retained = old.content.len() - offset;
            scrolled.content[..retained].clone_from_slice(&old.content[offset..]);
            scrolled
        });
        let previous = scrolled.as_ref().or(previous);
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
        let changes = previous.diff(next);
        let redraw = !changes.is_empty();
        let structural = !started || reset || switch || !history.is_empty();
        let visibility_changed = !cursor_known || self.cursor.is_some() != cursor.is_some();
        if structural || redraw || !cursor_known || self.cursor != cursor {
            // backend 的定位和显隐方法会 flush；先写内存，使同步更新结束序列
            // 与整帧一起提交到终端，避免实际 writer 在一帧中途被多次刷新。
            let mut output = Vec::new();
            output.sync_update(|writer| -> io::Result<()> {
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
                }
                if cursor.is_none() && visibility_changed {
                    queue!(writer, Hide)?;
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
                if redraw {
                    backend.draw(changes.into_iter())?;
                }
                if let Some(cursor) = cursor {
                    // 正文写入也会移动物理光标，即使输入位置没变仍需恢复。
                    if structural || redraw || !cursor_known || self.cursor != Some(cursor) {
                        backend.set_cursor_position(cursor)?;
                    }
                    if visibility_changed {
                        backend.show_cursor()?;
                    }
                }
                Backend::flush(&mut backend)
            })??;
            if alternate {
                // 部分写入后失败也可能已进入备用屏幕，退出清理仍须尝试恢复。
                ALTERNATE_ACTIVE.store(true, Ordering::SeqCst);
            }
            self.writer.write_all(&output)?;
            self.writer.flush()?;
        }
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
        self.cursor = cursor;
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

    #[derive(Default)]
    struct RecordingWriter {
        bytes: Vec<u8>,
        flushes: usize,
    }

    impl Write for RecordingWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            // 每次实际刷新都必须包含完整的同步更新结束序列。
            assert!(self.bytes.ends_with(b"\x1b[?2026l"));
            self.flushes += 1;
            Ok(())
        }
    }

    fn assert_frame(parser: &vt100::Parser, buffer: &Buffer, cursor: Option<Position>) {
        let mut expected = Surface::new(Vec::new());
        expected.paint(buffer, cursor, false, &[]).unwrap();
        let mut reference = vt100::Parser::new(buffer.area.height, buffer.area.width, 0);
        reference.process(&expected.writer);
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                let actual = parser.screen().cell(y, x).unwrap();
                let expected = reference.screen().cell(y, x).unwrap();
                // 未写入的空格与显式写入的空格在格式化快照中不同，视觉效果相同。
                assert_eq!(
                    actual.contents().trim_end_matches(' '),
                    expected.contents().trim_end_matches(' '),
                    "单元格 ({x}, {y}) 正文不同"
                );
                let appearance = |cell: &vt100::Cell| {
                    (
                        cell.fgcolor(),
                        cell.bgcolor(),
                        [
                            cell.bold(),
                            cell.dim(),
                            cell.italic(),
                            cell.underline(),
                            cell.inverse(),
                            cell.is_wide(),
                            cell.is_wide_continuation(),
                        ],
                    )
                };
                assert_eq!(
                    appearance(actual),
                    appearance(expected),
                    "单元格 ({x}, {y}) 样式不同"
                );
            }
        }
        assert_eq!(parser.screen().hide_cursor(), cursor.is_none());
        if let Some(cursor) = cursor {
            assert_eq!(parser.screen().cursor_position(), (cursor.y, cursor.x));
        }
    }

    #[test]
    fn native_surface_identical_frames_produce_no_output_or_flush() {
        for cursor in [None, Some(Position::new(3, 4))] {
            let mut surface = Surface::new(RecordingWriter::default());
            let buffer = frame("unchanged");
            surface.paint(&buffer, cursor, false, &[]).unwrap();
            let bytes = surface.writer.bytes.clone();
            assert_eq!(surface.writer.flushes, 1);
            for _ in 0..3 {
                surface.paint(&buffer, cursor, false, &[]).unwrap();
            }
            assert_eq!(surface.writer.bytes, bytes);
            assert_eq!(surface.writer.flushes, 1);
        }
    }

    #[test]
    fn native_surface_stream_updates_preserve_visible_cursor_and_input() {
        let mut surface = Surface::new(RecordingWriter::default());
        let mut parser = vt100::Parser::new(6, 24, 100);
        let cursor = Some(Position::new(8, 4));
        for text in ["first", "first中", "first中😀", "first中x", "f"] {
            let mut buffer = frame(text);
            buffer.set_string(0, 4, "draft 中", ratatui::style::Style::default());
            let before = surface.writer.bytes.len();
            surface.paint(&buffer, cursor, false, &[]).unwrap();
            let output = &surface.writer.bytes[before..];
            parser.process(output);
            assert_frame(&parser, &buffer, cursor);
            assert!(!parser.screen().hide_cursor());
            assert_eq!(parser.screen().cursor_position(), (4, 8));
            if before > 0 {
                let output = String::from_utf8_lossy(output);
                assert!(!output.contains("\x1b[?25l"));
                assert!(!output.contains("\x1b[?25h"));
                assert!(!output.contains("draft"));
            }
        }
        assert_eq!(surface.writer.flushes, 5);
    }

    #[test]
    fn native_surface_cursor_only_changes_update_position_and_visibility() {
        let mut surface = Surface::new(Vec::new());
        let buffer = frame("unchanged");
        let mut parser = vt100::Parser::new(6, 24, 0);
        surface
            .paint(&buffer, Some(Position::new(1, 4)), false, &[])
            .unwrap();
        parser.process(&surface.writer);
        for cursor in [Some(Position::new(3, 4)), None, Some(Position::new(5, 4))] {
            let before = surface.writer.len();
            surface.paint(&buffer, cursor, false, &[]).unwrap();
            let output = &surface.writer[before..];
            assert!(!String::from_utf8_lossy(output).contains("unchanged"));
            parser.process(output);
            assert_eq!(parser.screen().hide_cursor(), cursor.is_none());
            if let Some(cursor) = cursor {
                assert_eq!(parser.screen().cursor_position(), (cursor.y, cursor.x));
            }
        }
        let output = String::from_utf8_lossy(&surface.writer);
        assert_eq!(output.matches("\x1b[?25l").count(), 1);
        assert_eq!(output.matches("\x1b[?25h").count(), 2);
    }

    #[test]
    fn native_surface_history_scroll_reuses_unchanged_physical_rows() {
        let mut surface = Surface::new(Vec::new());
        let mut buffer = frame("history-to-commit");
        let style = ratatui::style::Style::default();
        buffer.set_string(0, 1, "retained 中😀", style);
        buffer.set_string(0, 4, "draft", style);
        let cursor = Some(Position::new(5, 4));
        surface.paint(&buffer, cursor, false, &[]).unwrap();
        let mut next = frame("retained 中😀");
        next.set_string(0, 4, "draft", style);
        let before = surface.writer.len();
        surface
            .paint(&next, cursor, false, &[buffer.content[..24].to_vec()])
            .unwrap();
        let output = String::from_utf8_lossy(&surface.writer[before..]);
        assert!(output.contains("history-to-commit"));
        assert!(
            !output.contains("retained"),
            "重复输出未变化的物理行：{output:?}"
        );
        assert!(!output.contains("\x1b[?25l") && !output.contains("\x1b[?25h"));
        let mut parser = vt100::Parser::new(6, 24, 100);
        parser.process(&surface.writer);
        assert_frame(&parser, &next, cursor);
        parser.screen_mut().set_scrollback(1);
        assert!(parser.screen().contents().starts_with("history-to-commit"));
    }

    #[test]
    fn native_surface_history_scroll_restores_cursor_without_cell_changes() {
        let mut surface = Surface::new(Vec::new());
        let buffer = Buffer::empty(Rect::new(0, 0, 24, 6));
        let cursor = Some(Position::new(3, 4));
        surface.paint(&buffer, cursor, false, &[]).unwrap();
        let before = surface.writer.len();
        surface
            .paint(
                &buffer,
                cursor,
                false,
                &[frame("committed").content[..24].to_vec()],
            )
            .unwrap();
        let output = String::from_utf8_lossy(&surface.writer[before..]);
        assert!(output.contains("committed"));
        assert!(!output.contains("\x1b[?25l") && !output.contains("\x1b[?25h"));
        let mut parser = vt100::Parser::new(6, 24, 100);
        parser.process(&surface.writer);
        assert_frame(&parser, &buffer, cursor);
    }

    #[test]
    fn native_surface_history_batches_match_full_redraw_with_styles_and_wide_cells() {
        use ratatui::style::{Color, Modifier, Style};

        for height in [1, 6] {
            let mut surface = Surface::new(Vec::new());
            let mut parser = vt100::Parser::new(height, 24, 100);
            let mut buffer = Buffer::empty(Rect::new(0, 0, 24, height));
            let cursor = Some(Position::new(3, height - 1));
            for batch_size in [0, 1, 2, 6, 9] {
                let history = (0..batch_size)
                    .map(|i| frame(&format!("committed-{i:02} 中😀")).content[..24].to_vec())
                    .collect::<Vec<_>>();
                for y in 0..height {
                    let mut row = frame(&format!("{batch_size}-{y} 中😀"));
                    row.set_style(
                        Rect::new(0, 0, 24, 1),
                        Style::default()
                            .fg(Color::Green)
                            .bg(Color::Blue)
                            .add_modifier(Modifier::BOLD),
                    );
                    copy_row(&row.content[..24], &mut buffer, 0, y, 24);
                }
                let before = surface.writer.len();
                surface.paint(&buffer, cursor, false, &history).unwrap();
                parser.process(&surface.writer[before..]);
                assert_frame(&parser, &buffer, cursor);
            }
        }
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

    struct FailingWriter {
        fail_write: bool,
    }
    impl Write for FailingWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.fail_write {
                Err(io::Error::other("测试写入失败"))
            } else {
                Ok(bytes.len())
            }
        }
        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::other("测试写入失败"))
        }
    }

    #[test]
    fn native_surface_never_acknowledges_failed_output() {
        for fail_write in [false, true] {
            let mut surface = Surface::new(FailingWriter { fail_write });
            assert!(
                surface
                    .paint(&frame("pending"), Some(Position::new(3, 4)), false, &[])
                    .is_err()
            );
            assert!(surface.previous.is_none());
            assert!(surface.main.is_none());
            assert!(!surface.started);
            assert_eq!(surface.cursor, None);
        }
    }
}
