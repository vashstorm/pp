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
        if entry.depth() == 0
            || !entry
                .file_type()
                .is_some_and(|kind| kind.is_file() || kind.is_dir())
        {
            continue;
        }
        let relative = entry.path().strip_prefix(cwd)?;
        if let Some(path) = relative.to_str()
            && !path.chars().any(char::is_control)
        {
            paths.push(if entry.file_type().is_some_and(|kind| kind.is_dir()) {
                format!("{path}/")
            } else {
                path.to_owned()
            });
        }
    }
    paths.sort_unstable();
    paths.dedup();
    Ok(paths)
}

pub(super) fn scan_root(prefix: &str) -> String {
    if prefix.starts_with('/')
        || prefix.starts_with("~/")
        || prefix.starts_with("../")
        || prefix.starts_with("./")
    {
        prefix
            .rfind('/')
            .map_or_else(String::new, |end| prefix[..=end].to_owned())
    } else {
        String::new()
    }
}

pub(super) fn parent_directory(directory: &str) -> String {
    if directory == "/" {
        return "/".into();
    }
    if directory == "~/" {
        return std::env::var_os("HOME")
            .and_then(|home| {
                std::path::PathBuf::from(home)
                    .parent()
                    .map(|parent| format!("{}/", parent.display()))
            })
            .unwrap_or_default();
    }
    if directory == "./" {
        return "../".into();
    }
    if directory.trim_end_matches('/').ends_with("..") {
        return format!("{directory}../");
    }
    let end = directory
        .trim_end_matches('/')
        .rfind('/')
        .map_or(0, |end| end + 1);
    directory[..end].to_owned()
}

fn scan_explicit(cwd: &Path, root: &str) -> Result<Vec<String>> {
    let directory = pp_runtime::user_input::resolve(root, cwd)?;
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(&directory)
        .with_context(|| format!("无法列出 {}", directory.display()))?
    {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name
            .to_str()
            .filter(|s| !s.chars().any(char::is_control) && *s != ".git")
        else {
            continue;
        };
        let metadata = entry.metadata()?;
        if metadata.is_file() || metadata.is_dir() {
            paths.push(format!(
                "{root}{name}{}",
                if metadata.is_dir() { "/" } else { "" }
            ));
        }
    }
    paths.sort_unstable();
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
        if path == directory {
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
        let mut quoted_closed = false;
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
            quoted_closed = closed;
            if closed && characters.peek().is_some_and(|(_, c)| !c.is_whitespace()) {
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
        if first != '@' || doubled || cursor <= start || (quoted_closed && cursor >= end) {
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
        // 用同一解析器判断代码上下文；补上临时路径后可覆盖尚未输入完整的 @。
        let source = input.lines().join("\n");
        let row_offset = input.lines()[..row]
            .iter()
            .map(|line| line.len() + 1)
            .sum::<usize>();
        let byte_start = row_offset + line.chars().take(start).map(char::len_utf8).sum::<usize>();
        let byte_end = row_offset + line.chars().take(end).map(char::len_utf8).sum::<usize>();
        let mut probe = source;
        probe.replace_range(byte_start..byte_end, "@__pp_completion__");
        if !pp_runtime::user_input::references(&probe)
            .iter()
            .any(|reference| reference.start == byte_start && reference.path.is_some())
        {
            return None;
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
    replace_reference(input, query, path, false);
}

pub(super) fn insert_prefix(input: &mut ratatui_textarea::TextArea<'_>, query: &Query, path: &str) {
    replace_reference(input, query, path, true);
}

fn replace_reference(
    input: &mut ratatui_textarea::TextArea<'_>,
    query: &Query,
    path: &str,
    partial: bool,
) {
    use ratatui_textarea::CursorMove;

    let line = &input.lines()[query.row];
    let quoted = (partial && line.chars().nth(query.start + 1) == Some('"'))
        || path
            .chars()
            .any(|c| c.is_whitespace() || c == '"' || c == '\\');
    let reference = if quoted {
        format!(
            "@{}",
            serde_json::to_string(path).expect("serializing a file path cannot fail")
        )
    } else {
        format!("@{path}")
    };
    let separated = line.chars().nth(query.end).is_some() || query.row + 1 < input.lines().len();
    input.cancel_selection();
    for _ in query.start..query.cursor {
        input.move_cursor(CursorMove::Back);
    }
    input.delete_str(query.end - query.start);
    input.insert_str(reference);
    if partial || path.is_empty() || path.ends_with(std::path::MAIN_SEPARATOR) {
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
    pub(super) requests: Sender<String>,
    pub(super) replies: Receiver<Result<Vec<String>>>,
}

impl Worker {
    pub(super) fn new(cwd: &Path) -> Result<Self> {
        let cwd = cwd.to_owned();
        let (requests, receiver) = mpsc::channel::<String>();
        let (sender, replies) = mpsc::channel();
        thread::Builder::new()
            .name("pp-files".into())
            .spawn(move || {
                while let Ok(root) = receiver.recv() {
                    let result = if root.is_empty() {
                        scan(&cwd)
                    } else {
                        scan_explicit(&cwd, &root)
                    };
                    if sender.send(result).is_err() {
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
    fn file_picker_entries_and_query_boundaries() {
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
        assert_eq!(visible("docs/", "说明"), [("docs/my folder/", true)]);
        assert!(visible("src/", "missing").is_empty() && visible("missing/", "").is_empty());

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
    fn file_picker_insert_quoting_separators_wide_and_undo() {
        let mut ctx_in = draft(&["第一行", "分析🙂 @src/old.rs 后续", "尾部"]);
        move_to(&mut ctx_in, 1, 11);
        let active = query(&ctx_in).expect("active reference");
        assert_eq!(
            (active.start, active.end, active.prefix.as_str()),
            (4, 15, "src/ol")
        );
        insert(&mut ctx_in, &active, "src/main.rs");
        assert_eq!(
            ctx_in.lines(),
            ["第一行", "分析🙂 @src/main.rs 后续", "尾部"]
        );
        assert_eq!(ctx_in.cursor(), (1, 17));

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
            assert_eq!(input.lines()[1], format!("分析🙂 {expected} 后续 @other"));
            let editable = query(&input).expect("directory remains editable");
            assert_eq!(editable.prefix, path);
            insert(&mut input, &editable, "src/main.rs");
            assert_eq!(input.cursor(), (1, 17));
        }

        for (path, expected) in [
            ("src/main.rs", "@src/main.rs"),
            ("docs/my file.md", "@\"docs/my file.md\""),
            ("say \"hi\".txt", "@\"say \\\"hi\\\".txt\""),
            ("back\\slash.md", "@\"back\\\\slash.md\""),
        ] {
            let mut input = draft(&["@old"]);
            move_to(&mut input, 0, 4);
            let active = query(&input).unwrap();
            insert(&mut input, &active, path);
            assert_eq!(input.lines(), [format!("{expected} ")], "{path}");
        }

        for (lines, target_line, cursor) in [
            (&["@old bar"][..], "@x bar", (0, 3)),
            (&["@old\tbar"][..], "@x\tbar", (0, 3)),
            (&["@old"][..], "@x ", (0, 3)),
        ] {
            let mut input = draft(lines);
            move_to(&mut input, 0, 4);
            let active = query(&input).unwrap();
            insert(&mut input, &active, "x");
            assert_eq!(input.lines(), [target_line]);
            assert_eq!(input.cursor(), cursor);
        }
        let mut nl_in = draft(&["@old", "bar"]);
        move_to(&mut nl_in, 0, 4);
        let active = query(&nl_in).unwrap();
        insert(&mut nl_in, &active, "x");
        assert_eq!(nl_in.lines(), ["@x", "bar"]);
        assert_eq!(nl_in.cursor(), (1_usize, 0_usize));

        let prefix = "x".repeat(70_000);
        let mut wide = TextArea::default();
        wide.insert_str(&prefix);
        wide.insert_str(" @old.rs");
        let active = query(&wide).unwrap();
        insert(&mut wide, &active, "src/main.rs");
        assert_eq!(wide.lines()[0].chars().count(), 70_014);
        assert_eq!(wide.cursor(), (0_usize, 70_014_usize));

        let mut undo_in = draft(&["分析 @old.rs"]);
        move_to(&mut undo_in, 0, 10);
        let active = query(&undo_in).unwrap();
        insert(&mut undo_in, &active, "src/main.rs");
        assert!(undo_in.undo() && undo_in.undo() && undo_in.undo());
        assert_eq!(undo_in.lines(), ["分析 @old.rs"]);
    }

    #[test]
    fn file_picker_scan_ignores_errors_and_worker() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path();
        for d in ["src", "ignored", ".git/info", "local", "excluded"] {
            fs::create_dir_all(root.join(d))?;
        }
        for f in [
            "src/main.rs",
            ".hidden",
            "ignored/no",
            ".git/secret",
            "local/no",
            "excluded/no",
        ] {
            fs::write(root.join(f), "not read")?;
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
        }
        assert_eq!(
            scan(root)?,
            [".gitignore", ".hidden", ".ignore", "src/", "src/main.rs"]
        );
        fs::write(root.join(".ignore"), "[z-a]\n")?;
        assert!(format!("{:#}", scan(root).unwrap_err()).contains(".ignore"));

        let d2 = tempfile::tempdir()?;
        assert!(scan(d2.path())?.is_empty());
        assert!(scan(&d2.path().join("missing")).is_err());
        let worker = Worker::new(d2.path())?;
        worker.requests.send(String::new())?;
        assert!(worker.replies.recv()??.is_empty());
        fs::write(d2.path().join("new"), "")?;
        worker.requests.send(String::new())?;
        assert_eq!(worker.replies.recv()??, ["new"]);
        Ok(())
    }
}

#[cfg(test)]
mod explicit_tests {
    use super::*;
    #[test]
    fn file_picker_explicit_scan_and_parent_navigation() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("empty")).unwrap();
        std::fs::write(dir.path().join("my file.txt"), "body").unwrap();
        let root = format!("{}/", dir.path().display());
        let paths = scan_explicit(Path::new("."), &root).unwrap();
        assert!(paths.contains(&format!("{root}empty/")));
        assert!(paths.contains(&format!("{root}my file.txt")));
        assert_eq!(parent_directory("../"), "../../");
        assert_eq!(parent_directory("../../"), "../../../");
        assert_eq!(parent_directory("/"), "/");
        assert_eq!(scan_root("../file"), "../");
    }
    #[test]
    fn file_picker_code_context_and_unclosed_quoted_path() {
        for lines in [
            vec!["```rust", "@path"],
            vec!["~~~", "@path"],
            vec!["`@path"],
        ] {
            let mut input = ratatui_textarea::TextArea::from(lines);
            input.move_cursor(ratatui_textarea::CursorMove::Bottom);
            input.move_cursor(ratatui_textarea::CursorMove::End);
            assert!(query(&input).is_none());
        }
        let mut input = ratatui_textarea::TextArea::from(["@\"my path"]);
        input.move_cursor(ratatui_textarea::CursorMove::End);
        assert_eq!(query(&input).unwrap().prefix, "my path");
    }
}
