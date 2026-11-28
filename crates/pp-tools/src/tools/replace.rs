use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::{borrow::Cow, ops::Range};
use unicode_normalization::UnicodeNormalization;
use unicode_segmentation::UnicodeSegmentation;

mod diagnostics;
use diagnostics::{format_duplicate_error, format_missing_error};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Edit {
    #[serde(alias = "old_text", alias = "old_string", alias = "oldString")]
    old_text: String,
    #[serde(alias = "new_text", alias = "new_string", alias = "newString")]
    new_text: String,
    #[serde(default, alias = "replace_all")]
    replace_all: Option<bool>,
}
impl Edit {
    pub(super) fn new(old_text: String, new_text: String, replace_all: bool) -> Self {
        Self {
            old_text,
            new_text,
            replace_all: Some(replace_all),
        }
    }

    pub(super) fn apply_default_replace_all(&mut self, default: bool) {
        if self.replace_all.is_none() {
            self.replace_all = Some(default);
        }
    }

    fn is_replace_all(&self) -> bool {
        self.replace_all.unwrap_or(false)
    }
}

struct Replacement<'a> {
    index: usize,
    range: Range<usize>,
    text: Cow<'a, str>,
}

const MAX_REPORTED_BATCH_ERRORS: usize = 5;

pub(super) fn apply(source: &str, edits: &[Edit]) -> Result<(String, usize)> {
    ensure!(!edits.is_empty(), "edit requires at least one replacement");
    let (bom, body) = source
        .strip_prefix('\u{feff}')
        .map_or(("", source), |body| ("\u{feff}", body));
    let crlf = body
        .find('\n')
        .is_some_and(|lf| lf > 0 && body.as_bytes()[lf - 1] == b'\r');
    let original = normalize_newlines(body);
    // Also needed for conservative uniqueness, even for exact matches. Reuse once per batch.
    let mapped = MappedText::new(&original);
    let fuzzy = &mapped.text;
    let mut replacements = Vec::with_capacity(edits.len());
    let mut errors = Vec::new();

    for (index, edit) in edits.iter().enumerate() {
        let old = normalize_newlines(&edit.old_text);
        if old.is_empty() {
            errors.push(format!("edits[{index}].oldText must not be empty"));
            continue;
        }
        let normalized = normalize_fuzzy(&old);
        if normalized.is_empty() {
            errors.push(format!("edits[{index}].oldText must not be empty"));
            continue;
        }
        let new_text = normalize_newlines(&edit.new_text);

        if edit.is_replace_all() {
            let exact: Vec<usize> = original
                .match_indices(old.as_ref())
                .map(|(s, _)| s)
                .collect();
            if !exact.is_empty() {
                for start in exact {
                    replacements.push(Replacement {
                        index,
                        range: start..start + old.len(),
                        text: new_text.clone(),
                    });
                }
                continue;
            }
            let fuzzy_hits: Vec<usize> = fuzzy
                .match_indices(normalized.as_str())
                .map(|(s, _)| s)
                .collect();
            if !fuzzy_hits.is_empty() {
                let mut mapped_ranges = Vec::with_capacity(fuzzy_hits.len());
                let mut map_error = None;
                for start in fuzzy_hits {
                    match mapped.original_range(start..start + normalized.len()) {
                        Ok(range) => mapped_ranges.push(range),
                        Err(err) => {
                            map_error = Some(format!("edits[{index}]: {err}"));
                            break;
                        }
                    }
                }
                if let Some(err) = map_error {
                    errors.push(err);
                } else {
                    for range in mapped_ranges {
                        replacements.push(Replacement {
                            index,
                            range,
                            text: new_text.clone(),
                        });
                    }
                }
                continue;
            }
            let indent_hits = find_indent_matches(&original, &old);
            if !indent_hits.is_empty() {
                for hit in indent_hits {
                    let aligned = reindent_replacement(&new_text, hit.old_indent, hit.src_indent);
                    replacements.push(Replacement {
                        index,
                        range: hit.range,
                        text: Cow::Owned(aligned.into_owned()),
                    });
                }
                continue;
            }
            errors.push(format_missing_error(&original, index, &old));
            continue;
        }

        let mut candidates = fuzzy.match_indices(normalized.as_str());
        let candidate = candidates.next();
        if let Some((second, _)) = candidates.next() {
            let first = candidate.expect("checked above").0;
            let mut lines = vec![line_number_at(fuzzy, first), line_number_at(fuzzy, second)];
            let mut total = 2usize;
            for (pos, _) in candidates {
                total += 1;
                if lines.len() < 5 {
                    lines.push(line_number_at(fuzzy, pos));
                }
            }
            errors.push(format_duplicate_error(&original, index, &lines, total));
            continue;
        }
        if let Some(start) = original.find(old.as_ref()) {
            replacements.push(Replacement {
                index,
                range: start..start + old.len(),
                text: new_text,
            });
            continue;
        }
        if let Some((start, _)) = candidate {
            match mapped.original_range(start..start + normalized.len()) {
                Ok(range) => replacements.push(Replacement {
                    index,
                    range,
                    text: new_text,
                }),
                Err(err) => errors.push(err.to_string()),
            }
            continue;
        }
        let indent_hits = find_indent_matches(&original, &old);
        match indent_hits.as_slice() {
            [only] => {
                let aligned = reindent_replacement(&new_text, only.old_indent, only.src_indent);
                replacements.push(Replacement {
                    index,
                    range: only.range.clone(),
                    text: Cow::Owned(aligned.into_owned()),
                });
            }
            [_, ..] => {
                let total = indent_hits.len();
                let lines: Vec<usize> = indent_hits
                    .iter()
                    .take(5)
                    .map(|hit| hit.line_number)
                    .collect();
                errors.push(format_duplicate_error(&original, index, &lines, total));
            }
            [] => errors.push(format_missing_error(&original, index, &old)),
        }
    }

    if !errors.is_empty() {
        anyhow::bail!("{}", join_batch_errors(&errors));
    }

    replacements.sort_unstable_by_key(|replacement| replacement.range.start);
    let mut overlaps = Vec::new();
    for pair in replacements.windows(2) {
        if pair[0].range.end > pair[1].range.start {
            overlaps.push(format!(
                "edits[{}] and edits[{}] overlap; merge them into one edit",
                pair[0].index, pair[1].index
            ));
        }
    }
    if !overlaps.is_empty() {
        anyhow::bail!("{}", join_batch_errors(&overlaps));
    }

    let applied_count = replacements.len();
    let mut output = String::with_capacity(original.len());
    splice(&original, 0..original.len(), &replacements, &mut output);
    ensure!(output != original, "edit produced no changes");
    if bom.is_empty() && !crlf {
        return Ok((output, applied_count));
    }
    let extra_crs = if crlf {
        output.bytes().filter(|byte| *byte == b'\n').count()
    } else {
        0
    };
    let mut restored = String::with_capacity(bom.len() + output.len() + extra_crs);
    restored.push_str(bom);
    if crlf {
        for part in output.split_inclusive('\n') {
            if let Some(line) = part.strip_suffix('\n') {
                restored.push_str(line);
                restored.push_str("\r\n");
            } else {
                restored.push_str(part);
            }
        }
    } else {
        restored.push_str(&output);
    }
    Ok((restored, applied_count))
}

fn join_batch_errors(errors: &[String]) -> String {
    if errors.len() == 1 {
        return errors[0].clone();
    }
    let shown = errors.len().min(MAX_REPORTED_BATCH_ERRORS);
    let mut out = errors[..shown].join("\n");
    if errors.len() > shown {
        out.push_str(&format!(
            "\n... and {} more edit error(s)",
            errors.len() - shown
        ));
    }
    out
}

fn line_number_at(text: &str, byte_offset: usize) -> usize {
    text[..byte_offset]
        .bytes()
        .filter(|&byte| byte == b'\n')
        .count()
        + 1
}

struct LineSpan {
    start: usize,
    body_end: usize,
    end: usize,
}

fn scan_lf_lines(text: &str) -> Vec<LineSpan> {
    let mut spans = Vec::new();
    let mut offset = 0;
    for part in text.split_inclusive('\n') {
        let body_end = if part.ends_with('\n') {
            offset + part.len() - 1
        } else {
            offset + part.len()
        };
        let end = offset + part.len();
        spans.push(LineSpan {
            start: offset,
            body_end,
            end,
        });
        offset = end;
    }
    spans
}

fn trim_start_indent(line: &str) -> (&str, &str) {
    let indent_len = line
        .bytes()
        .take_while(|&b| b == b' ' || b == b'\t')
        .count();
    (&line[..indent_len], &line[indent_len..])
}

fn is_blank_ecma(line: &str) -> bool {
    line.trim_end_matches(ecma_whitespace).is_empty()
}

fn common_indent<'a>(lines: impl IntoIterator<Item = &'a str>) -> &'a str {
    let mut common: Option<&'a str> = None;
    for line in lines {
        if is_blank_ecma(line) {
            continue;
        }
        let (indent, _) = trim_start_indent(line);
        common = Some(match common {
            None => indent,
            Some(prev) => {
                let shared = prev
                    .bytes()
                    .zip(indent.bytes())
                    .take_while(|(a, b)| a == b)
                    .count();
                &prev[..shared]
            }
        });
    }
    common.unwrap_or("")
}

struct IndentMatch<'a> {
    range: Range<usize>,
    old_indent: &'a str,
    src_indent: &'a str,
    line_number: usize,
}

/// Pass 3 fallback: full-line match tolerant only to uniform block indentation shift (or single
/// full-line leading indentation shift when the trimmed content is non-trivial). Relative
/// indentation inside a multi-line block is always preserved verbatim.
fn find_indent_matches<'a>(original: &'a str, old: &'a str) -> Vec<IndentMatch<'a>> {
    let src_lines = scan_lf_lines(original);
    if src_lines.is_empty() {
        return Vec::new();
    }
    let (old_body, trailing_newline) = match old.strip_suffix('\n') {
        Some(body) => (body, true),
        None => (old, false),
    };
    let old_lines: Vec<&str> = old_body.split('\n').collect();
    let n = old_lines.len();
    if n == 0 || n > src_lines.len() {
        return Vec::new();
    }
    let Some(first_non_blank) = old_lines.iter().position(|line| !is_blank_ecma(line)) else {
        return Vec::new();
    };

    let mut matches = Vec::new();
    if n == 1 {
        let (old_lead, old_rest) = trim_start_indent(old_lines[0]);
        let old_norm = normalize_fuzzy(old_rest);
        // Guard against matching tiny single-line fragments like `}` or `x` on indentation alone.
        if old_norm.len() < 4 {
            return Vec::new();
        }
        for (i, span) in src_lines.iter().enumerate() {
            if trailing_newline && span.end == span.body_end {
                continue;
            }
            let src_body = &original[span.start..span.body_end];
            let (src_lead, src_rest) = trim_start_indent(src_body);
            if src_lead != old_lead && normalize_fuzzy(src_rest) == old_norm {
                let range_end = if trailing_newline {
                    span.end
                } else {
                    span.body_end
                };
                matches.push(IndentMatch {
                    range: span.start..range_end,
                    old_indent: old_lead,
                    src_indent: src_lead,
                    line_number: i + 1,
                });
            }
        }
        return matches;
    }

    let old_common = common_indent(old_lines.iter().copied());
    let old_rel: Vec<Option<String>> = old_lines
        .iter()
        .map(|line| {
            if is_blank_ecma(line) {
                None
            } else {
                Some(normalize_fuzzy(&line[old_common.len()..]))
            }
        })
        .collect();
    let first_stripped = normalize_fuzzy(trim_start_indent(old_lines[first_non_blank]).1);

    let mut i = 0;
    while i + n <= src_lines.len() {
        if trailing_newline && src_lines[i + n - 1].end == src_lines[i + n - 1].body_end {
            i += 1;
            continue;
        }
        let probe_span = &src_lines[i + first_non_blank];
        let probe_body = &original[probe_span.start..probe_span.body_end];
        if is_blank_ecma(probe_body)
            || normalize_fuzzy(trim_start_indent(probe_body).1) != first_stripped
        {
            i += 1;
            continue;
        }
        let src_common = common_indent(
            src_lines[i..i + n]
                .iter()
                .map(|span| &original[span.start..span.body_end]),
        );
        let all_match = old_rel.iter().enumerate().all(|(j, expected)| {
            let span = &src_lines[i + j];
            let src_body = &original[span.start..span.body_end];
            match expected {
                None => is_blank_ecma(src_body),
                Some(expected_norm) => {
                    !is_blank_ecma(src_body)
                        && normalize_fuzzy(&src_body[src_common.len()..]) == *expected_norm
                }
            }
        });
        if all_match {
            let range_end = if trailing_newline {
                src_lines[i + n - 1].end
            } else {
                src_lines[i + n - 1].body_end
            };
            matches.push(IndentMatch {
                range: src_lines[i].start..range_end,
                old_indent: old_common,
                src_indent: src_common,
                line_number: i + 1,
            });
            i += n;
        } else {
            i += 1;
        }
    }
    matches
}

fn reindent_replacement<'a>(new_text: &'a str, old_indent: &str, src_indent: &str) -> Cow<'a, str> {
    if old_indent == src_indent || new_text.is_empty() {
        return Cow::Borrowed(new_text);
    }
    let mut out = String::with_capacity(new_text.len() + 32);
    for part in new_text.split_inclusive('\n') {
        let (body, nl) = match part.strip_suffix('\n') {
            Some(b) => (b, "\n"),
            None => (part, ""),
        };
        if is_blank_ecma(body) {
            out.push_str(body);
        } else if !old_indent.is_empty() {
            if let Some(stripped) = body.strip_prefix(old_indent) {
                out.push_str(src_indent);
                out.push_str(stripped);
            } else {
                out.push_str(body);
            }
        } else {
            out.push_str(src_indent);
            out.push_str(body);
        }
        out.push_str(nl);
    }
    Cow::Owned(out)
}

fn normalize_newlines(text: &str) -> Cow<'_, str> {
    if !text.contains('\r') {
        return Cow::Borrowed(text);
    }
    let mut output = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            output.push('\n');
        } else {
            output.push(ch);
        }
    }
    Cow::Owned(output)
}

fn ecma_whitespace(ch: char) -> bool {
    matches!(ch, '\u{0009}'..='\u{000d}' | '\u{0020}' | '\u{00a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}')
}

fn fold_char(ch: char) -> char {
    match ch {
        '\u{2018}'..='\u{201b}' => '\'',
        '\u{201c}'..='\u{201f}' => '"',
        '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
        '\u{00a0}' | '\u{2002}'..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}' => ' ',
        _ => ch,
    }
}

fn normalize_fuzzy(text: &str) -> String {
    if text.is_ascii() {
        let mut output = String::with_capacity(text.len());
        for (index, line) in text.split('\n').enumerate() {
            if index > 0 {
                output.push('\n');
            }
            output.push_str(line.trim_end_matches(ecma_whitespace));
        }
        return output;
    }
    let nfkc: String = text.nfkc().collect();
    let mut output = String::with_capacity(nfkc.len());
    for (index, line) in nfkc.split('\n').enumerate() {
        if index > 0 {
            output.push('\n');
        }
        for ch in line.trim_end_matches(ecma_whitespace).chars() {
            output.push(fold_char(ch));
        }
    }
    output
}

fn splice(
    source: &str,
    block: Range<usize>,
    replacements: &[Replacement<'_>],
    output: &mut String,
) {
    let mut cursor = block.start;
    for edit in replacements {
        output.push_str(&source[cursor..edit.range.start]);
        output.push_str(&edit.text);
        cursor = edit.range.end;
    }
    output.push_str(&source[cursor..block.end]);
}

/// Normalization is only for locating; it keeps a byte mapping back to the original per Unicode grapheme.
struct Mapping {
    normalized: Range<usize>,
    original: Range<usize>,
    exact: bool,
}
struct MappedText {
    text: String,
    spans: Vec<Mapping>,
}
impl MappedText {
    fn new(original: &str) -> Self {
        let mut mapped = Self {
            text: String::with_capacity(original.len()),
            spans: Vec::new(),
        };
        let mut source_start = 0;
        for part in original.split_inclusive('\n') {
            let line_start = mapped.text.len();
            let body = part.strip_suffix('\n').unwrap_or(part);
            if body.is_ascii() {
                let trimmed = body.trim_end_matches(ecma_whitespace);
                mapped.push(source_start..source_start + trimmed.len(), trimmed, true);
            } else {
                for (offset, grapheme) in body.grapheme_indices(true) {
                    let range = source_start + offset..source_start + offset + grapheme.len();
                    if grapheme.is_ascii() {
                        mapped.push(range, grapheme, true);
                    } else {
                        let folded: String = grapheme.nfkc().map(fold_char).collect();
                        mapped.push(range, &folded, folded == grapheme);
                    }
                }
                let kept = line_start
                    + mapped.text[line_start..]
                        .trim_end_matches(ecma_whitespace)
                        .len();
                mapped.text.truncate(kept);
                while mapped
                    .spans
                    .last()
                    .is_some_and(|s| s.normalized.start >= kept)
                {
                    mapped.spans.pop();
                }
                if let Some(last) = mapped.spans.last_mut()
                    && last.normalized.end > kept
                {
                    if last.exact {
                        last.original.end -= last.normalized.end - kept;
                    } else {
                        last.exact = false;
                    }
                    last.normalized.end = kept;
                }
            }
            if part.ends_with('\n') {
                mapped.push(
                    source_start + body.len()..source_start + part.len(),
                    "\n",
                    true,
                );
            }
            source_start += part.len();
        }
        mapped
    }
    fn push(&mut self, original: Range<usize>, text: &str, exact: bool) {
        let start = self.text.len();
        self.text.push_str(text);
        if exact
            && let Some(last) = self.spans.last_mut()
            && last.exact
            && last.original.end == original.start
            && last.normalized.end == start
        {
            last.original.end = original.end;
            last.normalized.end = self.text.len();
            return;
        }
        if !text.is_empty() {
            self.spans.push(Mapping {
                normalized: start..self.text.len(),
                original,
                exact,
            });
        }
    }
    fn original_range(&self, range: Range<usize>) -> Result<Range<usize>> {
        let start_span = self
            .spans
            .get(
                self.spans
                    .partition_point(|s| s.normalized.end <= range.start),
            )
            .context("cannot map fuzzy match; read the file again")?;
        let end_index = self
            .spans
            .partition_point(|s| s.normalized.start < range.end);
        let end_span = self
            .spans
            .get(end_index.saturating_sub(1))
            .context("cannot map fuzzy match; read the file again")?;
        ensure!(
            (start_span.exact || range.start == start_span.normalized.start)
                && (end_span.exact || range.end == end_span.normalized.end),
            "fuzzy match splits a Unicode normalization unit; provide more context or read the file again"
        );
        let start = if start_span.exact {
            start_span.original.start + range.start - start_span.normalized.start
        } else {
            start_span.original.start
        };
        let end = if end_span.exact {
            end_span.original.start + range.end - end_span.normalized.start
        } else {
            end_span.original.end
        };
        Ok(start..end)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn apply_edits(source: &str, edits: &[(&str, &str)]) -> Result<String> {
        apply(
            source,
            &edits
                .iter()
                .map(|(old, new)| Edit::new((*old).into(), (*new).into(), false))
                .collect::<Vec<_>>(),
        )
        .map(|(output, _)| output)
    }

    #[test]
    fn replace_ordering_fuzzy_matching_and_newlines() {
        assert_eq!(
            apply_edits("one\ntwo\nthree\n", &[("three", "THREE"), ("one", "ONE")]).unwrap(),
            "ONE\ntwo\nTHREE\n"
        );
        assert_eq!(
            apply_edits("a\nb\n", &[("a", "b"), ("b", "c")]).unwrap(),
            "b\nc\n"
        );
        assert_eq!(
            apply_edits("abc\n", &[("a", "A"), ("b", "B")]).unwrap(),
            "ABc\n"
        );
        assert_eq!(apply_edits("aaa", &[("aa", "X")]).unwrap(), "Xa");

        assert_eq!(
            apply_edits(
                "keep “Ｋ”  \nsay “Hi”—x  \nend\n",
                &[("say \"Hi\"-x", "said\nok")]
            )
            .unwrap(),
            "keep “Ｋ”  \nsaid\nok  \nend\n"
        );
        assert_eq!(
            apply_edits(
                "Ｋ  \nＫ\nＡ\ntarget “x”\n",
                &[("Ａ", "B"), ("target \"x\"", "done")]
            )
            .unwrap(),
            "Ｋ  \nＫ\nB\ndone\n"
        );
        assert_eq!(
            apply_edits(
                "“a”  middle  “b”  \nkeep  \n",
                &[("\"a\"", "A"), ("middle", "MIDDLE"), ("\"b\"", "B")]
            )
            .unwrap(),
            "A  MIDDLE  B  \nkeep  \n"
        );
        assert_eq!(
            apply_edits("“x”  \nkeep Ｋ  \n", &[("\"x\"\n", "done\n")]).unwrap(),
            "done\nkeep Ｋ  \n"
        );
        assert_eq!(
            apply_edits("keep Ｋ  \n“x”  ", &[("\"x\"", "")]).unwrap(),
            "keep Ｋ  \n  "
        );

        for (source, old, new, expected) in [
            ("\u{feff}a\r\nb\r\n", "a\nb", "A\nB", "\u{feff}A\r\nB\r\n"),
            ("a\rb\r", "a\rb", "A\rB", "A\nB\n"),
            ("a\rb\r\nc\n", "a", "A", "A\r\nb\r\nc\r\n"),
            ("a\nb\r\n", "a", "A", "A\nb\n"),
            ("abc", "b", "", "ac"),
            ("abc", "abc", "", ""),
            ("a\nb", "b", "B", "a\nB"),
        ] {
            assert_eq!(apply_edits(source, &[(old, new)]).unwrap(), expected);
        }
    }

    #[test]
    fn replace_rejects_invalid_and_preserves_unicode_boundaries() {
        for (source, edits) in [
            ("X\nＡ\nA\n", vec![("Ａ", "B")]),
            ("abc\n", vec![("abc", "x"), ("bc", "y")]),
            ("abc\n", vec![("abc", "x"), ("abc", "y")]),
            ("one\ntwo\n", vec![("one", "ONE"), ("missing", "x")]),
            ("one\n", vec![]),
            ("one\n", vec![("", "x")]),
            ("one  \n", vec![("  ", "x")]),
            ("", vec![("a", "x")]),
            ("one\n", vec![("one", "one")]),
        ] {
            assert!(apply_edits(source, &edits).is_err(), "{source:?} {edits:?}");
        }
        assert_eq!(
            apply_edits("one\ntwo\n", &[("one", "one"), ("two", "TWO")]).unwrap(),
            "one\nTWO\n"
        );

        assert_eq!(
            apply_edits("x\u{feff}\ny\n", &[("x\ny", "z")]).unwrap(),
            "z\n"
        );
        // U+0085 is Rust whitespace, but not ECMAScript trimEnd whitespace.
        assert!(apply_edits("x\u{85}\ny", &[("x\ny", "z")]).is_err());
        for old in ["X", " x", "x  y", "xy"] {
            assert!(apply_edits("x y", &[(old, "z")]).is_err());
        }
        assert_eq!(
            apply_edits("say ‘hi’−Ｋ\u{a0}x\n", &[("say 'hi'-K x", "ok")]).unwrap(),
            "ok\n"
        );

        assert_eq!(
            apply_edits("before Ｋ “x” Ａ  \n", &[("\"x\"", "Y")]).unwrap(),
            "before Ｋ Y Ａ  \n"
        );
        assert_eq!(
            apply_edits("prefix e\u{301} tail ﬃ Ｋ  \n", &[("é", "E"), ("ffi", "F")]).unwrap(),
            "prefix E tail F Ｋ  \n"
        );
        assert_eq!(
            apply_edits("x\n\n“a”  \n", &[("\"a\"", "A")]).unwrap(),
            "x\n\nA  \n"
        );
        assert!(apply_edits("ﬃ", &[("fi", "X")]).is_err());
        for text in [
            "e\u{301}",
            "\u{1100}\u{1161}\u{11a8}",
            "a\u{315}\u{300}",
            "“Ｋ”  \n\nＡ\t\n",
        ] {
            assert_eq!(MappedText::new(text).text, normalize_fuzzy(text));
        }
    }

    #[test]
    fn replace_diagnostics_report_duplicate_and_near_match_lines() {
        let duplicate = apply_edits(
            "alpha\nneedle\nbeta\nneedle\ngamma\n",
            &[("needle", "fixed")],
        )
        .unwrap_err()
        .to_string();
        assert!(
            duplicate.contains("is not unique") && duplicate.contains("lines 2, 4"),
            "{duplicate}"
        );

        let missing = apply_edits(
            "fn one() {}\nfn target_anchor() {\n    old_body();\n}\n",
            &[("fn target_anchor() {\n    wrong_body();\n}", "x")],
        )
        .unwrap_err()
        .to_string();
        assert!(
            missing.contains("could not find edits[0].oldText")
                && missing.contains("first line matches near line 2")
                && missing.contains("actual line 3: `    old_body();`"),
            "{missing}"
        );
    }

    #[test]
    fn replace_all_indent_fallback_and_batch_diagnostics() {
        // replaceAll replaces exact, fuzzy, and indent-shifted occurrences and reports actual count.
        let (out, count) = apply(
            "alpha\nneedle\nbeta\nneedle\ngamma\n",
            &[Edit::new("needle".into(), "fixed".into(), true)],
        )
        .unwrap();
        assert_eq!(out, "alpha\nfixed\nbeta\nfixed\ngamma\n");
        assert_eq!(count, 2);

        let (out, count) = apply(
            "say “Hi”\nkeep\nsay “Hi”  \n",
            &[Edit::new("say \"Hi\"".into(), "said".into(), true)],
        )
        .unwrap();
        assert_eq!(out, "said\nkeep\nsaid  \n");
        assert_eq!(count, 2);

        // Multi-line uniform indentation shift re-indents newText to match source indentation.
        let src = "fn main() {\n    if ready {\n        run();\n    }\n}\n";
        let out = apply_edits(
            src,
            &[(
                "if ready {\n    run();\n}",
                "if ready {\n    prepare();\n    run();\n}",
            )],
        )
        .unwrap();
        assert_eq!(
            out,
            "fn main() {\n    if ready {\n        prepare();\n        run();\n    }\n}\n"
        );

        // Dedenting an over-indented model block aligns back to column 0.
        let out = apply_edits(
            "/// First doc line.\n/// Second doc line.\nfn demo() {}\n",
            &[(
                "    /// First doc line.\n    /// Second doc line.",
                "    /// Updated doc line.",
            )],
        )
        .unwrap();
        assert_eq!(out, "/// Updated doc line.\nfn demo() {}\n");

        // Single full-line leading indent shift works for non-trivial lines and preserves source indent.
        let out = apply_edits(
            "fn f() {\n        let value = compute();\n}\n",
            &[(
                "    let value = compute();",
                "    let value = next_compute();",
            )],
        )
        .unwrap();
        assert_eq!(out, "fn f() {\n        let value = next_compute();\n}\n");

        // Ambiguous indent matches are still rejected.
        let dup_indent = apply_edits(
            "fn a() {\n    let value = compute();\n}\nfn b() {\n        let value = compute();\n}\n",
            &[("let value = compute();", "let value = 2;")],
        )
        .unwrap_err()
        .to_string();
        assert!(
            dup_indent.contains("is not unique")
                && dup_indent.contains("lines 2, 5")
                && dup_indent.contains("replaceAll: true"),
            "{dup_indent}"
        );

        // Batch validation reports multiple failing items in one response instead of stopping at the first.
        let batch_err = apply_edits(
            "alpha\nrepeat\nbeta\nrepeat\n",
            &[("missing_item", "x"), ("repeat", "y"), ("", "z")],
        )
        .unwrap_err()
        .to_string();
        assert!(
            batch_err.contains("edits[0].oldText")
                && batch_err.contains("edits[1].oldText is not unique")
                && batch_err.contains("edits[2].oldText must not be empty"),
            "{batch_err}"
        );

        // Actionable hints: line-number prefix, over-escaped quotes, and single-char CJK/ASCII typos.
        let line_num = apply_edits(
            "alpha\nlet target = compute();\nomega\n",
            &[("2: let target = compute();", "x")],
        )
        .unwrap_err()
        .to_string();
        assert!(
            line_num.contains("line-number prefixes") && line_num.contains("line 2"),
            "{line_num}"
        );

        let escaped = apply_edits(
            "let raw = r#\"{\"status\":503}\"#;\n",
            &[("let raw = r#\"{\\\"status\\\":503}\"#;", "x")],
        )
        .unwrap_err()
        .to_string();
        assert!(
            escaped.contains("over-escaped") && escaped.contains("line 1"),
            "{escaped}"
        );

        let ascii_typo = apply_edits(
            "const action = e.target.closest(\"tr[data-provider-row],tr[data-model-row]\");\n",
            &[(
                "const action = e.target.closest(\"tr[data-provider-row],th[data-model-row]\");",
                "x",
            )],
        )
        .unwrap_err()
        .to_string();
        assert!(
            ascii_typo.contains("closest match near line 1")
                && ascii_typo.contains("tr[data-model-row]"),
            "{ascii_typo}"
        );

        // CJK payload verifies Unicode bigram similarity matches a single mistyped character.
        let cjk_typo = apply_edits(
            "// 嵌套宿主工具调用期间暂停计算超时窗口，返回后再顺延。\n",
            &[(
                "// 嵡套宿主工具调用期间暂停计算超时窗口，返回后再顺延。",
                "x",
            )],
        )
        .unwrap_err()
        .to_string();
        assert!(
            cjk_typo.contains("closest match near line 1") && cjk_typo.contains("嵌套宿主工具"),
            "{cjk_typo}"
        );
    }
}
