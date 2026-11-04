use crossterm::{
    queue,
    style::{ContentStyle, PrintStyledContent},
};
use ratatui::{
    backend::IntoCrossterm,
    buffer::{Buffer, Cell},
    layout::Rect,
    style::Style,
    text::Span,
};
use std::io::{self, Write};

#[cfg(test)]
pub(super) fn write_rows(writer: &mut impl Write, buffer: &Buffer, area: Rect) -> io::Result<()> {
    for y in area.y..area.bottom() {
        write_row(writer, buffer, Rect::new(area.x, y, area.width, 1))?;
        writer.write_all(b"\r\n")?;
    }
    Ok(())
}

pub(super) fn write_row(writer: &mut impl Write, buffer: &Buffer, area: Rect) -> io::Result<()> {
    let y = area.y;
    let mut end = area.right();
    while end > area.x && buffer[(end - 1, y)] == Cell::EMPTY {
        end -= 1;
    }
    let mut x = area.x;
    let mut style = Style::default();
    let mut text = String::new();
    while x < end {
        let cell = &buffer[(x, y)];
        if cell.style() != style {
            write_run(writer, style, &text)?;
            text.clear();
            style = cell.style();
        }
        // 不回放单元格内的终端控制字符；宽字符的占位单元格不重复输出。
        text.extend(cell.symbol().chars().filter(|ch| !ch.is_control()));
        let width = Span::raw(cell.symbol()).width().max(1);
        x = x.saturating_add(u16::try_from(width).unwrap_or(u16::MAX));
    }
    write_run(writer, style, &text)?;
    Ok(())
}

fn write_run(writer: &mut impl Write, style: Style, text: &str) -> io::Result<()> {
    if !text.is_empty() {
        let style: ContentStyle = style.into_crossterm();
        queue!(writer, PrintStyledContent(style.apply(text)))?;
    }
    Ok(())
}
