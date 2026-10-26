use crate::{
    cancellation::{self, CancellationToken},
    config::{EditMode, ToolLimits},
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    fs::OpenOptions,
    io::{BufRead, BufReader, Cursor, Read},
    os::unix::fs::OpenOptionsExt,
    path::Path,
};

pub(crate) const MAX_INPUT_BYTES: usize = 20 * 1024 * 1024;
const MAX_CACHE_BYTES: usize = 8 * 1024 * 1024;

pub(crate) struct HttpResponse {
    pub(crate) bytes: Vec<u8>,
    pub(crate) final_url: String,
    pub(crate) content_type: String,
}

pub(crate) type Fetch<'a> = dyn FnMut(&str, &CancellationToken) -> Result<HttpResponse> + 'a;

pub(crate) struct ToolImage {
    pub(crate) data_url: String,
}

pub(super) struct ReadResult {
    pub(super) value: Value,
    pub(super) images: Vec<ToolImage>,
}

#[derive(Default)]
pub(crate) struct WebCache(VecDeque<(String, WebText)>);

#[derive(Clone)]
struct WebText {
    text: String,
    final_url: String,
    content_type: String,
    note: Option<&'static str>,
}

impl WebCache {
    pub(crate) fn clear(&mut self) {
        self.0.clear();
    }

    fn get(&mut self, key: &str) -> Option<WebText> {
        let index = self.0.iter().position(|(k, _)| k == key)?;
        let item = self.0.remove(index)?;
        let value = item.1.clone();
        self.0.push_back(item);
        Some(value)
    }

    fn insert(&mut self, key: String, value: WebText) {
        self.0.retain(|(k, _)| k != &key);
        if value.text.len() > MAX_CACHE_BYTES {
            return;
        }
        while self.0.len() >= 4
            || self.0.iter().map(|(_, v)| v.text.len()).sum::<usize>() + value.text.len()
                > MAX_CACHE_BYTES
        {
            self.0.pop_front();
        }
        self.0.push_back((key, value));
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Range {
    Lines(usize, Option<usize>),
    Tail(usize),
}

#[derive(Debug, Default)]
struct Target {
    source: String,
    ranges: Vec<Range>,
    raw: bool,
    explicit: bool,
}

fn positive(value: &str) -> Result<usize> {
    let value = value.strip_prefix('L').unwrap_or(value);
    let n = value.parse::<usize>().ok().filter(|n| *n > 0);
    n.context("read line numbers and counts must be positive integers")
}

fn range(value: &str, raw: bool, multiple: bool) -> Result<Range> {
    if let Some(tail) = value.strip_prefix('-') {
        return Ok(Range::Tail(positive(tail)?));
    }
    if let Some((start, count)) = value.split_once('+') {
        let start = positive(start)?;
        let end = start
            .checked_add(positive(count)? - 1)
            .context("read range overflow")?;
        return Ok(Range::Lines(start, Some(end)));
    }
    if let Some((start, end)) = value.split_once('-').or_else(|| value.split_once("..")) {
        let start = positive(start)?;
        let end = if end.is_empty() {
            None
        } else {
            Some(positive(end)?)
        };
        ensure!(
            end.is_none_or(|end| end >= start),
            "read range end precedes start"
        );
        return Ok(Range::Lines(start, end));
    }
    let start = positive(value)?;
    Ok(Range::Lines(start, (raw || multiple).then_some(start)))
}

fn looks_like_range(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || b"L-+.,:".contains(&b))
}

fn url_parts(source: &str) -> (&str, &str) {
    if source.starts_with("http://") || source.starts_with("https://") {
        source
            .find(['?', '#'])
            .map_or((source, ""), |i| source.split_at(i))
    } else {
        (source, "")
    }
}

fn suffixed(source: &str, suffix: &str) -> String {
    let (head, rest) = url_parts(source);
    format!("{head}{suffix}{rest}")
}

impl Target {
    fn parse(path: &str, cwd: &Path) -> Result<Self> {
        ensure!(!path.trim().is_empty(), "path must not be blank");
        let web = path.starts_with("http://") || path.starts_with("https://");
        // POSIX 中包含冒号的真实文件名优先，不能误删合法路径尾部。
        if !web && cwd.join(path).symlink_metadata().is_ok() {
            return Ok(Self {
                source: path.into(),
                ..Self::default()
            });
        }
        let (head, rest) = url_parts(path);
        let mut source = head;
        let mut raw = false;
        if let Some(prefix) = source.strip_suffix(":raw") {
            source = prefix;
            raw = true;
        }
        let mut selected = None;
        if let Some((prefix, suffix)) = source.rsplit_once(':')
            && looks_like_range(suffix)
            && (!web
                || prefix
                    .split_once("://")
                    .is_some_and(|(_, v)| v.contains('/')))
        {
            selected = Some(suffix);
            source = prefix;
        }
        if let Some(prefix) = source.strip_suffix(":raw") {
            source = prefix;
            raw = true;
        }
        let ranges = match selected {
            Some(value) => {
                let chunks: Vec<_> = value.split(',').collect();
                ensure!(chunks.len() <= 128, "too many read ranges");
                chunks
                    .iter()
                    .map(|v| range(v.trim_start_matches(':'), raw, chunks.len() > 1))
                    .collect::<Result<_>>()?
            }
            None => Vec::new(),
        };
        ensure!(!source.is_empty(), "read source must not be empty");
        Ok(Self {
            source: format!("{source}{rest}"),
            ranges,
            raw,
            explicit: selected.is_some() || raw,
        })
    }

    fn resolved_ranges(
        &self,
        total: impl FnOnce() -> Result<usize>,
    ) -> Result<Vec<(usize, usize)>> {
        let lines = if self.ranges.iter().any(|v| matches!(v, Range::Tail(_))) {
            total()?
        } else {
            0
        };
        let mut ranges: Vec<_> = self
            .ranges
            .iter()
            .filter_map(|r| match *r {
                Range::Lines(start, end) => Some((start, end.unwrap_or(usize::MAX))),
                Range::Tail(count) if lines > 0 => {
                    Some((lines.saturating_sub(count).saturating_add(1), lines))
                }
                Range::Tail(_) => None,
            })
            .collect();
        if self.ranges.is_empty() {
            ranges.push((1, usize::MAX));
        }
        ranges.sort_unstable();
        let mut merged: Vec<(usize, usize)> = Vec::new();
        for (start, end) in ranges {
            if let Some(last) = merged.last_mut()
                && start <= last.1.saturating_add(1)
            {
                last.1 = last.1.max(end);
            } else {
                merged.push((start, end));
            }
        }
        Ok(merged)
    }

    fn next_path(&self, ranges: &[(usize, usize)]) -> String {
        let selection = ranges
            .iter()
            .map(|(start, end)| {
                if *end == usize::MAX {
                    format!("{start}-")
                } else {
                    format!("{start}-{end}")
                }
            })
            .collect::<Vec<_>>()
            .join(",");
        suffixed(
            &self.source,
            &format!(":{selection}{}", if self.raw { ":raw" } else { "" }),
        )
    }
}

fn count_lines(mut reader: impl BufRead) -> Result<usize> {
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

fn open(path: &Path) -> Result<std::fs::File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NONBLOCK.bits() as i32)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    ensure!(file.metadata()?.is_file(), "read requires a regular file");
    Ok(file)
}

fn page_text(
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
            content.push_str(&super::hashline::render_page(&result.bytes, start));
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

fn image(bytes: &[u8], source: &str) -> Result<ReadResult> {
    use base64::Engine;
    use image::{ImageFormat, ImageReader, Limits};
    ensure!(bytes.len() <= MAX_INPUT_BYTES, "image exceeds 20 MiB limit");
    let format =
        image::guess_format(bytes).map_err(|_| anyhow::anyhow!("unsupported image format"))?;
    ensure!(
        matches!(
            format,
            ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::WebP | ImageFormat::Gif
        ),
        "unsupported image format"
    );
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = Limits::default();
    limits.max_alloc = Some(256 * 1024 * 1024);
    reader.limits(limits.clone());
    let (width, height) = ImageReader::with_format(Cursor::new(bytes), format)
        .into_dimensions()
        .map_err(|_| anyhow::anyhow!("invalid image dimensions"))?;
    ensure!(
        u64::from(width) * u64::from(height) <= 32_000_000,
        "image exceeds 32000000 pixel limit"
    );
    let decoded = reader
        .decode()
        .map_err(|_| anyhow::anyhow!("image decoding failed"))?;
    let mut converted = Cursor::new(Vec::new());
    let (data, mime) = if format == ImageFormat::Gif {
        decoded
            .write_to(&mut converted, ImageFormat::Png)
            .map_err(|_| anyhow::anyhow!("image conversion failed"))?;
        (converted.get_ref().as_slice(), "image/png")
    } else {
        (bytes, format.to_mime_type())
    };
    ensure!(
        data.len() <= MAX_INPUT_BYTES,
        "converted image exceeds 20 MiB limit"
    );
    let data_url = format!(
        "data:{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(data)
    );
    let content = format!(
        "Image: {source}\nFormat: {mime}\nDimensions: {width} x {height}\nBytes: {}{}",
        bytes.len(),
        if format == ImageFormat::Gif {
            "\nGIF: first frame"
        } else {
            ""
        }
    );
    Ok(ReadResult {
        value: json!({"ok":true,"kind":"image","content":content,"mime_type":mime,"width":width,"height":height,"bytes":bytes.len()}),
        images: vec![ToolImage { data_url }],
    })
}

fn is_image(bytes: &[u8]) -> bool {
    image::guess_format(bytes).is_ok()
}

fn convert_web(response: HttpResponse, raw: bool) -> Result<WebText> {
    let body = String::from_utf8_lossy(&response.bytes);
    ensure!(
        !response.bytes.contains(&0),
        "unsupported binary web response"
    );
    let html = response.content_type.contains("html")
        || body
            .trim_start()
            .to_ascii_lowercase()
            .starts_with("<!doctype html")
        || body.trim_start().starts_with("<html");
    let mut note = None;
    let text = if html && !raw {
        let extracted =
            dom_smoothie::Readability::new(body.as_ref(), Some(&response.final_url), None)
                .ok()
                .and_then(|mut reader| reader.parse().ok())
                .map(|article| article.content.to_string());
        let content = match extracted {
            Some(ref text) => text.as_str(),
            None => {
                note = Some("正文提取失败，已回退整页 HTML 转文本。");
                body.as_ref()
            }
        };
        html2text::from_read(content.as_bytes(), 120)
            .map_err(|_| anyhow::anyhow!("HTML conversion failed"))?
    } else {
        body.into_owned()
    };
    Ok(WebText {
        text,
        final_url: response.final_url,
        content_type: response.content_type,
        note,
    })
}

pub(super) fn run(
    path: &str,
    cwd: &Path,
    limits: ToolLimits,
    mode: EditMode,
    cache: &mut WebCache,
    fetch: &mut Fetch<'_>,
    cancellation: &CancellationToken,
) -> Result<ReadResult> {
    let target = Target::parse(path, cwd)?;
    let web = target.source.starts_with("http://") || target.source.starts_with("https://");
    if web {
        let key = format!("{}\n{}", target.source, target.raw);
        if !target.explicit {
            cache.0.retain(|(k, _)| k != &key);
        }
        let text = if let Some(cached) = cache.get(&key) {
            cached
        } else {
            let response = fetch(&target.source, cancellation)?;
            cancellation::check(cancellation)?;
            if is_image(&response.bytes) || response.content_type.starts_with("image/") {
                ensure!(
                    target.ranges.is_empty() && !target.raw,
                    "image reads do not accept selectors"
                );
                return image(&response.bytes, &target.source);
            }
            let text = convert_web(response, target.raw)?;
            cache.insert(key, text.clone());
            text
        };
        let ranges = target.resolved_ranges(|| count_lines(Cursor::new(text.text.as_bytes())))?;
        let mut value = page_text(&target, limits, mode, false, ranges, |start, count, max| {
            read_from(
                BufReader::new(Cursor::new(text.text.as_bytes())),
                start,
                count,
                max,
            )
        })?;
        value["kind"] = "url".into();
        value["final_url"] = text.final_url.into();
        value["content_type"] = text.content_type.into();
        if let Some(note) = text.note {
            value["note"] = note.into();
        }
        return Ok(ReadResult {
            value,
            images: Vec::new(),
        });
    }
    let local = cwd.join(&target.source);
    let mut file = open(&local)?;
    let mut header = [0; 32];
    let n = file.read(&mut header)?;
    let image_extension = local.extension().and_then(|s| s.to_str()).is_some_and(|s| {
        ["png", "jpg", "jpeg", "webp", "gif"]
            .iter()
            .any(|e| s.eq_ignore_ascii_case(e))
    });
    if is_image(&header[..n]) || image_extension {
        ensure!(
            target.ranges.is_empty() && !target.raw,
            "image reads do not accept selectors"
        );
        let mut bytes = Vec::new();
        open(&local)?
            .take(MAX_INPUT_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        return image(&bytes, &target.source);
    }
    ensure!(
        !header[..n].contains(&0),
        "read requires text or a supported image"
    );
    let ranges = target.resolved_ranges(|| count_lines(BufReader::new(open(&local)?)))?;
    let value = page_text(&target, limits, mode, true, ranges, |start, count, max| {
        read(&local, start, count, max)
    })?;
    Ok(ReadResult {
        value,
        images: Vec::new(),
    })
}

pub(super) struct Page {
    pub(super) bytes: Vec<u8>,
    pub(super) lines_read: usize,
    pub(super) next_offset: Option<usize>,
}

pub(super) fn read(path: &Path, offset: usize, limit: usize, max_bytes: usize) -> Result<Page> {
    read_from(BufReader::new(open(path)?), offset, limit, max_bytes)
}

fn read_from(
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
    use super::*;

    fn run_text(path: &str, cwd: &Path, limits: ToolLimits, mode: EditMode) -> Value {
        run(
            path,
            cwd,
            limits,
            mode,
            &mut WebCache::default(),
            &mut |_, _| unreachable!(),
            &CancellationToken::new(),
        )
        .unwrap()
        .value
    }

    #[test]
    fn read_selectors_are_exact_merge_ranges_and_preserve_newlines() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("file"),
            "中\r\ntwo\r\nthree\r\nfour\r\nlast",
        )
        .unwrap();
        for (suffix, expected) in [
            (":2-3", "two\r\nthree\r\n"),
            (":2+2", "two\r\nthree\r\n"),
            (":L2..L3", "two\r\nthree\r\n"),
            (":-2", "four\r\nlast"),
            (":2", "two\r\nthree\r\nfour\r\nlast"),
            (":2:raw", "two\r\n"),
            (":raw:2-3", "two\r\nthree\r\n"),
            (":2-:raw", "two\r\nthree\r\nfour\r\nlast"),
            (":4,2-3,1-2", "中\r\ntwo\r\nthree\r\nfour\r\n"),
            (":1,-2", "中\r\nfour\r\nlast"),
            (":99-100", ""),
        ] {
            let value = run_text(
                &format!("file{suffix}"),
                dir.path(),
                ToolLimits::default(),
                EditMode::Replace,
            );
            assert_eq!(value["content"], expected, "{suffix}");
            assert_eq!(value["truncated"], false, "{suffix}");
        }
        let anchored = run_text(
            "file:2,4",
            dir.path(),
            ToolLimits::default(),
            EditMode::Hashline,
        );
        assert!(anchored["content"].as_str().unwrap().starts_with("2:"));
        assert!(
            anchored["content"]
                .as_str()
                .unwrap()
                .lines()
                .nth(1)
                .unwrap()
                .starts_with("4:")
        );
        assert_eq!(
            run_text(
                "file:2:raw",
                dir.path(),
                ToolLimits::default(),
                EditMode::Hashline
            )["content"],
            "two\r\n"
        );
    }

    #[test]
    fn read_selectors_validate_literals_and_url_queries() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("file:20-30"), "literal").unwrap();
        assert_eq!(
            run_text(
                "file:20-30",
                dir.path(),
                ToolLimits::default(),
                EditMode::Replace
            )["content"],
            "literal"
        );
        for suffix in [
            ":0",
            ":-0",
            ":2-1",
            ":1+0",
            ":1,,2",
            ":999999999999999999999999",
            ":2+18446744073709551615",
        ] {
            assert!(
                Target::parse(&format!("missing{suffix}"), dir.path()).is_err(),
                "{suffix}"
            );
        }
        let url = Target::parse(
            "https://example.com:8443/page:2-5?q=text:20#x:3",
            dir.path(),
        )
        .unwrap();
        assert_eq!(url.source, "https://example.com:8443/page?q=text:20#x:3");
        assert_eq!(
            url.next_path(&[(3, 5)]),
            "https://example.com:8443/page:3-5?q=text:20#x:3"
        );
        assert_eq!(
            Target::parse("https://example.com:8443", dir.path())
                .unwrap()
                .source,
            "https://example.com:8443"
        );
    }

    #[test]
    fn read_multirange_next_path_never_loses_pending_ranges() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("file"), "a\nb\nc\nd\ne\nf\ng\n").unwrap();
        let limits = ToolLimits {
            read_max_lines: 2,
            read_max_bytes: 4,
            ..Default::default()
        };
        let first = run_text("file:1-3,5-7:raw", dir.path(), limits, EditMode::Hashline);
        assert_eq!(first["content"], "a\nb\n");
        assert_eq!(first["next_path"], "file:3-3,5-7:raw");
        let second = run_text(
            first["next_path"].as_str().unwrap(),
            dir.path(),
            limits,
            EditMode::Hashline,
        );
        assert_eq!(second["content"], "c\ne\n");
        assert_eq!(second["next_path"], "file:6-7:raw");
        let third = run_text(
            second["next_path"].as_str().unwrap(),
            dir.path(),
            limits,
            EditMode::Hashline,
        );
        assert_eq!(third["content"], "f\ng\n");
        assert_eq!(third["next_path"], Value::Null);
        // 第二个区间放不进当前页时，保留整个区间供下一页读取。
        std::fs::write(dir.path().join("file"), "a\nskip\nlong\n").unwrap();
        let first = run_text(
            "file:1,3",
            dir.path(),
            ToolLimits {
                read_max_bytes: 5,
                ..Default::default()
            },
            EditMode::Replace,
        );
        assert_eq!(first["content"], "a\n");
        assert_eq!(first["next_path"], "file:3-3");
        assert_eq!(
            run_text(
                first["next_path"].as_str().unwrap(),
                dir.path(),
                ToolLimits {
                    read_max_bytes: 5,
                    ..Default::default()
                },
                EditMode::Replace
            )["content"],
            "long\n"
        );
    }

    #[test]
    fn read_web_extracts_body_and_reuses_bounded_range_cache() {
        let mut cache = WebCache::default();
        let mut fetches = 0;
        let mut fetch = |_: &str, _: &CancellationToken| {
            fetches += 1;
            Ok(HttpResponse { bytes: b"<html><head><title>Read fixture</title></head><body><nav>Navigation</nav><main><article><h1>Article</h1><p>Main text with <a href='https://example.com/link'>a link</a>.</p><pre>fn main() {}</pre></article></main><script>secretScript()</script></body></html>".to_vec(), final_url: "https://example.com/final".into(), content_type: "text/html".into() })
        };
        let cancel = CancellationToken::new();
        let limits = ToolLimits {
            read_max_lines: 1,
            ..Default::default()
        };
        let first = run(
            "https://example.com/",
            Path::new("."),
            limits,
            EditMode::Hashline,
            &mut cache,
            &mut fetch,
            &cancel,
        )
        .unwrap();
        assert_eq!(first.value["kind"], "url");
        assert_eq!(first.value["final_url"], "https://example.com/final");
        let path = first.value["next_path"].as_str().unwrap();
        let next = run(
            path,
            Path::new("."),
            ToolLimits::default(),
            EditMode::Hashline,
            &mut cache,
            &mut fetch,
            &cancel,
        )
        .unwrap();
        let body = next.value["content"].as_str().unwrap();
        assert!(
            body.contains("Main text")
                && body.contains("fn main()")
                && body.contains("https://example.com/link"),
            "{body}"
        );
        assert!(!body.contains("secretScript"));
        run(
            "https://example.com/",
            Path::new("."),
            limits,
            EditMode::Replace,
            &mut cache,
            &mut fetch,
            &cancel,
        )
        .unwrap();
        run(
            "https://example.com/:raw",
            Path::new("."),
            ToolLimits::default(),
            EditMode::Replace,
            &mut cache,
            &mut fetch,
            &cancel,
        )
        .unwrap();
        assert_eq!(fetches, 3);
        for index in 0..6 {
            cache.insert(
                index.to_string(),
                WebText {
                    text: "x".repeat(2 * 1024 * 1024),
                    final_url: String::new(),
                    content_type: String::new(),
                    note: None,
                },
            );
        }
        assert_eq!(cache.0.len(), 4);
        assert!(cache.0.iter().map(|(_, v)| v.text.len()).sum::<usize>() <= MAX_CACHE_BYTES);
        cache.clear();
        assert!(cache.0.is_empty());
    }

    #[test]
    fn read_images_validate_and_keep_payload_out_of_display() {
        use base64::Engine;
        for format in [
            image::ImageFormat::Png,
            image::ImageFormat::Jpeg,
            image::ImageFormat::WebP,
            image::ImageFormat::Gif,
        ] {
            let mut bytes = Cursor::new(Vec::new());
            image::DynamicImage::new_rgb8(2, 3)
                .write_to(&mut bytes, format)
                .unwrap();
            let result = image(bytes.get_ref(), "fixture").unwrap();
            assert_eq!(result.value["width"], 2);
            assert_eq!(result.value["height"], 3);
            assert_eq!(result.images.len(), 1);
            assert!(!result.value.to_string().contains("base64"));
            let data = result.images[0].data_url.split_once(',').unwrap().1;
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(data)
                .unwrap();
            assert!(image::load_from_memory(&decoded).is_ok());
            if format == image::ImageFormat::Gif {
                assert_eq!(result.value["mime_type"], "image/png");
            }
        }
        assert!(image(b"not an image", "fixture").is_err());
        assert!(image(&vec![0; MAX_INPUT_BYTES + 1], "fixture").is_err());
        assert!(image(b"GIF89a\xff\xff\xff\xff\x00\x00\x00", "fixture").is_err());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image.png");
        image::DynamicImage::new_rgb8(1, 1).save(&path).unwrap();
        assert!(
            run(
                "image.png:1",
                dir.path(),
                ToolLimits::default(),
                EditMode::Replace,
                &mut WebCache::default(),
                &mut |_, _| unreachable!(),
                &CancellationToken::new()
            )
            .is_err()
        );
    }

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
