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

use super::transcript_style::{
    code_block, dim, link, md_border, md_bullet, md_code, md_heading, md_strong, muted,
};
use pp_core::text::display_text;

mod highlight;

pub(super) fn highlight_code_document(
    language: &str,
    text: &str,
) -> Option<std::sync::Arc<Vec<Arc<Vec<Span<'static>>>>>> {
    highlight::highlight(Some(language), text)
}

pub(super) fn highlight_shell_document(
    language: &str,
    text: &str,
) -> Option<std::sync::Arc<Vec<Arc<Vec<Span<'static>>>>>> {
    highlight_code_document(language, text)
}

pub(super) fn highlight_shell_segment(language: &str, text: &str) -> Option<Vec<Span<'static>>> {
    highlight::highlight(Some(language), text)
        .and_then(|lines| lines.first().map(|spans| (**spans).clone()))
}

pub(super) fn prewarm_highlight() {
    highlight::prewarm();
}

pub(super) fn release_highlight_context(id: u64) {
    highlight::release_context(id);
}

pub(super) fn highlight_context(id: u64, epoch: u64) {
    highlight::context(id, epoch);
}

const TAB_STOP: usize = 4;
const CELL_MIN_WIDTH: usize = 3;

enum CachedBlock {
    Plain {
        width: u16,
        rows: Vec<Line<'static>>,
    },
    Code {
        width: u16,
        top: Line<'static>,
        lines: Vec<(Arc<Vec<Span<'static>>>, Vec<Line<'static>>)>,
        closed: bool,
        bottom: Option<Line<'static>>,
    },
}

#[derive(Clone, Default)]
pub(super) struct TailMeta {
    /// 稳定前缀占用的物理行数（跨帧共享 Arc，绝不被预览改写）。
    pub(super) stable_rows: usize,
    /// 活跃尾块在 source 中的起始字节偏移。
    pub(super) tail_source_start: usize,
    /// 活跃尾块非空时，与稳定前缀之间是否需要插入一个空行。
    pub(super) needs_separator: bool,
    /// 末块为未闭合代码块时的续行前缀（用于在预览期 O(delta) 续接并保留既有语法高亮）。
    pub(super) open_code_prefix: Option<Vec<Span<'static>>>,
    /// 末行的悬挂缩进前缀（超大活跃块超限回退时保留列表/引用缩进）。
    pub(super) fallback_prefix: Vec<Span<'static>>,
}

pub(super) struct MarkdownDocument {
    blocks: Vec<Block>,
    /// 与 `blocks` 一一对应的顶层块行首字节偏移。
    block_top_starts: Vec<usize>,
    /// 最后一个顶层块是否为列表（列表受后续空行 tight→loose 规则影响，不提前封闭）。
    last_top_is_list: bool,
    /// 源文本末尾是否包含空行（用于判断已封闭的顶层块）。
    ends_with_blank_line: bool,
    /// 源文本末尾最后一个换行之后的位置（封闭切分点保留末尾水平缩进）。
    trailing_line_start: usize,
    cached: Vec<Option<CachedBlock>>,
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
    lines: Arc<Vec<Arc<Vec<Span<'static>>>>>,
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
pub(super) use parse::{parse, parse_streaming, parse_streaming_preview};
pub(super) use table::table_tail_line;

pub(super) fn wrap_word_preview(
    spans: &[Span<'static>],
    first_avail: usize,
    rest_avail: usize,
) -> Vec<Vec<Span<'static>>> {
    wrap(WrapMode::Word, spans, first_avail.max(1), rest_avail.max(1))
}

pub(super) fn wrap_char_preview(
    spans: &[Span<'static>],
    first_avail: usize,
    rest_avail: usize,
) -> Vec<Vec<Span<'static>>> {
    wrap(WrapMode::Char, spans, first_avail.max(1), rest_avail.max(1))
}

pub(super) fn expand_tabs_preview(line: &str) -> String {
    expand_tabs(line, TAB_STOP)
}

pub(super) fn fit_prefix_preview(prefix: &[Span<'static>], width: usize) -> Vec<Span<'static>> {
    fit_prefix(prefix, width)
}

pub(super) fn spans_width_preview(spans: &[Span<'static>]) -> usize {
    spans_width(spans)
}

impl MarkdownDocument {
    pub(super) fn ends_with_table(&self) -> bool {
        matches!(self.blocks.last(), Some(Block::Table(_)))
    }

    // 保留全篇语义解析，只复用语义相等块（或正在生长的同一代码块已定型行）的物理行。
    pub(super) fn update(&mut self, source: &str, streaming: bool) {
        let mut next = parse_streaming(source, streaming);
        next.cached = next
            .blocks
            .iter()
            .enumerate()
            .map(|(i, block)| {
                let matches = match (self.blocks.get(i), block) {
                    (Some(prev), next) if prev == next => true,
                    (Some(Block::Code(prev)), Block::Code(next)) => {
                        prev.lang == next.lang && prev.first == next.first && prev.rest == next.rest
                    }
                    _ => false,
                };
                matches
                    .then(|| self.cached.get_mut(i).and_then(Option::take))
                    .flatten()
            })
            .collect();
        *self = next;
    }
    #[cfg(test)]
    pub(super) fn layout_cached(&mut self, width: u16) -> Vec<Line<'static>> {
        self.layout_cached_with_meta(width).0
    }

    pub(super) fn layout_cached_with_meta(&mut self, width: u16) -> (Vec<Line<'static>>, TailMeta) {
        if width == 0 {
            return (Vec::new(), TailMeta::default());
        }
        self.cached.resize_with(self.blocks.len(), || None);
        let width_usize = usize::from(width);
        let mut out = Vec::new();
        let mut previous_group = None;
        let last_top = self.block_top_starts.last().copied().unwrap_or(0);
        let split_block = self
            .block_top_starts
            .iter()
            .position(|&start| start == last_top)
            .unwrap_or(self.blocks.len());
        let mut split_rows = 0usize;
        for (i, block) in self.blocks.iter().enumerate() {
            if i == split_block {
                split_rows = out.len();
            }
            let group = match block {
                Block::Paragraph(_, group) => *group,
                _ => None,
            };
            if i > 0 && (group.is_none() || group != previous_group) {
                out.push(Line::from(""));
            }
            previous_group = group;
            match (block, &mut self.cached[i]) {
                (
                    Block::Code(cb),
                    Some(CachedBlock::Code {
                        width: cached_width,
                        top,
                        lines,
                        closed,
                        bottom,
                    }),
                ) if *cached_width == width => {
                    let rest = fit_prefix(&cb.rest, width_usize);
                    let body_width = width_usize.saturating_sub(spans_width(&rest)).max(1);
                    let mut old_iter = std::mem::take(lines).into_iter();
                    let mut rebuilt = Vec::with_capacity(cb.lines.len());
                    for line_spans in cb.lines.iter() {
                        let reused = old_iter.next().and_then(|(old_spans, old_rows)| {
                            (Arc::ptr_eq(&old_spans, line_spans) || *old_spans == **line_spans)
                                .then_some(old_rows)
                        });
                        let rows = match reused {
                            Some(rows) => rows,
                            None => {
                                let mut rows = Vec::new();
                                wrap_code_line(
                                    line_spans,
                                    &rest,
                                    width_usize,
                                    body_width,
                                    &mut rows,
                                );
                                rows
                            }
                        };
                        rebuilt.push((line_spans.clone(), rows));
                    }
                    *lines = rebuilt;
                    if *closed != cb.closed {
                        *closed = cb.closed;
                        *bottom = cb.closed.then(|| code_bottom_line(cb, width_usize));
                    }
                    out.push(top.clone());
                    for (_, rows) in lines.iter() {
                        out.extend(rows.iter().cloned());
                    }
                    if let Some(bottom) = bottom {
                        out.push(bottom.clone());
                    }
                }
                (Block::Code(cb), slot) => {
                    let top = code_top_line(cb, width_usize);
                    let rest = fit_prefix(&cb.rest, width_usize);
                    let body_width = width_usize.saturating_sub(spans_width(&rest)).max(1);
                    let mut lines = Vec::with_capacity(cb.lines.len());
                    for line_spans in cb.lines.iter() {
                        let mut rows = Vec::new();
                        wrap_code_line(line_spans, &rest, width_usize, body_width, &mut rows);
                        lines.push((line_spans.clone(), rows));
                    }
                    let bottom = cb.closed.then(|| code_bottom_line(cb, width_usize));
                    out.push(top.clone());
                    for (_, rows) in &lines {
                        out.extend(rows.iter().cloned());
                    }
                    if let Some(bottom_line) = &bottom {
                        out.push(bottom_line.clone());
                    }
                    *slot = Some(CachedBlock::Code {
                        width,
                        top,
                        lines,
                        closed: cb.closed,
                        bottom,
                    });
                }
                (_, slot) => {
                    if !matches!(slot, Some(CachedBlock::Plain { width: w, .. }) if *w == width) {
                        let mut rows = Vec::new();
                        layout_block(block, width_usize, &mut rows);
                        *slot = Some(CachedBlock::Plain { width, rows });
                    }
                    if let Some(CachedBlock::Plain { rows, .. }) = slot {
                        out.extend(rows.iter().cloned());
                    }
                }
            }
        }
        let open_code_prefix = match self.blocks.last() {
            Some(Block::Code(cb)) if !cb.closed => Some(fit_prefix(&cb.rest, width_usize)),
            _ => None,
        };
        let fallback_prefix = match self.blocks.last() {
            Some(Block::Paragraph(lines, _) | Block::Heading(lines, _)) => lines
                .last()
                .map(|line| fit_prefix(&line.rest, width_usize))
                .unwrap_or_default(),
            Some(Block::Code(cb)) => fit_prefix(&cb.rest, width_usize),
            Some(Block::Table(tb)) => fit_prefix(&tb.rest, width_usize),
            Some(Block::Rule(_)) | None => Vec::new(),
        };
        let closed_by_blank = self.ends_with_blank_line
            && open_code_prefix.is_none()
            && !self.last_top_is_list
            && self.table_tail_start.is_none();
        let meta = if self.blocks.is_empty() {
            TailMeta::default()
        } else if closed_by_blank {
            TailMeta {
                stable_rows: out.len(),
                tail_source_start: self.trailing_line_start,
                needs_separator: !out.is_empty(),
                open_code_prefix,
                fallback_prefix: Vec::new(),
            }
        } else {
            TailMeta {
                stable_rows: split_rows,
                tail_source_start: last_top,
                needs_separator: split_rows > 0,
                open_code_prefix,
                fallback_prefix,
            }
        };
        (out, meta)
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

fn code_top_line(cb: &CodeBlock, width: usize) -> Line<'static> {
    let first = fit_prefix(&cb.first, width);
    let first_width = width.saturating_sub(spans_width(&first)).max(1);
    let mut top = first;
    top.extend(frame_top(cb.lang.as_deref(), first_width).spans);
    Line::from(top)
}

fn code_bottom_line(cb: &CodeBlock, width: usize) -> Line<'static> {
    let rest = fit_prefix(&cb.rest, width);
    let body_width = width.saturating_sub(spans_width(&rest)).max(1);
    let mut bottom = rest;
    bottom.extend(rule_line(body_width).spans);
    Line::from(bottom)
}

fn wrap_code_line(
    line_spans: &[Span<'static>],
    rest: &[Span<'static>],
    width: usize,
    body_width: usize,
    out: &mut Vec<Line<'static>>,
) {
    let wrapped = wrap(WrapMode::Char, line_spans, body_width, body_width);
    if wrapped.is_empty() {
        out.push(Line::from(rest.to_vec()));
        return;
    }
    for line in wrapped {
        let mut spans = fit_prefix(
            rest,
            width.saturating_sub(spans_width(&line)).saturating_add(1),
        );
        spans.extend(line);
        out.push(Line::from(spans));
    }
}

fn layout_code(cb: &CodeBlock, width: usize, out: &mut Vec<Line<'static>>) {
    let rest = fit_prefix(&cb.rest, width);
    let body_width = width.saturating_sub(spans_width(&rest)).max(1);
    out.push(code_top_line(cb, width));
    for line_spans in cb.lines.iter() {
        wrap_code_line(line_spans, &rest, width, body_width, out);
    }
    if cb.closed {
        out.push(code_bottom_line(cb, width));
    }
}

fn frame_top(lang: Option<&str>, width: usize) -> Line<'static> {
    let dash = Style::new().fg(dim());
    if let Some(lang) = lang.filter(|l| !l.is_empty()) {
        let label = format!(" {} ", lang);
        let lw = Span::raw(label.as_str()).width();
        if width > 2 + lw {
            return Line::from(vec![
                Span::styled("──", dash),
                Span::styled(label, Style::new().fg(muted())),
                Span::styled("─".repeat(width - 2 - lw), dash),
            ]);
        }
    }
    Line::from(vec![Span::styled("─".repeat(width), dash)])
}

fn rule_line(width: usize) -> Line<'static> {
    Line::from(vec![Span::styled(
        "─".repeat(width),
        Style::new().fg(dim()),
    )])
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
