//! Assistant Markdown: semantic document + physical-line layout.
//!
//! `parse` turns assistant text into a [`MarkdownDocument`] of semantic blocks
//! (paragraph/heading lines with resolved list/quote prefixes, fenced/indented
//! code frames, rules, tables). `layout` renders those blocks into already
//! wrapped physical [`Line`]s for a given terminal width; the caller draws them
//! with a non-wrapping Paragraph and clips at the viewport. Wrapping never
//! splits a grapheme: a single grapheme wider than the available columns keeps
//! its own physical line and is clipped by the viewport.
//!
//! 流式更新完整解析累计正文，保留后置引用、列表续行与 Setext 标题语义。
//! 后台按语义块与宽度复用物理行，避免未变化块重复换行。

use std::borrow::Cow;
use std::ops::Range;
use std::sync::Arc;

use pulldown_cmark::{Alignment, CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;

use super::transcript_style::CODE_BLOCK;
use super::transcript_style::DIM;
use super::transcript_style::LINK;
use super::transcript_style::MUTED;
use super::transcript_style::{MD_BORDER, MD_BULLET, MD_CODE, MD_HEADING, MD_STRONG};
use pp_core::text::display_text;

mod highlight;

pub(super) fn highlight_shell_document(
    language: &str,
    text: &str,
) -> Option<std::sync::Arc<Vec<Vec<Span<'static>>>>> {
    highlight::highlight(Some(language), text)
}

pub(super) fn highlight_shell_segment(language: &str, text: &str) -> Option<Vec<Span<'static>>> {
    highlight::highlight(Some(language), text).and_then(|lines| lines.first().cloned())
}

pub(super) fn release_highlight_context(id: u64) {
    highlight::release_context(id);
}

pub(super) fn highlight_context(id: u64, epoch: u64) {
    highlight::context(id, epoch);
}

const TAB_STOP: usize = 4;
const CELL_MIN_WIDTH: usize = 3;

pub(super) struct MarkdownDocument {
    blocks: Vec<Block>,
    cached: Vec<Option<(u16, Vec<Line<'static>>)>>,
    pub(super) table_tail_start: Option<usize>,
}

#[derive(PartialEq)]
enum Block {
    /// Lines with resolved hanging prefixes (list markers, quote bars).
    Paragraph(Vec<WrapLine>, Option<usize>),
    Heading(Vec<WrapLine>, u8),
    Code(CodeBlock),
    Rule(Vec<Span<'static>>),
    Table(TableBlock),
}

/// One logical source line: styled spans plus the prefix shown on its first
/// physical line (`first`, e.g. `• `, `│ `) and on wrapped continuations
/// (`rest`, the hanging indent).
#[derive(PartialEq)]
struct WrapLine {
    first: Vec<Span<'static>>,
    rest: Vec<Span<'static>>,
    spans: Vec<Span<'static>>,
}

#[derive(PartialEq)]
struct CodeBlock {
    lang: Option<String>,
    /// 已着色的每行 spans（tab 已展开；高亮失败时为纯色单 span）。
    lines: Arc<Vec<Vec<Span<'static>>>>,
    closed: bool,
    first: Vec<Span<'static>>,
    rest: Vec<Span<'static>>,
}

#[derive(PartialEq)]
struct TableBlock {
    aligns: Vec<Alignment>,
    header: Vec<Cell>,
    rows: Vec<Vec<Cell>>,
    first: Vec<Span<'static>>,
    rest: Vec<Span<'static>>,
}

type Cell = Vec<Span<'static>>;

pub(super) struct HeadingAnchor {
    pub level: u8,
    pub title: String,
    pub row: usize,
}

mod parse;
mod table;

use self::table::layout_table;
pub(super) use parse::{parse, parse_streaming};
pub(super) use table::table_tail_line;

impl MarkdownDocument {
    pub(super) fn ends_with_table(&self) -> bool {
        matches!(self.blocks.last(), Some(Block::Table(_)))
    }

    // 保留全篇语义解析，只复用语义相等块的物理行。
    pub(super) fn update(&mut self, source: &str, streaming: bool) {
        let mut next = parse_streaming(source, streaming);
        next.cached = next
            .blocks
            .iter()
            .enumerate()
            .map(|(i, block)| {
                if self.blocks.get(i) == Some(block) {
                    self.cached.get_mut(i).and_then(Option::take)
                } else {
                    None
                }
            })
            .collect();
        *self = next;
    }
    pub(super) fn layout_cached(&mut self, width: u16) -> Vec<Line<'static>> {
        if width == 0 {
            return Vec::new();
        }
        self.cached.resize_with(self.blocks.len(), || None);
        let mut out = Vec::new();
        let mut previous_group = None;
        for (i, block) in self.blocks.iter().enumerate() {
            let group = match block {
                Block::Paragraph(_, group) => *group,
                _ => None,
            };
            if i > 0 && (group.is_none() || group != previous_group) {
                out.push(Line::from(""));
            }
            previous_group = group;
            let slot = &mut self.cached[i];
            if slot.as_ref().is_none_or(|(w, _)| *w != width) {
                let mut rows = Vec::new();
                layout_block(block, usize::from(width), &mut rows);
                *slot = Some((width, rows));
            }
            out.extend(slot.as_ref().expect("块排版已完成").1.iter().cloned());
        }
        out
    }

    /// Physical lines for `width` terminal columns. `width == 0` yields no
    /// lines. Lines are already wrapped; draw without a second wrap pass.
    pub(super) fn layout(&self, width: u16) -> Vec<Line<'static>> {
        self.layout_blocks(width, |_, _| {})
    }

    /// 目录锚点与正文共用同一轮排版，行号包含块间空行与实际换行。
    pub(super) fn layout_with_headings(
        &self,
        width: u16,
    ) -> (Vec<Line<'static>>, Vec<HeadingAnchor>) {
        let mut headings = Vec::new();
        let lines = self.layout_blocks(width, |block, row| {
            if let Block::Heading(lines, level) = block {
                let title = lines
                    .iter()
                    .map(|line| {
                        line.spans
                            .iter()
                            .map(|span| span.content.as_ref())
                            .collect::<String>()
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                headings.push(HeadingAnchor {
                    level: *level,
                    title,
                    row,
                });
            }
        });
        (lines, headings)
    }

    fn layout_blocks(
        &self,
        width: u16,
        mut on_block: impl FnMut(&Block, usize),
    ) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        if width == 0 {
            return out;
        }
        let width = width as usize;
        let mut previous_group = None;
        for (i, block) in self.blocks.iter().enumerate() {
            let group = match block {
                Block::Paragraph(_, group) => *group,
                _ => None,
            };
            if i > 0 && (group.is_none() || group != previous_group) {
                out.push(Line::from(""));
            }
            previous_group = group;
            on_block(block, out.len());
            layout_block(block, width, &mut out);
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Layout
// ---------------------------------------------------------------------------

fn spans_width(spans: &[Span<'static>]) -> usize {
    spans.iter().map(Span::width).sum()
}

fn fit_prefix(prefix: &[Span<'static>], width: usize) -> Vec<Span<'static>> {
    let available = width.saturating_sub(1);
    if spans_width(prefix) <= available {
        return prefix.to_vec();
    }
    let mut result = Vec::new();
    let mut used = 0;
    for span in prefix {
        for grapheme in span.styled_graphemes(Style::new()) {
            let columns = Span::raw(grapheme.symbol).width();
            if used + columns > available {
                return result;
            }
            result.push(Span::styled(grapheme.symbol.to_owned(), grapheme.style));
            used += columns;
        }
    }
    result
}

fn layout_paragraph(lines: &[WrapLine], width: usize, out: &mut Vec<Line<'static>>) {
    for wl in lines {
        let first = fit_prefix(&wl.first, width);
        let rest = fit_prefix(&wl.rest, width);
        let fw = spans_width(&first);
        let rw = spans_width(&rest);
        let af = width.saturating_sub(fw).max(1);
        let ar = width.saturating_sub(rw).max(1);
        let content = wrap(WrapMode::Word, &wl.spans, af, ar);
        if content.is_empty() {
            out.push(Line::from(first));
        } else {
            for (i, mut line) in content.into_iter().enumerate() {
                let pre = if i == 0 { &first } else { &rest };
                let mut spans = fit_prefix(
                    pre,
                    width.saturating_sub(spans_width(&line)).saturating_add(1),
                );
                spans.append(&mut line);
                out.push(Line::from(spans));
            }
        }
    }
}

fn layout_code(cb: &CodeBlock, width: usize, out: &mut Vec<Line<'static>>) {
    let first = fit_prefix(&cb.first, width);
    let rest = fit_prefix(&cb.rest, width);
    let first_width = width.saturating_sub(spans_width(&first)).max(1);
    let body_width = width.saturating_sub(spans_width(&rest)).max(1);
    let mut top = first;
    top.extend(frame_top(cb.lang.as_deref(), first_width).spans);
    out.push(Line::from(top));
    for line_spans in cb.lines.iter() {
        let wrapped = wrap(WrapMode::Char, line_spans, body_width, body_width);
        if wrapped.is_empty() {
            out.push(Line::from(rest.clone()));
        }
        for line in wrapped {
            let mut spans = fit_prefix(
                &rest,
                width.saturating_sub(spans_width(&line)).saturating_add(1),
            );
            spans.extend(line);
            out.push(Line::from(spans));
        }
    }
    if cb.closed {
        let mut bottom = rest;
        bottom.extend(rule_line(body_width).spans);
        out.push(Line::from(bottom));
    }
}

fn frame_top(lang: Option<&str>, width: usize) -> Line<'static> {
    let dash = Style::new().fg(DIM);
    if let Some(lang) = lang.filter(|l| !l.is_empty()) {
        let label = format!(" {} ", lang);
        let lw = Span::raw(label.as_str()).width();
        if width > 2 + lw {
            return Line::from(vec![
                Span::styled("──", dash),
                Span::styled(label, Style::new().fg(MUTED)),
                Span::styled("─".repeat(width - 2 - lw), dash),
            ]);
        }
    }
    Line::from(vec![Span::styled("─".repeat(width), dash)])
}

fn rule_line(width: usize) -> Line<'static> {
    Line::from(vec![Span::styled("─".repeat(width), Style::new().fg(DIM))])
}

fn expand_tabs(line: &str, tabstop: usize) -> String {
    if !line.contains('\t') {
        return line.to_string();
    }
    let mut out = String::with_capacity(line.len());
    let mut col = 0usize;
    for (i, part) in line.split('\t').enumerate() {
        if i > 0 {
            let n = tabstop - col % tabstop;
            out.push_str(&" ".repeat(n));
            col += n;
        }
        if part.is_empty() {
            continue;
        }
        for g in Span::raw(part).styled_graphemes(Style::new()) {
            out.push_str(g.symbol);
            col += Span::raw(g.symbol).width();
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Grapheme-safe wrapping
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum WrapMode {
    /// Prose: break at whitespace runs, drop the break spaces, never split a
    /// grapheme; a grapheme wider than the line keeps its own physical line.
    Word,
    /// Code/table cells: exact greedy fill, spaces preserved.
    Char,
}

fn wrap(
    mode: WrapMode,
    spans: &[Span<'static>],
    first_avail: usize,
    rest_avail: usize,
) -> Vec<Vec<Span<'static>>> {
    let mut out: Vec<Vec<Span<'static>>> = Vec::new();
    let mut cur: Vec<Span<'static>> = Vec::new();
    let mut cur_w = 0usize;
    // (span index, byte offset) of the first space of the last whitespace run.
    let mut brk: Option<(usize, usize)> = None;
    let mut prev_space = false;

    for span in spans {
        for g in span.styled_graphemes(Style::new()) {
            let avail = if out.is_empty() {
                first_avail
            } else {
                rest_avail
            };
            let is_space = !g.symbol.is_empty() && g.symbol.chars().all(char::is_whitespace);
            let gw = Span::raw(g.symbol).width();
            if cur_w > 0 && cur_w + gw > avail {
                if mode == WrapMode::Word && is_space {
                    out.push(std::mem::take(&mut cur));
                    cur_w = 0;
                    brk = None;
                    prev_space = false;
                    continue;
                }
                let mut broke = false;
                if mode == WrapMode::Word
                    && let Some((si, off)) = brk
                {
                    let mut tail = split_spans(&mut cur, si, off);
                    trim_leading_spaces(&mut tail);
                    out.push(std::mem::take(&mut cur));
                    cur = tail;
                    cur_w = spans_width(&cur);
                    broke = true;
                }
                if !broke {
                    out.push(std::mem::take(&mut cur));
                    cur_w = 0;
                }
                brk = None;
                prev_space = false;
                let avail = if out.is_empty() {
                    first_avail
                } else {
                    rest_avail
                };
                if cur_w > 0 && cur_w + gw > avail {
                    out.push(std::mem::take(&mut cur));
                    cur_w = 0;
                }
            }
            if mode == WrapMode::Word && is_space && cur.is_empty() {
                prev_space = true;
                continue;
            }
            if mode == WrapMode::Word && is_space && !prev_space {
                brk = Some(match cur.last() {
                    Some(last) if last.style == g.style => (cur.len() - 1, last.content.len()),
                    _ => (cur.len(), 0),
                });
            }
            match cur.last_mut() {
                Some(last) if last.style == g.style => last.content.to_mut().push_str(g.symbol),
                _ => cur.push(Span::styled(g.symbol.to_string(), g.style)),
            }
            cur_w += gw;
            prev_space = is_space;
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Split `spans` at grapheme index `si`/byte offset `off`; returns the tail.
/// The offset is a grapheme start recorded during wrapping, so it is always a
/// char boundary inside the span.
fn split_spans(spans: &mut Vec<Span<'static>>, si: usize, off: usize) -> Vec<Span<'static>> {
    if si >= spans.len() {
        return Vec::new();
    }
    let tail_spans = spans.split_off(si + 1);
    let s = &mut spans[si];
    let style = s.style;
    let content = std::mem::replace(&mut s.content, Cow::Borrowed(""));
    let (head, tail) = match content {
        Cow::Owned(mut owned) => {
            let tail = owned.split_off(off.min(owned.len()));
            (owned, tail)
        }
        Cow::Borrowed(b) => {
            let o = off.min(b.len());
            (b[..o].to_string(), b[o..].to_string())
        }
    };
    let head_empty = head.is_empty();
    s.content = Cow::Owned(head);
    if head_empty {
        spans.pop();
    }
    let mut tail_vec = Vec::with_capacity(tail_spans.len() + 1);
    if !tail.is_empty() {
        tail_vec.push(Span::styled(tail, style));
    }
    tail_vec.extend(tail_spans);
    tail_vec
}

fn trim_leading_spaces(spans: &mut Vec<Span<'static>>) {
    while let Some(first) = spans.first() {
        if !first.content.starts_with(char::is_whitespace) {
            break;
        }
        let trimmed: String = first
            .content
            .trim_start_matches(char::is_whitespace)
            .to_string();
        if trimmed.is_empty() {
            spans.remove(0);
        } else {
            spans[0].content = Cow::Owned(trimmed);
            break;
        }
    }
}

fn trim_cell(cell: &mut Cell) {
    while let Some(first) = cell.first() {
        if !first.content.starts_with(char::is_whitespace) {
            break;
        }
        let trimmed: String = first
            .content
            .trim_start_matches(char::is_whitespace)
            .to_string();
        if trimmed.is_empty() {
            cell.remove(0);
        } else {
            cell[0].content = Cow::Owned(trimmed);
            break;
        }
    }
    while let Some(last) = cell.last() {
        if !last.content.ends_with(char::is_whitespace) {
            break;
        }
        let trimmed: String = last
            .content
            .trim_end_matches(char::is_whitespace)
            .to_string();
        if trimmed.is_empty() {
            cell.pop();
        } else {
            let n = cell.len();
            cell[n - 1].content = Cow::Owned(trimmed);
            break;
        }
    }
}

// ---------------------------------------------------------------------------
// Behavioral tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;

fn layout_block(block: &Block, width: usize, out: &mut Vec<Line<'static>>) {
    match block {
        Block::Paragraph(lines, _) | Block::Heading(lines, _) => {
            layout_paragraph(lines, width, out);
        }
        Block::Code(cb) => layout_code(cb, width, out),
        Block::Rule(prefix) => {
            let mut spans = fit_prefix(prefix, width);
            let available = width.saturating_sub(spans_width(&spans)).max(1);
            spans.extend(rule_line(available).spans);
            out.push(Line::from(spans));
        }
        Block::Table(tb) => {
            let first = fit_prefix(&tb.first, width);
            let rest = fit_prefix(&tb.rest, width);
            let available = width
                .saturating_sub(spans_width(&first).max(spans_width(&rest)))
                .max(1);
            let mut rows = Vec::new();
            layout_table(tb, available, &mut rows);
            for (index, row) in rows.into_iter().enumerate() {
                let prefix = if index == 0 { &first } else { &rest };
                let mut spans =
                    fit_prefix(prefix, width.saturating_sub(row.width()).saturating_add(1));
                spans.extend(row.spans);
                out.push(Line::from(spans));
            }
        }
    }
}

#[cfg(test)]
mod cache_tests {
    use super::*;
    #[test]
    fn markdown_block_cache_reduces_layout_work() {
        let prefix = "Stable paragraph with **bold** and 中文🙂.\n\n".repeat(100);
        let mut document = parse(&format!("{prefix}tail"));
        document.layout_cached(80);
        let mut cached_builds = 0;
        let mut full_builds = 0;
        for i in 0..20 {
            let source = format!("{prefix}tail{}", "x".repeat(i + 1));
            document.update(&source, false);
            cached_builds += document.cached.iter().filter(|slot| slot.is_none()).count();
            full_builds += document.blocks.len();
            assert_eq!(document.layout_cached(80), parse(&source).layout(80));
        }
        assert_eq!(cached_builds, 20);
        assert_eq!(full_builds, 2020);
        println!("METRIC cached_block_builds={cached_builds} full_block_builds={full_builds}");
    }

    #[test]
    fn markdown_block_cache_matches_full_semantics_after_updates_and_resize() {
        let mut document = parse("稳定段落中文🙂\n\n[link][id]\n\ntitle");
        document.layout_cached(40);
        document.update("稳定段落中文🙂\n\n[link][id]\n\ntitle\n---\n\n[id]: https://example.com\n\n```rust\nlet x = 1;", false);
        assert!(document.cached[0].is_some());
        assert!(document.cached[1].is_none());
        for source in [
            "稳定段落中文🙂\n\n[link][id]\n\ntitle\n---\n\n[id]: https://example.com\n\n```rust\nlet x = 1;",
            "- item\n\n  continuation\n\n| 表 | 值 |\n| - | - |\n| 🙂 | 中文 |",
            "替换正文\n\n> quote\n\n```rust\nlet x = 2;\n```",
        ] {
            document.update(source, false);
            for width in [80, 12, 40, 0] {
                assert_eq!(document.layout_cached(width), parse(source).layout(width));
            }
        }
    }
}
