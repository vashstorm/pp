use super::Read;
use super::ReadContext;
use super::ReadResult;
use super::image::ImageInput;
use super::image::ImageReader;
use super::image::is_image;
use super::selector::Target;
use super::text::count_lines;
use super::text::page_text;
use super::text::read_from;
use anyhow::{Result, ensure};
use pp_core::cancellation;
use pp_llm::http::HttpResponse;
use std::{
    collections::VecDeque,
    io::{BufReader, Cursor},
};

const MAX_CACHE_BYTES: usize = 8 * 1024 * 1024;

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

pub(super) struct UrlReader;
pub(super) struct UrlInput(pub(super) Target);

impl Read for UrlReader {
    type Input = UrlInput;

    fn read(
        &self,
        UrlInput(target): Self::Input,
        context: &mut ReadContext<'_, '_>,
    ) -> Result<ReadResult> {
        let key = format!("{}\n{}", target.source, target.raw);
        if !target.explicit {
            context.cache.0.retain(|(k, _)| k != &key);
        }
        let text = if let Some(cached) = context.cache.get(&key) {
            cached
        } else {
            let response = (context.fetch)(&target.source, context.cancellation)?;
            cancellation::check(context.cancellation)?;
            if is_image(&response.bytes) || response.content_type.starts_with("image/") {
                return ImageReader.read(
                    ImageInput {
                        target,
                        bytes: response.bytes,
                    },
                    context,
                );
            }
            let text = convert_web(response, target.raw)?;
            context.cache.insert(key, text.clone());
            text
        };
        let ranges = target.resolved_ranges(|| count_lines(Cursor::new(text.text.as_bytes())))?;
        let mut value = page_text(
            &target,
            context.limits,
            context.mode,
            false,
            ranges,
            |start, count, max| {
                read_from(
                    BufReader::new(Cursor::new(text.text.as_bytes())),
                    start,
                    count,
                    max,
                )
            },
        )?;
        value["kind"] = "url".into();
        value["final_url"] = text.final_url.into();
        value["content_type"] = text.content_type.into();
        if let Some(note) = text.note {
            value["note"] = note.into();
        }
        Ok(ReadResult {
            value,
            images: Vec::new(),
        })
    }
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
        let stripped_fallback;
        let content = match extracted {
            Some(ref text) => text.as_str(),
            None => {
                note = Some("content extraction failed; fell back to full-page HTML as text");
                stripped_fallback = strip_html_noise(body.as_ref());
                stripped_fallback.as_str()
            }
        };
        let rendered = html2text::from_read(content.as_bytes(), 120)
            .map_err(|_| anyhow::anyhow!("HTML conversion failed"))?;
        compact_web_text(&rendered)
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

/// Readability 失败回退整页 HTML 时，先剥离常见非正文噪声标签块，降低无效 token。
fn strip_html_noise(html: &str) -> String {
    const NOISE_TAGS: &[&str] = &[
        "script", "style", "noscript", "svg", "nav", "footer", "aside", "header",
    ];
    let lower = html.to_ascii_lowercase();
    let mut out = String::with_capacity(html.len());
    let mut cursor = 0usize;
    let bytes = lower.as_bytes();
    while cursor < html.len() {
        let Some(rel_lt) = lower[cursor..].find('<') else {
            out.push_str(&html[cursor..]);
            break;
        };
        let tag_start = cursor + rel_lt;
        out.push_str(&html[cursor..tag_start]);
        let rest = &lower[tag_start + 1..];
        let matched = NOISE_TAGS.iter().copied().find(|tag| {
            rest.starts_with(tag)
                && rest
                    .as_bytes()
                    .get(tag.len())
                    .is_some_and(|b| b.is_ascii_whitespace() || matches!(b, b'>' | b'/'))
        });
        if let Some(tag) = matched {
            let close_needle = format!("</{tag}");
            if let Some(rel_close) = lower[tag_start + 1..].find(&close_needle) {
                let close_pos = tag_start + 1 + rel_close + close_needle.len();
                if let Some(rel_gt) = lower[close_pos..].find('>') {
                    cursor = close_pos + rel_gt + 1;
                    continue;
                }
            }
        }
        // 非噪声标签：仅推进 `<`，保留原字符（保证 UTF-8 字符边界）。
        let next = tag_start + 1;
        out.push('<');
        cursor = if next < bytes.len() { next } else { html.len() };
    }
    out
}

/// 折叠行尾空白与连续 3 个以上空行，保留正文段落与代码块结构。
fn compact_web_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank_run = 0usize;
    for line in text.lines() {
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            blank_run += 1;
            if blank_run <= 1 && !out.is_empty() {
                out.push('\n');
            }
        } else {
            blank_run = 0;
            out.push_str(trimmed);
            out.push('\n');
        }
    }
    while out.ends_with("\n\n") {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::read::run;
    use pp_config::config::EditMode;
    use pp_config::config::ToolLimits;
    use pp_core::cancellation::CancellationToken;
    use std::{cell::Cell, path::Path};

    #[test]
    fn read_web_extracts_body_and_reuses_bounded_range_cache() {
        let mut cache = WebCache::default();
        let fetches = Cell::new(0);
        let mut fetch = |_: &str, _: &CancellationToken| {
            fetches.set(fetches.get() + 1);
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
            &pp_config::skills::Skills::default().shared(),
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
            &pp_config::skills::Skills::default().shared(),
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
        assert_eq!(fetches.get(), 1);
        run(
            "https://example.com/",
            Path::new("."),
            limits,
            EditMode::Replace,
            &pp_config::skills::Skills::default().shared(),
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
            &pp_config::skills::Skills::default().shared(),
            &mut cache,
            &mut fetch,
            &cancel,
        )
        .unwrap();
        assert_eq!(fetches.get(), 3);
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
    fn strip_html_noise_and_compact_web_text_remove_boilerplate_and_extra_blank_lines() {
        let html = "<html><head><style>.a{color:red}</style></head><body>\
            <header>Top Banner</header><nav>Menu Item</nav>\
            <div class='main'>Keep Chinese 正文 &amp; code</div>\
            <aside>Sidebar Ad</aside><svg><path d='M0 0'/></svg>\
            <script>alert(1)</script><footer>Copyright 2099</footer></body></html>";
        let stripped = strip_html_noise(html);
        assert!(stripped.contains("Keep Chinese 正文"), "{stripped}");
        for noise in [
            "Top Banner",
            "Menu Item",
            "Sidebar Ad",
            "alert(1)",
            "Copyright 2099",
            "color:red",
        ] {
            assert!(
                !stripped.contains(noise),
                "noise `{noise}` leaked: {stripped}"
            );
        }
        let compacted = compact_web_text("\n\nLine 1   \n\n\n\nLine 2\t \n\n\n");
        assert_eq!(compacted, "Line 1\n\nLine 2\n");
    }
}
