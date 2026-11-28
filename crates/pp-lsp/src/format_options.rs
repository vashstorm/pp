//! Per-file LSP `FormattingOptions` resolution.
//!
//! Mirrors oh-my-pi at the pinned commit
//! `1c291b87e4b889398cc4b8e4ccdfa650b56a0486`,
//! files `packages/coding-agent/src/lsp/format-options.ts` and
//! `packages/utils/src/tab-spacing.ts`. Priority, highest first:
//! 1. `.editorconfig` on the file's directory chain (`indent_style`, `indent_size`, `tab_width`);
//! 2. the indent style revealed by the first indented line of the content to write, and the GCD of all space-indent widths;
//! 3. fixed fallback: 2 spaces.
//!
//! `.editorconfig` discovery only reads the parent chain above the file's directory and grants no extra write permission; when
//! the configuration is missing, unreadable or invalid it is silently ignored and the fallback continues, never blocking the save. Results are cached by
//! the config path's mtime and length, and invalidate automatically once the file changes.

use globset::GlobMatcher;
use rustix::fs::{Mode, OFlags};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::SystemTime;

/// EditorConfig file name on the parent chain.
const EDITORCONFIG_NAME: &str = ".editorconfig";
/// Allowed `tabSize` range: both EditorConfig value parsing and the final result clamp to it.
const MIN_TAB_SIZE: u64 = 1;
const MAX_TAB_SIZE: u64 = 16;
/// Fallback when neither EditorConfig nor the content decides: 2 spaces.
const FALLBACK_TAB_SIZE: u64 = 2;
const FALLBACK_INSERT_SPACES: bool = true;
/// Upper bound for a single path component (the `NAME_MAX` of Linux ext4 / macOS APFS / NTFS).
/// Beyond it the file cannot be opened at all, so EditorConfig discovery is skipped outright.
const NAME_MAX_BYTES: usize = 255;
/// Parse cache cap; once reached the whole cache is cleared, so a long-running process cannot grow it unboundedly.
const MAX_CACHE_ENTRIES: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IndentStyle {
    Space,
    Tab,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IndentSize {
    Spaces(u64),
    Tab,
}

/// Indentation hints resolved from the `.editorconfig` chain; `None` means that chain does not pin this item.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct EditorConfigFormatting {
    tab_size: Option<u64>,
    insert_spaces: Option<bool>,
}

#[derive(Clone, Debug, Default)]
struct EditorConfigMatch {
    indent_style: Option<IndentStyle>,
    indent_size: Option<IndentSize>,
    tab_width: Option<u64>,
}

/// One `[pattern]` section.
///
/// Globs are compiled at parse time and reused with the cache; a pattern without a slash also generates a recursive candidate, so
/// `*.rs` matches files at any depth (EditorConfig semantics).
#[derive(Debug)]
struct Section {
    primary: Option<GlobMatcher>,
    recursive: Option<GlobMatcher>,
    properties: HashMap<String, String>,
}

impl Section {
    fn matches(&self, relative_path: &str) -> bool {
        if let Some(matcher) = &self.primary
            && matcher.is_match(relative_path)
        {
            return true;
        }
        if let Some(matcher) = &self.recursive
            && matcher.is_match(relative_path)
        {
            return true;
        }
        false
    }
}

#[derive(Debug)]
struct ParsedEditorConfig {
    root: bool,
    sections: Vec<Section>,
}

/// Parse cache entry for one configuration file: invalidated when mtime or length changes.
#[derive(Clone, Debug)]
struct CacheEntry {
    modified: Option<SystemTime>,
    len: u64,
    parsed: Arc<ParsedEditorConfig>,
}

static EDITORCONFIG_CACHE: LazyLock<Mutex<HashMap<PathBuf, CacheEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Resolve the LSP `FormattingOptions` for the file at `path`.
///
/// `path` need not exist; it only locates the containing directory, and a relative path is resolved against the current working directory.
pub(crate) fn resolve(path: &Path, content: &str) -> Value {
    let absolute = absolute_lexical(path);
    let from_config = resolve_editor_config_formatting(&absolute);
    let detected = detect_indent_from_content(content);

    let tab_size = from_config
        .tab_size
        .or(detected.tab_size)
        .unwrap_or(FALLBACK_TAB_SIZE)
        .clamp(MIN_TAB_SIZE, MAX_TAB_SIZE);
    let insert_spaces = from_config
        .insert_spaces
        .or(detected.insert_spaces)
        .unwrap_or(FALLBACK_INSERT_SPACES);

    json!({
        "tabSize": tab_size,
        "insertSpaces": insert_spaces,
        "trimTrailingWhitespace": true,
        "insertFinalNewline": true,
        "trimFinalNewlines": true,
    })
}

/// Infer indentation from the content to write; defaults are left unset so the caller can keep falling back.
fn detect_indent_from_content(content: &str) -> EditorConfigFormatting {
    if content.is_empty() {
        return EditorConfigFormatting::default();
    }

    let mut insert_spaces: Option<bool> = None;
    let mut unit = 0u64;

    for line in content.split('\n') {
        // Blank lines carry no indentation signal.
        if line.is_empty() || line.trim().is_empty() {
            continue;
        }

        let first = line.as_bytes()[0];
        if first != b' ' && first != b'\t' {
            continue;
        }

        // The first indented line decides spaces or tabs; later lines cannot override it.
        if insert_spaces.is_none() {
            insert_spaces = Some(first == b' ');
        }

        // For tab indentation the width is a display matter, left to EditorConfig or the fallback value.
        if first == b'\t' {
            continue;
        }

        let mut spaces = 0u64;
        for byte in line.bytes() {
            if byte == b' ' {
                spaces += 1;
            } else {
                break;
            }
        }
        if spaces == 0 {
            continue;
        }
        unit = if unit == 0 { spaces } else { gcd(unit, spaces) };
    }

    EditorConfigFormatting {
        // Only use the GCD as the step when the first indented line uses spaces; for tab files the
        // `tabSize` comes from EditorConfig or the fixed fallback.
        tab_size: (unit > 0 && insert_spaces == Some(true)).then_some(unit),
        insert_spaces,
    }
}

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        let remainder = a % b;
        a = b;
        b = remainder;
    }
    a
}

/// Resolve the file's own `.editorconfig` chain (far to near) into formatting hints.
fn resolve_editor_config_formatting(file: &Path) -> EditorConfigFormatting {
    if has_overlong_component(file) {
        return EditorConfigFormatting::default();
    }
    let Some(matched) = resolve_editor_config_match(file) else {
        return EditorConfigFormatting::default();
    };

    let tab_size = match matched.indent_size {
        Some(IndentSize::Spaces(n)) => Some(n.clamp(MIN_TAB_SIZE, MAX_TAB_SIZE)),
        _ => matched
            .tab_width
            .map(|n| n.clamp(MIN_TAB_SIZE, MAX_TAB_SIZE)),
    };

    let insert_spaces = match matched.indent_style {
        Some(IndentStyle::Space) => Some(true),
        Some(IndentStyle::Tab) => Some(false),
        None => match matched.indent_size {
            Some(IndentSize::Spaces(_)) => Some(true),
            Some(IndentSize::Tab) => Some(false),
            None => None,
        },
    };

    EditorConfigFormatting {
        tab_size,
        insert_spaces,
    }
}

/// Merge all matching sections along the parent chain; configuration closer to the file is applied later and thus overrides the outer ones.
fn resolve_editor_config_match(file: &Path) -> Option<EditorConfigMatch> {
    let dir = file.parent()?;
    let chain = collect_editor_config_chain(dir);
    if chain.is_empty() {
        return None;
    }

    let mut matched = EditorConfigMatch::default();
    // The chain was reversed to far → near while collecting.
    for (base, parsed) in chain {
        let relative_path = relative_path_unified(&base, file);
        for section in &parsed.sections {
            if !section.matches(&relative_path) {
                continue;
            }
            apply_section_properties(section, &mut matched);
        }
    }

    if matched.indent_style.is_none()
        && matched.indent_size.is_none()
        && matched.tab_width.is_none()
    {
        None
    } else {
        Some(matched)
    }
}

fn apply_section_properties(section: &Section, matched: &mut EditorConfigMatch) {
    if let Some(style) = section.properties.get("indent_style") {
        match style.as_str() {
            "space" => matched.indent_style = Some(IndentStyle::Space),
            "tab" => matched.indent_style = Some(IndentStyle::Tab),
            _ => {}
        }
    }

    if let Some(raw_size) = section.properties.get("indent_size") {
        if raw_size == "tab" {
            matched.indent_size = Some(IndentSize::Tab);
        } else if let Some(n) = parse_positive_integer(raw_size) {
            matched.indent_size = Some(IndentSize::Spaces(n));
        }
    }

    if let Some(n) = section
        .properties
        .get("tab_width")
        .and_then(|raw| parse_positive_integer(raw))
    {
        matched.tab_width = Some(n);
    }
}

/// Collect configurations from the file's directory up to a `root = true` hit (or the filesystem root), ordered far → near.
fn collect_editor_config_chain(start_dir: &Path) -> Vec<(PathBuf, Arc<ParsedEditorConfig>)> {
    let mut chain = Vec::new();
    let mut cursor = start_dir.to_path_buf();
    loop {
        let config_path = cursor.join(EDITORCONFIG_NAME);
        if let Some(parsed) = parse_cached(&config_path) {
            let is_root = parsed.root;
            chain.push((cursor.clone(), parsed));
            if is_root {
                break;
            }
        }

        let Some(parent) = cursor.parent() else {
            break;
        };
        if parent == cursor {
            break;
        }
        cursor = parent.to_path_buf();
    }
    chain.reverse();
    chain
}

/// Read and cache a single EditorConfig.
///
/// The cache key is the config path itself, and a hit requires both mtime and length unchanged. The fd is first opened with
/// `O_NONBLOCK | O_CLOEXEC`, then confirmed to be a regular file before reading: even if the path is swapped from a regular file to a
/// FIFO after opening, `read` will not hang forever; mtime/length and content all
/// come from the same fd, avoiding a path race between stat→read. Any failure (missing, a
/// directory/FIFO/device, no permission, non-UTF-8) writes no cache entry and never reports an error upward.
fn parse_cached(path: &Path) -> Option<Arc<ParsedEditorConfig>> {
    let fd = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .ok()?;
    let mut file = File::from(fd);
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() {
        return None;
    }
    let modified = metadata.modified().ok();
    let len = metadata.len();

    {
        let guard = EDITORCONFIG_CACHE
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(entry) = guard.get(path)
            && entry.len == len
            && entry.modified == modified
        {
            return Some(Arc::clone(&entry.parsed));
        }
    }

    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    let content = String::from_utf8(bytes).ok()?;
    let parsed = Arc::new(parse_editor_config(&content));

    let mut guard = EDITORCONFIG_CACHE
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if guard.len() >= MAX_CACHE_ENTRIES && !guard.contains_key(path) {
        guard.clear();
    }
    guard.insert(
        path.to_path_buf(),
        CacheEntry {
            modified,
            len,
            parsed: Arc::clone(&parsed),
        },
    );
    Some(parsed)
}

fn parse_editor_config(content: &str) -> ParsedEditorConfig {
    let mut parsed = ParsedEditorConfig {
        root: false,
        sections: Vec::new(),
    };
    let mut current: Option<usize> = None;

    for raw_line in content.split('\n') {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }

        if line.starts_with('[') && line.ends_with(']') && line.len() >= 2 {
            let pattern = line[1..line.len() - 1].trim();
            if pattern.is_empty() {
                current = None;
                continue;
            }
            let (primary, recursive) = compile_section_matchers(pattern);
            parsed.sections.push(Section {
                primary,
                recursive,
                properties: HashMap::new(),
            });
            current = Some(parsed.sections.len() - 1);
            continue;
        }

        let Some(equals) = line.find('=') else {
            continue;
        };
        let key = line[..equals].trim().to_ascii_lowercase();
        let value = line[equals + 1..].trim().to_ascii_lowercase();
        if key.is_empty() {
            continue;
        }

        match current {
            Some(index) => {
                parsed.sections[index].properties.insert(key, value);
            }
            // `root` only takes effect before any section.
            None if key == "root" => parsed.root = value == "true",
            None => {}
        }
    }

    parsed
}

/// Compile globs for a section pattern; a pattern without a slash also gets a recursive candidate.
fn compile_section_matchers(pattern: &str) -> (Option<GlobMatcher>, Option<GlobMatcher>) {
    let normalized = pattern.trim_start_matches('/');
    if normalized.is_empty() {
        return (None, None);
    }

    let primary = compile_glob(&build_glob_pattern(normalized, false));
    let recursive = if normalized.contains('/') {
        None
    } else {
        compile_glob(&build_glob_pattern(normalized, true))
    };
    (primary, recursive)
}

/// Mirror the upstream `buildGlobPattern`: unify separators and add a recursive prefix for patterns without a slash.
fn build_glob_pattern(glob: &str, recursive: bool) -> String {
    let normalized = glob.replace('\\', "/");
    let mut pattern = if !recursive || normalized.contains('/') || normalized.starts_with("**") {
        normalized
    } else {
        format!("**/{normalized}")
    };
    fix_unclosed_braces(&mut pattern);
    pattern
}

fn fix_unclosed_braces(pattern: &mut String) {
    let opens = pattern.chars().filter(|c| *c == '{').count();
    let closes = pattern.chars().filter(|c| *c == '}').count();
    for _ in 0..opens.saturating_sub(closes) {
        pattern.push('}');
    }
}

fn compile_glob(pattern: &str) -> Option<GlobMatcher> {
    globset::GlobBuilder::new(pattern)
        // EditorConfig's `*` does not cross `/`; recursive matching for patterns without a slash is handled by
        // the extra `**/` candidate instead of letting `*` swallow separators.
        .literal_separator(true)
        .build()
        .ok()
        .map(|glob| glob.compile_matcher())
}

/// `parsePositiveInteger`: accepts only plain decimal digits, treats 0 as invalid, and clamps the result to 1..=16.
fn parse_positive_integer(raw: &str) -> Option<u64> {
    if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    // Overlong numbers behave like JS `parseInt` saturation, then clamp to the upper bound.
    let parsed = raw.parse::<u64>().unwrap_or(u64::MAX);
    if parsed == 0 {
        return None;
    }
    Some(parsed.clamp(MIN_TAB_SIZE, MAX_TAB_SIZE))
}

/// Compute the relative path with unified `/` separators; returns `.` when the file is not under the base directory.
fn relative_path_unified(base: &Path, file: &Path) -> String {
    match file.strip_prefix(base) {
        Ok(relative) => relative
            .components()
            .map(|component| component.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/"),
        Err(_) => ".".to_string(),
    }
}

fn has_overlong_component(path: &Path) -> bool {
    path.components()
        .any(|component| component.as_os_str().as_encoded_bytes().len() > NAME_MAX_BYTES)
}

/// Lexical absolutize + normalize (without resolving symlinks), matching `path.resolve`/`path.normalize`.
fn absolute_lexical(path: &Path) -> PathBuf {
    let base = if path.is_absolute() {
        path.to_path_buf()
    } else {
        match std::env::current_dir() {
            Ok(cwd) => cwd.join(path),
            Err(_) => path.to_path_buf(),
        }
    };

    let mut normalized = PathBuf::new();
    for component in base.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() && !normalized.has_root() {
                    normalized.push("..");
                }
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }

    if normalized.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        normalized
    }
}

#[cfg(test)]
mod tests {
    use super::{EDITORCONFIG_NAME, resolve};
    use serde_json::{Value, json};
    use std::fs;
    use std::io::Write as _;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime};

    /// Isolated directory: built-in `root = true`, blocking any upward read of configuration on the developer machine.
    fn project() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join(EDITORCONFIG_NAME), "root = true\n").expect("root marker");
        dir
    }

    /// Each scenario uses its own temp directory, so the global cache and real parent-directory configuration cannot interfere.
    fn scenario(editorconfig: &str, file: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(
            dir.path().join(EDITORCONFIG_NAME),
            format!("root = true\n{editorconfig}"),
        )
        .expect("write editorconfig");
        let path = dir.path().join(file);
        (dir, path)
    }

    fn write(path: &Path, contents: &str) {
        fs::write(path, contents).expect("write fixture");
    }

    fn tab_size(options: &Value) -> u64 {
        options["tabSize"].as_u64().expect("tabSize")
    }

    fn insert_spaces(options: &Value) -> bool {
        options["insertSpaces"].as_bool().expect("insertSpaces")
    }

    #[test]
    fn lsp_format_options_content_inference_and_fallback() {
        {
            let dir = project();
            let options = resolve(&dir.path().join("notes.txt"), "no indentation\n");

            assert_eq!(tab_size(&options), 2);
            assert!(insert_spaces(&options));
            assert_eq!(options["trimTrailingWhitespace"], json!(true));
            assert_eq!(options["insertFinalNewline"], json!(true));
            assert_eq!(options["trimFinalNewlines"], json!(true));
        }

        {
            let dir = project();

            let two = resolve(
                &dir.path().join("two.rs"),
                "fn a() {\n  b();\n    c();\n      d();\n}\n",
            );
            assert_eq!(tab_size(&two), 2);
            assert!(insert_spaces(&two));

            let four = resolve(
                &dir.path().join("four.rs"),
                "fn a() {\n    b();\n        c();\n}\n",
            );
            assert_eq!(tab_size(&four), 4);
            assert!(insert_spaces(&four));
        }

        {
            let dir = project();

            let tabbed = resolve(&dir.path().join("tab.rs"), "\tfirst\n    second\n");
            assert!(!insert_spaces(&tabbed));
            assert_eq!(tab_size(&tabbed), 2);

            let spaced = resolve(&dir.path().join("space.rs"), "    first\n\tsecond\n");
            assert!(insert_spaces(&spaced));
            assert_eq!(tab_size(&spaced), 4);
        }

        {
            let dir = project();

            let blank = resolve(&dir.path().join("blank.rs"), "\n   \n\t\n  x\n");
            assert_eq!(tab_size(&blank), 2);
            assert!(insert_spaces(&blank));

            let crlf = resolve(&dir.path().join("crlf.rs"), "fn a() {\r\n    x\r\n}\r\n");
            assert_eq!(tab_size(&crlf), 4);
            assert!(insert_spaces(&crlf));
        }
    }

    #[test]
    fn lsp_format_options_editorconfig_inheritance_globs_and_precedence() {
        {
            let dir = tempfile::tempdir().expect("tempdir");
            write(
                &dir.path().join(EDITORCONFIG_NAME),
                "root = true\n[*.rs]\nindent_style = space\nindent_size = 4\n",
            );

            let nested = dir.path().join("crate");
            fs::create_dir(&nested).expect("nested dir");
            write(
                &nested.join(EDITORCONFIG_NAME),
                "[*.rs]\nindent_style = tab\n",
            );

            let inherited = resolve(&nested.join("lib.rs"), "fn a() {}\n");
            assert_eq!(tab_size(&inherited), 4);
            assert!(!insert_spaces(&inherited));

            let sections = dir.path().join("sections");
            fs::create_dir(&sections).expect("sections dir");
            write(
                &sections.join(EDITORCONFIG_NAME),
                "[*.rs]\nindent_size = 2\n[lib.rs]\nindent_size = 6\n",
            );
            assert_eq!(tab_size(&resolve(&sections.join("lib.rs"), "")), 6);
            assert_eq!(tab_size(&resolve(&sections.join("mod.rs"), "")), 2);
            assert_eq!(tab_size(&resolve(&sections.join("other.rs"), "")), 2);
        }

        {
            let dir = tempfile::tempdir().expect("tempdir");
            write(
                &dir.path().join(EDITORCONFIG_NAME),
                "root = true\n[*.rs]\nindent_size = 8\n",
            );

            let nested = dir.path().join("pkg");
            fs::create_dir(&nested).expect("nested dir");

            assert_eq!(tab_size(&resolve(&nested.join("a.rs"), "")), 8);

            write(&nested.join(EDITORCONFIG_NAME), "root = true\n");
            assert_eq!(tab_size(&resolve(&nested.join("b.rs"), "")), 2);
        }

        {
            let (dir, path) = scenario(
                "[*.rs]\nindent_size = 2\n\
                 [src/*.ts]\nindent_size = 8\n\
                 [*.{md,txt}]\nindent_size = 6\n\
                 [/special/*.go]\nindent_size = 5\n",
                "src/deep/lib.rs",
            );

            assert_eq!(tab_size(&resolve(&path, "    x\n")), 2);

            assert_eq!(
                tab_size(&resolve(&dir.path().join("src/direct.ts"), "    x\n")),
                8
            );
            assert_eq!(
                tab_size(&resolve(&dir.path().join("src/deep/lib.ts"), "    x\n")),
                4
            );

            assert_eq!(
                tab_size(&resolve(&dir.path().join("README.md"), "    x\n")),
                6
            );
            assert_eq!(
                tab_size(&resolve(&dir.path().join("docs/readme.txt"), "    x\n")),
                6
            );

            assert_eq!(
                tab_size(&resolve(&dir.path().join("special/x.go"), "    x\n")),
                5
            );
            assert_eq!(
                tab_size(&resolve(&dir.path().join("src/deep/y.go"), "    x\n")),
                4
            );
        }

        {
            let (_tab_style, tab_style) =
                scenario("[*]\nindent_style = tab\nindent_size = 8\n", "a.rs");
            let options = resolve(&tab_style, "  fn a() {}\n");
            assert_eq!(tab_size(&options), 8);
            assert!(!insert_spaces(&options));

            let (_tab_width, tab_width) =
                scenario("[*]\nindent_style = tab\ntab_width = 6\n", "a.rs");
            let options = resolve(&tab_width, "");
            assert_eq!(tab_size(&options), 6);
            assert!(!insert_spaces(&options));

            let (_size_tab, size_tab) = scenario("[*]\nindent_size = tab\ntab_width = 3\n", "a.rs");
            let options = resolve(&size_tab, "");
            assert_eq!(tab_size(&options), 3);
            assert!(!insert_spaces(&options));

            let (_spaces, spaces) = scenario("[*]\nindent_size = 3\n", "a.rs");
            let options = resolve(&spaces, "");
            assert_eq!(tab_size(&options), 3);
            assert!(insert_spaces(&options));

            let (_size_wins, size_wins) = scenario("[*]\nindent_size = 4\ntab_width = 8\n", "a.rs");
            assert_eq!(tab_size(&resolve(&size_wins, "")), 4);

            let (_clamped, clamped) = scenario("[*]\nindent_size = 99\n", "a.rs");
            assert_eq!(tab_size(&resolve(&clamped, "")), 16);

            let (_zero, zero) = scenario("[*]\nindent_size = 0\ntab_width = 0\n", "a.rs");
            let options = resolve(&zero, "    x\n");
            assert_eq!(tab_size(&options), 4);
            assert!(insert_spaces(&options));
        }
    }

    #[test]
    fn lsp_format_options_invalid_unreadable_and_fifo_fallback() {
        {
            let (_dir, path) = scenario(
                "this is not a key value\n\
                 [*.rs]\n\
                 indent_size = banana\n\
                 tab_width = 0\n",
                "a.rs",
            );
            let options = resolve(&path, "");
            assert_eq!(tab_size(&options), 2);
            assert!(insert_spaces(&options));

            let (_reset_dir, reset_path) =
                scenario("[*]\nindent_size = 6\n[]\nindent_size = 2\n", "a.rs");
            assert_eq!(tab_size(&resolve(&reset_path, "")), 6);
        }

        {
            let dir = project();
            let nested = dir.path().join("blocked");
            fs::create_dir(&nested).expect("nested dir");
            fs::create_dir(nested.join(EDITORCONFIG_NAME)).expect("config dir");

            let options = resolve(&nested.join("a.rs"), "    x\n");
            assert_eq!(tab_size(&options), 4);
            assert!(insert_spaces(&options));
        }

        {
            let dir = project();

            let pipe_dir = dir.path().join("pipe");
            fs::create_dir(&pipe_dir).expect("pipe dir");
            let pipe = pipe_dir.join(EDITORCONFIG_NAME);
            let status = std::process::Command::new("mkfifo")
                .arg(&pipe)
                .status()
                .expect("spawn mkfifo");
            assert!(status.success(), "mkfifo failed with {status}");

            let link_dir = dir.path().join("link");
            fs::create_dir(&link_dir).expect("link dir");
            std::os::unix::fs::symlink(&pipe, link_dir.join(EDITORCONFIG_NAME))
                .expect("symlink to fifo");

            assert_eq!(tab_size(&resolve(&pipe_dir.join("a.rs"), "    x\n")), 4);
            assert_eq!(tab_size(&resolve(&link_dir.join("a.rs"), "    x\n")), 4);
        }
    }

    #[test]
    fn lsp_format_options_cache_invalidation() {
        {
            let dir = project();
            let config = dir.path().join(EDITORCONFIG_NAME);
            write(&config, "root = true\n[*.rs]\nindent_size = 2\n");
            let path = dir.path().join("a.rs");
            assert_eq!(tab_size(&resolve(&path, "")), 2);

            let mut file = fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(&config)
                .expect("open config");
            file.write_all(b"root = true\n[*.rs]\nindent_size = 4\n")
                .expect("rewrite config");
            file.flush().expect("flush");
            let times =
                fs::FileTimes::new().set_modified(SystemTime::now() + Duration::from_secs(30));
            file.set_times(times).expect("set mtime");
            drop(file);

            assert_eq!(tab_size(&resolve(&path, "")), 4);
        }

        {
            let dir = project();
            let nested = dir.path().join("sub");
            fs::create_dir(&nested).expect("nested dir");
            let config = nested.join(EDITORCONFIG_NAME);
            write(&config, "[*.rs]\nindent_size = 8\n");

            let path = nested.join("a.rs");
            assert_eq!(tab_size(&resolve(&path, "")), 8);

            fs::remove_file(&config).expect("remove config");
            assert_eq!(tab_size(&resolve(&path, "")), 2);
        }
    }
}
