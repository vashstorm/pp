//! 带图片附件的用户消息排版：高亮正文内联标记，并将尾部原始元数据收拢为紧凑附件条。

use super::*;
use pp_config::config::IconMode;
use std::borrow::Cow;
use std::collections::HashMap;
use std::path::Path;

const MAX_ALIGNED_NAME_WIDTH: usize = 32;
const MAX_ALIGNED_NAME_SPREAD: usize = 20;

#[derive(Debug, PartialEq, Eq)]
struct UserImage<'a> {
    /// 正文中可能出现的原始标记（如 `"\u{f03e} #1"`、`"[图片 1]"` 或 `"@shot.png"`）。
    inline_marker: &'a str,
    /// 提取的附件序号（纯数字）。
    number: Option<&'a str>,
    /// 原始 `Image: <source>` 文本。
    source: &'a str,
    /// `@path` 直接引用时保留原始引用路径。
    reference_path: Option<&'a str>,
    mime: &'a str,
    dimensions: (u32, u32),
    bytes: u64,
    gif_first_frame: bool,
    resized_from: Option<(u32, u32)>,
}

impl TranscriptEntry {
    /// 若用户消息尾部包含结构化图片附件元数据，返回重排版后的行；普通纯文本返回 `None`。
    pub(super) fn user_body_lines(&self, width: u16) -> Option<Vec<LayoutLine>> {
        let (body, images) = parse_user_images(self.visible_text())?;
        let icons = crate::tui::themes::current_icons();
        // `render.rs` 左侧已有 2 列 inset；宽屏时右侧也留出 2 列对称余白。
        let inner = if width > 6 { width - 2 } else { width.max(1) };
        let mut rows = Vec::new();
        let has_body = !is_markers_only(body, &images);
        if has_body {
            for logical in body.trim_end_matches('\n').split('\n') {
                let line = style_user_body_line(logical, &images, icons);
                rows.extend(wrap_block(line, inner, ""));
            }
            rows.push(Line::default());
        }
        rows.extend(format_attachment_rows(
            &images,
            self.cwd.as_deref(),
            inner,
            icons,
        ));
        Some(
            rows.into_iter()
                .map(|line| LayoutLine {
                    content: LayoutContent::Styled(line),
                    height: 1,
                })
                .collect(),
        )
    }
}

/// 从用户消息尾部贪心解析连续的图片元数据块；若不存在完整块则返回 `None`。
fn parse_user_images(text: &str) -> Option<(&str, Vec<UserImage<'_>>)> {
    let mut lines: Vec<(usize, &str)> = Vec::new();
    let mut offset = 0;
    for line in text.split('\n') {
        lines.push((offset, line));
        offset += line.len() + 1;
    }

    let mut end = lines.len();
    let mut images = Vec::new();
    while end >= 4 {
        let Some((start, image)) = parse_trailing_image(&lines[..end]) else {
            break;
        };
        images.push(image);
        end = start;
    }
    if images.is_empty() {
        return None;
    }
    images.reverse();

    let body = if end == 0 {
        ""
    } else {
        let cut = lines[end].0.saturating_sub(1);
        &text[..cut]
    };
    Some((body, images))
}

fn parse_trailing_image<'a>(lines: &[(usize, &'a str)]) -> Option<(usize, UserImage<'a>)> {
    let mut idx = lines.len();

    let mut resized_from = None;
    if idx > 0
        && let Some(dims) = parse_resized_line(lines[idx - 1].1)
    {
        resized_from = Some(dims);
        idx -= 1;
    }

    let mut gif_first_frame = false;
    if idx > 0 && lines[idx - 1].1 == "GIF: first frame" {
        gif_first_frame = true;
        idx -= 1;
    }

    if idx < 4 {
        return None;
    }
    let bytes = lines[idx - 1]
        .1
        .strip_prefix("Bytes: ")?
        .parse::<u64>()
        .ok()?;
    let dimensions = parse_dimensions(lines[idx - 2].1.strip_prefix("Dimensions: ")?)?;
    let mime = lines[idx - 3].1.strip_prefix("Format: ")?.trim();
    if mime.is_empty() || !mime.contains('/') {
        return None;
    }
    let header_idx = idx - 4;
    let header = lines[header_idx].1;

    // 形态 1：`[图片引用 N] @path` 后跟 `Image: <source>`。
    if let Some(source) = header.strip_prefix("Image: ")
        && !source.is_empty()
        && header_idx > 0
        && let Some((number, ref_path, inline_marker)) =
            parse_reference_header(lines[header_idx - 1].1)
    {
        return Some((
            header_idx - 1,
            UserImage {
                inline_marker,
                number: Some(number),
                source,
                reference_path: Some(ref_path),
                mime,
                dimensions,
                bytes,
                gif_first_frame,
                resized_from,
            },
        ));
    }

    // 形态 2：`<marker> Image: <source>`（标准附件与旧 `[图片 N]` 历史）。
    let (marker, source) = header.split_once(" Image: ")?;
    if marker.is_empty() || source.is_empty() {
        return None;
    }
    let number = extract_marker_number(marker);
    Some((
        header_idx,
        UserImage {
            inline_marker: marker,
            number,
            source,
            reference_path: None,
            mime,
            dimensions,
            bytes,
            gif_first_frame,
            resized_from,
        },
    ))
}

fn parse_dimensions(raw: &str) -> Option<(u32, u32)> {
    let (w, h) = raw.split_once(" x ")?;
    let w = w.trim().parse::<u32>().ok()?;
    let h = h.trim().parse::<u32>().ok()?;
    (w > 0 && h > 0).then_some((w, h))
}

fn parse_resized_line(line: &str) -> Option<(u32, u32)> {
    let rest = line.strip_prefix("Resized from: ")?;
    let dims = rest.split(" (").next().unwrap_or(rest);
    parse_dimensions(dims)
}

fn parse_reference_header(line: &str) -> Option<(&str, &str, &str)> {
    let rest = line.strip_prefix("[图片引用 ")?;
    let (number, tail) = rest.split_once("] ")?;
    if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let path = tail.strip_prefix('@')?;
    if path.is_empty() {
        return None;
    }
    Some((number, path, tail))
}

fn extract_marker_number(marker: &str) -> Option<&str> {
    if let Some((_, digits)) = marker.rsplit_once('#')
        && !digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit())
    {
        return Some(digits);
    }
    if let Some(digits) = marker
        .strip_prefix("[图片 ")
        .and_then(|rest| rest.strip_suffix(']'))
        && !digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit())
    {
        return Some(digits);
    }
    None
}

fn badge_label(image: &UserImage<'_>, icons: IconMode) -> String {
    if let Some(number) = image.number {
        match icons {
            IconMode::Nerd => format!("{} #{number}", crate::tui::attachment_edit::ICON),
            IconMode::Ascii => format!("[img #{number}]"),
        }
    } else {
        pp_core::text::display_text(image.inline_marker).into_owned()
    }
}

/// 若正文除去已绑定的附件标记与空白后为空，则不重复渲染纯标记行。
fn is_markers_only(body: &str, images: &[UserImage<'_>]) -> bool {
    let mut cursor = 0;
    while cursor < body.len() {
        let rest = &body[cursor..];
        if let Some((len, _)) = match_inline_marker(rest, images) {
            cursor += len;
            continue;
        }
        let ch = rest.chars().next().expect("valid utf-8 boundary");
        if !ch.is_whitespace() {
            return false;
        }
        cursor += ch.len_utf8();
    }
    true
}

fn style_user_body_line(logical: &str, images: &[UserImage<'_>], icons: IconMode) -> Line<'static> {
    let text_style = Style::default().fg(palette::text());
    let badge_style = Style::default()
        .fg(palette::attachment_color())
        .add_modifier(Modifier::BOLD);
    let mut spans = Vec::new();
    let mut cursor = 0;
    let mut plain_start = 0;

    while cursor < logical.len() {
        if let Some((len, image)) = match_inline_marker(&logical[cursor..], images) {
            if plain_start < cursor {
                spans.push(Span::styled(
                    logical[plain_start..cursor].to_owned(),
                    text_style,
                ));
            }
            let display = if image.reference_path.is_some() {
                pp_core::text::display_text(&logical[cursor..cursor + len]).into_owned()
            } else {
                badge_label(image, icons)
            };
            spans.push(Span::styled(display, badge_style));
            cursor += len;
            plain_start = cursor;
        } else {
            let ch = logical[cursor..]
                .chars()
                .next()
                .expect("valid utf-8 boundary");
            cursor += ch.len_utf8();
        }
    }
    if plain_start < logical.len() {
        spans.push(Span::styled(logical[plain_start..].to_owned(), text_style));
    }
    Line::from(spans)
}

/// 匹配最长的内联附件标记；以数字结尾的标记后不能紧跟 ASCII 数字，避免 `#1` 误切 `#10`。
fn match_inline_marker<'a>(
    rest: &str,
    images: &'a [UserImage<'a>],
) -> Option<(usize, &'a UserImage<'a>)> {
    let mut best: Option<(usize, &'a UserImage<'a>)> = None;
    for image in images {
        let marker = image.inline_marker;
        if let Some(after) = rest.strip_prefix(marker) {
            let ends_digit = marker.as_bytes().last().is_some_and(u8::is_ascii_digit);
            if ends_digit && after.as_bytes().first().is_some_and(u8::is_ascii_digit) {
                continue;
            }
            if best.is_none_or(|(len, _)| marker.len() > len) {
                best = Some((marker.len(), image));
            }
        }
        // `@"path with spaces.png"` 形式也视为匹配该图片引用。
        if let Some(ref_path) = image.reference_path
            && let Some(after_at) = rest.strip_prefix("@\"")
            && let Some(after_path) = after_at.strip_prefix(ref_path)
            && after_path.starts_with('"')
        {
            let len = 2 + ref_path.len() + 1;
            if best.is_none_or(|(best_len, _)| len > best_len) {
                best = Some((len, image));
            }
        }
    }
    best
}

fn format_attachment_rows(
    images: &[UserImage<'_>],
    cwd: Option<&Path>,
    inner_width: u16,
    icons: IconMode,
) -> Vec<Line<'static>> {
    let sources = display_sources(images, cwd);
    let badges: Vec<String> = images.iter().map(|img| badge_label(img, icons)).collect();
    let badge_col = badges
        .iter()
        .map(|label| Span::raw(label.as_str()).width())
        .max()
        .unwrap_or(0);
    let name_col = if images.len() > 1 {
        let mut min_w = usize::MAX;
        let mut max_w = 0;
        let mut count = 0;
        for name in &sources {
            let w = Span::raw(name.as_str()).width();
            if w <= MAX_ALIGNED_NAME_WIDTH {
                min_w = min_w.min(w);
                max_w = max_w.max(w);
                count += 1;
            }
        }
        if count > 1 && max_w.saturating_sub(min_w) <= MAX_ALIGNED_NAME_SPREAD {
            max_w
        } else {
            0
        }
    } else {
        0
    };

    let rail = match icons {
        IconMode::Nerd => "▎ ",
        IconMode::Ascii => "| ",
    };
    let rail_width = Span::raw(rail).width();
    let rail_style = Style::default().fg(palette::attachment_color());
    let badge_style = Style::default()
        .fg(palette::attachment_color())
        .add_modifier(Modifier::BOLD);
    let name_style = Style::default().fg(palette::activity());
    let content_width = usize::from(inner_width).saturating_sub(rail_width).max(1);
    let content_u16 = u16::try_from(content_width).unwrap_or(u16::MAX);

    let mut rows = Vec::new();
    for ((image, badge), source) in images.iter().zip(badges.iter()).zip(sources.iter()) {
        let badge_width = Span::raw(badge.as_str()).width();
        let badge_pad = " ".repeat(badge_col.saturating_sub(badge_width) + 2);
        let name_width = Span::raw(source.as_str()).width();
        let meta = meta_spans(image, icons);
        let meta_width: usize = meta.iter().map(Span::width).sum();
        let aligned_gap = if name_col > 0 && name_width <= name_col {
            name_col - name_width + 2
        } else {
            2
        };

        let single_line_width = badge_col + 2 + name_width + aligned_gap + meta_width;
        if single_line_width <= content_width {
            let mut spans = Vec::with_capacity(meta.len() + 5);
            spans.push(Span::styled(rail, rail_style));
            spans.push(Span::styled(badge.clone(), badge_style));
            spans.push(Span::raw(badge_pad));
            spans.push(Span::styled(source.clone(), name_style));
            spans.push(Span::raw(" ".repeat(aligned_gap)));
            spans.extend(meta);
            rows.push(Line::from(spans));
            continue;
        }

        // 宽度不足时先拆成「编号 + 文件名」与「规格元信息」两段，避免把尺寸和体积劈断。
        let first_line = Line::from(vec![
            Span::styled(badge.clone(), badge_style),
            Span::raw(badge_pad),
            Span::styled(source.clone(), name_style),
        ]);
        let indent_cols = if badge_col + 2 + meta_width <= content_width {
            badge_col + 2
        } else {
            2.min(content_width.saturating_sub(1))
        };
        let indent = " ".repeat(indent_cols);

        for mut wrapped in wrap_block(first_line, content_u16, &indent) {
            wrapped.spans.insert(0, Span::styled(rail, rail_style));
            rows.push(wrapped);
        }

        let mut meta_line_spans = Vec::with_capacity(meta.len() + 1);
        if !indent.is_empty() {
            meta_line_spans.push(Span::raw(indent.clone()));
        }
        meta_line_spans.extend(meta);
        for mut wrapped in wrap_block(Line::from(meta_line_spans), content_u16, &indent) {
            wrapped.spans.insert(0, Span::styled(rail, rail_style));
            rows.push(wrapped);
        }
    }
    rows
}

fn meta_spans(image: &UserImage<'_>, icons: IconMode) -> Vec<Span<'static>> {
    let meta_style = Style::default().fg(palette::muted());
    let dot_style = Style::default().fg(palette::dim());
    let mut parts = vec![
        format_mime(image.mime).into_owned(),
        format_dimensions(image.dimensions, icons),
        format_bytes(image.bytes),
    ];
    if image.gif_first_frame {
        parts.push("first frame".into());
    }
    if let Some(orig) = image.resized_from {
        parts.push(format!("resized from {}", format_dimensions(orig, icons)));
    }
    let mut spans = Vec::with_capacity(parts.len() * 2);
    for (index, part) in parts.into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(" · ", dot_style));
        }
        spans.push(Span::styled(part, meta_style));
    }
    spans
}

/// 生成易读来源名：项目内显示相对路径，项目外提取文件名；同名外部文件补充一级父目录消歧。
fn display_sources(images: &[UserImage<'_>], cwd: Option<&Path>) -> Vec<String> {
    let base: Vec<(String, bool)> = images
        .iter()
        .map(|image| {
            if let Some(ref_path) = image.reference_path {
                return (pp_core::text::display_text(ref_path).into_owned(), false);
            }
            let source = image.source;
            if source.contains("://") || (!source.contains('/') && !source.contains('\\')) {
                return (pp_core::text::display_text(source).into_owned(), false);
            }
            let rel = super::entry_tool::display_path(cwd, source);
            if rel != source {
                return (pp_core::text::display_text(rel).into_owned(), false);
            }
            let name = Path::new(source)
                .file_name()
                .and_then(|s| s.to_str())
                .filter(|s| !s.is_empty())
                .unwrap_or(source);
            (pp_core::text::display_text(name).into_owned(), true)
        })
        .collect();

    let mut counts: HashMap<&str, usize> = HashMap::new();
    for (name, shortened) in &base {
        if *shortened {
            *counts.entry(name.as_str()).or_default() += 1;
        }
    }

    images
        .iter()
        .zip(base.iter())
        .map(|(image, (name, shortened))| {
            if *shortened
                && counts.get(name.as_str()).copied().unwrap_or(0) > 1
                && let Some(parent) = Path::new(image.source)
                    .parent()
                    .and_then(|p| p.file_name())
                    .and_then(|s| s.to_str())
                    .filter(|s| !s.is_empty())
            {
                let parent = pp_core::text::display_text(parent);
                format!("{parent}/{name}")
            } else {
                name.clone()
            }
        })
        .collect()
}

fn format_mime(mime: &str) -> Cow<'static, str> {
    if mime.eq_ignore_ascii_case("image/png") {
        Cow::Borrowed("PNG")
    } else if mime.eq_ignore_ascii_case("image/jpeg") || mime.eq_ignore_ascii_case("image/jpg") {
        Cow::Borrowed("JPEG")
    } else if mime.eq_ignore_ascii_case("image/webp") {
        Cow::Borrowed("WebP")
    } else if mime.eq_ignore_ascii_case("image/gif") {
        Cow::Borrowed("GIF")
    } else {
        Cow::Owned(
            mime.strip_prefix("image/")
                .unwrap_or(mime)
                .to_ascii_uppercase(),
        )
    }
}

fn format_dimensions((width, height): (u32, u32), icons: IconMode) -> String {
    match icons {
        IconMode::Nerd => format!("{width}×{height}"),
        IconMode::Ascii => format!("{width}x{height}"),
    }
}

fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * 1024;
    if bytes < KB {
        format!("{bytes} B")
    } else if bytes < MB {
        let kb = bytes as f64 / KB as f64;
        if kb >= 100.0 || ((kb * 10.0).round() as u64).is_multiple_of(10) {
            format!("{kb:.0} KB")
        } else {
            format!("{kb:.1} KB")
        }
    } else {
        let mb = bytes as f64 / MB as f64;
        format!("{mb:.1} MB")
    }
}
