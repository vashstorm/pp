use super::selector::Target;
use crate::config::{EditMode, ToolLimits};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::io::BufRead;

pub(super) fn count_lines(mut reader: impl BufRead) -> Result<usize> {
    let mut count = 0usize;
    let mut last = None;
    loop {
        let bytes = reader.fill_buf()?;
        if bytes.is_empty() {
            break;
        }
        count = count
            .checked_add(bytes.iter().filter(|b| **b == b'\n').count())
            .context("read line count overflow")?;
        last = bytes.last().copied();
        let n = bytes.len();
        reader.consume(n);
    }
    Ok(count.saturating_add(usize::from(last.is_some_and(|b| b != b'\n'))))
}

pub(super) fn page_text(
    target: &Target,
    limits: ToolLimits,
    mode: EditMode,
    anchored: bool,
    ranges: Vec<(usize, usize)>,
    mut page: impl FnMut(usize, usize, usize) -> Result<Page>,
) -> Result<Value> {
    let mut content = String::new();
    let mut lines_read = 0;
    let mut used_bytes = 0;
    let mut shown = Vec::new();
    let mut remaining = Vec::new();
    for (index, &(start, end)) in ranges.iter().enumerate() {
        if lines_read == limits.read_max_lines || used_bytes == limits.read_max_bytes {
            remaining.extend_from_slice(&ranges[index..]);
            break;
        }
        let count = end
            .saturating_sub(start)
            .saturating_add(1)
            .min(limits.read_max_lines - lines_read);
        let result = match page(start, count, limits.read_max_bytes - used_bytes) {
            Ok(page) => page,
            Err(error)
                if lines_read > 0 && error.to_string().contains("exceeds read_max_bytes") =>
            {
                remaining.extend_from_slice(&ranges[index..]);
                break;
            }
            Err(error) => return Err(error),
        };
        used_bytes += result.bytes.len();
        lines_read += result.lines_read;
        if result.lines_read > 0 {
            shown.push(json!({"start":start,"end":start + result.lines_read - 1}));
        }
        if anchored && !target.raw && matches!(mode, EditMode::Hashline) {
            content.push_str(&crate::tools::hashline::render_page(&result.bytes, start));
        } else {
            content.push_str(&String::from_utf8_lossy(&result.bytes));
        }
        if let Some(next) = result.next_offset
            && next <= end
        {
            remaining.push((next, end));
            remaining.extend_from_slice(&ranges[index + 1..]);
            break;
        }
    }
    let next_path = (!remaining.is_empty()).then(|| target.next_path(&remaining));
    Ok(
        json!({"ok":true,"source":target.source,"content":content,"truncated":next_path.is_some(),"lines_read":lines_read,
        "next_offset":remaining.first().map(|r| r.0),"next_path":next_path,"ranges":shown}),
    )
}

pub(super) struct Page {
    pub(super) bytes: Vec<u8>,
    pub(super) lines_read: usize,
    pub(super) next_offset: Option<usize>,
}

pub(super) fn read_from(
    mut reader: impl BufRead,
    offset: usize,
    limit: usize,
    max_bytes: usize,
) -> Result<Page> {
    let mut skipped = 0;
    while skipped < offset - 1 {
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            return Ok(Page {
                bytes: Vec::new(),
                lines_read: 0,
                next_offset: None,
            });
        }
        let count = match buffer.iter().position(|&b| b == b'\n') {
            Some(at) => {
                skipped += 1;
                at + 1
            }
            None => buffer.len(),
        };
        reader.consume(count);
    }

    let mut bytes = Vec::new();
    let mut lines_read = 0;
    let mut line_start = 0;
    let more = loop {
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            if bytes.len() > line_start {
                lines_read += 1;
            }
            break false;
        }
        if lines_read == limit {
            break true;
        }
        let newline = buffer.iter().position(|&b| b == b'\n');
        let count = newline.map_or(buffer.len(), |at| at + 1);
        let room = max_bytes - bytes.len();
        if count > room {
            bytes.truncate(line_start);
            ensure!(
                lines_read > 0,
                "line at offset {offset} exceeds read_max_bytes ({max_bytes}); increase the configured limit or inspect a bounded fragment with bash"
            );
            break true;
        }
        bytes.extend_from_slice(&buffer[..count]);
        reader.consume(count);
        if newline.is_some() {
            lines_read += 1;
            line_start = bytes.len();
        }
    };
    let next_offset = if more {
        Some(
            offset
                .checked_add(lines_read)
                .context("read line offset overflow")?,
        )
    } else {
        None
    };
    Ok(Page {
        bytes,
        lines_read,
        next_offset,
    })
}

#[cfg(test)]
mod tests {
    use crate::tools::read::file::read;

    #[test]
    fn read_pages_preserve_complete_lines_and_source_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source");
        std::fs::write(&path, "中\r\n\nlast").unwrap();
        let first = read(&path, 1, 10, 7).unwrap();
        assert_eq!(first.bytes, "中\r\n\n".as_bytes());
        assert_eq!((first.lines_read, first.next_offset), (2, Some(3)));
        let second = read(&path, first.next_offset.unwrap(), 10, 7).unwrap();
        assert_eq!(second.bytes, b"last");
        assert_eq!((second.lines_read, second.next_offset), (1, None));
        let capped = read(&path, 1, 1, 7).unwrap();
        assert_eq!(capped.bytes, "中\r\n".as_bytes());
        assert_eq!((capped.lines_read, capped.next_offset), (1, Some(2)));
        assert!(read(&path, 1, 10, 4).is_err());
        assert!(read(&path, 100, 10, 4).unwrap().bytes.is_empty());
    }

    #[test]
    fn read_exact_caps_empty_files_and_partial_final_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source");
        for source in ["", "abc", "ab\n", "\n\n\n"] {
            std::fs::write(&path, source).unwrap();
            let page = read(&path, 1, 3, 3).unwrap();
            assert_eq!(page.bytes, source.as_bytes());
            assert_eq!(page.next_offset, None);
            assert_eq!(page.lines_read, source.lines().count());
        }
        std::fs::write(&path, "ab\nc").unwrap();
        let page = read(&path, 1, 3, 3).unwrap();
        assert_eq!(page.bytes, b"ab\n");
        assert_eq!(page.next_offset, Some(2));
        assert_eq!(read(&path, 2, 3, 3).unwrap().bytes, b"c");
        std::fs::write(&path, "\n".repeat(2001)).unwrap();
        let page = read(&path, 1, 2000, 65536).unwrap();
        assert_eq!(page.lines_read, 2000);
        assert_eq!(page.bytes.len(), 2000);
        assert_eq!(page.next_offset, Some(2001));
        assert!(read(dir.path(), 1, 1, 1).is_err());
    }

    #[test]
    fn read_buffers_stay_bounded_while_skipping_long_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source");
        let source = format!("{}\nsecond\n", "x".repeat(100_000));
        std::fs::write(&path, source).unwrap();
        assert!(read(&path, 1, 1, 8).is_err());
        let page = read(&path, 2, 1, 8).unwrap();
        assert_eq!(page.bytes, b"second\n");
        assert_eq!(page.next_offset, None);
    }
}
