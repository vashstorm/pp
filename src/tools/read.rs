use anyhow::{Context, Result, ensure};
use std::{
    fs::OpenOptions,
    io::{BufRead, BufReader},
    os::unix::fs::OpenOptionsExt,
    path::Path,
};

pub(super) struct Page {
    pub(super) bytes: Vec<u8>,
    pub(super) lines_read: usize,
    pub(super) next_offset: Option<usize>,
}

pub(super) fn read(path: &Path, offset: usize, limit: usize, max_bytes: usize) -> Result<Page> {
    // 非阻塞打开，避免在检查文件类型前被 FIFO 挂住。
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NONBLOCK.bits() as i32)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    ensure!(file.metadata()?.is_file(), "read requires a regular file");
    let mut reader = BufReader::new(file);
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
    use super::*;

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
