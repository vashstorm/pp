//! 文件版本标签、会话快照与单文件行编辑协议。
use anyhow::{Context, Result, ensure};
use std::{
    collections::{HashMap, HashSet},
    fmt::Write as _,
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
};
use xxhash_rust::xxh32::Xxh32;

pub(super) const MAX_FILE_BYTES: usize = 4 * 1024 * 1024;
const MAX_PATHS: usize = 256;
const MAX_TOTAL_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Debug)]
pub(super) struct Line {
    pub start: usize,
    pub body_end: usize,
    pub end: usize,
}

/// 与展示和编辑共用；CRLF 与独立 CR 都代表一个换行。
pub(super) fn scan(source: &str) -> Vec<Line> {
    let bytes = source.as_bytes();
    let mut start = usize::from(bytes.starts_with(b"\xef\xbb\xbf")) * 3;
    let mut lines = Vec::new();
    while start < bytes.len() {
        let body_end = bytes[start..]
            .iter()
            .position(|b| matches!(b, b'\r' | b'\n'))
            .map_or(bytes.len(), |n| start + n);
        let end = if bytes.get(body_end) == Some(&b'\r') && bytes.get(body_end + 1) == Some(&b'\n')
        {
            body_end + 2
        } else {
            body_end + usize::from(body_end < bytes.len())
        };
        lines.push(Line {
            start,
            body_end,
            end,
        });
        start = end;
    }
    lines
}

fn hash_input(source: &str) -> String {
    let source = source.strip_prefix('\u{feff}').unwrap_or(source);
    let normalized = source.replace("\r\n", "\n").replace('\r', "\n");
    normalized
        .split('\n')
        .map(|line| line.trim_end_matches([' ', '\t', '\r']))
        .collect::<Vec<_>>()
        .join("\n")
}

/// 协议规则参考 oh-my-pi/pi-edit 的 file_hash；只保留 xxHash32 低 16 位。
pub(super) fn file_hash(source: &str) -> String {
    let source = source.strip_prefix('\u{feff}').unwrap_or(source);
    let bytes = source.as_bytes();
    let mut hasher = Xxh32::new(0);
    let mut start = 0;
    while start < bytes.len() {
        let end = bytes[start..]
            .iter()
            .position(|b| matches!(b, b'\r' | b'\n'))
            .map_or(bytes.len(), |offset| start + offset);
        hasher.update(source[start..end].trim_end_matches([' ', '\t']).as_bytes());
        if end == bytes.len() {
            break;
        }
        hasher.update(b"\n");
        start = end
            + if bytes.get(end..end + 2) == Some(b"\r\n") {
                2
            } else {
                1
            };
    }
    format!("{:04X}", hasher.digest() & 0xffff)
}

pub(super) fn render_page(bytes: &[u8], offset: usize) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut output = String::new();
    let mut lines = scan(&text);
    if offset > 1 && text.starts_with('\u{feff}') {
        if let Some(line) = lines.first_mut() {
            line.start = 0;
        } else {
            lines.push(Line {
                start: 0,
                body_end: text.len(),
                end: text.len(),
            });
        }
    }
    for (i, line) in lines.iter().enumerate() {
        writeln!(
            output,
            "{}:{}",
            offset + i,
            &text[line.start..line.body_end]
        )
        .expect("写入字符串不会失败");
    }
    output
}

struct Entry {
    raw: Arc<str>,
    tag: String,
    seen: Vec<Range<usize>>,
    collisions: HashSet<String>,
    versions: Vec<(String, Arc<str>)>,
    active: bool,
    touched: u64,
}

pub(super) struct Store {
    entries: HashMap<PathBuf, Entry>,
    clock: u64,
    max_paths: usize,
    max_bytes: usize,
}
impl Default for Store {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            clock: 0,
            max_paths: MAX_PATHS,
            max_bytes: MAX_TOTAL_BYTES,
        }
    }
}
impl Store {
    pub(super) fn clear(&mut self) {
        self.entries.clear();
    }
    pub(super) fn invalidate(&mut self, path: &Path) {
        if let Some(entry) = self.entries.get_mut(path) {
            entry.active = false;
            entry.seen.clear();
        }
    }
    pub(super) fn record(
        &mut self,
        path: PathBuf,
        raw: Arc<str>,
        ranges: Vec<Range<usize>>,
    ) -> bool {
        if raw.len() > MAX_FILE_BYTES {
            self.invalidate(&path);
            return false;
        }
        self.clock += 1;
        let tag = file_hash(&raw);
        let previous = self.entries.remove(&path);
        let mut collisions = previous
            .as_ref()
            .map(|e| e.collisions.clone())
            .unwrap_or_default();
        let mut versions = previous
            .as_ref()
            .map(|e| e.versions.clone())
            .unwrap_or_default();
        if versions.iter().any(|(old_tag, old)| {
            old_tag == &tag && old != &raw && hash_input(old) != hash_input(&raw)
        }) {
            collisions.insert(tag.clone());
        }
        versions.retain(|(_, old)| old != &raw);
        versions.insert(0, (tag.clone(), raw.clone()));
        versions.truncate(4);
        let mut seen = previous
            .filter(|e| e.active && e.raw == raw)
            .map(|e| e.seen)
            .unwrap_or_default();
        seen.extend(ranges);
        seen.sort_unstable_by_key(|r| r.start);
        let mut merged: Vec<Range<usize>> = Vec::new();
        for range in seen {
            if let Some(last) = merged.last_mut()
                && range.start <= last.end
            {
                last.end = last.end.max(range.end);
            } else {
                merged.push(range);
            }
        }
        let active = !collisions.contains(&tag);
        self.entries.insert(
            path.clone(),
            Entry {
                raw,
                tag,
                seen: merged,
                collisions,
                versions,
                active,
                touched: self.clock,
            },
        );
        while self.entries.len() > self.max_paths
            || self
                .entries
                .values()
                .flat_map(|e| &e.versions)
                .map(|(_, raw)| raw.len())
                .sum::<usize>()
                > self.max_bytes
        {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.touched)
                .map(|(p, _)| p.clone())
            else {
                break;
            };
            self.entries.remove(&oldest);
        }
        active && self.entries.contains_key(&path)
    }
    pub(super) fn validate(
        &mut self,
        path: &Path,
        tag: &str,
        current: &str,
        edits: &[Edit],
    ) -> Result<()> {
        let entry = self
            .entries
            .get_mut(path)
            .context("hashline snapshot unavailable; read the local file again")?;
        ensure!(
            entry.active && !entry.collisions.contains(tag),
            "hashline snapshot unavailable or ambiguous; read the local file again"
        );
        ensure!(
            entry.tag == tag && entry.raw.as_ref() == current,
            "hashline snapshot changed; read the file again"
        );
        for edit in edits {
            let range = edit.range();
            ensure!(
                entry
                    .seen
                    .iter()
                    .any(|seen| seen.start <= range.start && seen.end >= range.end),
                "hashline targets unread lines; read the target range again"
            );
        }
        self.clock += 1;
        entry.touched = self.clock;
        Ok(())
    }
}

#[derive(Debug)]
pub(super) enum Edit {
    Replace {
        start: usize,
        end: usize,
        lines: Vec<String>,
    },
    Before {
        anchor: usize,
        lines: Vec<String>,
    },
    After {
        anchor: usize,
        lines: Vec<String>,
    },
}
impl Edit {
    fn range(&self) -> Range<usize> {
        match self {
            Self::Replace { start, end, .. } => *start..end + 1,
            Self::Before { anchor, .. } | Self::After { anchor, .. } => *anchor..anchor + 1,
        }
    }
}
pub(super) struct Patch {
    pub path: String,
    pub tag: String,
    pub edits: Vec<Edit>,
}

fn number(text: &str) -> Result<usize> {
    ensure!(
        !text.is_empty() && !text.starts_with('0') && text.bytes().all(|b| b.is_ascii_digit()),
        "invalid hashline line number"
    );
    text.parse::<usize>()
        .ok()
        .filter(|n| *n < usize::MAX)
        .context("invalid hashline line number")
}
fn span(text: &str) -> Result<(usize, usize)> {
    let (a, b) = text
        .split_once(".=")
        .context("hashline range requires N.=M")?;
    let (a, b) = (number(a)?, number(b)?);
    ensure!(a <= b, "hashline range is reversed");
    Ok((a, b))
}
pub(super) fn parse(input: &str) -> Result<Patch> {
    let input = input.trim_matches('\n');
    let input = if let Some(inner) = input.strip_prefix("*** Begin Patch\n") {
        inner
            .strip_suffix("\n*** End Patch")
            .context("incomplete hashline patch wrapper")?
    } else {
        input
    };
    let mut rows = input.split('\n').peekable();
    let header = rows.next().context("hashline requires [path#TAG]")?;
    let header = header
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .context("hashline requires [path#TAG]")?;
    let (path, tag) = header
        .rsplit_once('#')
        .context("hashline requires [path#TAG]")?;
    ensure!(
        !path.trim().is_empty()
            && !path.contains(['\r', '\n', '[', ']'])
            && tag.len() == 4
            && tag
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'A'..=b'F').contains(&b)),
        "invalid hashline file header"
    );
    let mut edits = Vec::new();
    while let Some(row) = rows.next() {
        if row.is_empty() {
            continue;
        }
        if let Some(range) = row.strip_prefix("CUT ") {
            let (start, end) = span(range)?;
            edits.push(Edit::Replace {
                start,
                end,
                lines: Vec::new(),
            });
            continue;
        }
        let locator = row
            .strip_prefix("PUT ")
            .and_then(|s| s.strip_suffix(':'))
            .context("unsupported hashline operation; use PUT or CUT in one file section")?;
        let mut lines = Vec::new();
        while rows.peek().is_some_and(|r| r.starts_with('+')) {
            let text = rows
                .next()
                .expect("已经检查存在")
                .strip_prefix('+')
                .expect("已经检查前缀");
            ensure!(
                !text.contains('\r'),
                "hashline body lines must not contain CR"
            );
            lines.push(text.to_owned());
        }
        ensure!(
            !lines.is_empty(),
            "hashline PUT requires +body lines; use CUT to delete"
        );
        let edit = if let Some(n) = locator.strip_prefix('<') {
            Edit::Before {
                anchor: number(n)?,
                lines,
            }
        } else if let Some(n) = locator.strip_prefix('>') {
            Edit::After {
                anchor: number(n)?,
                lines,
            }
        } else {
            let (start, end) = span(locator)?;
            Edit::Replace { start, end, lines }
        };
        edits.push(edit);
    }
    ensure!(
        !edits.is_empty(),
        "hashline requires at least one operation"
    );
    Ok(Patch {
        path: path.to_owned(),
        tag: tag.to_owned(),
        edits,
    })
}

pub(super) fn apply(source: &str, edits: &[Edit]) -> Result<String> {
    let lines = scan(source);
    let separator = lines
        .iter()
        .find(|l| l.end > l.body_end)
        .map_or("\n", |l| &source[l.body_end..l.end]);
    let mut changes = Vec::with_capacity(edits.len());
    for (op, edit) in edits.iter().enumerate() {
        let range = edit.range();
        ensure!(
            range.start > 0 && range.end - 1 <= lines.len(),
            "hashline line out of range in operation {}",
            op + 1
        );
        let (start, end, new, leading, trailing) = match edit {
            Edit::Replace {
                start,
                end,
                lines: new,
            } => (
                lines[start - 1].start,
                lines[end - 1].end,
                new,
                false,
                lines[end - 1].end > lines[end - 1].body_end,
            ),
            Edit::Before { anchor, lines: new } => (
                lines[anchor - 1].start,
                lines[anchor - 1].start,
                new,
                false,
                true,
            ),
            Edit::After { anchor, lines: new } => {
                let line = &lines[anchor - 1];
                let unterminated = line.end == line.body_end;
                (line.end, line.end, new, unterminated, !unterminated)
            }
        };
        changes.push((start, end, new, leading, trailing, op));
    }
    changes.sort_by_key(|c| (c.0, c.1));
    for pair in changes.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        let conflict = if a.0 == a.1 || b.0 == b.1 {
            b.0 <= a.1
        } else {
            b.0 < a.1
        };
        ensure!(
            !conflict,
            "conflicting hashline edits in operations {} and {}",
            a.5 + 1,
            b.5 + 1
        );
    }
    let mut output = String::with_capacity(source.len());
    let mut cursor = 0;
    for (start, end, new, leading, trailing, _) in changes {
        output.push_str(&source[cursor..start]);
        if leading {
            output.push_str(separator);
        }
        for (i, line) in new.iter().enumerate() {
            output.push_str(line);
            if i + 1 < new.len() || trailing {
                output.push_str(separator);
            }
        }
        cursor = end;
    }
    output.push_str(&source[cursor..]);
    ensure!(output != source, "edit produced no changes");
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashline_hash_matches_upstream_vectors() {
        // 上游 pi-edit/src/store.rs 的固定向量，同时覆盖 CRLF 和行尾空白。
        assert_eq!(file_hash("a \n b\t\r\nc"), "80BA");
        assert_eq!(file_hash("hello\n"), "5BF9");
        assert_eq!(file_hash(""), "5D05");
        assert_eq!(file_hash("\u{feff}hello\r\n"), "5BF9");
        assert_eq!(file_hash("hello\r"), "5BF9");
        assert_ne!(file_hash(" hello\n"), file_hash("hello\n"));
    }

    fn apply_patch(source: &str, operations: &str) -> Result<String> {
        let patch = parse(&format!("[file#A1B2]\n{operations}"))?;
        apply(source, &patch.edits)
    }

    #[test]
    fn hashline_snapshot_order_endings_bom_and_literal_payload() {
        assert_eq!(
            apply_patch(
                "one\ntwo\nthree\n",
                "PUT >3:\n+four\nPUT 2.=2:\n+TWO\nPUT <1:\n+zero"
            )
            .unwrap(),
            "zero\none\nTWO\nthree\nfour\n"
        );
        assert_eq!(
            apply_patch("\u{feff}one\r\ntwo\nthree", "PUT 2.=2:\n+TWO").unwrap(),
            "\u{feff}one\r\nTWO\r\nthree"
        );
        assert_eq!(apply_patch("a\rb\r", "PUT 2.=2:\n+B").unwrap(), "a\rB\r");
        assert_eq!(apply_patch("one", "PUT >1:\n+two").unwrap(), "one\ntwo");
        assert_eq!(apply_patch("one\ntwo", "CUT 1.=1").unwrap(), "two");
        assert_eq!(
            apply_patch("one\n", "PUT 1.=1:\n++literal\n+-literal\n+\n+  ").unwrap(),
            "+literal\n-literal\n\n  \n"
        );
        assert_eq!(
            apply_patch("a\nb\nc", "PUT 2.=2:\n+b\nPUT 3.=3:\n+C").unwrap(),
            "a\nb\nC"
        );
        assert_eq!(apply_patch("one", "CUT 1.=1").unwrap(), "");
        assert!(apply_patch("", "CUT 1.=1").is_err());
    }

    #[test]
    fn hashline_store_seen_ranges_lru_and_limits() {
        let mut store = Store {
            max_paths: 2,
            max_bytes: 20,
            ..Default::default()
        };
        let raw: Arc<str> = Arc::from("a\nb\nc\n");
        let patch = parse(&format!("[a#{}]\nCUT 1.=3", file_hash(&raw))).unwrap();
        assert!(store.record("a".into(), raw.clone(), vec![1..2, 3..4]));
        assert!(
            store
                .validate(Path::new("a"), &patch.tag, &raw, &patch.edits)
                .is_err()
        );
        store.record("a".into(), raw.clone(), std::iter::once(2..3).collect());
        store
            .validate(Path::new("a"), &patch.tag, &raw, &patch.edits)
            .unwrap();
        store.record("b".into(), raw.clone(), std::iter::once(1..4).collect());
        store
            .validate(Path::new("a"), &patch.tag, &raw, &patch.edits)
            .unwrap();
        store.record("c".into(), raw.clone(), std::iter::once(1..4).collect());
        assert!(store.entries.contains_key(Path::new("a")));
        assert!(!store.entries.contains_key(Path::new("b")));
        store.max_bytes = 5;
        assert!(!store.record("d".into(), raw.clone(), std::iter::once(1..4).collect()));
        assert!(store.entries.is_empty());
        assert!(!store.record(
            "large".into(),
            Arc::from("x".repeat(MAX_FILE_BYTES + 1)),
            std::iter::once(1..2).collect()
        ));
    }

    #[test]
    fn hashline_store_rejects_known_short_tag_collision() {
        let mut observed = HashMap::<String, String>::new();
        let (first, second, tag) = (0..=65536)
            .find_map(|n| {
                let text = format!("line {n}\n");
                let tag = file_hash(&text);
                observed
                    .insert(tag.clone(), text.clone())
                    .map(|first| (first, text, tag))
            })
            .expect("65537 个不同文本必定碰撞四位标签");
        let mut store = Store::default();
        store.record(
            "file".into(),
            Arc::from(first),
            std::iter::once(1..2).collect(),
        );
        store.record(
            "file".into(),
            Arc::from("intermediate version\n"),
            std::iter::once(1..2).collect(),
        );
        assert!(!store.record(
            "file".into(),
            Arc::from(second.as_str()),
            std::iter::once(1..2).collect()
        ));
        let patch = parse(&format!("[file#{tag}]\nCUT 1.=1")).unwrap();
        assert!(
            store
                .validate(Path::new("file"), &tag, &second, &patch.edits)
                .is_err()
        );
    }
}
