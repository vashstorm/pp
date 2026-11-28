//! Main-screen incremental commit and the alternate screen share a single output entry point.

use super::transcript::NativeRow;
use crossterm::{
    SynchronizedUpdate,
    cursor::{Hide, MoveTo, Show},
    event::{
        DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
        EnableFocusChange, EnableMouseCapture, KeyboardEnhancementFlags,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute, queue,
    style::{Attribute, Print, ResetColor, SetAttribute},
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
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

static ALTERNATE_ACTIVE: AtomicBool = AtomicBool::new(false);
static GRAPHICS: Mutex<GraphicsCleanup> = Mutex::new(GraphicsCleanup {
    areas: Vec::new(),
    kitty: Vec::new(),
});

#[derive(Default, Clone)]
struct GraphicsCleanup {
    areas: Vec<(Rect, bool)>,
    kitty: Vec<(u32, bool)>,
}

impl GraphicsCleanup {
    fn from_frame(buffer: &Buffer, graphics: &[Rect]) -> Self {
        let mut cleanup = Self::default();
        for area in graphics {
            let tmux = buffer
                .cell((area.x, area.y))
                .is_some_and(|cell| cell.symbol().contains("\x1bPtmux;"));
            cleanup.areas.push((*area, tmux));
            if let Some(cell) = buffer.cell((area.x, area.y)) {
                let symbol = cell.symbol();
                if let Some((_, command)) = symbol.split_once("\x1b_G") {
                    let header = command.split(';').next().unwrap_or_default();
                    if let Some(id) = header.split(',').find_map(|part| {
                        part.strip_prefix("i=")
                            .and_then(|id| id.parse::<u32>().ok())
                    }) {
                        cleanup.kitty.push((id, symbol.contains("\x1bPtmux;")));
                    }
                }
            }
        }
        cleanup.kitty.sort_unstable();
        cleanup.kitty.dedup();
        cleanup
    }

    fn clear(&self, writer: &mut impl Write) -> io::Result<()> {
        for (id, tmux) in &self.kitty {
            // Delete only the images owned by the current pp frame, keeping images from other terminal applications.
            if *tmux {
                write!(
                    writer,
                    "\x1bPtmux;\x1b\x1b_Ga=d,d=I,i={id},q=2\x1b\x1b\\\x1b\\"
                )?;
            } else {
                write!(writer, "\x1b_Ga=d,d=I,i={id},q=2\x1b\\")?;
            }
        }
        for (area, tmux) in &self.areas {
            for row in area.top()..area.bottom() {
                queue!(
                    writer,
                    ResetColor,
                    SetAttribute(Attribute::Reset),
                    MoveTo(area.x, row)
                )?;
                // ECH only erases the owned rectangle so other thumbnails on the same row are untouched.
                write!(writer, "\x1b[{}X", area.width)?;
                if *tmux {
                    // Native images live outside tmux's character buffer, so erasing over blank cells may be optimized away.
                    write!(writer, "\x1bPtmux;\x1b\x1b[{}X\x1b\\", area.width)?;
                }
            }
        }
        Ok(())
    }
}

pub(super) fn initialize() -> io::Result<()> {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        previous(info);
    }));
    terminal::enable_raw_mode()?;
    // Request both extended key protocols: tmux only honors xterm's modifyOtherKeys (`CSI > 4;2m`),
    // and terminals that only support the Kitty protocol ignore it; the Kitty push comes last as the preferred decodable CSI-u.
    // Terminals supporting neither ignore these sequences and can still use Ctrl + J for newlines.
    execute!(
        io::stdout(),
        EnableBracketedPaste,
        EnableMouseCapture,
        Print("\x1b[>4;2m"),
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    )
}

pub(super) fn enable_focus_reporting() -> io::Result<()> {
    execute!(io::stdout(), EnableFocusChange)
}

pub(super) fn restore() {
    // Hand the real terminal back to fd 0 first so disabling raw mode and restoring the terminal apply to the original terminal.
    super::input_shim::restore();
    let mut stdout = io::stdout();
    if let Ok(mut graphics) = GRAPHICS.lock() {
        let _ = graphics.clear(&mut stdout);
        *graphics = GraphicsCleanup::default();
    }
    if ALTERNATE_ACTIVE.swap(false, Ordering::SeqCst) {
        let _ = execute!(stdout, LeaveAlternateScreen);
    }
    let _ = execute!(
        stdout,
        EndSynchronizedUpdate,
        Print("\x1b[>4;0m"),
        PopKeyboardEnhancementFlags,
        DisableMouseCapture,
        DisableBracketedPaste,
        DisableFocusChange,
        ResetColor,
        SetAttribute(Attribute::Reset),
        Show
    );
    let _ = terminal::disable_raw_mode();
}

/// A width change only clips frozen rows still on screen; rows already in native scrollback are not resent.
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
    // The diff baseline shares one full-frame buffer: the main-screen frame's previous is the same memory as main.
    previous: Option<Arc<Buffer>>,
    main: Option<Arc<Buffer>>,
    alternate: bool,
    reset_pending: bool,
    started: bool,
    cursor: Option<Position>,
    graphics: Vec<Rect>,
    graphics_cleanup: GraphicsCleanup,
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
            graphics: Vec::new(),
            graphics_cleanup: GraphicsCleanup::default(),
        }
    }

    pub(super) fn reset_history(&mut self) {
        self.reset_pending = true;
    }

    /// A batch of history and its trailing frame commit within one short synchronous update.
    #[cfg(test)]
    pub(super) fn paint(
        &mut self,
        buffer: &Buffer,
        cursor: Option<Position>,
        alternate: bool,
        history: &[NativeRow],
    ) -> io::Result<()> {
        self.paint_images(Arc::new(buffer.clone()), cursor, alternate, history, &[])
    }

    pub(super) fn paint_images(
        &mut self,
        buffer: Arc<Buffer>,
        cursor: Option<Position>,
        alternate: bool,
        history: &[NativeRow],
        graphics: &[Rect],
    ) -> io::Result<()> {
        let reset = self.reset_pending;
        let switch = self.alternate != alternate;
        let clear_all = reset
            || switch
            || !history.is_empty()
            || self
                .previous
                .as_ref()
                .is_none_or(|old| old.area != buffer.area);
        let mut removed: Vec<_> = self
            .graphics
            .iter()
            .copied()
            .filter(|area| {
                clear_all
                    || !graphics.contains(area)
                    || self.previous.as_ref().is_none_or(|old| {
                        old.cell((area.x, area.y)) != buffer.cell((area.x, area.y))
                    })
            })
            .collect();
        // Old images covered by the erased area must be repainted too; a new overlay must not delete or resend unchanged thumbnails.
        loop {
            let before = removed.len();
            for area in &self.graphics {
                if !removed.contains(area) && removed.iter().any(|old| old.intersects(*area)) {
                    removed.push(*area);
                }
            }
            if removed.len() == before {
                break;
            }
        }
        let cleanup = self
            .previous
            .as_ref()
            .map(|old| GraphicsCleanup::from_frame(old, &removed))
            .unwrap_or_default();
        let clear_graphics = !removed.is_empty();
        let previous: Option<&Buffer> = if reset {
            None
        } else if switch && !alternate {
            self.main.as_deref()
        } else if switch {
            None
        } else {
            self.previous.as_deref()
        };
        let unknown = Buffer::empty(buffer.area);
        let previous = previous.filter(|old| old.area == buffer.area);
        // Graphics are erased from the physical screen first, then scrolling and diffing are computed from the same state.
        let cleaned = previous.filter(|_| clear_graphics && !switch).map(|old| {
            let mut cleaned = old.clone();
            for area in &removed {
                let area = area.intersection(cleaned.area);
                for row in area.top()..area.bottom() {
                    for x in area.left()..area.right() {
                        cleaned[(x, row)].reset();
                    }
                }
            }
            cleaned
        });
        let previous = cleaned.as_ref().or(previous);
        let cursor_known = self.started && !reset && !switch && previous.is_some();
        // Every history commit shifts the whole visible screen up one row, leaving a default-styled blank row at the bottom.
        // The diff must be based on the physically scrolled screen, not on pre-scroll coordinates or a forced repaint of every row.
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
            let mut next = (*buffer).clone();
            for cell in &mut next.content {
                if cell.diff_option == CellDiffOption::None {
                    cell.set_diff_option(CellDiffOption::AlwaysUpdate);
                }
            }
            next
        });
        let previous = previous.unwrap_or(&unknown);
        let next = forced.as_ref().unwrap_or(&buffer);
        let was_alternate = self.alternate;
        let started = self.started;
        let changes = previous.diff(next);
        let redraw = !changes.is_empty();
        let structural = !started || reset || switch || !history.is_empty();
        let visibility_changed = !cursor_known || self.cursor.is_some() != cursor.is_some();
        if clear_graphics || structural || redraw || !cursor_known || self.cursor != cursor {
            // The backend's positioning and visibility methods flush; write to memory first so the synchronous-update
            // end sequence and the full frame commit together, avoiding several real writer flushes mid-frame.
            let mut output = Vec::new();
            output.sync_update(|writer| -> io::Result<()> {
                if clear_graphics {
                    cleanup.clear(writer)?;
                }
                if cursor.is_none() && visibility_changed {
                    // Hide first, then scroll, clear or position, so rebuilding never exposes an intermediate position.
                    queue!(writer, Hide)?;
                }
                if !started {
                    // Reserve a dedicated screen for the main UI, pushing pre-startup shell content into history.
                    writer.write_all("\r\n".repeat(usize::from(buffer.area.height)).as_bytes())?;
                }
                if was_alternate && (!alternate || reset) {
                    queue!(writer, LeaveAlternateScreen)?;
                    if cursor.is_none() {
                        queue!(writer, Hide)?;
                    }
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
                    if cursor.is_none() {
                        queue!(writer, Hide)?;
                    }
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
                    // The graphics protocol may move the cursor or change styles; the next text batch must reposition explicitly.
                    let mut text = Vec::new();
                    for cell in changes {
                        if cell.2.symbol().contains('\x1b') {
                            if !text.is_empty() {
                                backend.draw(text.drain(..))?;
                            }
                            backend.draw(std::iter::once(cell))?;
                        } else {
                            text.push(cell);
                        }
                    }
                    if !text.is_empty() {
                        backend.draw(text.into_iter())?;
                    }
                }
                if let Some(cursor) = cursor {
                    // Writing body text also moves the physical cursor, so it must be restored even when the input position did not change.
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
                // A failure after a partial write may already have entered the alternate screen; exit cleanup must still attempt a restore.
                ALTERNATE_ACTIVE.store(true, Ordering::SeqCst);
            }
            self.writer.write_all(&output)?;
            self.writer.flush()?;
        }
        // The main-screen frame's diff baseline shares the same full-frame buffer as main, so it is copied once.
        let main_area = buffer.area;
        let snapshot = buffer.clone();
        self.previous = Some(snapshot.clone());
        if reset
            || clear_graphics
            || self
                .main
                .as_ref()
                .is_some_and(|main| main.area != main_area)
        {
            // Cleanup or resize during the alternate screen also changes the main screen, so the old diff baseline cannot be reused.
            self.main = None;
        }
        if !alternate {
            self.main = Some(snapshot);
        }
        self.alternate = alternate;
        ALTERNATE_ACTIVE.store(alternate, Ordering::SeqCst);
        self.reset_pending = false;
        self.started = true;
        self.cursor = cursor;
        self.graphics = graphics.to_vec();
        self.graphics_cleanup = GraphicsCleanup::from_frame(&buffer, graphics);
        if let Ok(mut graphics) = GRAPHICS.lock() {
            *graphics = self.graphics_cleanup.clone();
        }
        Ok(())
    }

    pub(super) fn finish(&mut self, height: u16) -> io::Result<()> {
        // A normal frame already remains on the main screen; only free the single row next to the content for the shell.
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
            // Every real flush must contain the complete synchronous-update end sequence.
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
                // Unwritten spaces differ from explicitly written spaces in a formatted snapshot but look identical.
                assert_eq!(
                    actual.contents().trim_end_matches(' '),
                    expected.contents().trim_end_matches(' '),
                    "cell ({x}, {y}) contents differ"
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
                    "cell ({x}, {y}) style differs"
                );
            }
        }
        assert_eq!(parser.screen().hide_cursor(), cursor.is_none());
        if let Some(cursor) = cursor {
            assert_eq!(parser.screen().cursor_position(), (cursor.y, cursor.x));
        }
    }

    #[test]
    fn native_surface_diff_history_batches_overlays_and_failures() {
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

        let mut s_stream = Surface::new(RecordingWriter::default());
        let mut p_stream = vt100::Parser::new(6, 24, 100);
        let cursor = Some(Position::new(8, 4));
        for text in ["first", "first中", "first中😀", "first中x", "f"] {
            let mut buffer = frame(text);
            buffer.set_string(0, 4, "draft 中", ratatui::style::Style::default());
            let before = s_stream.writer.bytes.len();
            s_stream.paint(&buffer, cursor, false, &[]).unwrap();
            p_stream.process(&s_stream.writer.bytes[before..]);
            assert_frame(&p_stream, &buffer, cursor);
        }
        assert_eq!(s_stream.writer.flushes, 5);

        let mut s_cur = Surface::new(Vec::new());
        let b_cur = frame("unchanged");
        let mut p_cur = vt100::Parser::new(6, 24, 0);
        s_cur
            .paint(&b_cur, Some(Position::new(1, 4)), false, &[])
            .unwrap();
        p_cur.process(&s_cur.writer);
        for c in [Some(Position::new(3, 4)), None, Some(Position::new(5, 4))] {
            let before = s_cur.writer.len();
            s_cur.paint(&b_cur, c, false, &[]).unwrap();
            let out = &s_cur.writer[before..];
            assert!(!String::from_utf8_lossy(out).contains("unchanged"));
            p_cur.process(out);
            assert_eq!(p_cur.screen().hide_cursor(), c.is_none());
        }
        let out_str = String::from_utf8_lossy(&s_cur.writer);
        assert_eq!(
            (
                out_str.matches("\x1b[?25l").count(),
                out_str.matches("\x1b[?25h").count()
            ),
            (1, 2)
        );

        for reset in [false, true] {
            let mut surface = Surface::new(Vec::new());
            let mut parser = vt100::Parser::new(6, 24, 100);
            if reset {
                surface
                    .paint(&frame("old session"), Some(Position::new(3, 4)), false, &[])
                    .unwrap();
                parser.process(&surface.writer);
                surface.reset_history();
            }
            let before = surface.writer.len();
            surface
                .paint(
                    &frame("restored session"),
                    None,
                    false,
                    &[NativeRow::from(frame("committed").content[..24].to_vec())],
                )
                .unwrap();
            for byte in &surface.writer[before..] {
                let previous = parser.screen().cursor_position();
                parser.process(&[*byte]);
                if parser.screen().cursor_position() != previous {
                    assert!(parser.screen().hide_cursor());
                }
            }
            assert_frame(&parser, &frame("restored session"), None);
        }

        let mut s_scroll = Surface::new(Vec::new());
        let mut buffer = frame("history-to-commit");
        let style = ratatui::style::Style::default();
        buffer.set_string(0, 1, "retained 中😀", style);
        buffer.set_string(0, 4, "draft", style);
        let cursor = Some(Position::new(5, 4));
        s_scroll.paint(&buffer, cursor, false, &[]).unwrap();
        let mut next = frame("retained 中😀");
        next.set_string(0, 4, "draft", style);
        let before = s_scroll.writer.len();
        s_scroll
            .paint(
                &next,
                cursor,
                false,
                &[NativeRow::from(buffer.content[..24].to_vec())],
            )
            .unwrap();
        let out = String::from_utf8_lossy(&s_scroll.writer[before..]);
        assert!(out.contains("history-to-commit") && !out.contains("retained"));
        let mut p_scroll = vt100::Parser::new(6, 24, 100);
        p_scroll.process(&s_scroll.writer);
        assert_frame(&p_scroll, &next, cursor);

        let mut s_empty = Surface::new(Vec::new());
        let b_empty = Buffer::empty(Rect::new(0, 0, 24, 6));
        s_empty.paint(&b_empty, cursor, false, &[]).unwrap();
        let before_e = s_empty.writer.len();
        s_empty
            .paint(
                &b_empty,
                cursor,
                false,
                &[NativeRow::from(frame("committed").content[..24].to_vec())],
            )
            .unwrap();
        assert!(String::from_utf8_lossy(&s_empty.writer[before_e..]).contains("committed"));
    }

    #[test]
    fn native_surface_styled_batches_overlays_purge_and_failures() {
        {
            use ratatui::style::{Color, Modifier, Style};

            for height in [1, 6] {
                let mut surface = Surface::new(Vec::new());
                let mut parser = vt100::Parser::new(height, 24, 100);
                let mut buffer = Buffer::empty(Rect::new(0, 0, 24, height));
                let cursor = Some(Position::new(3, height - 1));
                for batch_size in [0, 1, 2, 6, 9] {
                    let history = (0..batch_size)
                        .map(|i| {
                            NativeRow::from(
                                frame(&format!("committed-{i:02} 中😀")).content[..24].to_vec(),
                            )
                        })
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

        {
            let mut s1 = Surface::new(Vec::new());
            let main = frame("retained 中😀");
            s1.paint(&main, Some(Position::new(1, 4)), false, &[])
                .unwrap();
            let first = s1.writer.len();
            s1.paint(&frame("temporary panel"), None, true, &[])
                .unwrap();
            s1.paint(&main, Some(Position::new(1, 4)), false, &[])
                .unwrap();
            let restored = s1.writer.len();
            let mut p1 = vt100::Parser::new(6, 24, 100);
            p1.process(&s1.writer);
            assert!(
                !p1.screen().alternate_screen() && p1.screen().contents().contains("retained 中😀")
            );
            assert!(!String::from_utf8_lossy(&s1.writer[first..]).contains("retained"));
            s1.paint(&main, Some(Position::new(1, 4)), false, &[])
                .unwrap();
            s1.finish(6).unwrap();
            let exit = String::from_utf8_lossy(&s1.writer[restored..]);
            assert!(!exit.contains("retained") && !exit.contains("\x1b[3J"));

            let mut s2 = Surface::new(Vec::new());
            s2.paint(&frame("old"), None, false, &[]).unwrap();
            let before = s2.writer.len();
            s2.reset_history();
            s2.paint(&frame("new"), None, false, &[]).unwrap();
            let reset = String::from_utf8_lossy(&s2.writer[before..]);
            assert_eq!(
                (
                    reset.matches("\x1b[3J").count(),
                    reset.matches("\x1b[2J").count()
                ),
                (1, 1)
            );
            let b2 = s2.writer.len();
            s2.paint(&frame("new"), None, false, &[]).unwrap();
            assert!(!String::from_utf8_lossy(&s2.writer[b2..]).contains("\x1b[3J"));

            for do_reset in [false, true] {
                let mut s3 = Surface::new(Vec::new());
                let mut m3 = frame("normal content");
                m3.set_string(0, 5, "persistent footer", ratatui::style::Style::default());
                s3.paint(&m3, None, false, &[]).unwrap();
                s3.paint(&frame("panel"), None, true, &[]).unwrap();
                if do_reset {
                    s3.reset_history();
                } else {
                    s3.paint(&Buffer::empty(Rect::new(0, 0, 12, 4)), None, true, &[])
                        .unwrap();
                }
                s3.paint(&frame("panel restored"), None, true, &[]).unwrap();
                assert!(s3.main.is_none());
                s3.paint(&m3, None, false, &[]).unwrap();
                let mut p3 = vt100::Parser::new(6, 24, 100);
                p3.process(&s3.writer);
                assert!(
                    p3.screen().contents().contains("normal content")
                        && p3.screen().contents().contains("persistent footer")
                );
            }
        }

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

    struct FailingWriter {
        fail_write: bool,
    }
    impl Write for FailingWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.fail_write {
                Err(io::Error::other("test write failed"))
            } else {
                Ok(bytes.len())
            }
        }
        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::other("test write failed"))
        }
    }

    fn graphics_frame() -> (Buffer, Rect) {
        let mut buffer = frame("conversation");
        let area = Rect::new(2, 1, 8, 2);
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                buffer[(x, y)].set_diff_option(CellDiffOption::Skip);
            }
        }
        buffer[(area.x, area.y)]
            .set_symbol("\x1b_Ga=T,i=123,q=2;AAAA\x1b\\X")
            .set_diff_option(CellDiffOption::ForcedWidth(std::num::NonZeroU16::MIN));
        (buffer, area)
    }

    #[test]
    fn image_surface_updates_scroll_and_screen_switch() {
        {
            let (image, area) = graphics_frame();
            let mut s1 = Surface::new(Vec::new());
            s1.paint_images(Arc::new(image.clone()), None, false, &[], &[area])
                .unwrap();
            let initial = s1.writer.len();
            s1.paint_images(Arc::new(image), None, false, &[], &[area])
                .unwrap();
            assert_eq!(s1.writer.len(), initial);
            s1.paint_images(Arc::new(frame("conversation")), None, false, &[], &[])
                .unwrap();
            let bytes = String::from_utf8_lossy(&s1.writer[initial..]);
            assert!(
                bytes.contains("a=d,d=I,i=123")
                    && bytes.matches("\x1b[8X").count() == 2
                    && s1.graphics.is_empty()
            );

            let (mut tmux_img, t_area) = graphics_frame();
            tmux_img[(t_area.x, t_area.y)]
                .set_symbol("\x1bPtmux;\x1b\x1b]1337;File=inline=1:AAAA\x07\x1b\\");
            let mut s2 = Surface::new(Vec::new());
            s2.paint_images(Arc::new(tmux_img), None, false, &[], &[t_area])
                .unwrap();
            let start2 = s2.writer.len();
            s2.paint(&frame("conversation"), None, false, &[]).unwrap();
            let out2 = String::from_utf8_lossy(&s2.writer[start2..]);
            assert_eq!(out2.matches("\x1bPtmux;\x1b\x1b[8X\x1b\\").count(), 2);

            let thumbnail = Rect::new(2, 4, 8, 2);
            let neighbor = Rect::new(14, 4, 8, 2);
            let popup = Rect::new(2, 0, 20, 3);
            let add = |buffer: &mut Buffer, area: Rect, id| {
                for row in area.top()..area.bottom() {
                    for x in area.left()..area.right() {
                        buffer[(x, row)].set_diff_option(CellDiffOption::Skip);
                    }
                }
                buffer[(area.x, area.y)]
                    .set_symbol(&format!("\x1b_Ga=T,i={id},q=2;AAAA\x1b\\X"))
                    .set_diff_option(CellDiffOption::ForcedWidth(std::num::NonZeroU16::MIN));
            };
            let mut single = frame("");
            add(&mut single, thumbnail, 123);
            let mut multiple = single.clone();
            add(&mut multiple, neighbor, 456);
            let mut preview = multiple.clone();
            add(&mut preview, popup, 789);
            let mut s3 = Surface::new(Vec::new());
            s3.paint_images(
                Arc::new(multiple.clone()),
                None,
                false,
                &[],
                &[thumbnail, neighbor],
            )
            .unwrap();
            for (buffer, areas, deleted, transmitted) in [
                (&preview, vec![thumbnail, neighbor, popup], None, Some(789)),
                (&multiple, vec![thumbnail, neighbor], Some(789), None),
                (&single, vec![thumbnail], Some(456), None),
            ] {
                let start = s3.writer.len();
                s3.paint_images(Arc::new(buffer.clone()), None, false, &[], &areas)
                    .unwrap();
                let out = String::from_utf8_lossy(&s3.writer[start..]);
                assert!(!out.contains("a=d,d=I,i=123") && !out.contains("a=T,i=123"));
                if let Some(id) = deleted {
                    assert!(out.contains(&format!("a=d,d=I,i={id}")));
                }
                if let Some(id) = transmitted {
                    assert!(out.contains(&format!("a=T,i={id}")));
                }
            }
        }

        {
            let (image, area) = graphics_frame();
            let mut surface = Surface::new(Vec::new());
            surface
                .paint_images(
                    Arc::new(image.clone()),
                    Some(Position::new(2, 4)),
                    false,
                    &[],
                    &[area],
                )
                .unwrap();
            let start = surface.writer.len();
            let history = NativeRow::from(vec![Cell::new("h"); 24]);
            surface
                .paint_images(
                    Arc::new(image.clone()),
                    Some(Position::new(2, 4)),
                    false,
                    &[history],
                    &[area],
                )
                .unwrap();
            let bytes = String::from_utf8_lossy(&surface.writer[start..]);
            assert!(bytes.find("a=d,d=I").unwrap() < bytes.find("\r\n").unwrap());
            assert!(bytes.find("\r\n").unwrap() < bytes.find("a=T,i=123").unwrap());
            let start = surface.writer.len();
            surface.paint(&frame("panel"), None, true, &[]).unwrap();
            let bytes = String::from_utf8_lossy(&surface.writer[start..]);
            assert!(bytes.find("a=d,d=I").unwrap() < bytes.find("\x1b[?1049h").unwrap());
            assert!(surface.main.is_none());
            surface
                .paint_images(Arc::new(image), None, false, &[], &[area])
                .unwrap();
            assert_eq!(surface.graphics, [area]);
        }
    }
}
