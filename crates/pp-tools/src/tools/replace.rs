use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::{borrow::Cow, ops::Range};
use unicode_normalization::UnicodeNormalization;
use unicode_segmentation::UnicodeSegmentation;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Edit {
    old_text: String,
    new_text: String,
}
impl Edit {
    pub(super) fn new(old_text: String, new_text: String) -> Self {
        Self { old_text, new_text }
    }
}

struct Replacement<'a> {
    index: usize,
    range: Range<usize>,
    text: Cow<'a, str>,
}

pub(super) fn apply(source: &str, edits: &[Edit]) -> Result<String> {
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

    for (index, edit) in edits.iter().enumerate() {
        let old = normalize_newlines(&edit.old_text);
        ensure!(!old.is_empty(), "edits[{index}].oldText must not be empty");
        let normalized = normalize_fuzzy(&old);
        ensure!(
            !normalized.is_empty(),
            "edits[{index}].oldText must not be empty"
        );
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
            let list = lines
                .iter()
                .map(usize::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            let suffix = if total > lines.len() {
                format!(", ... ({total} total)")
            } else {
                String::new()
            };
            anyhow::bail!(
                "edits[{index}].oldText is not unique (matches at lines {list}{suffix}); provide more context"
            );
        }
        let range = if let Some(start) = original.find(old.as_ref()) {
            start..start + old.len()
        } else {
            let start = candidate.map(|(start, _)| start).ok_or_else(|| {
                if let Some(hint) = missing_edit_hint(&original, &old) {
                    anyhow::anyhow!(
                        "could not find edits[{index}].oldText ({hint}); read the file again"
                    )
                } else {
                    anyhow::anyhow!("could not find edits[{index}].oldText; read the file again")
                }
            })?;
            mapped.original_range(start..start + normalized.len())?
        };
        replacements.push(Replacement {
            index,
            range,
            text: normalize_newlines(&edit.new_text),
        });
    }
    replacements.sort_unstable_by_key(|replacement| replacement.range.start);
    for pair in replacements.windows(2) {
        ensure!(
            pair[0].range.end <= pair[1].range.start,
            "edits[{}] and edits[{}] overlap; merge them into one edit",
            pair[0].index,
            pair[1].index
        );
    }
    let mut output = String::with_capacity(original.len());
    splice(&original, 0..original.len(), &replacements, &mut output);
    ensure!(output != original, "edit produced no changes");
    if bom.is_empty() && !crlf {
        return Ok(output);
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
    Ok(restored)
}

fn line_number_at(text: &str, byte_offset: usize) -> usize {
    text[..byte_offset]
        .bytes()
        .filter(|&byte| byte == b'\n')
        .count()
        + 1
}

/// When `oldText` misses entirely, locate the nearby line in the original from the first distinctive non-empty line, helping the model narrow the re-read.
fn missing_edit_hint(original: &str, old: &str) -> Option<String> {
    let anchor = old.lines().map(str::trim).find(|line| line.len() >= 6)?;
    let multiline = old.contains('\n');
    let mut lines = Vec::new();
    for (idx, line) in original.lines().enumerate() {
        let trimmed = line.trim();
        let matched = if multiline {
            trimmed == anchor
        } else {
            trimmed == anchor || trimmed.contains(anchor)
        };
        if matched {
            lines.push(idx + 1);
            if lines.len() > 3 {
                return None;
            }
        }
    }
    if lines.is_empty() {
        return None;
    }
    let label = if lines.len() == 1 { "line" } else { "lines" };
    let list = lines
        .iter()
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!("first line matches near {label} {list}"))
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
                .map(|(old, new)| Edit {
                    old_text: (*old).into(),
                    new_text: (*new).into(),
                })
                .collect::<Vec<_>>(),
        )
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
                && missing.contains("first line matches near line 2"),
            "{missing}"
        );
    }
}
