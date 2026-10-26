use super::{
    HttpResponse, Read, ReadContext, ReadResult,
    image::{ImageInput, ImageReader, is_image},
    selector::Target,
    text::{count_lines, page_text, read_from},
};
use crate::cancellation;
use anyhow::{Result, ensure};
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::read::run;
    use crate::{
        cancellation::CancellationToken,
        config::{EditMode, ToolLimits},
    };
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
        assert_eq!(fetches.get(), 1);
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
}
