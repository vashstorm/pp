//! Resource snapshot of the main chat input; the protocol layer receives only prepared content.
use anyhow::{Context, Result, ensure};
use pp_config::config::ToolLimits;
use pp_core::cancellation::CancellationToken;
use pp_core::cancellation::Cancelled;
use pp_tools::read;
use serde::{Deserialize, Serialize};
use std::{
    io::Read,
    path::{Path, PathBuf},
};

#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
pub enum Part {
    Text(String),
    Image(std::sync::Arc<str>),
}

#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
pub struct Image {
    pub marker: String,
    pub summary: String,
    pub data_url: std::sync::Arc<str>,
    /// Exact position in the draft source; legacy records lack this field and keep restoring by the old marker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_range: Option<std::ops::Range<usize>>,
}

impl Image {
    pub fn dimensions(&self) -> String {
        self.summary
            .lines()
            .find_map(|line| line.strip_prefix("Dimensions: "))
            .unwrap_or_default()
            .replace(' ', "")
    }
}

#[derive(Clone, Default, Serialize, Deserialize, Debug, PartialEq)]
pub struct UserInput {
    pub source: String,
    pub display: String,
    pub parts: Vec<Part>,
    /// Only pasted images are saved along with the recalled input; @paths are read again when resent.
    pub images: Vec<Image>,
}

impl UserInput {
    pub fn has_images(&self) -> bool {
        self.parts.iter().any(|p| matches!(p, Part::Image(_)))
    }
}

#[derive(Debug, PartialEq)]
pub struct Reference {
    pub start: usize,
    pub end: usize,
    /// None means an escaped @@ that never touches the filesystem.
    pub path: Option<String>,
}

/// Shares reference boundaries with completion; @ inside code fences and inline code does not trigger a read.
pub fn references(text: &str) -> Vec<Reference> {
    let mut refs = Vec::new();
    let mut fence: Option<(u8, usize)> = None;
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        let first = trimmed.as_bytes().first().copied();
        let ticks = trimmed.bytes().take_while(|b| Some(*b) == first).count();
        if matches!(first, Some(b'`' | b'~')) && ticks >= 3 {
            if fence.is_none() {
                fence = Some((first.unwrap_or(b'`'), ticks));
            } else if fence.is_some_and(|(ch, n)| Some(ch) == first && ticks >= n) {
                fence = None;
            }
            offset += line.len();
            continue;
        }
        if fence.is_some() {
            offset += line.len();
            continue;
        }
        let mut cursor = 0;
        let mut inline = 0;
        while cursor < line.len() {
            let rest = &line[cursor..];
            let ch = rest.chars().next().expect("char boundary");
            if ch == '`' {
                let n = rest.bytes().take_while(|b| *b == b'`').count();
                if inline == 0 {
                    inline = n;
                } else if inline == n {
                    inline = 0;
                }
                cursor += n;
                continue;
            }
            let boundary = cursor == 0
                || line[..cursor]
                    .chars()
                    .next_back()
                    .is_some_and(char::is_whitespace);
            if inline != 0 || ch != '@' || !boundary {
                cursor += ch.len_utf8();
                continue;
            }
            if rest.starts_with("@@") {
                refs.push(Reference {
                    start: offset + cursor,
                    end: offset + cursor + 2,
                    path: None,
                });
                cursor += 2;
                continue;
            }
            let start = cursor;
            cursor += 1;
            if line[cursor..].starts_with('"') {
                let mut end = cursor + 1;
                let mut escaped = false;
                let mut closed = false;
                while end < line.len() {
                    let c = line[end..].chars().next().expect("char boundary");
                    end += c.len_utf8();
                    if c == '"' && !escaped {
                        closed = true;
                        break;
                    }
                    escaped = c == '\\' && !escaped;
                }
                if closed
                    && (end == line.len() || line[end..].starts_with(char::is_whitespace))
                    && let Ok(path) = serde_json::from_str::<String>(&line[cursor..end])
                {
                    refs.push(Reference {
                        start: offset + start,
                        end: offset + end,
                        path: Some(path),
                    });
                }
                cursor = end;
            } else {
                while cursor < line.len() && !line[cursor..].starts_with(char::is_whitespace) {
                    cursor += line[cursor..]
                        .chars()
                        .next()
                        .expect("char boundary")
                        .len_utf8();
                }
                if cursor > start + 1 {
                    refs.push(Reference {
                        start: offset + start,
                        end: offset + cursor,
                        path: Some(line[start + 1..cursor].into()),
                    });
                }
            }
        }
        offset += line.len();
    }
    refs
}

pub fn resolve(path: &str, cwd: &Path) -> Result<PathBuf> {
    ensure!(
        !path.is_empty() && !path.chars().any(char::is_control),
        "invalid local reference path"
    );
    ensure!(
        !path.contains("://"),
        "the first version of @references supports local files and directories only"
    );
    let path = if let Some(rest) = path.strip_prefix("~/") {
        PathBuf::from(std::env::var_os("HOME").context("cannot locate the home directory")?)
            .join(rest)
    } else {
        cwd.join(path)
    };
    Ok(path)
}

pub fn image_file(path: &Path) -> Result<Image> {
    let mut bytes = Vec::new();
    read::open_local_file(path)?
        .take(pp_llm::http::MAX_INPUT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    image_bytes(&path.display().to_string(), &bytes)
}

pub fn image_bytes(source: &str, bytes: &[u8]) -> Result<Image> {
    let result = read::decode_image(source, bytes)?;
    Ok(Image {
        marker: String::new(),
        source_range: None,
        summary: result.value["content"].as_str().unwrap_or_default().into(),
        data_url: result
            .images
            .into_iter()
            .next()
            .context("image encoding produced no result")?
            .data_url
            .into(),
    })
}

pub fn image_extension(path: &Path) -> bool {
    path.extension().and_then(|v| v.to_str()).is_some_and(|s| {
        ["png", "jpg", "jpeg", "webp", "gif"]
            .iter()
            .any(|e| s.eq_ignore_ascii_case(e))
    })
}

/// Only decides whether a paste could be an image path; the input thread must never touch the filesystem.
pub fn possible_image_paste(text: &str) -> bool {
    let trimmed = text.trim().trim_end_matches(['\'', '"']);
    let lower = trimmed.to_ascii_lowercase();
    [".png", ".jpg", ".jpeg", ".webp", ".gif"]
        .iter()
        .any(|suffix| lower.ends_with(suffix))
}

/// Accepts complete paths, quoted paths, shell-escaped paths and file:// URIs; no image is attached when body text is mixed in.
pub fn pasted_paths(text: &str, cwd: &Path) -> Option<Vec<PathBuf>> {
    fn candidate(text: &str, cwd: &Path) -> Option<PathBuf> {
        let decoded;
        let text = if let Some(uri) = text.strip_prefix("file://") {
            if !uri.starts_with('/') {
                return None;
            }
            decoded = percent_encoding::percent_decode_str(uri)
                .decode_utf8()
                .ok()?
                .into_owned();
            &decoded
        } else {
            text
        };
        let path = resolve(text, cwd).ok()?;
        (image_extension(&path) && path.is_file()).then_some(path)
    }
    let text = text.trim();
    if let Some(path) = candidate(text, cwd) {
        return Some(vec![path]);
    }
    let mut tokens = Vec::new();
    let mut token = String::new();
    let mut quote = None;
    let mut escaped = false;
    for c in text.chars() {
        if escaped {
            token.push(c);
            escaped = false;
            continue;
        }
        if c == '\\' && quote != Some('\'') {
            escaped = true;
            continue;
        }
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else {
                token.push(c);
            }
        } else if c == '"' || c == '\'' {
            quote = Some(c);
        } else if c.is_whitespace() {
            if !token.is_empty() {
                tokens.push(std::mem::take(&mut token));
            }
        } else {
            token.push(c);
        }
    }
    if escaped || quote.is_some() {
        return None;
    }
    if !token.is_empty() {
        tokens.push(token);
    }
    if tokens.is_empty() {
        return None;
    }
    tokens.iter().map(|t| candidate(t, cwd)).collect()
}

pub fn prepare(
    source: String,
    text: &str,
    images: Vec<Image>,
    cwd: &Path,
    limits: ToolLimits,
    cancellation: &CancellationToken,
) -> Result<UserInput> {
    let mut input = UserInput {
        display: source.clone(),
        source,
        ..Default::default()
    };
    let mut segments: Vec<(usize, usize, Option<String>, Option<&Image>)> = references(text)
        .into_iter()
        .map(|r| (r.start, r.end, r.path, None))
        .collect();
    for image in &images {
        let start = image
            .source_range
            .as_ref()
            .and_then(|range| {
                if input.source.get(range.clone()) != Some(image.marker.as_str()) {
                    return None;
                }
                let start = if let Some(offset) = input.source.find(text) {
                    range.start.checked_sub(offset)?
                } else if let Some(offset) = text.find(&input.source) {
                    range.start.checked_add(offset)?
                } else {
                    // Template expansion may shift positions; accept only the single real attachment marker.
                    let mut matches = text.match_indices(&image.marker);
                    let (start, _) = matches.next()?;
                    if matches.next().is_some() {
                        return None;
                    }
                    start
                };
                let end = start.checked_add(image.marker.len())?;
                (text.get(start..end) == Some(image.marker.as_str())).then_some(start)
            })
            .or_else(|| {
                image
                    .source_range
                    .is_none()
                    .then(|| text.find(&image.marker))
                    .flatten()
            });
        if let Some(start) = start {
            // An image placeholder in code still maps to the real attachment; trust only links inside the draft.
            segments.retain(|(a, b, _, _)| *b <= start || *a >= start + image.marker.len());
            segments.push((start, start + image.marker.len(), None, Some(image)));
            input.images.push(image.clone());
        }
    }
    segments.sort_by_key(|s| s.0);
    let mut cursor = 0;
    for (start, end, path, image) in segments {
        if cancellation.is_cancelled() {
            return Err(Cancelled.into());
        }
        if start < cursor {
            continue;
        }
        push_text(&mut input.parts, &text[cursor..start]);
        if let Some(image) = image {
            push_text(
                &mut input.parts,
                &format!("{}\n{}\n", image.marker, image.summary),
            );
            input.parts.push(Part::Image(image.data_url.clone()));
            input
                .display
                .push_str(&format!("\n{} {}", image.marker, image.summary));
        } else if let Some(path) = path {
            let local = resolve(&path, cwd)?;
            // A selector must not be allowed to interpret a full filename that does not exist.
            let meta =
                std::fs::metadata(&local).with_context(|| format!("cannot reference {path}"))?;
            ensure!(
                meta.is_file() || meta.is_dir(),
                "cannot reference a special file: {path}"
            );
            let result = read::local_reference(&local, limits, cancellation)
                .with_context(|| format!("cannot reference {path}"))?;
            let content = result.value["content"].as_str().unwrap_or_default();
            ensure!(!content.contains('\0'), "file is not text: {path}");
            push_text(
                &mut input.parts,
                &format!(
                    "\n<resource path={}>\n{}\n",
                    serde_json::to_string(&path)?,
                    content
                ),
            );
            if result.value["truncated"] == true {
                let next = result.value["next_path"].as_str().unwrap_or(&path);
                let note = format!("content truncated; use read to continue: {next}");
                push_text(&mut input.parts, &format!("{note}\n"));
                input.display.push_str(&format!("\n@{path}: {note}"));
            }
            for image in result.images {
                let number = input
                    .parts
                    .iter()
                    .filter(|p| matches!(p, Part::Image(_)))
                    .count()
                    + 1;
                input
                    .display
                    .push_str(&format!("\n[图片引用 {number}] @{path}\n{content}"));
                input.parts.push(Part::Image(image.data_url.into()));
            }
            push_text(&mut input.parts, "</resource>\n");
        } else {
            push_text(&mut input.parts, "@");
        }
        cursor = end;
    }
    push_text(&mut input.parts, &text[cursor..]);
    if cancellation.is_cancelled() {
        return Err(Cancelled.into());
    }
    Ok(input)
}

fn push_text(parts: &mut Vec<Part>, text: &str) {
    if text.is_empty() {
        return;
    }
    if let Some(Part::Text(previous)) = parts.last_mut() {
        previous.push_str(text);
    } else {
        parts.push(Part::Text(text.into()));
    }
}

pub(crate) fn chat_parts(
    parts: &[Part],
) -> Vec<async_openai::types::chat::ChatCompletionRequestUserMessageContentPart> {
    use async_openai::types::chat as c;
    parts
        .iter()
        .map(|p| match p {
            Part::Text(text) => c::ChatCompletionRequestUserMessageContentPart::Text(
                c::ChatCompletionRequestMessageContentPartText {
                    text: text.clone(),
                    prompt_cache_breakpoint: None,
                },
            ),
            Part::Image(url) => c::ChatCompletionRequestUserMessageContentPart::ImageUrl(
                c::ChatCompletionRequestMessageContentPartImage {
                    image_url: c::ImageUrl {
                        url: url.to_string(),
                        detail: None,
                    },
                    prompt_cache_breakpoint: None,
                },
            ),
        })
        .collect()
}
pub(crate) fn response_parts(parts: &[Part]) -> Vec<async_openai::types::responses::InputContent> {
    use async_openai::types::responses as r;
    parts
        .iter()
        .map(|p| match p {
            Part::Text(text) => r::InputContent::InputText(r::InputTextContent {
                text: text.clone(),
                prompt_cache_breakpoint: None,
            }),
            Part::Image(url) => r::InputContent::InputImage(r::InputImageContent {
                image_url: Some(url.to_string()),
                ..Default::default()
            }),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn prepare_text(text: &str, cwd: &Path) -> Result<UserInput> {
        prepare(
            text.into(),
            text,
            Vec::new(),
            cwd,
            ToolLimits::default(),
            &CancellationToken::new(),
        )
    }
    fn joined(input: &UserInput) -> String {
        input
            .parts
            .iter()
            .filter_map(|p| {
                if let Part::Text(t) = p {
                    Some(t.as_str())
                } else {
                    None
                }
            })
            .collect()
    }

    #[test]
    fn resource_references_snapshots_and_truncation() {
        {
            let text = "a@b.com `@inline` @@literal @one\n```rust\n@decorator\n```\n~~~\n@fenced\n~~~\n@\"目录/my file.txt\"";
            assert_eq!(
                references(text)
                    .into_iter()
                    .map(|r| r.path)
                    .collect::<Vec<_>>(),
                vec![None, Some("one".into()), Some("目录/my file.txt".into())]
            );
            assert_eq!(
                joined(&prepare_text("@@literal", Path::new(".")).unwrap()),
                "@literal"
            );
        }

        {
            let dir = tempfile::tempdir().unwrap();
            let project = dir.path().join("project");
            std::fs::create_dir(&project).unwrap();
            std::fs::create_dir(project.join("目录")).unwrap();
            std::fs::write(project.join("目录/my file.txt"), "你好\nsecond\n").unwrap();
            std::fs::write(dir.path().join("outside.txt"), "outside").unwrap();
            let input = prepare_text(
                "查看 @\"目录/my file.txt\" 和 @目录/ @../outside.txt",
                &project,
            )
            .unwrap();
            let text = joined(&input);
            assert!(text.contains("你好"));
            assert!(text.contains("my file.txt"));
            assert!(text.contains("outside"));
            std::fs::write(dir.path().join("outside.txt"), "changed").unwrap();
            assert_eq!(joined(&input), text);
            assert!(prepare_text("@../outside.txt:1", &project).is_err());
            assert!(prepare_text("@missing", &project).is_err());
            assert!(prepare_text("@https://example.com", &project).is_err());
        }

        {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("file"), "first\nsecond\nthird\n").unwrap();
            let input = prepare(
                "@file".into(),
                "@file",
                Vec::new(),
                dir.path(),
                ToolLimits {
                    read_max_lines: 1,
                    ..Default::default()
                },
                &CancellationToken::new(),
            )
            .unwrap();
            assert!(input.display.contains("truncated"));
            assert!(joined(&input).contains("first"));
            assert!(!joined(&input).contains("second"));
            assert!(joined(&input).contains(":2"));
            for bytes in [
                vec![b'a'; 100].into_iter().chain([0]).collect::<Vec<_>>(),
                vec![0xff, 0xfe, 0xfd],
                vec![0xe4, 0xb8],
            ] {
                std::fs::write(dir.path().join("binary"), bytes).unwrap();
                assert!(prepare_text("@binary", dir.path()).is_err());
            }
        }
    }

    #[test]
    fn resource_images_and_pasted_paths() {
        {
            let dir = tempfile::tempdir().unwrap();
            image::DynamicImage::new_rgb8(2, 3)
                .save(dir.path().join("shot.png"))
                .unwrap();
            let input = prepare_text("before @shot.png after", dir.path()).unwrap();
            let mut bound = image_file(&dir.path().join("shot.png")).unwrap();
            bound.marker = "#1".into();
            for range in [usize::MAX..usize::MAX, 0..usize::MAX] {
                bound.source_range = Some(range);
                let input = prepare(
                    "#1".into(),
                    "#1",
                    vec![bound.clone()],
                    dir.path(),
                    ToolLimits::default(),
                    &CancellationToken::new(),
                )
                .unwrap();
                assert!(!input.has_images());
            }
            assert!(
                matches!(&input.parts[..], [Part::Text(before), Part::Image(url), Part::Text(after)] if before.contains("before") && url.starts_with("data:image/png;base64,") && after.contains("after"))
            );
            std::fs::remove_file(dir.path().join("shot.png")).unwrap();
            assert!(input.has_images());
            let cancelled = CancellationToken::new();
            cancelled.cancel();
            assert!(
                prepare(
                    "plain".into(),
                    "plain",
                    Vec::new(),
                    dir.path(),
                    ToolLimits::default(),
                    &cancelled
                )
                .is_err()
            );
        }

        {
            let dir = tempfile::tempdir().unwrap();
            for name in ["one.png", "my shot.png"] {
                image::DynamicImage::new_rgb8(1, 1)
                    .save(dir.path().join(name))
                    .unwrap();
            }
            for text in [
                "one.png",
                "\"my shot.png\"",
                "my\\ shot.png",
                "my shot.png",
                "one.png 'my shot.png'",
            ] {
                assert!(pasted_paths(text, dir.path()).is_some(), "{text}");
            }
            let uri = format!("file://{}/my%20shot.png", dir.path().display());
            assert!(pasted_paths(&uri, dir.path()).is_some());
            for text in [
                "explain one.png",
                "one.png missing.png",
                "one.png\nnotes",
                "'one.png",
            ] {
                assert!(pasted_paths(text, dir.path()).is_none());
            }
        }
    }
}
