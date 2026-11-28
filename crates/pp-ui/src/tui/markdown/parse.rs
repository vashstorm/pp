//! Markdown semantic parse: event stream -> semantic blocks plus the builder used during parsing.

use super::*;

pub(in crate::tui) fn parse(source: &str) -> MarkdownDocument {
    parse_styled(source, 0, None, true, 0)
}

pub(in crate::tui) fn parse_streaming(source: &str, streaming: bool) -> MarkdownDocument {
    parse_streaming_from(source, 0, streaming, 0)
}

/// Parse only the suffix of `source` starting at `base`, keeping every recorded byte offset absolute
/// inside `source`. `group_base` continues the top-level list numbering of the retained prefix, so a
/// list group on either side of `base` can never share an identifier.
pub(super) fn parse_streaming_from(
    source: &str,
    base: usize,
    streaming: bool,
    group_base: usize,
) -> MarkdownDocument {
    parse_styled(
        source,
        base,
        streaming_tail_start(source, streaming),
        true,
        group_base,
    )
}

/// Parser for the main-thread streaming preview: identical semantics to `parse_streaming` but skips syntect highlighting
/// so the draw loop is never blocked.
pub(in crate::tui) fn parse_streaming_preview(source: &str, streaming: bool) -> MarkdownDocument {
    parse_styled(source, 0, streaming_tail_start(source, streaming), false, 0)
}

/// Start of the still-mutable last line while streaming; `None` once the snapshot is complete.
fn streaming_tail_start(source: &str, streaming: bool) -> Option<usize> {
    (streaming && !source.ends_with('\n'))
        .then(|| source.rfind('\n').map(|newline| newline + 1))
        .flatten()
}

fn line_start(source: &str, offset: usize) -> usize {
    let offset = offset.min(source.len());
    source[..offset].rfind('\n').map_or(0, |index| index + 1)
}

fn ends_with_blank_line(source: &str) -> bool {
    let mut newlines = 0usize;
    for byte in source.bytes().rev() {
        match byte {
            b'\n' => {
                newlines += 1;
                if newlines >= 2 {
                    return true;
                }
            }
            b' ' | b'\t' | b'\r' => {}
            _ => return false,
        }
    }
    false
}

fn parse_styled(
    source: &str,
    base: usize,
    tail_start: Option<usize>,
    highlight_code: bool,
    group_base: usize,
) -> MarkdownDocument {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS | Options::ENABLE_STRIKETHROUGH);
    // Only the suffix is parsed, but every recorded offset stays absolute inside `source`.
    let text = &source[base..];
    let mut b = Builder::new(source, base, group_base, highlight_code);
    let mut block_depth = 0usize;
    for (ev, range) in Parser::new_ext(text, opts).into_offset_iter() {
        match ev {
            Event::Start(tag) => {
                let is_block = matches!(
                    tag,
                    Tag::Paragraph
                        | Tag::Heading { .. }
                        | Tag::BlockQuote(_)
                        | Tag::CodeBlock(_)
                        | Tag::HtmlBlock
                        | Tag::List(_)
                        | Tag::Table(_)
                );
                if is_block {
                    if block_depth == 0 {
                        b.current_top_start = base + line_start(text, range.start);
                        b.last_top_is_list = matches!(tag, Tag::List(_));
                    }
                    block_depth += 1;
                }
                match tag {
                    Tag::Paragraph => b.start_paragraph(),
                    Tag::Heading { level, .. } => b.start_heading(level as u8),
                    Tag::BlockQuote(_) => b.start_quote(),
                    Tag::CodeBlock(kind) => b.start_code(kind, base + range.start),
                    Tag::HtmlBlock => b.start_html(),
                    Tag::List(start) => b.start_list(start),
                    Tag::Item => b.start_item(),
                    Tag::Table(aligns) => {
                        if let Some(start) = tail_start.filter(|&start| {
                            base + range.start < start && base + range.end >= start
                        }) {
                            // The last line may still change the table structure, so lay out up to the nearest newline first.
                            let mut document = parse_styled(
                                &source[..start],
                                base,
                                None,
                                highlight_code,
                                group_base,
                            );
                            document.table_tail_start = Some(start);
                            return document;
                        }
                        b.start_table(aligns);
                    }
                    Tag::TableHead => b.start_table_head(),
                    Tag::TableRow => b.start_table_row(),
                    Tag::TableCell => b.start_cell(),
                    Tag::Emphasis => b.em += 1,
                    Tag::Strong => b.strong += 1,
                    Tag::Strikethrough => b.strike += 1,
                    Tag::Link { dest_url, .. } => {
                        b.links.push((dest_url.to_string(), String::new()))
                    }
                    Tag::Image { dest_url, .. } => {
                        b.images.push((dest_url.to_string(), String::new()))
                    }
                    _ => {}
                }
            }
            Event::End(end) => {
                match end {
                    TagEnd::Paragraph => {
                        b.flush_block();
                        b.paragraph = false;
                    }
                    TagEnd::Heading(_) => b.end_heading(),
                    TagEnd::BlockQuote(_) => b.end_quote(),
                    TagEnd::CodeBlock => b.end_code(base + range.end),
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
                }
                if matches!(
                    end,
                    TagEnd::Paragraph
                        | TagEnd::Heading(_)
                        | TagEnd::BlockQuote(_)
                        | TagEnd::CodeBlock
                        | TagEnd::HtmlBlock
                        | TagEnd::List(_)
                        | TagEnd::Table
                ) {
                    block_depth = block_depth.saturating_sub(1);
                }
            }
            Event::Text(t) => b.on_text(&t, base + range.end),
            Event::Code(t) => b.on_inline_code(&t),
            Event::SoftBreak => b.on_soft_break(),
            Event::HardBreak => b.on_hard_break(),
            Event::Rule => {
                if block_depth == 0 {
                    b.current_top_start = base + line_start(text, range.start);
                    b.last_top_is_list = false;
                }
                b.on_rule();
            }
            Event::TaskListMarker(checked) => b.on_task_marker(checked),
            Event::Html(t) => b.on_html(&t),
            Event::InlineHtml(t) => b.on_inline_html(&t),
            _ => {}
        }
    }
    b.finish()
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
    /// Byte offset of the parsed suffix inside `source`; recorded offsets stay absolute.
    base: usize,
    highlight_code: bool,
    blocks: Vec<Block>,
    block_top_starts: Vec<usize>,
    current_top_start: usize,
    last_top_is_list: bool,
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
    table_head: bool,
    in_cell: bool,
    cell: Cell,
    html: Option<Vec<String>>,
}

impl<'a> Builder<'a> {
    fn new(source: &'a str, base: usize, group_base: usize, highlight_code: bool) -> Self {
        Builder {
            source,
            base,
            highlight_code,
            blocks: Vec::new(),
            block_top_starts: Vec::new(),
            current_top_start: base,
            last_top_is_list: false,
            lines: Vec::new(),
            cur: Vec::new(),
            col: 0,
            heading: false,
            heading_level: 1,
            paragraph: false,
            list_group: group_base,
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
            table_head: false,
            in_cell: false,
            cell: Vec::new(),
            html: None,
        }
    }

    fn push_block(&mut self, block: Block) {
        self.blocks.push(block);
        self.block_top_starts.push(self.current_top_start);
    }

    fn finish(mut self) -> MarkdownDocument {
        self.flush_block();
        let text = &self.source[self.base..];
        let trailing_line_start = self.base + text.rfind('\n').map_or(0, |index| index + 1);
        MarkdownDocument {
            blocks: std::mem::take(&mut self.blocks),
            block_top_starts: std::mem::take(&mut self.block_top_starts),
            last_top_is_list: self.last_top_is_list,
            ends_with_blank_line: ends_with_blank_line(text),
            trailing_line_start,
            has_reference_definition: text.contains("]:"),
            cached: Vec::new(),
            table_tail_start: None,
        }
    }

    fn inline_style(&self) -> Style {
        let mut st = Style::new();
        if self.heading {
            st = st.fg(md_heading()).add_modifier(Modifier::BOLD);
        } else if self.table_head {
            st = st.fg(md_heading());
        }
        if self.strong > 0 {
            st = st.add_modifier(Modifier::BOLD);
            if let Some(fg) = self.emphasis_fg(md_strong()) {
                st = st.fg(fg);
            }
        }
        if self.em > 0 {
            st = st.add_modifier(Modifier::ITALIC);
        }
        if self.strike > 0 {
            st = st.add_modifier(Modifier::CROSSED_OUT);
        }
        if !self.links.is_empty() {
            st = st.fg(link());
        }
        st
    }

    /// Headings and headers keep yellow; the body distinguishes bold and code.
    fn emphasis_fg(&self, color: Color) -> Option<Color> {
        if self.heading || self.table_head {
            None
        } else {
            Some(color)
        }
    }

    /// Inline code uses green and keeps the surrounding modifiers and heading color.
    fn code_style(&self) -> Style {
        let mut st = self.inline_style();
        if let Some(fg) = self.emphasis_fg(md_code()) {
            st = st.fg(fg);
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
                PrefixSeg::Quote => spans.push(Span::styled("│ ", Style::new().fg(muted()))),
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
            let block = if self.heading {
                Block::Heading(lines, self.heading_level)
            } else {
                Block::Paragraph(
                    lines,
                    (!self.lists.is_empty() && !self.paragraph).then_some(self.list_group),
                )
            };
            self.push_block(block);
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
                Span::styled(m, Style::new().fg(md_bullet()))
            }
            _ => Span::styled("• ", Style::new().fg(md_bullet())),
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
        self.marker = Some(Span::styled(glyph, Style::new().fg(md_bullet())));
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

    fn end_code(&mut self, end: usize) {
        let Some(code) = self.code.take() else { return };
        let mut lines: Vec<String> = Vec::new();
        for raw in code.buf.split_terminator('\n') {
            let raw = raw.strip_suffix('\r').unwrap_or(raw);
            lines.push(display_text(raw).into_owned());
        }
        // Tab expansion moves into the parse phase: once spans exist a String can no longer be expanded.
        let expanded: Vec<String> = lines.iter().map(|l| expand_tabs(l, TAB_STOP)).collect();
        let text = expanded.join("\n");
        let lines: Arc<Vec<Arc<Vec<Span<'static>>>>> = match self
            .highlight_code
            .then(|| highlight::highlight(code.lang.as_deref(), &text))
            .flatten()
        {
            Some(spans) => spans,
            None => Arc::new(
                expanded
                    .into_iter()
                    .map(|line| {
                        Arc::new(if line.is_empty() {
                            Vec::new()
                        } else {
                            vec![Span::styled(line, Style::new().fg(code_block()))]
                        })
                    })
                    .collect(),
            ),
        };
        let closed = !code.fenced
            || match code.content_end {
                Some(ce) => {
                    let tail = self.source.get(ce..end).unwrap_or("");
                    let t = tail.trim_start_matches(['\n', '\r', ' ', '\t', '>']);
                    t.starts_with("```") || t.starts_with("~~~")
                }
                None => {
                    let region = self.source.get(code.start..end).unwrap_or("");
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
        self.push_block(Block::Code(CodeBlock {
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
            let block = Block::Paragraph(
                std::mem::take(&mut self.lines),
                (!self.lists.is_empty() && !self.paragraph && !self.heading)
                    .then_some(self.list_group),
            );
            self.push_block(block);
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
            let style = Style::new().fg(link());
            self.push_text(text, style);
            return;
        }
        self.push_text(text, self.code_style());
    }

    fn on_soft_break(&mut self) {
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
        self.push_block(Block::Rule(prefix));
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
            let style = Style::new().fg(dim());
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
            self.push_text(&format!(" ({})", dest), Style::new().fg(dim()));
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
        self.table_head = true;
        if let Some(t) = self.table.as_mut() {
            t.row.clear();
        }
    }

    fn end_table_head(&mut self) {
        self.table_head = false;
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
        self.push_block(Block::Table(TableBlock {
            aligns: t.aligns,
            header,
            rows,
            first: t.first,
            rest: t.rest,
        }));
    }
}
