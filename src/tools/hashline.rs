use anyhow::{Result, bail, ensure};
use serde::Deserialize;
use std::fmt::Write as _;

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Edit {
    Replace {
        start: String,
        end: String,
        lines: Vec<String>,
    },
    InsertBefore {
        anchor: String,
        lines: Vec<String>,
    },
    InsertAfter {
        anchor: String,
        lines: Vec<String>,
    },
}

struct Line {
    start: usize,
    body_end: usize,
    end: usize,
}

// Both rendering and editing use byte ranges, preserving untouched line endings.
fn scan(bytes: &[u8]) -> impl Iterator<Item = Line> + '_ {
    let mut start = 0;
    std::iter::from_fn(move || {
        if start == bytes.len() {
            return None;
        }
        let line_start = start;
        let (body_end, end) = match bytes[start..].iter().position(|&byte| byte == b'\n') {
            Some(offset) => {
                let lf = start + offset;
                (
                    lf - usize::from(lf > start && bytes[lf - 1] == b'\r'),
                    lf + 1,
                )
            }
            None => (bytes.len(), bytes.len()),
        };
        start = end;
        Some(Line {
            start: line_start,
            body_end,
            end,
        })
    })
}

fn hash(bytes: &[u8]) -> u64 {
    bytes.iter().fold(14695981039346656037, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(1099511628211)
    })
}

#[cfg(test)]
fn render_read(bytes: &[u8], truncated: bool) -> String {
    let bytes = if truncated {
        &bytes[..bytes
            .iter()
            .rposition(|&byte| byte == b'\n')
            .map_or(0, |at| at + 1)]
    } else {
        bytes
    };
    render_page(bytes, 1)
}

pub(super) fn render_page(bytes: &[u8], offset: usize) -> String {
    let mut output = String::with_capacity(bytes.len());
    for (index, line) in scan(bytes).enumerate() {
        let body = &bytes[line.start..line.body_end];
        writeln!(
            output,
            "{}:{:016x}|{}",
            index + offset,
            hash(body),
            String::from_utf8_lossy(body)
        )
        .expect("writing to a String cannot fail");
    }
    output
}

fn resolve(anchor: &str, lines: &[Line], source: &[u8], op: usize) -> Result<usize> {
    let invalid = || anyhow::anyhow!("invalid hashline anchor in operation {op}: {anchor:?}");
    let (number, digest) = anchor.split_once(':').ok_or_else(invalid)?;
    ensure!(
        !number.is_empty()
            && !number.starts_with('0')
            && number.bytes().all(|byte| byte.is_ascii_digit())
            && digest.len() == 16
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "invalid hashline anchor in operation {op}: {anchor:?}"
    );
    let number: usize = number.parse().map_err(|_| invalid())?;
    let line = lines
        .get(number - 1)
        .ok_or_else(|| anyhow::anyhow!("hashline line out of range in operation {op}: {number}"))?;
    let actual = hash(&source[line.start..line.body_end]);
    let supplied = u64::from_str_radix(digest, 16).map_err(|_| invalid())?;
    ensure!(
        supplied == actual,
        "hashline mismatch at line {number}: expected {anchor}, actual {number}:{actual:016x}; read the file again (operation {op})"
    );
    Ok(number - 1)
}

struct Change<'a> {
    start: usize,
    end: usize,
    lines: &'a [String],
    leading_separator: bool,
    trailing_separator: bool,
    op: usize,
}

pub(super) fn apply(source: &str, edits: &[Edit]) -> Result<String> {
    ensure!(
        !edits.is_empty(),
        "invalid hashline edits: edits must not be empty"
    );
    let bytes = source.as_bytes();
    let lines: Vec<_> = scan(bytes).collect();
    let separator = lines
        .iter()
        .find(|line| line.end > line.body_end)
        .map_or("\n", |line| &source[line.body_end..line.end]);
    let mut changes = Vec::with_capacity(edits.len());
    for (index, edit) in edits.iter().enumerate() {
        let op = index + 1;
        let (new_lines, insertion) = match edit {
            Edit::Replace { lines, .. } => (lines, false),
            Edit::InsertBefore { lines, .. } | Edit::InsertAfter { lines, .. } => (lines, true),
        };
        ensure!(
            !insertion || !new_lines.is_empty(),
            "invalid hashline edits in operation {op}: insertion lines must not be empty"
        );
        ensure!(
            new_lines.iter().all(|line| !line.contains(['\r', '\n'])),
            "invalid hashline edits in operation {op}: lines must not contain CR or LF"
        );
        let change = match edit {
            Edit::Replace { start, end, .. } => {
                let start = resolve(start, &lines, bytes, op)?;
                let end = resolve(end, &lines, bytes, op)?;
                ensure!(start <= end, "hashline range is reversed in operation {op}");
                Change {
                    start: lines[start].start,
                    end: lines[end].end,
                    lines: new_lines,
                    leading_separator: false,
                    trailing_separator: lines[end].end > lines[end].body_end,
                    op,
                }
            }
            Edit::InsertBefore { anchor, .. } | Edit::InsertAfter { anchor, .. } => {
                let line = &lines[resolve(anchor, &lines, bytes, op)?];
                let after = matches!(edit, Edit::InsertAfter { .. });
                let unterminated = after && line.end == line.body_end;
                let at = if after { line.end } else { line.start };
                Change {
                    start: at,
                    end: at,
                    lines: new_lines,
                    leading_separator: unterminated,
                    trailing_separator: !unterminated,
                    op,
                }
            }
        };
        changes.push(change);
    }
    changes.sort_unstable_by_key(|change| (change.start, change.end));
    for pair in changes.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        let conflict = if a.start == a.end || b.start == b.end {
            b.start <= a.end
        } else {
            b.start < a.end
        };
        if conflict {
            bail!(
                "conflicting hashline edits in operations {} and {}",
                a.op,
                b.op
            );
        }
    }
    let mut output = String::with_capacity(source.len());
    let mut cursor = 0;
    for change in changes {
        output.push_str(&source[cursor..change.start]);
        if change.leading_separator {
            output.push_str(separator);
        }
        for (index, line) in change.lines.iter().enumerate() {
            output.push_str(line);
            if index + 1 < change.lines.len() || change.trailing_separator {
                output.push_str(separator);
            }
        }
        cursor = change.end;
    }
    output.push_str(&source[cursor..]);
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn anchors(source: &str) -> Vec<String> {
        render_read(source.as_bytes(), false)
            .lines()
            .map(|line| line.split_once('|').unwrap().0.to_owned())
            .collect()
    }

    fn edit(source: &str, edits: Value) -> Result<String> {
        apply(source, &serde_json::from_value::<Vec<Edit>>(edits)?)
    }

    #[test]
    fn hashline_render_vectors_and_bytes() {
        assert_eq!(render_read(b"hello", false), "1:a430d84680aabd0b|hello\n");
        let source = "中文\n  x\nx\nx\n\n \r\nlast\r";
        let rendered = render_read(source.as_bytes(), false);
        let rows: Vec<_> = rendered.lines().collect();
        assert_eq!(rows.len(), 7);
        assert!(rows[0].ends_with("|中文"));
        assert_ne!(
            rows[1]
                .split_once('|')
                .unwrap()
                .0
                .split_once(':')
                .unwrap()
                .1,
            rows[2]
                .split_once('|')
                .unwrap()
                .0
                .split_once(':')
                .unwrap()
                .1
        );
        assert_eq!(
            rows[2].split_once(':').unwrap().1,
            rows[3].split_once(':').unwrap().1
        );
        assert!(rows[4].ends_with('|'));
        assert!(rendered.ends_with("|last\r\n"));
        assert_eq!(render_read(b"", false), "");
        assert_eq!(render_read(b"\n\n", false).lines().count(), 2);
        assert!(render_read(b"\xff\n", false).ends_with("|�\n"));
    }

    #[test]
    fn hashline_snapshot_operations_and_ranges() {
        let source = "one\ntwo\nthree\n";
        let a = anchors(source);
        assert_eq!(
            edit(
                source,
                json!([
                    {"op":"insert_after","anchor":a[2],"lines":["four"]},
                    {"op":"replace","start":a[1],"end":a[1],"lines":["TWO"]},
                    {"op":"insert_before","anchor":a[0],"lines":["zero"]}
                ])
            )
            .unwrap(),
            "zero\none\nTWO\nthree\nfour\n"
        );
        assert_eq!(
            edit(
                source,
                json!([
                    {"op":"replace","start":a[0],"end":a[1],"lines":["both", ""]},
                    {"op":"replace","start":a[2],"end":a[2],"lines":[]}
                ])
            )
            .unwrap(),
            "both\n\n"
        );
        assert_eq!(
            edit(
                source,
                json!([{"op":"replace","start":a[0],"end":a[2],"lines":[]}])
            )
            .unwrap(),
            ""
        );
        // Only supplied endpoints are checked, not unanchored interior lines.
        assert_eq!(
            edit(
                "one\nchanged\nthree\n",
                json!([{"op":"replace","start":a[0],"end":a[2],"lines":["all"]}])
            )
            .unwrap(),
            "all\n"
        );
    }

    #[test]
    fn hashline_line_endings_and_literal_content() {
        for (source, expected) in [
            ("a\r\nb\r\n", "a\r\nB\r\n"),
            ("a\nb", "a\nB"),
            ("a\r\nb\nc\r", "a\r\nB\r\nc\r"),
        ] {
            let a = anchors(source);
            assert_eq!(
                edit(
                    source,
                    json!([{"op":"replace","start":a[1],"end":a[1],"lines":["B"]}])
                )
                .unwrap(),
                expected
            );
        }
        let a = anchors("a\nB");
        assert_eq!(
            edit(
                "a\nB",
                json!([{"op":"insert_after","anchor":a[1],"lines":["c"]}])
            )
            .unwrap(),
            "a\nB\nc"
        );
        let a = anchors("a");
        assert_eq!(
            edit(
                "a",
                json!([
                    {"op":"insert_before","anchor":a[0],"lines":["1:literal|text", ""]},
                    {"op":"insert_after","anchor":a[0],"lines":["b", "c"]}
                ])
            )
            .unwrap(),
            "1:literal|text\n\na\nb\nc"
        );
    }

    #[test]
    fn hashline_invalid_anchors_and_conflicts() {
        let source = "a\nb\nc\n";
        let a = anchors(source);
        for anchor in [
            "0:af63dc4c8601ec8c",
            "01:af63dc4c8601ec8c",
            "184467440737095516160:af63dc4c8601ec8c",
            " 1:af63dc4c8601ec8c",
            "+1:af63dc4c8601ec8c",
            "1:AF63DC4C8601EC8C",
            "1:af63dc4c8601ec8c|a",
            "1:bad",
        ] {
            let error = edit(
                source,
                json!([{"op":"replace","start":anchor,"end":a[0],"lines":[]}]),
            )
            .unwrap_err()
            .to_string();
            assert!(error.starts_with("invalid hashline anchor"), "{error}");
            assert!(error.contains("operation 1"));
        }
        for (edits, prefix) in [
            (
                json!([{"op":"replace","start":"4:0000000000000000","end":a[0],"lines":[]}]),
                "hashline line out of range",
            ),
            (
                json!([{"op":"replace","start":a[2],"end":a[0],"lines":[]}]),
                "hashline range is reversed",
            ),
            (
                json!([{"op":"replace","start":a[0],"end":a[1],"lines":[]},{"op":"replace","start":a[1],"end":a[2],"lines":[]}]),
                "conflicting hashline edits",
            ),
        ] {
            assert!(
                edit(source, edits)
                    .unwrap_err()
                    .to_string()
                    .starts_with(prefix)
            );
        }
        for (op, anchor) in [
            ("insert_before", &a[0]),
            ("insert_before", &a[1]),
            ("insert_after", &a[1]),
        ] {
            assert!(
                edit(
                    source,
                    json!([
                        {"op":"replace","start":a[0],"end":a[1],"lines":[]},
                        {"op":op,"anchor":anchor,"lines":["x"]}
                    ])
                )
                .unwrap_err()
                .to_string()
                .starts_with("conflicting hashline edits")
            );
        }
        assert!(
            edit(
                source,
                json!([
                    {"op":"insert_after","anchor":a[0],"lines":["x"]},
                    {"op":"insert_before","anchor":a[1],"lines":["y"]}
                ])
            )
            .unwrap_err()
            .to_string()
            .starts_with("conflicting hashline edits")
        );
        let error = edit(
            source,
            json!([{"op":"replace","start":"1:0000000000000000","end":a[0],"lines":[]}]),
        )
        .unwrap_err()
        .to_string();
        assert_eq!(
            error,
            format!(
                "hashline mismatch at line 1: expected 1:0000000000000000, actual {}; read the file again (operation 1)",
                a[0]
            )
        );
    }

    #[test]
    fn hashline_invalid_payloads_and_truncation() {
        let a = anchors("a\n");
        for edits in [
            json!([]),
            json!([{"op":"insert_before","anchor":a[0],"lines":[]}]),
            json!([{"op":"replace","start":a[0],"end":a[0],"lines":["x\ny"]}]),
            json!([{"op":"replace","start":a[0],"end":a[0],"lines":["x\ry"]}]),
            json!([{"op":"unknown","anchor":a[0],"lines":["x"]}]),
            json!([{"op":"insert_after","anchor":a[0],"lines":["x"],"extra":true}]),
        ] {
            assert!(edit("a\n", edits).is_err());
        }
        assert!(
            edit(
                "",
                json!([{"op":"insert_after","anchor":a[0],"lines":["x"]}])
            )
            .unwrap_err()
            .to_string()
            .starts_with("hashline line out of range")
        );
        assert_eq!(
            render_read(b"ok\npartial\r", true),
            render_read(b"ok\n", false)
        );
        assert_eq!(
            render_read(b"ok\n\xe4\xb8", true),
            render_read(b"ok\n", false)
        );
        assert_eq!(render_read(b"partial", true), "");
    }
}
