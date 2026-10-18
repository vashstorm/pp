use anyhow::{Context, Result};
use std::{
    path::Path,
    sync::mpsc::{self, Receiver, Sender},
    thread,
};

pub(super) fn scan(cwd: &Path) -> Result<Vec<String>> {
    let mut paths = Vec::new();
    for entry in ignore::WalkBuilder::new(cwd)
        .hidden(false)
        .parents(false)
        .git_global(false)
        .require_git(false)
        .follow_links(false)
        .filter_entry(|entry| entry.depth() == 0 || entry.file_name() != ".git")
        .build()
    {
        let entry = entry.with_context(|| format!("walking project files in {}", cwd.display()))?;
        if let Some(error) = entry.error() {
            return Err(anyhow::Error::new(error.clone()).context(format!(
                "reading ignore rules in {}",
                entry.path().display()
            )));
        }
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let relative = entry.path().strip_prefix(cwd)?;
        if let Some(path) = relative.to_str()
            && !path.chars().any(char::is_control)
        {
            paths.push(path.to_owned());
        }
    }
    paths.sort_unstable();
    paths.dedup();
    Ok(paths)
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Entry<'a> {
    pub(super) index: usize,
    pub(super) path: &'a str,
    pub(super) directory: bool,
}

// Paths come from scan in lexical order. A directory is represented by the
// first matching file below it; ignored files never create directory entries.
pub(super) fn entries<'a>(paths: &'a [String], directory: &str, prefix: &str) -> Vec<Entry<'a>> {
    let mut entries: Vec<Entry<'a>> = Vec::new();
    for (index, path) in paths.iter().enumerate() {
        if !path.contains(prefix) {
            continue;
        }
        let Some((path, directory)) = child(path, directory) else {
            continue;
        };
        if entries.last().is_some_and(|entry| entry.path == path) {
            continue;
        }
        entries.push(Entry {
            index,
            path,
            directory,
        });
    }
    entries.sort_unstable_by(|a, b| b.directory.cmp(&a.directory).then(a.path.cmp(b.path)));
    entries
}

pub(super) fn child<'a>(path: &'a str, directory: &str) -> Option<(&'a str, bool)> {
    let relative = path.strip_prefix(directory)?;
    Some(match relative.find(std::path::MAIN_SEPARATOR) {
        Some(end) => (&path[..directory.len() + end + 1], true),
        None => (path, false),
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Query {
    pub(super) row: usize,
    pub(super) start: usize,
    pub(super) end: usize,
    pub(super) cursor: usize,
    pub(super) prefix: String,
}

pub(super) fn query(input: &ratatui_textarea::TextArea<'_>) -> Option<Query> {
    let ratatui_textarea::DataCursor(row, cursor) = input.cursor();
    let line = &input.lines()[row];
    let mut characters = line.chars().enumerate().peekable();
    while let Some((start, first)) = characters.next() {
        if first.is_whitespace() {
            continue;
        }
        let quoted = first == '@' && characters.peek().is_some_and(|(_, c)| *c == '"');
        let doubled = first == '@' && characters.peek().is_some_and(|(_, c)| *c == '@');
        let mut end = start + 1;
        if quoted {
            characters.next(); // opening quote
            let mut escaped = false;
            let mut closed = false;
            for (column, character) in characters.by_ref() {
                end = column + 1;
                if character == '"' && !escaped {
                    closed = true;
                    break;
                }
                escaped = character == '\\' && !escaped;
            }
            if !closed || characters.peek().is_some_and(|(_, c)| !c.is_whitespace()) {
                return None;
            }
        } else {
            while let Some(&(column, character)) = characters.peek() {
                if character.is_whitespace() {
                    break;
                }
                end = column + 1;
                characters.next();
            }
        }
        if cursor > end {
            continue;
        }
        if first != '@' || doubled || cursor <= start || (quoted && cursor >= end) {
            return None;
        }
        let mut prefix: String = line
            .chars()
            .skip(start + 1)
            .take(cursor - start - 1)
            .collect();
        if quoted {
            prefix.push('"');
            prefix = serde_json::from_str(&prefix).ok()?;
        }
        return Some(Query {
            row,
            start,
            end,
            cursor,
            prefix,
        });
    }
    None
}

pub(super) fn insert(input: &mut ratatui_textarea::TextArea<'_>, query: &Query, path: &str) {
    use ratatui_textarea::CursorMove;

    let reference = if path
        .chars()
        .any(|c| c.is_whitespace() || c == '"' || c == '\\')
    {
        format!(
            "@{}",
            serde_json::to_string(path).expect("serializing a file path cannot fail")
        )
    } else {
        format!("@{path}")
    };
    let separated = input
        .lines()
        .get(query.row)
        .is_some_and(|line| line.chars().nth(query.end).is_some())
        || query.row + 1 < input.lines().len();
    input.cancel_selection();
    for _ in query.start..query.cursor {
        input.move_cursor(CursorMove::Back);
    }
    input.delete_str(query.end - query.start);
    let quoted = reference.starts_with("@\"");
    input.insert_str(reference);
    if path.is_empty() || path.ends_with(std::path::MAIN_SEPARATOR) {
        if quoted {
            input.move_cursor(CursorMove::Back);
        }
    } else if separated {
        input.move_cursor(CursorMove::Forward);
    } else {
        input.insert_str(" ");
    }
}

pub(super) struct Worker {
    pub(super) requests: Sender<()>,
    pub(super) replies: Receiver<Result<Vec<String>>>,
}

impl Worker {
    pub(super) fn new(cwd: &Path) -> Result<Self> {
        let cwd = cwd.to_owned();
        let (requests, receiver) = mpsc::channel();
        let (sender, replies) = mpsc::channel();
        thread::Builder::new()
            .name("pp-files".into())
            .spawn(move || {
                while receiver.recv().is_ok() {
                    if sender.send(scan(&cwd)).is_err() {
                        break;
                    }
                }
            })
            .context("starting file picker worker")?;
        Ok(Self { requests, replies })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui_textarea::TextArea;
    use std::fs;

    fn draft(lines: &[&str]) -> TextArea<'static> {
        TextArea::from(lines.iter().copied())
    }

    fn move_to(input: &mut TextArea<'_>, row: usize, col: usize) {
        use ratatui_textarea::CursorMove;
        input.move_cursor(CursorMove::Top);
        input.move_cursor(CursorMove::Head);
        for _ in 0..row {
            input.move_cursor(CursorMove::Down);
        }
        for _ in 0..col {
            input.move_cursor(CursorMove::Forward);
        }
    }

    #[test]
    fn file_picker_entries_show_only_direct_children() {
        let paths: Vec<String> = [
            "README.md",
            "docs/my folder/说明.md",
            "src-extra/no.rs",
            "src/main.rs",
            "src/tui/files.rs",
            "src/tui/menu.rs",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        let visible = |directory, prefix| {
            entries(&paths, directory, prefix)
                .into_iter()
                .map(|entry| (entry.path, entry.directory))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            visible("", ""),
            [
                ("docs/", true),
                ("src-extra/", true),
                ("src/", true),
                ("README.md", false)
            ]
        );
        assert_eq!(
            visible("src/", ""),
            [("src/tui/", true), ("src/main.rs", false)]
        );
        assert_eq!(
            visible("src/tui/", ""),
            [("src/tui/files.rs", false), ("src/tui/menu.rs", false)]
        );
        assert_eq!(visible("", "menu"), [("src/", true)]);
        assert_eq!(visible("src/", "menu"), [("src/tui/", true)]);
        assert_eq!(visible("src/tui/", "menu"), [("src/tui/menu.rs", false)]);
        assert_eq!(visible("docs/", "说明"), [("docs/my folder/", true)]);
        assert!(visible("src/", "missing").is_empty());
        assert!(visible("missing/", "").is_empty());
    }

    #[test]
    fn file_picker_query_character_boundaries() {
        use ratatui_textarea::{CursorMove, TextArea};
        for (text, back, expected) in [
            ("@", 0, Some((0, 0, 1, ""))),
            ("分析 @src/ma 后续", 3, Some((0, 3, 10, "src/ma"))),
            ("@one @two", 0, Some((0, 5, 9, "two"))),
            ("first\n中文🙂 @目录/文", 0, Some((1, 4, 9, "目录/文"))),
            ("name@example.com", 0, None),
            ("@@foo", 0, None),
            ("@\"quoted\"", 0, None),
            ("before @foo after", 0, None),
        ] {
            let mut input = TextArea::default();
            input.insert_str(text);
            for _ in 0..back {
                input.move_cursor(CursorMove::Back);
            }
            let actual = query(&input);
            assert_eq!(
                actual
                    .as_ref()
                    .map(|q| (q.row, q.start, q.end, q.prefix.as_str())),
                expected,
                "{text}"
            );
        }
    }

    #[test]
    fn file_picker_insert_replaces_whole_token_and_keeps_context() {
        let mut input = draft(&["第一行", "分析🙂 @src/old.rs 后续", "尾部"]);
        move_to(&mut input, 1, 11); // cursor in the middle of "old"
        let active = query(&input).expect("active reference");
        assert_eq!(
            (active.start, active.end, active.prefix.as_str()),
            (4, 15, "src/ol")
        );
        insert(&mut input, &active, "src/main.rs");
        assert_eq!(
            input.lines(),
            ["第一行", "分析🙂 @src/main.rs 后续", "尾部"]
        );
        assert_eq!(input.cursor(), (1, 17));
    }

    #[test]
    fn file_picker_directory_completion_keeps_context_and_quoted_paths_editable() {
        for (path, expected) in [
            ("src/", "@src/"),
            ("目录/my folder/", "@\"目录/my folder/\""),
            ("say \"hi\"/", "@\"say \\\"hi\\\"/\""),
            ("back\\slash/", "@\"back\\\\slash/\""),
            ("", "@"),
        ] {
            let mut input = draft(&["第一行", "分析🙂 @old 后续 @other", "尾部"]);
            move_to(&mut input, 1, 7);
            let active = query(&input).unwrap();
            insert(&mut input, &active, path);
            assert_eq!(
                input.lines(),
                [
                    "第一行".to_owned(),
                    format!("分析🙂 {expected} 后续 @other"),
                    "尾部".to_owned()
                ]
            );
            let active = query(&input).expect("directory remains editable");
            assert_eq!(active.prefix, path);
            assert_eq!((active.row, active.start), (1, 4));
            insert(&mut input, &active, "src/main.rs");
            assert_eq!(
                input.lines(),
                ["第一行", "分析🙂 @src/main.rs 后续 @other", "尾部"]
            );
            assert_eq!(input.cursor(), (1, 17));
        }
    }

    #[test]
    fn file_picker_insert_quotes_paths_needing_json() {
        for (path, expected) in [
            ("src/main.rs", "@src/main.rs"),
            ("docs/my file.md", "@\"docs/my file.md\""),
            ("say \"hi\".txt", "@\"say \\\"hi\\\".txt\""),
            ("back\\slash.md", "@\"back\\\\slash.md\""),
        ] {
            let mut input = draft(&["@old"]);
            move_to(&mut input, 0, 4);
            let active = query(&input).expect("active reference");
            insert(&mut input, &active, path);
            assert_eq!(input.lines(), [format!("{expected} ")], "{path}");
            assert_eq!(input.cursor(), (0, expected.chars().count() + 1), "{path}");
        }
    }

    #[test]
    fn file_picker_insert_separator_cursor() {
        // Existing separator: cross it without adding whitespace.
        let mut input = draft(&["@old bar"]);
        move_to(&mut input, 0, 4);
        let active = query(&input).unwrap();
        insert(&mut input, &active, "x");
        assert_eq!(input.lines(), ["@x bar"]);
        assert_eq!(input.cursor(), (0, 3));

        // A tab separator behaves the same.
        let mut input = draft(&["@old\tbar"]);
        move_to(&mut input, 0, 4);
        let active = query(&input).unwrap();
        insert(&mut input, &active, "x");
        assert_eq!(input.lines(), ["@x\tbar"]);
        assert_eq!(input.cursor(), (0, 3));

        // A line break is an existing separator; no space is added.
        let mut input = draft(&["@old", "bar"]);
        move_to(&mut input, 0, 4);
        let active = query(&input).unwrap();
        insert(&mut input, &active, "x");
        assert_eq!(input.lines(), ["@x", "bar"]);
        assert_eq!(input.cursor(), (1, 0));

        // No separator at all: append one space and stop after it.
        let mut input = draft(&["@old"]);
        move_to(&mut input, 0, 4);
        let active = query(&input).unwrap();
        insert(&mut input, &active, "x");
        assert_eq!(input.lines(), ["@x "]);
        assert_eq!(input.cursor(), (0, 3));
    }

    #[test]
    fn file_picker_insert_beyond_u16_columns() {
        let prefix = "x".repeat(70_000);
        let mut input = TextArea::default();
        input.insert_str(&prefix);
        input.insert_str(" @old.rs");
        let active = query(&input).expect("active reference");
        assert_eq!((active.start, active.end), (70_001, 70_008));
        insert(&mut input, &active, "src/main.rs");
        let line = &input.lines()[0];
        assert!(line.starts_with(&prefix));
        assert!(line.ends_with(" @src/main.rs "));
        assert_eq!(line.chars().count(), 70_014);
        assert_eq!(input.cursor(), (0, 70_014));
    }

    #[test]
    fn file_picker_insert_undo_restores_draft() {
        let mut input = draft(&["分析 @old.rs"]);
        move_to(&mut input, 0, 10);
        let active = query(&input).unwrap();
        insert(&mut input, &active, "src/main.rs");
        assert_eq!(input.lines(), ["分析 @src/main.rs "]);
        assert!(input.undo());
        assert_eq!(input.lines(), ["分析 @src/main.rs"]);
        assert!(input.undo());
        assert_eq!(input.lines(), ["分析 "]);
        assert!(input.undo());
        assert_eq!(input.lines(), ["分析 @old.rs"]);
    }

    #[test]
    fn file_picker_scan_obeys_project_ignores() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path();
        for directory in ["src", "ignored", ".git/info", "local", "excluded"] {
            fs::create_dir_all(root.join(directory))?;
        }
        for file in [
            "src/main.rs",
            ".hidden",
            "ignored/no",
            ".git/secret",
            "local/no",
            "excluded/no",
        ] {
            fs::write(root.join(file), "not read")?;
        }
        fs::write(root.join(".gitignore"), "ignored/\n")?;
        fs::write(root.join(".ignore"), "local/\n")?;
        fs::write(root.join(".git/info/exclude"), "excluded/\n")?;
        fs::write(root.join("bad\nname"), "")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            symlink(root, root.join("loop"))?;
            symlink(root.join("src/main.rs"), root.join("link"))?;
            #[cfg(target_os = "linux")]
            {
                use std::os::unix::ffi::OsStrExt;
                fs::write(root.join(std::ffi::OsStr::from_bytes(b"bad\xff")), "")?;
            }
        }
        assert_eq!(
            scan(root)?,
            [".gitignore", ".hidden", ".ignore", "src/main.rs"]
        );
        Ok(())
    }

    #[test]
    fn file_picker_scan_invalid_ignore_glob_fails() -> Result<()> {
        let dir = tempfile::tempdir()?;
        fs::create_dir(dir.path().join("src"))?;
        fs::write(dir.path().join("src/main.rs"), "")?;
        assert_eq!(scan(dir.path())?, ["src/main.rs"]);
        fs::write(dir.path().join(".ignore"), "[z-a]\n")?;
        let error = scan(dir.path()).unwrap_err();
        assert!(format!("{error:#}").contains(".ignore"), "{error:#}");
        Ok(())
    }

    #[test]
    fn file_picker_scan_empty_missing_and_worker_refresh() -> Result<()> {
        let dir = tempfile::tempdir()?;
        assert!(scan(dir.path())?.is_empty());
        assert!(scan(&dir.path().join("missing")).is_err());
        let worker = Worker::new(dir.path())?;
        worker.requests.send(())?;
        assert!(worker.replies.recv()??.is_empty());
        fs::write(dir.path().join("new"), "")?;
        worker.requests.send(())?;
        assert_eq!(worker.replies.recv()??, ["new"]);
        Ok(())
    }
}
