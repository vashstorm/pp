//! 每文件 LSP `FormattingOptions` 解析。
//!
//! 复刻 oh-my-pi 固定提交
//! `1c291b87e4b889398cc4b8e4ccdfa650b56a0486` 的
//! `packages/coding-agent/src/lsp/format-options.ts` 与
//! `packages/utils/src/tab-spacing.ts`。优先级由高到低：
//! 1. 文件所在目录链上的 `.editorconfig`（`indent_style`、`indent_size`、`tab_width`）；
//! 2. 待写内容里第一个缩进行揭示的缩进风格，以及所有空格缩进宽度的 GCD；
//! 3. 固定回退：2 空格。
//!
//! `.editorconfig` 发现只读文件所在目录向上的父链，不扩大任何写权限；配置
//! 不存在、不可读或内容无效时静默忽略并继续回退，不阻止保存。解析结果按
//! 配置路径的 mtime 与长度缓存，文件变化后自动失效。

use globset::GlobMatcher;
use rustix::fs::{Mode, OFlags};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::SystemTime;

/// 父链中的 EditorConfig 文件名。
const EDITORCONFIG_NAME: &str = ".editorconfig";
/// `tabSize` 允许范围：EditorConfig 值解析与最终结果都钳制到该区间。
const MIN_TAB_SIZE: u64 = 1;
const MAX_TAB_SIZE: u64 = 16;
/// EditorConfig 与内容都无法确定时的回退值：2 空格。
const FALLBACK_TAB_SIZE: u64 = 2;
const FALLBACK_INSERT_SPACES: bool = true;
/// 单个路径组件上限（Linux ext4 / macOS APFS / NTFS 的 `NAME_MAX`）。
/// 超过后文件本身无法打开，直接跳过 EditorConfig 发现。
const NAME_MAX_BYTES: usize = 255;
/// 解析缓存上限；达到后整体清空，避免长时间运行后无界增长。
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

/// 由 `.editorconfig` 链解析出的缩进提示；`None` 表示该链未固定这一项。
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

/// 一个 `[pattern]` 小节。
///
/// glob 在解析时编译并随缓存复用；无斜杠 pattern 额外生成递归候选，
/// 使 `*.rs` 匹配任意深度的文件（EditorConfig 语义）。
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

/// 单份配置的解析缓存项：mtime + 长度变化即失效。
#[derive(Clone, Debug)]
struct CacheEntry {
    modified: Option<SystemTime>,
    len: u64,
    parsed: Arc<ParsedEditorConfig>,
}

static EDITORCONFIG_CACHE: LazyLock<Mutex<HashMap<PathBuf, CacheEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 解析 `path` 对应文件的 LSP `FormattingOptions`。
///
/// `path` 可以不存在，只用于定位所在目录；相对路径按当前工作目录解析。
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

/// 从待写内容推断缩进；不补齐默认值，让调用方继续向外层回退。
fn detect_indent_from_content(content: &str) -> EditorConfigFormatting {
    if content.is_empty() {
        return EditorConfigFormatting::default();
    }

    let mut insert_spaces: Option<bool> = None;
    let mut unit = 0u64;

    for line in content.split('\n') {
        // 空白行不携带缩进信号。
        if line.is_empty() || line.trim().is_empty() {
            continue;
        }

        let first = line.as_bytes()[0];
        if first != b' ' && first != b'\t' {
            continue;
        }

        // 第一个缩进行决定空格还是制表符；后续行不会改写它。
        if insert_spaces.is_none() {
            insert_spaces = Some(first == b' ');
        }

        // 制表符缩进时宽度是显示问题，交给 EditorConfig/回退值。
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
        // 只有首个缩进行是空格时才用 GCD 作为步进；制表符文件的
        // `tabSize` 由 EditorConfig 或固定回退值决定。
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

/// 解析文件自己的 `.editorconfig` 链（远到近），得到格式化提示。
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

/// 合并父链上所有匹配小节；越靠近文件的配置越晚应用，从而覆盖外层。
fn resolve_editor_config_match(file: &Path) -> Option<EditorConfigMatch> {
    let dir = file.parent()?;
    let chain = collect_editor_config_chain(dir);
    if chain.is_empty() {
        return None;
    }

    let mut matched = EditorConfigMatch::default();
    // 链已在收集时反转为远 → 近。
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

/// 收集从文件目录到命中 `root = true`（或文件系统根）的配置，按远 → 近排列。
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

/// 读取并缓存单份 EditorConfig。
///
/// 缓存键是配置路径本身，命中条件是 mtime 与长度都未变化。先用
/// `O_NONBLOCK | O_CLOEXEC` 打开 fd，再确认它是普通文件并读取：即使路径在
/// 打开后从普通文件被换成 FIFO，`read` 也不会永久挂住；mtime/长度与内容都
/// 来自同一个 fd，避免 stat→read 之间的路径竞态。任何失败（不存在、是
/// 目录/FIFO/设备、无权限、非 UTF-8）都不写缓存，也绝不向上报错。
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
            // `root` 只在任何小节之前生效。
            None if key == "root" => parsed.root = value == "true",
            None => {}
        }
    }

    parsed
}

/// 为小节 pattern 编译 glob；无斜杠 pattern 同时生成递归候选。
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

/// 复刻上游 `buildGlobPattern`：统一分隔符、为无斜杠 pattern 加递归前缀。
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
        // EditorConfig 的 `*` 不跨 `/`；无斜杠 pattern 的递归匹配由
        // 额外的 `**/` 候选负责，而不是让 `*` 吞掉分隔符。
        .literal_separator(true)
        .build()
        .ok()
        .map(|glob| glob.compile_matcher())
}

/// `parsePositiveInteger`：只接受纯十进制数字，0 视为无效，结果钳制到 1..=16。
fn parse_positive_integer(raw: &str) -> Option<u64> {
    if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    // 超长数字按 JS `parseInt` 的饱和行为处理，再钳制到上限。
    let parsed = raw.parse::<u64>().unwrap_or(u64::MAX);
    if parsed == 0 {
        return None;
    }
    Some(parsed.clamp(MIN_TAB_SIZE, MAX_TAB_SIZE))
}

/// 计算相对路径并统一为 `/` 分隔；不在基准目录下时返回 `.`。
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

/// 词法绝对化 + 规范化（不解析符号链接），对应 `path.resolve`/`path.normalize`。
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

    /// 隔离目录：内置 `root = true`，阻止向上读取开发者机器上的配置。
    fn project() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join(EDITORCONFIG_NAME), "root = true\n").expect("root marker");
        dir
    }

    /// 每次场景使用独立临时目录，避免全局缓存与真实父目录配置干扰。
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
    fn lsp_format_options_fallback_is_two_spaces() {
        let dir = project();
        let options = resolve(&dir.path().join("notes.txt"), "no indentation\n");

        assert_eq!(tab_size(&options), 2);
        assert!(insert_spaces(&options));
        assert_eq!(options["trimTrailingWhitespace"], json!(true));
        assert_eq!(options["insertFinalNewline"], json!(true));
        assert_eq!(options["trimFinalNewlines"], json!(true));
    }

    #[test]
    fn lsp_format_options_content_space_gcd() {
        let dir = project();

        // 2/4/6 的 GCD 是 2。
        let two = resolve(
            &dir.path().join("two.rs"),
            "fn a() {\n  b();\n    c();\n      d();\n}\n",
        );
        assert_eq!(tab_size(&two), 2);
        assert!(insert_spaces(&two));

        // 4/8 的 GCD 是 4。
        let four = resolve(
            &dir.path().join("four.rs"),
            "fn a() {\n    b();\n        c();\n}\n",
        );
        assert_eq!(tab_size(&four), 4);
        assert!(insert_spaces(&four));
    }

    #[test]
    fn lsp_format_options_content_first_line_picks_style() {
        let dir = project();

        // 第一个缩进行是制表符：即使后面出现空格缩进也不改回空格。
        let tabbed = resolve(&dir.path().join("tab.rs"), "\tfirst\n    second\n");
        assert!(!insert_spaces(&tabbed));
        assert_eq!(tab_size(&tabbed), 2);

        // 第一个缩进行是空格：后续制表符行不参与 GCD。
        let spaced = resolve(&dir.path().join("space.rs"), "    first\n\tsecond\n");
        assert!(insert_spaces(&spaced));
        assert_eq!(tab_size(&spaced), 4);
    }

    #[test]
    fn lsp_format_options_content_skips_blank_lines_and_crlf() {
        let dir = project();

        let blank = resolve(&dir.path().join("blank.rs"), "\n   \n\t\n  x\n");
        assert_eq!(tab_size(&blank), 2);
        assert!(insert_spaces(&blank));

        let crlf = resolve(&dir.path().join("crlf.rs"), "fn a() {\r\n    x\r\n}\r\n");
        assert_eq!(tab_size(&crlf), 4);
        assert!(insert_spaces(&crlf));
    }

    #[test]
    fn lsp_format_options_editorconfig_merges_far_to_near() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            &dir.path().join(EDITORCONFIG_NAME),
            "root = true\n[*.rs]\nindent_style = space\nindent_size = 4\n",
        );

        let nested = dir.path().join("crate");
        fs::create_dir(&nested).expect("nested dir");
        // 近层只覆盖风格，宽度继续继承外层。
        write(
            &nested.join(EDITORCONFIG_NAME),
            "[*.rs]\nindent_style = tab\n",
        );

        let inherited = resolve(&nested.join("lib.rs"), "fn a() {}\n");
        assert_eq!(tab_size(&inherited), 4);
        assert!(!insert_spaces(&inherited));

        // 同一文件内后出现的小节覆盖先出现的小节。
        let sections = dir.path().join("sections");
        fs::create_dir(&sections).expect("sections dir");
        write(
            &sections.join(EDITORCONFIG_NAME),
            "[*.rs]\nindent_size = 2\n[lib.rs]\nindent_size = 6\n",
        );
        assert_eq!(tab_size(&resolve(&sections.join("lib.rs"), "")), 6);
        assert_eq!(tab_size(&resolve(&sections.join("mod.rs"), "")), 2);

        // 外层 4 被近层 2 覆盖。
        assert_eq!(tab_size(&resolve(&sections.join("other.rs"), "")), 2);
    }

    #[test]
    fn lsp_format_options_editorconfig_root_stops_parent_chain() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            &dir.path().join(EDITORCONFIG_NAME),
            "root = true\n[*.rs]\nindent_size = 8\n",
        );

        let nested = dir.path().join("pkg");
        fs::create_dir(&nested).expect("nested dir");

        // 近层没有 root：继续继承外层 8。
        assert_eq!(tab_size(&resolve(&nested.join("a.rs"), "")), 8);

        // 近层声明 root=true 后，外层不再参与。
        write(&nested.join(EDITORCONFIG_NAME), "root = true\n");
        assert_eq!(tab_size(&resolve(&nested.join("b.rs"), "")), 2);
    }

    #[test]
    fn lsp_format_options_editorconfig_globs_match_descendants() {
        let (dir, path) = scenario(
            "[*.rs]\nindent_size = 2\n\
             [src/*.ts]\nindent_size = 8\n\
             [*.{md,txt}]\nindent_size = 6\n\
             [/special/*.go]\nindent_size = 5\n",
            "src/deep/lib.rs",
        );

        // 无斜杠 pattern 匹配任意深度的同名文件。
        assert_eq!(tab_size(&resolve(&path, "    x\n")), 2);

        // 含斜杠 pattern 只匹配自己目录的直接子文件；不匹配时回退内容缩进 4。
        assert_eq!(
            tab_size(&resolve(&dir.path().join("src/direct.ts"), "    x\n")),
            8
        );
        assert_eq!(
            tab_size(&resolve(&dir.path().join("src/deep/lib.ts"), "    x\n")),
            4
        );

        // 花括号候选与子目录同样按无斜杠规则递归。
        assert_eq!(
            tab_size(&resolve(&dir.path().join("README.md"), "    x\n")),
            6
        );
        assert_eq!(
            tab_size(&resolve(&dir.path().join("docs/readme.txt"), "    x\n")),
            6
        );

        // 前导斜杠被忽略。
        assert_eq!(
            tab_size(&resolve(&dir.path().join("special/x.go"), "    x\n")),
            5
        );
        assert_eq!(
            tab_size(&resolve(&dir.path().join("src/deep/y.go"), "    x\n")),
            4
        );
    }

    #[test]
    fn lsp_format_options_editorconfig_style_and_size_precedence() {
        let (_tab_style, tab_style) =
            scenario("[*]\nindent_style = tab\nindent_size = 8\n", "a.rs");
        let options = resolve(&tab_style, "  fn a() {}\n");
        assert_eq!(tab_size(&options), 8);
        assert!(!insert_spaces(&options));

        let (_tab_width, tab_width) = scenario("[*]\nindent_style = tab\ntab_width = 6\n", "a.rs");
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

        // `indent_size` 优先于 `tab_width`。
        let (_size_wins, size_wins) = scenario("[*]\nindent_size = 4\ntab_width = 8\n", "a.rs");
        assert_eq!(tab_size(&resolve(&size_wins, "")), 4);

        let (_clamped, clamped) = scenario("[*]\nindent_size = 99\n", "a.rs");
        assert_eq!(tab_size(&resolve(&clamped, "")), 16);

        let (_zero, zero) = scenario("[*]\nindent_size = 0\ntab_width = 0\n", "a.rs");
        let options = resolve(&zero, "    x\n");
        assert_eq!(tab_size(&options), 4);
        assert!(insert_spaces(&options));
    }

    #[test]
    fn lsp_format_options_invalid_editorconfig_falls_back() {
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

        // `[]` 结束前一个小节，之后的属性不归属任何小节。
        let (_reset_dir, reset_path) =
            scenario("[*]\nindent_size = 6\n[]\nindent_size = 2\n", "a.rs");
        assert_eq!(tab_size(&resolve(&reset_path, "")), 6);
    }

    #[test]
    fn lsp_format_options_unreadable_editorconfig_is_ignored() {
        let dir = project();
        let nested = dir.path().join("blocked");
        fs::create_dir(&nested).expect("nested dir");
        // 同名目录无法作为配置文件读取，发现过程必须静默跳过。
        fs::create_dir(nested.join(EDITORCONFIG_NAME)).expect("config dir");

        let options = resolve(&nested.join("a.rs"), "    x\n");
        assert_eq!(tab_size(&options), 4);
        assert!(insert_spaces(&options));
    }

    #[test]
    fn lsp_format_options_fifo_ignored() {
        let dir = project();

        let pipe_dir = dir.path().join("pipe");
        fs::create_dir(&pipe_dir).expect("pipe dir");
        let pipe = pipe_dir.join(EDITORCONFIG_NAME);
        let status = std::process::Command::new("mkfifo")
            .arg(&pipe)
            .status()
            .expect("spawn mkfifo");
        assert!(status.success(), "mkfifo failed with {status}");

        // 经符号链接指向 FIFO 的配置同样必须被忽略。
        let link_dir = dir.path().join("link");
        fs::create_dir(&link_dir).expect("link dir");
        std::os::unix::fs::symlink(&pipe, link_dir.join(EDITORCONFIG_NAME))
            .expect("symlink to fifo");

        // 读取使用 O_NONBLOCK 打开 fd 后先看类型，FIFO 既不会挂住也不会产生配置。
        assert_eq!(tab_size(&resolve(&pipe_dir.join("a.rs"), "    x\n")), 4);
        assert_eq!(tab_size(&resolve(&link_dir.join("a.rs"), "    x\n")), 4);
    }

    #[test]
    fn lsp_format_options_cache_refreshes_changed_config() {
        let dir = project();
        let config = dir.path().join(EDITORCONFIG_NAME);
        write(&config, "root = true\n[*.rs]\nindent_size = 2\n");
        let path = dir.path().join("a.rs");
        assert_eq!(tab_size(&resolve(&path, "")), 2);

        // 等长改写（只看长度无法区分）+ 强制 mtime 前进，验证缓存会失效。
        let mut file = fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&config)
            .expect("open config");
        file.write_all(b"root = true\n[*.rs]\nindent_size = 4\n")
            .expect("rewrite config");
        file.flush().expect("flush");
        let times = fs::FileTimes::new().set_modified(SystemTime::now() + Duration::from_secs(30));
        file.set_times(times).expect("set mtime");
        drop(file);

        assert_eq!(tab_size(&resolve(&path, "")), 4);
    }

    #[test]
    fn lsp_format_options_cache_sees_deleted_config() {
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
