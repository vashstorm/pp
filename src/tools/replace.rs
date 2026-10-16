use anyhow::{Result, ensure};
use serde::Deserialize;
use std::{borrow::Cow, ops::Range};
use unicode_normalization::UnicodeNormalization;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Edit {
    old_text: String,
    new_text: String,
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
    let fuzzy = normalize_fuzzy(&original);
    let mut replacements = Vec::with_capacity(edits.len());
    let mut old_texts = Vec::with_capacity(edits.len());
    let mut needs_fuzzy = false;
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
        ensure!(
            candidates.next().is_none(),
            "edits[{index}].oldText is not unique; provide more context"
        );
        let range = if let Some(start) = original.find(old.as_ref()) {
            start..start + old.len()
        } else {
            let start = candidate.map(|(start, _)| start).ok_or_else(|| {
                anyhow::anyhow!("could not find edits[{index}].oldText; read the file again")
            })?;
            needs_fuzzy = true;
            start..start + normalized.len()
        };
        old_texts.push((old, normalized));
        replacements.push(Replacement {
            index,
            range,
            text: normalize_newlines(&edit.new_text),
        });
    }
    if needs_fuzzy {
        // Once a fuzzy edit is necessary, every range must refer to the same snapshot.
        for (replacement, (old, normalized)) in replacements.iter_mut().zip(&old_texts) {
            let (start, len) = if let Some(start) = fuzzy.find(old.as_ref()) {
                (start, old.len())
            } else {
                (
                    fuzzy.find(normalized.as_str()).ok_or_else(|| {
                        anyhow::anyhow!(
                            "could not find edits[{}].oldText; read the file again",
                            replacement.index
                        )
                    })?,
                    normalized.len(),
                )
            };
            replacement.range = start..start + len;
        }
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
    let output = if needs_fuzzy {
        preserve_lines(&original, &fuzzy, &replacements)?
    } else {
        let mut output = String::with_capacity(
            original.len()
                + replacements
                    .iter()
                    .map(|edit| edit.text.len().saturating_sub(edit.range.len()))
                    .sum::<usize>(),
        );
        splice(&original, 0..original.len(), &replacements, &mut output);
        output
    };
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

fn normalize_fuzzy(text: &str) -> String {
    let nfkc: String = text.nfkc().collect();
    let mut output = String::with_capacity(nfkc.len());
    for (index, line) in nfkc.split('\n').enumerate() {
        if index > 0 {
            output.push('\n');
        }
        for ch in line.trim_end_matches(ecma_whitespace).chars() {
            output.push(match ch {
                '\u{2018}'..='\u{201b}' => '\'',
                '\u{201c}'..='\u{201f}' => '"',
                '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
                '\u{00a0}' | '\u{2002}'..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}' => ' ',
                _ => ch,
            });
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

fn line_starts(text: &str) -> Vec<usize> {
    std::iter::once(0)
        .chain(text.match_indices('\n').map(|(offset, _)| offset + 1))
        .collect()
}

fn preserve_lines(original: &str, fuzzy: &str, replacements: &[Replacement<'_>]) -> Result<String> {
    let original_lines = line_starts(original);
    let fuzzy_lines = line_starts(fuzzy);
    ensure!(
        original_lines.len() == fuzzy_lines.len(),
        "cannot preserve unchanged lines: normalized line count differs"
    );
    let line_at = |offset| fuzzy_lines.partition_point(|start| *start <= offset) - 1;
    let mut output = String::with_capacity(
        original.len()
            + replacements
                .iter()
                .map(|edit| edit.text.len().saturating_sub(edit.range.len()))
                .sum::<usize>(),
    );
    let mut cursor = 0;
    let mut first = 0;
    while first < replacements.len() {
        let start_line = line_at(replacements[first].range.start);
        let mut end_line = line_at(replacements[first].range.end - 1);
        let mut end = first + 1;
        while end < replacements.len() && line_at(replacements[end].range.start) <= end_line {
            end_line = end_line.max(line_at(replacements[end].range.end - 1));
            end += 1;
        }
        let original_end = original_lines
            .get(end_line + 1)
            .copied()
            .unwrap_or(original.len());
        let fuzzy_end = fuzzy_lines
            .get(end_line + 1)
            .copied()
            .unwrap_or(fuzzy.len());
        output.push_str(&original[cursor..original_lines[start_line]]);
        splice(
            fuzzy,
            fuzzy_lines[start_line]..fuzzy_end,
            &replacements[first..end],
            &mut output,
        );
        cursor = original_end;
        first = end;
    }
    output.push_str(&original[cursor..]);
    Ok(output)
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
    fn replace_snapshot_order_and_adjacency() {
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
    }

    #[test]
    fn replace_fuzzy_preserves_untouched_lines_by_position() {
        assert_eq!(
            apply_edits(
                "keep “Ｋ”  \nsay “Hi”—x  \nend\n",
                &[("say \"Hi\"-x", "said\nok")]
            )
            .unwrap(),
            "keep “Ｋ”  \nsaid\nok\nend\n"
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
            "A  MIDDLE  B\nkeep  \n"
        );
        assert_eq!(
            apply_edits("“x”  \nkeep Ｋ  \n", &[("\"x\"\n", "done\n")]).unwrap(),
            "done\nkeep Ｋ  \n"
        );
        assert_eq!(
            apply_edits("keep Ｋ  \n“x”  ", &[("\"x\"", "")]).unwrap(),
            "keep Ｋ  \n"
        );
    }

    #[test]
    fn replace_rejects_ambiguity_overlap_missing_and_empty() {
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
    }

    #[test]
    fn replace_bom_newlines_deletion_and_eof() {
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
    fn replace_fuzzy_whitespace_and_punctuation_boundaries() {
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
    }
}
