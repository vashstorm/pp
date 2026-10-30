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
//! Streaming: the parent re-parses the full accumulated source for dirty
//! entries, so late reference definitions, list continuations and setext
//! headings correctly reflow earlier text. Nothing here caches across frames.

use std::borrow::Cow;
use std::ops::Range;
use std::sync::Arc;

use pulldown_cmark::{Alignment, CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::transcript_style::{ACCENT, CODE, CODE_BLOCK, DIM, LINK, MUTED, display_text};

mod highlight;

pub(super) fn release_highlight_context(id: u64) {
    highlight::release_context(id);
}

pub(super) fn highlight_context(id: u64, epoch: u64) {
    highlight::context(id, epoch);
}

const TAB_STOP: usize = 4;
const CELL_MIN_WIDTH: usize = 3;
const CELL_SEPARATOR: &str = " │ ";
const CELL_SEPARATOR_WIDTH: usize = 3;

pub(super) struct MarkdownDocument {
    blocks: Vec<Block>,
}

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
struct WrapLine {
    first: Vec<Span<'static>>,
    rest: Vec<Span<'static>>,
    spans: Vec<Span<'static>>,
}

struct CodeBlock {
    lang: Option<String>,
    /// 已着色的每行 spans（tab 已展开；高亮失败时为纯色单 span）。
    lines: Arc<Vec<Vec<Span<'static>>>>,
    closed: bool,
    first: Vec<Span<'static>>,
    rest: Vec<Span<'static>>,
}

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

pub(super) fn parse(source: &str) -> MarkdownDocument {
    parse_styled(source, false)
}

/// 指令正文保留源换行；frontmatter 按原文展示，避免被识别为 Setext 标题。
pub(super) fn parse_instruction(source: &str) -> MarkdownDocument {
    let Some(metadata) = source
        .strip_prefix("---\n")
        .or_else(|| source.strip_prefix("---\r\n"))
    else {
        return parse_styled(source, true);
    };
    let mut header = Vec::new();
    let mut offset = 0;
    let mut closed = false;
    for line in metadata.split_inclusive('\n') {
        offset += line.len();
        if line.trim_end() == "---" {
            closed = true;
            break;
        }
        let text = line.trim_end_matches(['\r', '\n']);
        let content = text.trim_start_matches(' ');
        let indent = &text[..text.len() - content.len()];
        header.push(WrapLine {
            first: vec![Span::raw(indent.to_owned())],
            rest: Vec::new(),
            spans: vec![Span::raw(display_text(content).into_owned())],
        });
    }
    let mut blocks = vec![Block::Rule(Vec::new()), Block::Paragraph(header, None)];
    if closed {
        blocks.push(Block::Rule(Vec::new()));
    }
    blocks.extend(parse_styled(&metadata[offset..], true).blocks);
    MarkdownDocument { blocks }
}

fn parse_styled(source: &str, instruction: bool) -> MarkdownDocument {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS | Options::ENABLE_STRIKETHROUGH);
    let mut b = Builder::new(source);
    b.instruction = instruction;
    for (ev, range) in Parser::new_ext(source, opts).into_offset_iter() {
        match ev {
            Event::Start(tag) => match tag {
                Tag::Paragraph => b.start_paragraph(),
                Tag::Heading { level, .. } => b.start_heading(level as u8),
                Tag::BlockQuote(_) => b.start_quote(),
                Tag::CodeBlock(kind) => b.start_code(kind, range.start),
                Tag::HtmlBlock => b.start_html(),
                Tag::List(start) => b.start_list(start),
                Tag::Item => b.start_item(),
                Tag::Table(aligns) => b.start_table(aligns),
                Tag::TableHead => b.start_table_head(),
                Tag::TableRow => b.start_table_row(),
                Tag::TableCell => b.start_cell(),
                Tag::Emphasis => b.em += 1,
                Tag::Strong => b.strong += 1,
                Tag::Strikethrough => b.strike += 1,
                Tag::Link { dest_url, .. } => b.links.push((dest_url.to_string(), String::new())),
                Tag::Image { dest_url, .. } => b.images.push((dest_url.to_string(), String::new())),
                _ => {}
            },
            Event::End(end) => match end {
                TagEnd::Paragraph => {
                    b.flush_block();
                    b.paragraph = false;
                }
                TagEnd::Heading(_) => b.end_heading(),
                TagEnd::BlockQuote(_) => b.end_quote(),
                TagEnd::CodeBlock => b.end_code(range),
                TagEnd::HtmlBlock => b.end_html(),
                TagEnd::List(_) => b.end_list(),
                TagEnd::Item => b.end_item(),
                TagEnd::Table => b.end_table(),
                TagEnd::TableHead => b.end_table_head(),
                TagEnd::TableRow => b.end_table_row(),
                TagEnd::TableCell => b.end_cell(),
                TagEnd::Emphasis => b.em -= 1,
                TagEnd::Strong => b.strong -= 1,
                TagEnd::Strikethrough => b.strike -= 1,
                TagEnd::Link => b.end_link(),
                TagEnd::Image => b.end_image(),
                _ => {}
            },
            Event::Text(t) => b.on_text(&t, range.end),
            Event::Code(t) => b.on_inline_code(&t),
            Event::SoftBreak => b.on_soft_break(),
            Event::HardBreak => b.on_hard_break(),
            Event::Rule => b.on_rule(),
            Event::TaskListMarker(checked) => b.on_task_marker(checked),
            Event::Html(t) => b.on_html(&t),
            Event::InlineHtml(t) => b.on_inline_html(&t),
            _ => {}
        }
    }
    b.finish()
}

impl MarkdownDocument {
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
            match block {
                Block::Paragraph(lines, _) | Block::Heading(lines, _) => {
                    layout_paragraph(lines, width, &mut out);
                }
                Block::Code(cb) => layout_code(cb, width, &mut out),
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
        out
    }
}

// ---------------------------------------------------------------------------
// Parse-time builder
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum PrefixSeg {
    List,
    Quote,
}

struct ListFrame {
    ordered: bool,
    next: u64,
}

struct CodeState {
    lang: Option<String>,
    buf: String,
    start: usize,
    content_end: Option<usize>,
    fenced: bool,
    first: Vec<Span<'static>>,
    rest: Vec<Span<'static>>,
}

struct TableState {
    aligns: Vec<Alignment>,
    header: Vec<Cell>,
    rows: Vec<Vec<Cell>>,
    row: Vec<Cell>,
    first: Vec<Span<'static>>,
    rest: Vec<Span<'static>>,
}

struct Builder<'a> {
    source: &'a str,
    instruction: bool,
    blocks: Vec<Block>,
    lines: Vec<WrapLine>,
    cur: Vec<Span<'static>>,
    col: usize,
    heading: bool,
    heading_level: u8,
    paragraph: bool,
    list_group: usize,
    strong: u32,
    em: u32,
    strike: u32,
    links: Vec<(String, String)>,
    images: Vec<(String, String)>,
    prefix_stack: Vec<PrefixSeg>,
    lists: Vec<ListFrame>,
    marker: Option<Span<'static>>,
    marker_used: bool,
    item_states: Vec<(Option<Span<'static>>, bool)>,
    code: Option<CodeState>,
    table: Option<TableState>,
    in_cell: bool,
    cell: Cell,
    html: Option<Vec<String>>,
}

impl<'a> Builder<'a> {
    fn new(source: &'a str) -> Self {
        Builder {
            source,
            instruction: false,
            blocks: Vec::new(),
            lines: Vec::new(),
            cur: Vec::new(),
            col: 0,
            heading: false,
            heading_level: 1,
            paragraph: false,
            list_group: 0,
            strong: 0,
            em: 0,
            strike: 0,
            links: Vec::new(),
            images: Vec::new(),
            prefix_stack: Vec::new(),
            lists: Vec::new(),
            marker: None,
            marker_used: false,
            item_states: Vec::new(),
            code: None,
            table: None,
            in_cell: false,
            cell: Vec::new(),
            html: None,
        }
    }

    fn finish(mut self) -> MarkdownDocument {
        self.flush_block();
        MarkdownDocument {
            blocks: std::mem::take(&mut self.blocks),
        }
    }

    fn inline_style(&self) -> Style {
        let mut st = Style::new();
        if self.heading {
            st = if self.instruction {
                st.fg(super::transcript_style::READ_ACCENT)
                    .add_modifier(Modifier::UNDERLINED)
            } else {
                st.fg(ACCENT).add_modifier(Modifier::BOLD)
            };
        }
        if self.strong > 0 {
            st = st.add_modifier(Modifier::BOLD);
            if self.instruction {
                st = st.fg(super::transcript_style::READ_ACCENT);
            }
        }
        if self.em > 0 {
            st = st.add_modifier(Modifier::ITALIC);
        }
        if self.strike > 0 {
            st = st.add_modifier(Modifier::CROSSED_OUT);
        }
        if !self.links.is_empty() {
            st = st.fg(LINK);
        }
        st
    }

    fn sink(&mut self) -> &mut Vec<Span<'static>> {
        if self.in_cell {
            &mut self.cell
        } else {
            &mut self.cur
        }
    }

    fn append_span(&mut self, style: &Style, content: String) {
        if content.is_empty() {
            return;
        }
        if let Some(last) = self.sink().last_mut()
            && last.style == *style
        {
            last.content.to_mut().push_str(&content);
            return;
        }
        self.sink().push(Span::styled(content, *style));
    }

    fn push_text(&mut self, text: &str, style: Style) {
        let safe = display_text(text);
        let mut first = true;
        for part in safe.split('\t') {
            if !first {
                let pad = TAB_STOP - self.col % TAB_STOP;
                self.append_span(&style, " ".repeat(pad));
                self.col += pad;
            }
            first = false;
            if part.is_empty() {
                continue;
            }
            self.col += Span::raw(part).width();
            self.append_span(&style, part.to_string());
        }
    }

    /// Prefix spans for a line. `use_marker` places the pending item marker on
    /// the innermost list level; otherwise that level gets a matching run of
    /// spaces (the hanging indent). Enclosing list levels indent two spaces
    /// each; quote levels repeat the `│ ` bar.
    fn prefix_spans(&self, use_marker: bool) -> Vec<Span<'static>> {
        let last_list = self
            .prefix_stack
            .iter()
            .rposition(|s| matches!(s, PrefixSeg::List));
        let mut spans = Vec::new();
        for (i, seg) in self.prefix_stack.iter().enumerate() {
            match seg {
                PrefixSeg::Quote => spans.push(Span::styled("│ ", Style::new().fg(MUTED))),
                PrefixSeg::List => {
                    if Some(i) != last_list {
                        spans.push(Span::raw("  "));
                    } else if use_marker {
                        match &self.marker {
                            Some(m) => spans.push(m.clone()),
                            None => spans.push(Span::raw("  ")),
                        }
                    } else {
                        let w = self.marker.as_ref().map(|m| m.width()).unwrap_or(2);
                        spans.push(Span::raw(" ".repeat(w)));
                    }
                }
            }
        }
        spans
    }

    /// Push the current inline run as one logical line. The very first line
    /// pushed inside a list item carries the pending marker; later lines
    /// (hard-break splits, further paragraphs, wrapped continuations) use the
    /// hanging indent.
    fn push_line(&mut self) {
        let use_marker = !self.marker_used;
        let first = self.prefix_spans(use_marker);
        let rest = self.prefix_spans(false);
        if use_marker {
            self.marker_used = true;
        }
        self.lines.push(WrapLine {
            first,
            rest,
            spans: std::mem::take(&mut self.cur),
        });
        self.col = 0;
    }

    fn flush_block(&mut self) {
        if !self.cur.is_empty() {
            self.push_line();
        }
        if !self.lines.is_empty() {
            let lines = std::mem::take(&mut self.lines);
            self.blocks.push(if self.heading {
                Block::Heading(lines, self.heading_level)
            } else {
                Block::Paragraph(
                    lines,
                    (!self.lists.is_empty() && !self.paragraph).then_some(self.list_group),
                )
            });
        }
    }

    fn start_paragraph(&mut self) {
        self.flush_block();
        self.paragraph = true;
        self.col = 0;
    }

    fn start_heading(&mut self, level: u8) {
        self.flush_block();
        self.heading = true;
        self.heading_level = level;
        self.col = 0;
    }

    fn end_heading(&mut self) {
        self.flush_block();
        self.heading = false;
    }

    fn start_quote(&mut self) {
        self.flush_block();
        self.prefix_stack.push(PrefixSeg::Quote);
    }

    fn end_quote(&mut self) {
        self.flush_block();
        self.prefix_stack.pop();
    }

    fn start_list(&mut self, start: Option<u64>) {
        self.flush_block();
        if self.lists.is_empty() {
            self.list_group += 1;
        }
        self.lists.push(ListFrame {
            ordered: start.is_some(),
            next: start.unwrap_or(0),
        });
        self.prefix_stack.push(PrefixSeg::List);
    }

    fn end_list(&mut self) {
        self.flush_block();
        self.lists.pop();
        self.prefix_stack.pop();
    }

    fn start_item(&mut self) {
        self.flush_block();
        self.item_states
            .push((self.marker.take(), self.marker_used));
        let marker = match self.lists.last_mut() {
            Some(frame) if frame.ordered => {
                let m = format!("{}. ", frame.next);
                frame.next += 1;
                Span::styled(m, Style::new().fg(MUTED))
            }
            _ => Span::styled("• ", Style::new().fg(MUTED)),
        };
        self.marker = Some(marker);
        self.marker_used = false;
    }

    fn end_item(&mut self) {
        self.flush_block();
        let (marker, used) = self.item_states.pop().unwrap_or((None, false));
        self.marker = marker;
        self.marker_used = used;
    }

    fn on_task_marker(&mut self, checked: bool) {
        let glyph = if checked { "☑ " } else { "☐ " };
        self.marker = Some(Span::styled(glyph, Style::new().fg(MUTED)));
    }

    fn start_code(&mut self, kind: CodeBlockKind<'_>, start: usize) {
        self.flush_block();
        let fenced = matches!(&kind, CodeBlockKind::Fenced(_));
        let first = self.prefix_spans(!self.marker_used);
        let rest = self.prefix_spans(false);
        self.marker_used = true;
        let lang = match kind {
            CodeBlockKind::Fenced(info) => info
                .split_whitespace()
                .next()
                .filter(|l| !l.is_empty())
                .map(|language| display_text(language).into_owned()),
            CodeBlockKind::Indented => None,
        };
        self.code = Some(CodeState {
            lang,
            buf: String::new(),
            start,
            content_end: None,
            fenced,
            first,
            rest,
        });
    }

    fn end_code(&mut self, range: Range<usize>) {
        let Some(code) = self.code.take() else { return };
        let mut lines: Vec<String> = Vec::new();
        for raw in code.buf.split_terminator('\n') {
            let raw = raw.strip_suffix('\r').unwrap_or(raw);
            lines.push(display_text(raw).into_owned());
        }
        // tab 展开前移到解析期：span 化后无法再对 String 展开。
        let expanded: Vec<String> = lines.iter().map(|l| expand_tabs(l, TAB_STOP)).collect();
        let text = expanded.join("\n");
        let lines: Arc<Vec<Vec<Span<'static>>>> =
            match highlight::highlight(code.lang.as_deref(), &text) {
                Some(spans) => spans,
                None => Arc::new(
                    expanded
                        .into_iter()
                        .map(|line| {
                            if line.is_empty() {
                                Vec::new()
                            } else {
                                vec![Span::styled(line, Style::new().fg(CODE_BLOCK))]
                            }
                        })
                        .collect(),
                ),
            };
        let closed = !code.fenced
            || match code.content_end {
                Some(ce) => {
                    let tail = self.source.get(ce..range.end).unwrap_or("");
                    let t = tail.trim_start_matches(['\n', '\r', ' ', '\t', '>']);
                    t.starts_with("```") || t.starts_with("~~~")
                }
                None => {
                    let region = self.source.get(code.start..range.end).unwrap_or("");
                    region
                        .lines()
                        .filter(|l| {
                            let lt = l.trim_start_matches([' ', '\t', '>']);
                            lt.starts_with("```") || lt.starts_with("~~~")
                        })
                        .count()
                        >= 2
                }
            };
        self.blocks.push(Block::Code(CodeBlock {
            lang: code.lang,
            lines,
            closed,
            first: code.first,
            rest: code.rest,
        }));
    }

    fn start_html(&mut self) {
        self.flush_block();
        self.html = Some(Vec::new());
    }

    fn on_html(&mut self, t: &str) {
        if let Some(lines) = self.html.as_mut() {
            for line in t.lines() {
                lines.push(line.to_string());
            }
        } else {
            self.on_inline_html(t);
        }
    }

    fn end_html(&mut self) {
        let Some(lines) = self.html.take() else {
            return;
        };
        for line in &lines {
            self.col = 0;
            self.cur.clear();
            let style = Style::new();
            self.push_text(line, style);
            self.push_line();
        }
        if !self.lines.is_empty() {
            self.blocks.push(Block::Paragraph(
                std::mem::take(&mut self.lines),
                (!self.lists.is_empty() && !self.paragraph && !self.heading)
                    .then_some(self.list_group),
            ));
        }
    }

    fn on_inline_html(&mut self, t: &str) {
        if let Some((_, alt)) = self.images.last_mut() {
            alt.push_str(t);
            return;
        }
        let style = self.inline_style();
        self.push_text(t, style);
    }

    fn on_text(&mut self, text: &str, end: usize) {
        if let Some(code) = self.code.as_mut() {
            code.buf.push_str(text);
            code.content_end = Some(end);
            return;
        }
        if let Some((_, alt)) = self.images.last_mut() {
            alt.push_str(text);
            return;
        }
        if !self.links.is_empty() {
            self.links.last_mut().unwrap().1.push_str(text);
        }
        let style = self.inline_style();
        self.push_text(text, style);
    }

    fn on_inline_code(&mut self, text: &str) {
        if let Some((_, alt)) = self.images.last_mut() {
            alt.push_str(text);
            return;
        }
        if !self.links.is_empty() {
            self.links.last_mut().unwrap().1.push_str(text);
            let style = Style::new().fg(LINK);
            self.push_text(text, style);
            return;
        }
        self.push_text(text, Style::new().fg(CODE));
    }

    fn on_soft_break(&mut self) {
        if self.instruction && self.code.is_none() && self.images.is_empty() && !self.in_cell {
            self.push_line();
            return;
        }
        if let Some(code) = self.code.as_mut() {
            code.buf.push('\n');
            return;
        }
        if let Some((_, alt)) = self.images.last_mut() {
            alt.push(' ');
            return;
        }
        self.push_text(" ", self.inline_style());
    }

    fn on_hard_break(&mut self) {
        if let Some(code) = self.code.as_mut() {
            code.buf.push('\n');
            return;
        }
        if let Some((_, alt)) = self.images.last_mut() {
            alt.push(' ');
            return;
        }
        if self.in_cell {
            self.push_text(" ", Style::new());
            return;
        }
        self.push_line();
    }

    fn on_rule(&mut self) {
        self.flush_block();
        let prefix = self.prefix_spans(!self.marker_used);
        self.marker_used = true;
        self.blocks.push(Block::Rule(prefix));
    }

    fn end_link(&mut self) {
        let Some((dest, label)) = self.links.pop() else {
            return;
        };
        if !self.images.is_empty() {
            self.images.last_mut().unwrap().1.push_str(&label);
            return;
        }
        if !dest.is_empty() && label.trim() != dest {
            let style = Style::new().fg(DIM);
            self.push_text(&format!(" ({})", dest), style);
        }
    }

    fn end_image(&mut self) {
        let Some((dest, alt)) = self.images.pop() else {
            return;
        };
        if !self.images.is_empty() {
            self.images.last_mut().unwrap().1.push_str(&alt);
            return;
        }
        if !self.links.is_empty() {
            self.links
                .last_mut()
                .unwrap()
                .1
                .push_str(&format!("Image: {alt} ({dest})"));
        }
        let style = self.inline_style();
        self.push_text(&format!("Image: {}", alt), style);
        if !dest.is_empty() {
            self.push_text(&format!(" ({})", dest), Style::new().fg(DIM));
        }
    }

    fn start_table(&mut self, aligns: Vec<Alignment>) {
        self.flush_block();
        let first = self.prefix_spans(!self.marker_used);
        let rest = self.prefix_spans(false);
        self.marker_used = true;
        self.table = Some(TableState {
            aligns,
            header: Vec::new(),
            rows: Vec::new(),
            row: Vec::new(),
            first,
            rest,
        });
    }

    fn start_table_head(&mut self) {
        if let Some(t) = self.table.as_mut() {
            t.row.clear();
        }
    }

    fn end_table_head(&mut self) {
        if let Some(t) = self.table.as_mut() {
            t.header = std::mem::take(&mut t.row);
        }
    }

    fn start_table_row(&mut self) {
        if let Some(t) = self.table.as_mut() {
            t.row.clear();
        }
    }

    fn end_table_row(&mut self) {
        if let Some(t) = self.table.as_mut() {
            let row = std::mem::take(&mut t.row);
            t.rows.push(row);
        }
    }

    fn start_cell(&mut self) {
        self.in_cell = true;
        self.cell = Vec::new();
        self.col = 0;
    }

    fn end_cell(&mut self) {
        self.in_cell = false;
        self.col = 0;
        let mut cell = std::mem::take(&mut self.cell);
        trim_cell(&mut cell);
        if let Some(t) = self.table.as_mut() {
            t.row.push(cell);
        }
    }

    fn end_table(&mut self) {
        let Some(t) = self.table.take() else { return };
        self.in_cell = false;
        let n = t.aligns.len();
        let mut header = t.header;
        header.truncate(n);
        header.resize(n, Vec::new());
        let rows = t
            .rows
            .into_iter()
            .map(|mut r| {
                r.truncate(n);
                r.resize(n, Vec::new());
                r
            })
            .collect();
        self.blocks.push(Block::Table(TableBlock {
            aligns: t.aligns,
            header,
            rows,
            first: t.first,
            rest: t.rest,
        }));
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

fn layout_table(tb: &TableBlock, width: usize, out: &mut Vec<Line<'static>>) {
    let n = tb.aligns.len();
    if n == 0 {
        return;
    }
    let sep_total = CELL_SEPARATOR_WIDTH * (n - 1);
    let mut widths = vec![CELL_MIN_WIDTH; n];
    for (i, cell) in tb.header.iter().enumerate().take(n) {
        widths[i] = widths[i].max(spans_width(cell).max(CELL_MIN_WIDTH));
    }
    for row in &tb.rows {
        for (i, cell) in row.iter().enumerate().take(n) {
            widths[i] = widths[i].max(spans_width(cell).max(CELL_MIN_WIDTH));
        }
    }
    if CELL_MIN_WIDTH * n + sep_total > width {
        table_vertical_fallback(tb, n, width, out);
        return;
    }
    let mut excess = (widths.iter().sum::<usize>() + sep_total).saturating_sub(width);
    while excess > 0 {
        let Some(max) = widths.iter().copied().max().filter(|m| *m > CELL_MIN_WIDTH) else {
            break;
        };
        let group: Vec<usize> = (0..n).filter(|&i| widths[i] == max).collect();
        let next = widths
            .iter()
            .copied()
            .filter(|w| *w < max)
            .max()
            .unwrap_or(CELL_MIN_WIDTH)
            .max(CELL_MIN_WIDTH);
        let potential = group.len() * (max - next);
        if excess >= potential {
            for &i in &group {
                widths[i] = next;
            }
            excess -= potential;
        } else {
            let per = excess / group.len();
            let rem = excess % group.len();
            for (k, &i) in group.iter().enumerate() {
                widths[i] = max - per - usize::from(k < rem);
            }
            excess = 0;
        }
    }
    render_table_row(&tb.header, &widths, &tb.aligns, out);
    let mut divider = Vec::new();
    for (i, w) in widths.iter().enumerate() {
        if i > 0 {
            divider.push(Span::styled("─┼─", Style::new().fg(DIM)));
        }
        divider.push(Span::styled("─".repeat(*w), Style::new().fg(DIM)));
    }
    out.push(Line::from(divider));
    for row in &tb.rows {
        render_table_row(row, &widths, &tb.aligns, out);
    }
}

fn render_table_row(
    cells: &[Cell],
    widths: &[usize],
    aligns: &[Alignment],
    out: &mut Vec<Line<'static>>,
) {
    let n = widths.len();
    let wrapped: Vec<Vec<Vec<Span<'static>>>> = (0..n)
        .map(|i| {
            let empty: Cell = Vec::new();
            let cell = cells.get(i).unwrap_or(&empty);
            wrap(WrapMode::Char, cell, widths[i], widths[i])
        })
        .collect();
    let height = wrapped.iter().map(Vec::len).max().unwrap_or(0).max(1);
    for r in 0..height {
        let mut spans = Vec::new();
        for (i, w) in widths.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled(CELL_SEPARATOR, Style::new().fg(DIM)));
            }
            let mut line = wrapped[i].get(r).cloned().unwrap_or_default();
            apply_align(
                &mut line,
                *w,
                aligns.get(i).copied().unwrap_or(Alignment::None),
            );
            spans.append(&mut line);
        }
        out.push(Line::from(spans));
    }
}

fn apply_align(spans: &mut Vec<Span<'static>>, width: usize, align: Alignment) {
    let free = width.saturating_sub(spans.iter().map(Span::width).sum());
    if free == 0 {
        return;
    }
    match align {
        Alignment::Right => {
            let mut padded = vec![Span::raw(" ".repeat(free))];
            padded.append(spans);
            *spans = padded;
        }
        Alignment::Center => {
            let left = free / 2;
            let right = free - left;
            let mut padded = vec![Span::raw(" ".repeat(left))];
            padded.append(spans);
            padded.push(Span::raw(" ".repeat(right)));
            *spans = padded;
        }
        _ => spans.push(Span::raw(" ".repeat(free))),
    }
}

/// When even minimal three-cell columns plus separators do not fit, render
/// each data row as one `header: value` line per column; empty headers become
/// `Column N`.
fn table_vertical_fallback(tb: &TableBlock, n: usize, width: usize, out: &mut Vec<Line<'static>>) {
    let labels: Vec<String> = (0..n)
        .map(|i| {
            let t: String = tb
                .header
                .get(i)
                .map(|c| c.iter().map(|s| s.content.as_ref()).collect())
                .unwrap_or_default();
            let t = t.trim();
            if t.is_empty() {
                format!("Column {}", i + 1)
            } else {
                t.to_string()
            }
        })
        .collect();
    for (ri, row) in tb.rows.iter().enumerate() {
        if ri > 0 {
            out.push(Line::from(""));
        }
        for (ci, cell) in row.iter().enumerate().take(n) {
            let mut content = vec![Span::styled(
                format!("{}: ", labels[ci]),
                Style::new().fg(MUTED),
            )];
            let prefix_width = spans_width(&content);
            content.extend(cell.iter().cloned());
            let indent = if prefix_width < width {
                prefix_width
            } else {
                0
            };
            for (i, line) in wrap(WrapMode::Word, &content, width, width - indent)
                .into_iter()
                .enumerate()
            {
                let row_indent = indent.min(width.saturating_sub(spans_width(&line)));
                let mut spans = if i == 0 || row_indent == 0 {
                    Vec::new()
                } else {
                    vec![Span::raw(" ".repeat(row_indent))]
                };
                spans.extend(line);
                out.push(Line::from(spans));
            }
        }
    }
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
